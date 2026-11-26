#!/usr/bin/env node
/**
 * 多上游免费模型自动路由网关
 *
 * 对外一个标准 OpenAI 兼容端点，内部按质量分组把请求路由到十多家上游
 * （NVIDIA NIM / 智谱 / OpenRouter / Gemini / 魔搭 / Groq / LongCat ...），
 * 遇到限流、5xx、超时、空响应时自动降级到下一个候选。
 *
 * 队列项格式：`provider::model`，例如 `nvidia::moonshotai/kimi-k3.1-pro`。
 * 不带 `::` 的裸模型名会落到 config.defaultProvider。
 *
 * 踩过的坑（都在代码里做了处理）：
 *  - 外发一律走 /usr/bin/curl：Node fetch 不读 HTTPS_PROXY，国内直连必超时。
 *  - 请求体从 stdin 喂给 curl（--data-binary @-），避免中文/长 body 转义问题。
 *  - 流式请求只有在「拿到 HTTP 状态行」之前才能改主意；200 一旦开始吐 body 就锁定模型。
 *  - 思考型模型（kimi/step/muse/mistral-large）会把 max_tokens 吃光，返回
 *    content=null + reasoning_content，直接判空会误杀。这里认 reasoning_content，
 *    并在 content 为空时把它兜底填进 content，否则客户端看到一片空白。
 *  - 缺 apiKey 的 provider 不参与队列（跳过而非算失败），/health 里单独报。
 */
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync, renameSync, appendFileSync, existsSync, rmSync, openSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { doubaoChat, doubaoCheckin, doubaoQuota, doubaoStatus, startAutoCheckin, doubaoCatalog, doubaoModelIds } from './doubao_bridge.mjs';

/** 官方清单还没拉到时用的两条内置模型（快速 / 专家） */
const DOUBAO_DEFAULT_IDS = ['doubao', 'doubao-turbo'];
import { antigravityChat, antigravityQuota, antigravityStatus, antigravityStream, ANTIGRAVITY_MODELS } from './antigravity.mjs';

const DIR = path.dirname(fileURLToPath(import.meta.url));
// 运行中的脚本版本：app 每次解包会写 .bundle-version，/health 上报它，
// 方便判断「进程跑的是不是当前安装包里的脚本」（升级后没重启网关就会不一致）。
const BUNDLE_VERSION = (() => {
  try { return readFileSync(path.join(DIR, '.bundle-version'), 'utf8').trim(); } catch { return ''; }
})();

const CONFIG_PATH = path.join(DIR, 'config.json');
const STATUS_PATH = path.join(DIR, '.orcaterm-login-status.json');
const DOUBAO_STATUS_PATH = path.join(DIR, '.doubao-login-status.json');
const ANTIGRAVITY_STATUS_PATH = path.join(DIR, '.antigravity-login-status.json');
// 「重新登录」时旧 cookie 还在，登录器跑完前不能把旧登录态报成成功，否则前端轮询会提前停。
let ORCA_LOGIN_PENDING = false;
let ORCA_LOGIN_PENDING_AT = 0;
let CFG = JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));

// 外发一律走系统 curl：Node fetch 不读 HTTPS_PROXY，国内直连必超时。
// Windows 自带 curl.exe（System32），macOS 在 /usr/bin/curl。
const CURL = process.env.CURL_PATH || (process.platform === 'win32'
  ? (existsSync('C:\\Windows\\System32\\curl.exe') ? 'C:\\Windows\\System32\\curl.exe' : 'curl.exe')
  : '/usr/bin/curl');

/** 启动子进程（各平台登录器）用的 node：先自己的，再系统常见的；Windows 认 node.exe。 */
function nodeCandidates() {
  const local = path.join(DIR, process.platform === 'win32' ? 'node.exe' : 'node');
  if (process.platform === 'win32') {
    const extra = [];
    if (process.env.ProgramFiles) extra.push(path.join(process.env.ProgramFiles, 'nodejs', 'node.exe'));
    if (process.env['ProgramFiles(x86)']) extra.push(path.join(process.env['ProgramFiles(x86)'], 'nodejs', 'node.exe'));
    if (process.env.LOCALAPPDATA) extra.push(path.join(process.env.LOCALAPPDATA, 'Programs', 'nodejs', 'node.exe'));
    return [process.execPath, local, ...extra];
  }
  return [process.execPath, local, '/opt/homebrew/bin/node', '/usr/local/bin/node', '/usr/bin/node'];
}
const PORT = CFG.port ?? 8901;
const BIND = CFG.bind ?? '127.0.0.1';
const LOG_FILE = path.join(DIR, CFG.logFile ?? 'gateway.log');
let TIER = CFG.tier || 'high';
let TIERS = CFG.tiers || {};
const ATTEMPT_TIMEOUT = (CFG.attemptTimeoutMs ?? 30000) / 1000;
const MAX_ATTEMPTS = CFG.maxAttempts ?? 4;
const TOTAL_BUDGET = CFG.totalBudgetMs ?? 45000;
const COOLDOWN_429 = CFG.cooldown429Ms ?? 60000;
const COOLDOWN_5XX = CFG.cooldown5xxMs ?? 30000;
const COOLDOWN_TIMEOUT = CFG.cooldownTimeoutMs ?? 120000;
// 豆包风控（710022002/710022004 人机验证、710012001 登录失效）是账号级状态：
// 换模型没用，而且越换越像异常流量，会把账号晾得更久 —— 整组冷却，别再打。
const COOLDOWN_RISK = CFG.cooldownRiskMs ?? 30 * 60 * 1000;
// 账号级额度耗尽（OpenRouter free-models-per-day）冷却多久：到次日 UTC 0 点太复杂，
// 统一 6 小时，够跳过当天剩余时间又不会永久报废这个 provider
const QUOTA_COOLDOWN = CFG.cooldownQuotaMs ?? 6 * 3600 * 1000;

// ── provider 表 ───────────────────────────────────────────────────────────
let PROVIDERS = { ...(CFG.providers || {}) };
// 兼容旧的单上游配置
if (!CFG.providers && CFG.endpoint) {
  PROVIDERS.default = { endpoint: CFG.endpoint, apiKey: CFG.apiKey || '', proxy: CFG.proxy || '' };
  CFG.defaultProvider = 'default';
}
let DEFAULT_PROVIDER = CFG.defaultProvider
  || Object.keys(PROVIDERS).find((k) => PROVIDERS[k].apiKey)
  || Object.keys(PROVIDERS)[0];

// 老用户的 config.json 不会被 app 覆盖；启动时补齐豆包 / Antigravity / 注入开关默认项，
// 保证升级后就有这些东西。
//
// 注入开关（CFG.inject）是「哪些模型写进 WorkBuddy models.json」的唯一事实来源，
// 三个字段的含义：
//   enabled —— 这个平台整体开不开关；关掉就从 models.json 里清掉该平台全部条目
//   rotate  —— 「自动轮换」这个虚拟模型内部依次试哪些模型
//   inject  —— 真正写进 WorkBuddy 的条目，默认只有 ['auto']（单独模型默认不写，免得列表太长）
function ensureDefaults() {
  let changed = false;
  if (!CFG.providers) { CFG.providers = {}; changed = true; }
  if (!CFG.providers.doubao) {
    CFG.providers.doubao = {
      kind: 'doubao', allowNoKey: true, apiKey: '',
      endpoint: 'https://www.doubao.com/chat/completion',
      cookie: '', device_id: '', web_id: '', fp: '',
      note: '豆包网页额度 · 未登录', timeoutMs: 180000,
    };
    changed = true;
  }
  if (!CFG.doubao) {
    CFG.doubao = { models: [...DOUBAO_DEFAULT_IDS], autoCheckin: true };
    changed = true;
  }
  if (!CFG.providers.antigravity) {
    CFG.providers.antigravity = {
      kind: 'antigravity', allowNoKey: true, apiKey: '',
      endpoint: 'https://cloudcode-pa.googleapis.com/v1internal',
      proxy: '', refresh_token: '', access_token: '', expiry: 0,
      project: '', email: '', tier: '',
      note: 'Antigravity · 未登录', timeoutMs: 180000,
    };
    changed = true;
  }
  if (!CFG.antigravity) {
    CFG.antigravity = { models: [], labels: {}, quota: null };
    changed = true;
  }
  if (!CFG.inject) { CFG.inject = {}; changed = true; }
  const seed = {
    orcaterm: { rotate: CFG.orcaterm?.models || [] },
    doubao: { rotate: CFG.doubao?.models || [...DOUBAO_DEFAULT_IDS] },
    antigravity: { rotate: CFG.antigravity?.models || [] },
  };
  for (const [name, s] of Object.entries(seed)) {
    if (!CFG.inject[name]) {
      CFG.inject[name] = { enabled: true, rotate: s.rotate, inject: ['auto'] };
      changed = true;
      continue;
    }
    const cur = CFG.inject[name];
    if (typeof cur.enabled !== 'boolean') { cur.enabled = true; changed = true; }
    if (!Array.isArray(cur.rotate)) { cur.rotate = s.rotate; changed = true; }
    if (!Array.isArray(cur.inject) || !cur.inject.length) { cur.inject = ['auto']; changed = true; }
  }
  // 0.12.x 写的 doubao-think / doubao-expert 是错的（两个其实是同一条默认路由），升上来就换掉。
  // doubao-auto 在 0.14.0 起是真模型（豆包 2.1 Auto），不再当遗留名处理。
  const legacy = ['doubao-think', 'doubao-expert'];
  if (Array.isArray(CFG.doubao.models) && CFG.doubao.models.some((m) => legacy.includes(m))) {
    CFG.doubao.models = [...DOUBAO_DEFAULT_IDS];
    CFG.inject.doubao.rotate = [...DOUBAO_DEFAULT_IDS];
    changed = true;
  }
  if (CFG.tiers && !CFG.tiers.doubao) { CFG.tiers.doubao = ['doubao::auto']; changed = true; }
  if (CFG.tiers && !CFG.tiers.antigravity) { CFG.tiers.antigravity = ['antigravity::auto']; changed = true; }
  if (changed) {
    PROVIDERS = { ...(CFG.providers || {}) };
    try {
      const tmp = CONFIG_PATH + '.tmp';
      writeFileSync(tmp, JSON.stringify(CFG, null, 2) + '\n');
      renameSync(tmp, CONFIG_PATH);
    } catch (e) { console.error('[defaults] 写入默认配置失败:', e.message); }
  }
}
ensureDefaults();

// 启动后异步拉一次官方模型清单（6 小时缓存），清单变了下次保存/注入就用新的
setTimeout(() => {
  doubaoCatalog(true).then((list) => {
    if (!list?.length) return;
    const ids = list.map((m) => m.id);
    // 老默认只有「快速 + 专家」两条；官方清单拉到后自动扩成全部模型，
    // 这样「自动轮换」遇到某个模型限流能换下一个，而不是直接失败。
    const legacy = ['doubao', 'doubao-turbo'];
    const same = (a, b) => Array.isArray(a) && a.length === b.length && a.every((x, i) => x === b[i]);
    let changed = false;
    if (same(CFG.doubao?.models, legacy)) { CFG.doubao.models = ids; changed = true; }
    if (CFG.inject?.doubao && same(CFG.inject.doubao.rotate, legacy)) {
      CFG.inject.doubao.rotate = ids; changed = true;
    }
    if (changed) saveConfig();
    // 清单写进了 config 文件，内存里的 CFG 也要跟上，面板才能立刻看到新模型
    reloadConfig({ keepTier: true });
    log('INFO', `豆包模型清单已刷新：${list.length} 个`);
  }).catch(() => {});
}, 4000);

/** 某个平台「自动轮换」内部要试的模型列表 */
function rotateOf(name) {
  const inj = CFG.inject?.[name];
  if (Array.isArray(inj?.rotate) && inj.rotate.length) return inj.rotate;
  if (name === 'orcaterm') return CFG.orcaterm?.models || [];
  if (name === 'doubao') return CFG.doubao?.models || [...DOUBAO_DEFAULT_IDS];
  if (name === 'antigravity') {
    const fromQuota = (CFG.antigravity?.quota?.models || []).map((m) => m.id).filter(Boolean);
    return fromQuota.length ? fromQuota : ANTIGRAVITY_MODELS.map((m) => m.id);
  }
  return [];
}

/** 把队列项解析成可直接发请求的执行单元 */
function resolveEntry(raw) {
  const i = raw.indexOf('::');
  const pname = i === -1 ? DEFAULT_PROVIDER : raw.slice(0, i);
  const model = i === -1 ? raw : raw.slice(i + 2);
  const p = PROVIDERS[pname];
  if (!p) return { raw, pname, model, skip: `provider「${pname}」未在 config.providers 中定义` };
  if (p.disabled) return { raw, pname, model, skip: `provider「${pname}」已 disabled` };
  if (!p.apiKey && !p.allowNoKey) return { raw, pname, model, skip: `provider「${pname}」缺 apiKey` };
  const limits = p.limits?.[model] || CFG.limits?.[raw] || CFG.limits?.[model];
  return {
    raw, pname, model,
    endpoint: p.endpoint,
    apiKey: p.apiKey || '',
    proxy: p.proxy ?? '',
    headers: p.headers || {},
    // OrcaTerm 私有协议：走 cookie + CSRF，请求/响应都要翻译
    kind: p.kind || 'openai',
    cookie: p.cookie || '',
    uin: p.uin || '',
    csrf: p.csrf || '',
    // 豆包网页额度：同 cookie 指纹，聊天走页面内 fetch（签名由站点 hook 注入）
    device_id: p.device_id || '',
    web_id: p.web_id || '',
    fp: p.fp || '',
    // 单个模型可以覆盖超时（muse-glimmer 这类 60s+ 的慢模型需要）
    timeoutMs: limits?.timeoutMs || p.timeoutMs || CFG.attemptTimeoutMs || 30000,
    limits,
  };
}

let QUEUE = [];
let skippedEntries = [];

/** 按当前 tier 重建队列。面板改了 key/模型后调用，不必重启进程。 */
function rebuildQueue() {
  const active = [];
  const skipped = [];
  const expand = (raw) => {
    // orcaterm::auto → 按 inject.<平台>.rotate 展开成多个候选
    if (raw === 'orcaterm::auto') {
      const names = rotateOf('orcaterm');
      if (!names.length) { skipped.push({ entry: raw, reason: 'config.inject.orcaterm.rotate 为空，请配置模型列表' }); return; }
      for (const m of names) {
        const e = resolveEntry('orcaterm::' + m);
        if (e.skip) skipped.push({ entry: 'orcaterm::' + m, reason: e.skip });
        else active.push(e);
      }
      return;
    }
    const e = resolveEntry(raw);
    if (e.skip) skipped.push({ entry: raw, reason: e.skip });
    else active.push(e);
  };
  for (const raw of (TIERS[TIER] || CFG.queue || [])) expand(raw);
  QUEUE = active;
  skippedEntries = skipped;
}
rebuildQueue();

// ── 运行时状态 ────────────────────────────────────────────────────────────
const state = new Map(); // raw -> { ok, fail, cooldownUntil, lastError, lastLatency }

function stat(k) {
  if (!state.has(k)) state.set(k, { ok: 0, fail: 0, cooldownUntil: 0, lastError: '', lastLatency: 0 });
  return state.get(k);
}

function log(level, msg) {
  const line = `${new Date().toISOString()} [${level}] ${msg}`;
  if (level === 'DEBUG' && !CFG.verbose) return;
  try { appendFileSync(LOG_FILE, line + '\n'); } catch {}
  if (level !== 'DEBUG') console.log(line);
}

function cool(entry, ms, reason) {
  const s = stat(entry.raw);
  s.fail++;
  s.cooldownUntil = Date.now() + ms;
  s.lastError = reason;
}

// ── 候选队列 ──────────────────────────────────────────────────────────────
/**
 * 客户端点名的模型优先；`auto` / 未知模型名走整条分组队列。
 * 冷却中的排到最后（而不是剔除），避免全挂时无候选可用。
 */
function candidates(requestedModel, bodyModels) {
  const now = Date.now();
  // 旧模型 ID（照面板显示名猜的裸名 / 上一版退而求其次的老模型）→ 实测可用的真实 ID。
  // 真实 ID 必须带 TokenHub/ 或 Hunyuan3/ 前缀，裸名一律 not loaded。
  // 已装老版的电脑 config.json / models.json 不会被覆盖，靠这层映射自愈。
  const ORCA_LEGACY = {
    // 裸名 → 带前缀真实 ID
    'deepseek-v4.1-flash': 'TokenHub/deepseek-v4.1-flash',
    'deepseek-v4-flash': 'TokenHub/deepseek-v4-flash',
    'deepseek-v4-pro': 'TokenHub/deepseek-v4-pro',
    'kimi-k3': 'TokenHub/kimi-k3', 'kimi-k2': 'TokenHub/kimi-k3',
    'glm-5.3': 'TokenHub/glm-5.3', 'glm-5.3-flash': 'TokenHub/glm-5.3-flash',
    'glm-5.2': 'TokenHub/glm-5.2', 'hy3': 'Hunyuan3/hy3',
    // 显示名（大小写形式）→ 真实 ID
    'GLM-5.3': 'TokenHub/glm-5.3', 'GLM-5.3-Flash': 'TokenHub/glm-5.3-flash',
    'Kimi-K3': 'TokenHub/kimi-k3', 'Hy3': 'Hunyuan3/hy3',
    // 上一版误用的老模型 → 默认新模型
    'hunyuan-t1': 'TokenHub/deepseek-v4.1-flash',
    'glm-4.7': 'TokenHub/glm-5.3',
  };
  // WorkBuddy models.json 的 id 用 / 分隔（orcaterm/x），网关队列用 ::（orcaterm::x）
  const alias = (s) => {
    let v = String(s || '')
      .replace(/^orcaterm\//, 'orcaterm::')
      .replace(/^doubao\//, 'doubao::')
      .replace(/^antigravity\//, 'antigravity::');
    const m = v.match(/^orcaterm::(.+)$/);
    if (m && ORCA_LEGACY[m[1]]) v = 'orcaterm::' + ORCA_LEGACY[m[1]];
    return v;
  };
  requestedModel = alias(requestedModel);
  if (Array.isArray(bodyModels)) bodyModels = bodyModels.map(alias);
  const byRaw = new Map(QUEUE.map((e) => [e.raw, e]));
  const byModel = new Map(QUEUE.map((e) => [e.model, e]));
  const lookup = (name) => byRaw.get(name) || byModel.get(name) || null;

  const head = [];
  if (Array.isArray(bodyModels) && bodyModels.length) {
    for (const m of bodyModels) {
      const e = m && lookup(String(m));
      if (e && !head.some((x) => x.raw === e.raw)) head.push(e);
    }
  } else if (requestedModel && !/^(auto|free|free-auto|openrouter-free-auto)$/i.test(requestedModel)) {
    const e = lookup(requestedModel);
    if (e) head.push(e);
    else if (requestedModel.includes('::')) {
      // 客户端点名了队列外的 provider::model，也允许直接试一把
      const dyn = resolveEntry(requestedModel);
      if (!dyn.skip) head.push(dyn);
    }
  }

  // 点名 provider::model（尤其 orcaterm::）时锁定该 provider：这是「独立切换点」，
  // 不能被队列里其他家抢先返回，否则用户以为在用 OrcaTerm、实际走的是别的免费池。
  const pinned = /^([\w-]+)::/.exec(requestedModel || '')?.[1];
  const PINNED_KINDS = new Set(['orcaterm', 'doubao', 'antigravity']);
  if (pinned && PINNED_KINDS.has(PROVIDERS[pinned]?.kind)) {
    // auto → 按 rotate 列表展开，按冷却状态排序后在内部轮换
    if (requestedModel === pinned + '::auto') {
      const names = rotateOf(pinned);
      const entries = names.map((m) => resolveEntry(pinned + '::' + m)).filter((e) => !e.skip);
      const warm = entries.filter((e) => stat(e.raw).cooldownUntil <= now);
      const cold = entries.filter((e) => stat(e.raw).cooldownUntil > now)
        .sort((a, b) => stat(a.raw).cooldownUntil - stat(b.raw).cooldownUntil);
      return [...warm, ...cold];
    }
    const only = head.filter((e) => e.pname === pinned);
    if (only.length) return only;
    const dyn = resolveEntry(requestedModel);
    return dyn.skip ? [] : [dyn];
  }

  const seen = new Set(head.map((e) => e.raw));
  const cold = [];
  const warm = [];
  for (const e of QUEUE) {
    if (seen.has(e.raw)) continue;
    seen.add(e.raw);
    (stat(e.raw).cooldownUntil > now ? cold : warm).push(e);
  }
  cold.sort((a, b) => stat(a.raw).cooldownUntil - stat(b.raw).cooldownUntil);
  return [...head, ...warm, ...cold];
}

// ── curl 调用 ─────────────────────────────────────────────────────────────
function curlArgs(entry, extra = [], timeoutSec) {
  const t = timeoutSec ?? entry.timeoutMs / 1000;
  const args = ['-s', '--no-buffer', '--max-time', String(Math.max(5, Math.round(t)))];
  if (entry.proxy) args.push('--proxy', entry.proxy);
  const isOrca = entry.kind === 'orcaterm';
  const url = isOrca
    ? entry.endpoint + '&csrfCode=' + encodeURIComponent(entry.csrf) + (extra.includes('-i') ? '&sseresume=true' : '')
    : entry.endpoint;
  args.push('-X', 'POST', url,
    '-H', 'Content-Type: application/json',
    '-H', 'HTTP-Referer: http://127.0.0.1:' + PORT,
    '-H', 'X-Title: wb-checkin-gateway');
  if (isOrca) {
    // 上游靠 cookie 鉴权，Accept 必须 text/event-stream（SSE），且不能有 Authorization
    args.push('-H', 'Accept: text/event-stream');
    if (entry.cookie) args.push('-H', 'Cookie: ' + entry.cookie);
  }
  for (const [k, v] of Object.entries(entry.headers)) args.push('-H', `${k}: ${v}`);
  args.push('--data-binary', '@-', ...extra);
  if (entry.apiKey && !isOrca) args.push('-H', `Authorization: Bearer ${entry.apiKey}`);
  return args;
}

/** OrcaTerm: OpenAI body → LightAI 私有 body */
function buildOrcaBody(payload, entry) {
  const id = crypto.randomUUID();
  const lines = [];
  // system/developer 走 emphasisPrompt（服务端高优先级），能压过上游 OrcaTerm 运维人设
  const emphasis = [];
  for (const m of payload.messages || []) {
    const role = m.role || 'user';
    let text = m.content;
    if (Array.isArray(text)) text = text.map((p) => (typeof p === 'string' ? p : p?.text || '')).filter(Boolean).join('\n');
    text = String(text ?? '').trim();
    if (!text) continue;
    if (role === 'system' || role === 'developer') emphasis.push(text);
    else if (role === 'assistant') lines.push('助手：\n' + text);
    else lines.push(text);
  }
  const inject = CFG.orcaterm?.injectSystem || '';
  if (inject) emphasis.unshift(inject);
  return {
    conversationId: 'cid-' + id,
    requestId: id,
    user: {
      id: entry.uin || '0',
      setting: {
        mcpServers: [], uiServers: [], tools: [], approvedTools: [], approvedMCPTools: [],
        enableAutoSubtaskExecution: false, env: [],
        model: entry.model, modelDesc: '', extra: null,
        selectedMCPTools: {}, selectedUITools: {}, a2a: {},
        hideToolMessage: false, language: 'zh', multimodal: false,
        enableThinking: Boolean(payload.enable_thinking),
      },
    },
    input: {
      id,
      type: 'start',
      input: lines.join('\n\n').trim(),
      emphasisPrompt: emphasis.join('\n\n'),
      files: [],
    },
    stream: true,
  };
}

/** OrcaTerm: SSE 聚合 + JSON 壳剥取 → OpenAI chat.completion JSON */
function orcaResponse(bodyStr, model) {
  // 上游验证失败时返回的是纯 JSON（不是 SSE data: 帧），旧逻辑会把整段丢掉变成
  // 「200 但空内容」，用户只看到空回复。这里先识别并给出可操作的错误。
  const raw = String(bodyStr).trim();
  if (raw.startsWith('{')) {
    try {
      const err = JSON.parse(raw);
      const msg = err.message || err.error?.message || '';
      if (err.code && err.code !== 200 || /FAILED|INVALID|EXPIRED|UNAUTHORIZED/i.test(msg)) {
        if (/SKEY|UIN|SESSION|TOKEN|AUTH|LOGIN/i.test(msg)) {
          throw new Error(`OrcaTerm 登录态已失效（${msg}）：请在 WB-Checkin 的 OrcaTerm 标签点「重新登录」扫码`);
        }
        throw new Error(`OrcaTerm 上游错误：${msg || 'code=' + err.code}`);
      }
    } catch (e) {
      if (/登录态已失效|上游错误/.test(e.message)) throw e;
      // 不是错误 JSON，继续按正常响应解析
    }
  }
  let full = '';
  for (const block of String(bodyStr).split(/\r?\n\r?\n/)) {
    const data = block.split(/\r?\n/).filter((l) => l.startsWith('data:'))
      .map((l) => l.slice(5).trimStart()).join('\n');
    if (!data || data === '[DONE]') continue;
    if (data.startsWith('[ERROR]')) throw new Error(data.replace(/^\[ERROR\]:?\s*/, '').slice(0, 200));
    let ev;
    try { ev = JSON.parse(data); } catch { continue; }
    if (ev.code && ev.code !== 0 && ev.code !== 200) throw new Error(ev.message || ev.error?.message || `上游 code=${ev.code}`);
    full += ev.content || ev.delta || ev.text || '';
  }
  // 剥 JSON 壳（taskCompletion / question / review 工具请求）
  const trimmed = full.trim();
  const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/i);
  const candidate = fenced ? fenced[1] : trimmed;
  let content = full;
  try {
    const d = JSON.parse(candidate);
    if (d && typeof d === 'object') {
      if (d.action === 'review') {
        const tool = d.tool || {};
        content = `[模型请求执行工具 ${[tool.mcpServer, tool.name].filter(Boolean).join('/') || 'unknown'}]\n${JSON.stringify(tool.arguments || tool.args || {}, null, 2)}`;
      } else {
        content = d.taskCompletion || d.question || d.content || full;
      }
    }
  } catch {
    // 偶发形态：正文后面又追加了 JSON 壳（如 `2{"action":"completion",...,"taskCompletion":"2"}`）。
    // 整段 JSON.parse 会失败，这里从文本里扫出壳、取 taskCompletion，避免把 JSON 原文吐给用户。
    const embedded = full.match(/```(?:json)?\s*([\s\S]*?)```/i) || full.match(/(\{[\s\S]*"taskCompletion"\s*:[\s\S]*\})/);
    if (embedded) {
      try {
        const d = JSON.parse(embedded[1]);
        const answer = d.taskCompletion || d.question || d.content;
        if (answer) {
          const prefix = full.slice(0, full.indexOf(embedded[0])).trim();
          // 前缀若是答案的一部分（如裸数字）且已被壳内答案包含，直接用壳内答案，避免重复。
          content = (prefix && !String(answer).includes(prefix)) ? prefix + String(answer) : String(answer);
        }
      } catch {}
    }
  }
  return JSON.stringify({
    id: 'chatcmpl-' + crypto.randomUUID().slice(0, 12),
    object: 'chat.completion',
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message: { role: 'assistant', content }, finish_reason: 'stop' }],
    usage: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
  });
}

/** 非流式：等 curl 跑完，返回 { code, body } */
function requestOnce(entry, bodyStr, timeoutSec) {
  return new Promise((resolve) => {
    const started = Date.now();
    const child = spawn(CURL, curlArgs(entry, ['-w', '\n@@%{http_code}'], timeoutSec));
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => { out += d.toString('utf8'); });
    child.stderr.on('data', (d) => { err += d.toString('utf8'); });
    child.on('error', (e) => resolve({ code: 0, body: '', error: String(e), ms: Date.now() - started }));
    child.on('close', () => {
      const ms = Date.now() - started;
      const idx = out.lastIndexOf('@@');
      if (idx === -1) return resolve({ code: 0, body: out.slice(0, 400), error: err || 'no-status', ms });
      const code = parseInt(out.slice(idx + 2).trim(), 10) || 0;
      resolve({ code, body: out.slice(0, idx), error: err, ms });
    });
    child.stdin.write(bodyStr);
    child.stdin.end();
  });
}

/**
 * 流式：spawn curl -i，先切出 HTTP 状态行判断能否锁定模型。
 *
 * 关键点：200 的瞬间要把 stdout 控制权交给上层直通，此后不能再消费；
 * 非 200 则必须把 body 收完（才知道是 429 还是 400）。
 * 两件事不能共用一个 settled 标志，否则非 200 分支永远不 resolve。
 */
function streamOnce(entry, bodyStr, timeoutSec) {
  const started = Date.now();
  return new Promise((resolve) => {
    const child = spawn(CURL, curlArgs(entry, ['-i'], timeoutSec));
    let headerBuf = Buffer.alloc(0);
    let handedOff = false;
    let failed = false;
    let statusCode = 0;
    const tail = [];
    let done = false;

    const settle = (obj) => { if (!done) { done = true; resolve(obj); } };

    child.stdout.on('data', (d) => {
      if (handedOff) return;
      if (failed) { tail.push(d); return; }

      headerBuf = Buffer.concat([headerBuf, d]);
      let sep;
      while ((sep = headerBuf.indexOf('\r\n\r\n')) !== -1) {
        const head = headerBuf.subarray(0, sep).toString('utf8');
        const m = /^HTTP\/[\d.]+\s+(\d{3})/m.exec(head);
        statusCode = m ? parseInt(m[1], 10) : 0;
        const rest = Buffer.from(headerBuf.subarray(sep + 4));
        headerBuf = Buffer.alloc(0);
        if (statusCode >= 100 && statusCode < 200) { headerBuf = rest; continue; }
        if (statusCode === 200) {
          handedOff = true;
          settle({ code: 200, stream: true, first: rest, child, ms: Date.now() - started });
        } else {
          failed = true;
          if (rest.length) tail.push(rest);
        }
        return;
      }
    });

    child.stderr.on('data', () => {});
    child.on('error', (e) => settle({ code: 0, error: String(e), body: '', ms: Date.now() - started }));
    child.on('close', () => {
      if (handedOff) return;
      settle({ code: statusCode, body: Buffer.concat(tail).toString('utf8'), ms: Date.now() - started });
    });

    child.stdin.write(bodyStr);
    child.stdin.end();
  });
}

/** 按模型上限钳制采样参数，并按输入模态过滤不兼容的模型 */
function clampBody(payload, entry) {
  const lim = entry.limits;
  const out = { ...payload, model: entry.model };
  // 网关自己的多候选字段不能透传：NVIDIA 会直接 400 Unsupported parameter(s): models
  delete out.models;
  delete out.provider;
  delete out.route;
  if (!lim) return out;
  for (const k of ['max_tokens', 'max_completion_tokens']) {
    const v = out[k];
    if (typeof v !== 'number') continue;
    // 思考型模型：max_tokens 太小会被思考吃光，正文变空。抬到 minOut 下限。
    if (lim.minOut && v < lim.minOut) out[k] = Math.min(lim.minOut, lim.maxOut ?? Number.MAX_SAFE_INTEGER);
    else if (lim.maxOut && v > lim.maxOut) out[k] = lim.maxOut;
  }
  // 客户端没给 max_tokens 但模型有 minOut 下限时才补，避免擅自放大输出预算
  if (lim.minOut && typeof out.max_tokens !== 'number' && typeof out.max_completion_tokens !== 'number') {
    out.max_tokens = lim.minOut;
  }
  if (typeof out.maxInputTokens === 'number') delete out.maxInputTokens;
  return out;
}

function payloadHasImage(payload) {
  for (const m of payload.messages || []) {
    if (!Array.isArray(m.content)) continue;
    for (const p of m.content) {
      if (p && (p.type === 'image_url' || p.type === 'input_image' || p.type === 'image')) return true;
    }
  }
  return false;
}

function supportsPayload(entry, payload) {
  const lim = entry.limits;
  if (!lim) return true;
  if (lim.image === false && payloadHasImage(payload)) return false;
  return true;
}

// ── 响应解析与错误分类 ────────────────────────────────────────────────────
function parseError(bodyStr) {
  try {
    const d = JSON.parse(bodyStr);
    if (d.error) return { message: String(d.error.message || d.error), code: d.error.code };
    if (d.detail) return { message: String(d.detail), code: d.status };
  } catch {}
  return { message: bodyStr.slice(0, 200) || '(空响应体)' };
}

/** 200 但内容为空 / 明显无效，也算失败（免费模型常见「假成功」） */
function isEmptySuccess(bodyStr) {
  try {
    const d = JSON.parse(bodyStr);
    const c = d.choices?.[0];
    if (!c) return '响应无 choices';
    const msg = c.message || {};
    const hasTool = Array.isArray(msg.tool_calls) && msg.tool_calls.length > 0;
    const text = (msg.content || '').trim();
    // 思考型模型正文在 reasoning_content / reasoning 里，不能判空
    const reasoning = (msg.reasoning_content || msg.reasoning || '').trim();
    if (!hasTool && !text && !reasoning) return `空内容 (finish=${c.finish_reason})`;
    return null;
  } catch {
    return '响应非 JSON';
  }
}

/**
 * 把「只有 reasoning_content、content 为 null」的响应改写为可用响应。
 * 不这么做的话，标准客户端会显示一个空白回复（kimi/step/muse/mistral-large 都这样）。
 */
function normalizeSuccess(bodyStr) {
  let d;
  try { d = JSON.parse(bodyStr); } catch { return bodyStr; }
  const c = d.choices?.[0];
  if (!c) return bodyStr;
  const msg = c.message || {};
  const hasTool = Array.isArray(msg.tool_calls) && msg.tool_calls.length > 0;
  const text = (msg.content || '').trim();
  if (hasTool || text) return bodyStr;
  const reasoning = (msg.reasoning_content || msg.reasoning || '').trim();
  if (!reasoning) return bodyStr;

  // 正文空、只有思考过程：把思考过程落成正文，思考字段保留原样
  msg.content = reasoning;
  c.message = msg;
  d['x_gateway_note'] = 'upstream returned reasoning only; content filled from reasoning_content';
  return JSON.stringify(d);
}

function classify(entry, code, bodyStr) {
  if (code === 0) return { retry: true, cooldown: COOLDOWN_TIMEOUT, reason: '连接失败/超时' };
  const errMsg = parseError(bodyStr).message;
  // 豆包账号级风控：这不是「这个模型挂了」，是账号被要求验证。冷却整组并给出唯一有效的下一步。
  if (/710022002|710022004/.test(bodyStr)) {
    return {
      retry: true, accountWide: true, cooldown: COOLDOWN_RISK,
      reason: '豆包要求人机验证（710022004）：打开 www.doubao.com 或豆包客户端，用同一账号聊一句、过掉验证后自动恢复；'
        + '30 分钟内不再重试，免得越试越难解',
    };
  }
  if (/710012001/.test(bodyStr)) {
    return {
      retry: true, accountWide: true, cooldown: COOLDOWN_RISK,
      reason: '豆包登录态已失效（710012001）：到「豆包」面板重新登录一次即可',
    };
  }

  if (code === 429) {
    // 账号级日额度：同一 provider 下所有模型共用一个池，换模型没用，立刻停并说清原因
    if (/free-models-per-day|Add \d+ credits/i.test(errMsg)) {
      const hint = /(\d+)\s*credits/i.exec(errMsg);
      return {
        retry: false, quota: true, accountWide: true,
        reason: `${entry.pname} 免费模型日额度已用完（OpenRouter 未充值档 50 次/天，UTC 0 点即北京时间 08:00 重置）。`
          + (hint ? `充值 ${hint[1]} credits 可提到 1000 次/天。` : '')
          + ' 这是账号级限制，换模型不会绕过。',
      };
    }
    // 模型级 RPM 限流（NVIDIA 40 RPM 之类）：冷却后换别的模型，不必停
    return { retry: true, cooldown: COOLDOWN_429, reason: '429 ' + errMsg.slice(0, 80) };
  }
  if (code === 402) return { retry: true, cooldown: COOLDOWN_5XX, reason: '402 余额/额度不足 ' + errMsg.slice(0, 60) };
  if (code === 410) return { retry: true, cooldown: COOLDOWN_TIMEOUT, reason: '410 模型已下线 ' + errMsg.slice(0, 60) };
  if (code >= 500) return { retry: true, cooldown: COOLDOWN_5XX, reason: code + ' ' + errMsg.slice(0, 80) };
  if (code === 422) return { retry: true, cooldown: COOLDOWN_5XX, reason: '422 ' + errMsg.slice(0, 80) };
  if (code === 200) {
    const empty = isEmptySuccess(bodyStr);
    if (empty) return { retry: true, cooldown: COOLDOWN_5XX, reason: '200 但 ' + empty };
    return { retry: false };
  }
  // 401/403 多半是这家 provider 的 key 失效或是「未开通该模型」，换下一家比透传有用
  if (code === 401 || code === 403) {
    return { retry: true, cooldown: COOLDOWN_TIMEOUT, reason: `${code} ${entry.pname} 鉴权/未开通：` + errMsg.slice(0, 70) };
  }
  return { retry: false, passthrough: true };
}

// ── 主逻辑 ────────────────────────────────────────────────────────────────
function sendJson(res, code, obj, extraHeaders = {}) {
  const body = JSON.stringify(obj);
  res.writeHead(code, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(body),
    ...extraHeaders,
  });
  res.end(body);
}

function openAiError(res, code, message, type = 'upstream_error') {
  sendJson(res, code, { error: { message, type, code } });
}

/** OrcaTerm 是否已登录（cookie 里有 skey 才算）。登录器写入后立即生效。 */
function orcaLoggedIn() {
  const c = PROVIDERS.orcaterm?.cookie || '';
  return /skey=[^;]{6,}/i.test(c);
}
const ORCA_LOGIN_HINT = 'OrcaTerm 未登录：请在 WB-Checkin 软件的「OrcaTerm」标签里点「扫码登录」腾讯云，'
  + '登录后凭据会自动写入网关（未登录时上游是静默空流，会误报"空内容"，所以这里直接拦下）。';
/** 判断这次请求是否点名了 OrcaTerm 模型 */
function wantsOrca(payload, requested) {
  const hit = (s) => /^orcaterm[\/:]/i.test(String(s || ''));
  if (hit(requested)) return true;
  if (Array.isArray(payload.models) && payload.models.some(hit)) return true;
  return TIER === 'orcaterm';
}
/** 豆包网页额度：页面内 fetch 才能拿到站点签名 */
function doubaoLoggedIn() {
  return /sessionid=[^;]{6,}/i.test(PROVIDERS.doubao?.cookie || '');
}
const DOUBAO_LOGIN_HINT = '豆包未登录：请在 WB-Checkin 软件的「豆包」标签里点「登录豆包」，'
  + '登录后 cookie 自动写入本机网关，额度直接走你的豆包账号。';
function wantsDoubao(payload, requested) {
  const hit = (s) => /^doubao[\/:]/i.test(String(s || ''));
  if (hit(requested)) return true;
  if (Array.isArray(payload.models) && payload.models.some(hit)) return true;
  return TIER === 'doubao';
}
/** Antigravity（Google Cloud Code Assist）：OAuth refresh_token 在 config.providers 里 */
function antigravityLoggedIn() {
  return Boolean(PROVIDERS.antigravity?.refresh_token);
}
const ANTIGRAVITY_LOGIN_HINT = 'Antigravity 未登录：请在 WB-Checkin 软件的「Antigravity」标签里点「Google 登录」，'
  + '登录后 token 自动写入本机网关，之后 WorkBuddy 里就能直接用 Gemini / Claude。';
function wantsAntigravity(payload, requested) {
  const hit = (s) => /^antigravity[\/:]/i.test(String(s || ''));
  if (hit(requested)) return true;
  if (Array.isArray(payload.models) && payload.models.some(hit)) return true;
  return TIER === 'antigravity';
}
function lastUserText(payload) {
  const msgs = Array.isArray(payload.messages) ? payload.messages : [];
  for (let i = msgs.length - 1; i >= 0; i -= 1) {
    if (msgs[i]?.role !== 'user') continue;
    let t = msgs[i].content;
    if (Array.isArray(t)) t = t.map((p) => (typeof p === 'string' ? p : p?.text || '')).filter(Boolean).join('\n');
    if (String(t || '').trim()) return String(t).trim();
  }
  return '';
}

/**
 * 把「已经聚合好的完整回答」拆成 OpenAI 的 SSE 帧。
 *
 * 豆包/OrcaTerm/Antigravity 非流式分支拿到的都是整段 JSON，而上游不提供逐 token 的
 * 增量给网关。客户端请求 stream:true 时按 SSE 发出去，内容一次性到达，
 * 但协议正确 —— 否则客户端等不到 data: 帧，会判定超时并自行取消。
 */
function writeJsonAsSse(res, body, entry, headers) {
  let d = null;
  try { d = JSON.parse(body); } catch { d = null; }
  const msg = d?.choices?.[0]?.message || { role: 'assistant', content: '' };
  res.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-cache',
    Connection: 'keep-alive',
    ...headers,
  });
  if (res.flushHeaders) res.flushHeaders();
  const emit = (delta, finish) => {
    if (res.writableEnded) return;
    res.write('data: ' + JSON.stringify({
      id: d?.id || 'chatcmpl-gw', object: 'chat.completion.chunk',
      created: d?.created || Math.floor(Date.now() / 1000), model: entry.model,
      choices: [{ index: 0, delta, finish_reason: finish }],
    }) + '\n\n');
  };
  emit({ role: 'assistant' }, null);
  if (msg.reasoning_content) emit({ reasoning_content: msg.reasoning_content }, null);
  if (msg.content) emit({ content: msg.content }, null);
  emit({}, 'stop');
  res.end('data: [DONE]\n\n');
}

async function handleCompletions(req, res, rawBody) {
  let payload;
  try { payload = JSON.parse(rawBody); }
  catch { return openAiError(res, 400, '请求体不是合法 JSON', 'invalid_request_error'); }

  const requested = String(payload.model || '').trim();
  const wantsStream = payload.stream === true;
  // 未登录 OrcaTerm 时上游是静默空流（200 无内容），会被误判成"空内容/模型故障"。
  // 这里提前拦下，给出明确提示，避免用户以为是模型坏了。
  if (wantsOrca(payload, requested) && !orcaLoggedIn()) {
    log('WARN', '拦截 OrcaTerm 请求：未登录');
    return openAiError(res, 401, ORCA_LOGIN_HINT, 'orcaterm_not_logged_in');
  }
  if (wantsDoubao(payload, requested) && !doubaoLoggedIn()) {
    log('WARN', '拦截豆包请求：未登录');
    return openAiError(res, 401, DOUBAO_LOGIN_HINT, 'doubao_not_logged_in');
  }
  if (wantsAntigravity(payload, requested) && !antigravityLoggedIn()) {
    log('WARN', '拦截 Antigravity 请求：未登录');
    return openAiError(res, 401, ANTIGRAVITY_LOGIN_HINT, 'antigravity_not_logged_in');
  }
  const rawList = candidates(requested, payload.models);
  if (!rawList.length) {
    const why = skippedEntries.length
      ? `当前分组「${TIER}」下 ${skippedEntries.length} 个候选全部因缺 apiKey 被跳过。请到 http://127.0.0.1:${PORT}/providers 查看。`
      : '网关队列为空，请检查 config.json 的 tiers';
    return openAiError(res, 500, why);
  }

  const list = rawList.filter((e) => supportsPayload(e, payload));
  if (!list.length) {
    return openAiError(res, 400, '本次请求含图片，但当前分组没有支持视觉输入的模型。', 'invalid_request_error');
  }

  const tried = [];
  const blockedProviders = new Set();
  let lastDetail = '';
  let lastCode = 502;
  const startedAt = Date.now();

  for (let i = 0; i < Math.min(list.length, MAX_ATTEMPTS); i++) {
    if (i > 0 && Date.now() - startedAt > TOTAL_BUDGET) {
      log('WARN', `降级预算 ${TOTAL_BUDGET}ms 用尽，停止尝试（已试 ${tried.length} 个）`);
      break;
    }
    const entry = list[i];
    // 同一家已经判为账号级故障（风控/额度/登录失效），换它下面别的模型只是白打一遍
    if (blockedProviders.has(entry.pname)) continue;
    const isOrca = entry.kind === 'orcaterm';
    const isDoubao = entry.kind === 'doubao';
    const isAg = entry.kind === 'antigravity';
    // OrcaTerm 是私有协议：请求体完全不同，且上游本身就是 SSE，响应要聚合成
    // OpenAI JSON 才能过 isEmptySuccess/classify/normalizeSuccess，所以强制非流式收集。
    const outBody = isOrca
      ? JSON.stringify(buildOrcaBody(payload, entry))
      : JSON.stringify(clampBody(payload, entry));
    const budgetLeft = TOTAL_BUDGET - (Date.now() - startedAt);
    const timeoutSec = i === 0
      ? entry.timeoutMs / 1000
      : Math.min(entry.timeoutMs / 1000, Math.max(8, budgetLeft / 1000));

    if (!wantsStream || isOrca || isDoubao || isAg) {
      let r;
      if (isDoubao || isAg) {
        const t0 = Date.now();
        try {
          const d = isAg ? await antigravityChat(payload, entry.model) : await doubaoChat(lastUserText(payload), entry.model);
          r = {
            code: 200,
            body: JSON.stringify({
              id: `chatcmpl-${isAg ? 'ag' : 'doubao'}-` + Date.now(), object: 'chat.completion',
              created: Math.floor(Date.now() / 1000), model: entry.model,
              choices: [{ index: 0, message: { role: 'assistant', content: d.answer || '' }, finish_reason: 'stop' }],
            }),
            ms: Date.now() - t0,
          };
        } catch (e) {
          log('WARN', `${entry.raw} → ${isAg ? 'Antigravity' : '豆包'}调用失败：${e.message}`);
          // 429 / 授权失效要按真实类别回，否则会被当成 5xx 重试到别家（用户看到的是别家回答）
          const code = /429|额度已用完/.test(e.message) ? 429 : (/401|403|授权失效|未登录/.test(e.message) ? 401 : 502);
          r = { code, body: JSON.stringify({ error: { message: e.message } }), ms: Date.now() - t0 };
        }
      } else {
        r = await requestOnce(entry, outBody, timeoutSec);
      }
      // OrcaTerm 上游返回 SSE 文本，先聚合成 OpenAI JSON（失败时原样透传给 classify）
      let rBody = r.body;
      let orcaAuthError = '';
      if (isOrca && r.code === 200) {
        try { rBody = orcaResponse(r.body, entry.model); }
        catch (e) {
          log('WARN', `${entry.raw} → OrcaTerm 响应解析失败：${e.message}`);
          rBody = JSON.stringify({ error: { message: e.message } });
          // 登录态失效是确定性问题，降级到别的模型也没用：直接把可操作提示回给客户端
          if (/登录态已失效/.test(e.message)) orcaAuthError = e.message;
        }
      }
      if (orcaAuthError) {
        return openAiError(res, 401, orcaAuthError, 'orcaterm_not_logged_in');
      }
      const cls = classify(entry, r.code, rBody);
      if (!cls.retry) {
        if (cls.quota) {
          log('ERROR', cls.reason);
          // 账号级额度：同 provider 的其他候选一起长时间冷却，但后面还有别家就继续试
          for (const e of list) if (e.pname === entry.pname) cool(e, QUOTA_COOLDOWN, '账号额度耗尽');
          tried.push(entry.raw + '(额度耗尽)');
          lastDetail = cls.reason;
          lastCode = 429;
          if (!list.slice(i + 1).some((e) => e.pname !== entry.pname)) {
            return openAiError(res, 429, cls.reason, 'quota_exhausted');
          }
          continue;
        }
        if (cls.passthrough) {
          log('WARN', `${entry.raw} → ${r.code}（客户端请求问题，直接透传）`);
          res.writeHead(r.code, { 'Content-Type': 'application/json; charset=utf-8' });
          return res.end(rBody);
        }
        const s = stat(entry.raw);
        s.ok++; s.lastLatency = r.ms; s.lastError = '';
        log('INFO', `${entry.raw} → 200 (${r.ms}ms)${tried.length ? ' 降级自 ' + tried.join(',') : ''}`);
        const body = normalizeSuccess(rBody);
        const hdrs = {
          'x-gateway-model': entry.model,
          'x-gateway-provider': entry.pname,
          'x-gateway-fallbacks': String(tried.length),
        };
        // OrcaTerm 上游是 SSE、豆包是浏览器内聚合 —— 两种拿到的都是完整 JSON。
        // 客户端（WorkBuddy 等）带 stream:true 时只认 text/event-stream，
        // 给它 application/json 会一直等不到 data: 帧，最后超时变成「已取消」。
        if (wantsStream && (isOrca || isDoubao || isAg)) {
          writeJsonAsSse(res, body, entry, hdrs);
          return;
        }
        res.writeHead(200, {
          'Content-Type': 'application/json; charset=utf-8',
          'Content-Length': Buffer.byteLength(body), ...hdrs,
        });
        return res.end(body);
      }
      cool(entry, cls.cooldown, cls.reason);
      if (cls.accountWide) {
        blockedProviders.add(entry.pname);
        for (const e of list) if (e.pname === entry.pname) cool(e, cls.cooldown, cls.reason);
      }
      tried.push(entry.raw);
      lastDetail = cls.reason;
      lastCode = r.code === 429 ? 429 : (r.code === 401 || r.code === 403 ? 502 : (r.code || 502));
      log('WARN', `${entry.raw} → 失败(${r.code}) ${cls.reason} · 换下一个`);
      continue;
    }

    // Antigravity 上游是 Gemini 风格 SSE，必须逐帧转成 OpenAI chunk：
    // 先缓冲到「第一段有内容的帧」再发响应头，这样上游失败还能降到下一个候选。
    if (isAg) {
      const t0 = Date.now();
      const created = Math.floor(Date.now() / 1000);
      const chunkId = `chatcmpl-ag-${Date.now()}`;
      const frame = (delta, finish) => 'data: ' + JSON.stringify({
        id: chunkId, object: 'chat.completion.chunk', created, model: entry.model,
        choices: [{ index: 0, delta, finish_reason: finish }],
      }) + '\n\n';
      let started = false;
      const pending = [];
      const flush = () => {
        if (started) return;
        started = true;
        res.writeHead(200, {
          'Content-Type': 'text/event-stream; charset=utf-8',
          'Cache-Control': 'no-cache',
          Connection: 'keep-alive',
          'x-gateway-model': entry.model,
          'x-gateway-provider': entry.pname,
          'x-gateway-fallbacks': String(tried.length),
        });
        if (res.flushHeaders) res.flushHeaders();
        res.write(frame({ role: 'assistant' }, null));
        for (const p of pending) res.write(p);
        pending.length = 0;
      };
      const push = (f) => { if (started) { if (!res.writableEnded) res.write(f); } else pending.push(f); };
      let failure = null;
      try {
        await antigravityStream(payload, entry.model, (e) => {
          if (e.thought) { push(frame({ reasoning_content: e.thought }, null)); flush(); }
          if (e.delta) { push(frame({ content: e.delta }, null)); flush(); }
        });
      } catch (e) {
        failure = e;
      }
      if (!failure) {
        flush();
        const s = stat(entry.raw);
        s.ok++; s.lastLatency = Date.now() - t0; s.lastError = '';
        log('INFO', `${entry.raw} → 200 stream (${s.lastLatency}ms)${tried.length ? ' 降级自 ' + tried.join(',') : ''}`);
        res.write(frame({}, 'stop'));
        return res.end('data: [DONE]\n\n');
      }
      log('WARN', `${entry.raw} → Antigravity 流失败：${failure.message}`);
      if (started) {
        // 已经吐了半截，再换模型会拼接两个回答，只能收尾
        if (!res.writableEnded) { res.write(frame({}, 'stop')); res.end('data: [DONE]\n\n'); }
        return;
      }
      const code = /429|额度已用完/.test(failure.message) ? 429 : (/401|403|授权失效|未登录/.test(failure.message) ? 401 : 502);
      cool(entry, code === 429 ? QUOTA_COOLDOWN : COOLDOWN_5XX, failure.message);
      tried.push(entry.raw);
      lastDetail = failure.message;
      lastCode = code;
      continue;
    }

    // 流式
    const r = await streamOnce(entry, outBody, timeoutSec);
    if (r.code === 200 && r.stream) {
      const s = stat(entry.raw);
      s.ok++; s.lastError = '';
      log('INFO', `${entry.raw} → 200 stream${tried.length ? ' 降级自 ' + tried.join(',') : ''}`);
      res.writeHead(200, {
        'Content-Type': 'text/event-stream; charset=utf-8',
        'Cache-Control': 'no-cache',
        Connection: 'keep-alive',
        'x-gateway-model': entry.model,
        'x-gateway-provider': entry.pname,
        'x-gateway-fallbacks': String(tried.length),
      });
      if (res.flushHeaders) res.flushHeaders();
      if (r.first && r.first.length) res.write(r.first);
      r.child.stdout.on('data', (d) => { if (!res.writableEnded) res.write(d); });
      r.child.on('close', () => { if (!res.writableEnded) res.end(); });
      req.on('close', () => { try { r.child.kill('SIGKILL'); } catch {} });
      return;
    }

    const failBody = r.body ?? '';
    const cls = classify(entry, r.code, failBody);
    if (!cls.retry) {
      if (cls.quota) {
        log('ERROR', cls.reason);
        for (const e of list) if (e.pname === entry.pname) cool(e, QUOTA_COOLDOWN, '账号额度耗尽');
        tried.push(entry.raw + '(额度耗尽)');
        lastDetail = cls.reason;
        lastCode = 429;
        if (!list.slice(i + 1).some((e) => e.pname !== entry.pname)) {
          return openAiError(res, 429, cls.reason, 'quota_exhausted');
        }
        continue;
      }
      log('WARN', `${entry.raw} → ${r.code}（透传）`);
      res.writeHead(r.code || 502, { 'Content-Type': 'application/json; charset=utf-8' });
      return res.end(failBody || JSON.stringify({ error: { message: '上游无响应' } }));
    }
    cool(entry, cls.cooldown, cls.reason);
    if (cls.accountWide) {
      blockedProviders.add(entry.pname);
      for (const e of list) if (e.pname === entry.pname) cool(e, cls.cooldown, cls.reason);
    }
    tried.push(entry.raw);
    lastDetail = cls.reason;
    lastCode = r.code === 429 ? 429 : (r.code || 502);
    log('WARN', `${entry.raw} → 流式失败(${r.code}) ${cls.reason} · 换下一个`);
  }

  log('ERROR', `全部候选失败：${tried.join(', ')} · 最后错误 ${lastDetail}`);
  const tailMsg = blockedProviders.size
    ? ''
    : '建议稍后重试，或在 /health 查看各模型冷却状态。';
  openAiError(
    res,
    blockedProviders.size ? 429 : lastCode,
    `网关已尝试 ${tried.length} 个模型均失败。最后错误：${lastDetail}。${tailMsg}`,
  );
}

function handleModels(res) {
  const now = Date.now();
  const orcaOk = orcaLoggedIn();
  const data = QUEUE.map((e) => {
    const s = stat(e.raw);
    const cooling = s.cooldownUntil > now;
    const m = {
      id: e.model, object: 'model',
      created: Math.floor(Date.parse(CFG.createdAt || '2026-11-24') / 1000),
      owned_by: e.pname,
      gateway: {
        entry: e.raw, ok: s.ok, fail: s.fail, cooling,
        lastError: s.lastError || null,
        cooldownLeftMs: cooling ? s.cooldownUntil - now : 0,
      },
    };
    if (e.pname === 'orcaterm') {
      m.available = orcaOk;
      m.status = orcaOk ? 'ok' : 'not_logged_in';
      m.hint = orcaOk ? undefined : ORCA_LOGIN_HINT;
      m.name = (e.model.startsWith('auto') ? 'OrcaTerm 自动轮换' : 'OrcaTerm · ' + e.model)
        + (orcaOk ? '' : '（未登录）');
    }
    if (e.pname === 'antigravity') {
      const agOk = antigravityLoggedIn();
      m.available = agOk;
      m.status = agOk ? 'ok' : 'not_logged_in';
      m.hint = agOk ? undefined : ANTIGRAVITY_LOGIN_HINT;
      m.name = (e.model === 'auto' ? 'Antigravity 自动轮换' : 'Antigravity · ' + e.model)
        + (agOk ? '' : '（未登录）');
    }
    return m;
  });
  data.unshift({ id: 'auto', object: 'model', created: 0, owned_by: 'wb-free-auto' });
  sendJson(res, 200, { object: 'list', data });
}

function handleHealth(res) {
  const now = Date.now();
  const models = QUEUE.map((e) => {
    const s = stat(e.raw);
    return {
      entry: e.raw, provider: e.pname, model: e.model,
      ok: s.ok, fail: s.fail,
      cooling: s.cooldownUntil > now,
      cooldownLeftMs: Math.max(0, s.cooldownUntil - now),
      lastLatencyMs: s.lastLatency || null,
      lastError: s.lastError || null,
    };
  });
  sendJson(res, 200, {
    ok: true, uptimeSec: Math.round(process.uptime()), port: PORT, version: BUNDLE_VERSION,
    orcaterm: { loggedIn: orcaLoggedIn(), hint: orcaLoggedIn() ? undefined : ORCA_LOGIN_HINT },
    tier: TIER, tiersAvailable: Object.keys(TIERS),
    queueSize: QUEUE.length,
    skipped: skippedEntries.length,
    order: candidates('auto').slice(0, 10).map((e) => e.raw),
    models,
  });
}

/** provider 视角：谁配了 key、谁缺、队列里各占几个 */
function handleProviders(res) {
  const now = Date.now();
  const usage = {};
  for (const e of QUEUE) {
    usage[e.pname] = usage[e.pname] || { active: 0, cooling: 0 };
    usage[e.pname].active++;
    if (stat(e.raw).cooldownUntil > now) usage[e.pname].cooling++;
  }
  const out = Object.entries(PROVIDERS).map(([name, p]) => ({
    name,
    endpoint: p.endpoint,
    keyPresent: Boolean(p.apiKey),
    allowNoKey: Boolean(p.allowNoKey),
    disabled: Boolean(p.disabled),
    proxy: p.proxy ? 'yes' : 'direct',
    login: p.login || null,
    note: p.note || null,
    inTier: usage[name] || { active: 0, cooling: 0 },
  }));
  sendJson(res, 200, {
    ok: true, defaultProvider: DEFAULT_PROVIDER,
    providers: out,
    skipped: skippedEntries,
  });
}

// ── 配置面板 ──────────────────────────────────────────────────────────────
// 只监听 127.0.0.1，且只接受同源无 Origin 的请求（浏览器直接打开本页时 Origin 为 null）。
// 用途：给「有免费额度但需要验证」的渠道填 key、增删自定义渠道、试连、切分组。
const ADMIN_CATALOG = {
  nvidia:    { title: 'NVIDIA NIM',        quota: '40 RPM · 无日额度', verify: 'phone',    network: '直连', apply: 'https://build.nvidia.com' },
  modelscope:{ title: '魔搭 ModelScope',   quota: '每天 2000 次（按次，不计 token）', verify: 'realname', network: '直连', apply: 'https://www.modelscope.cn/my/myaccesstoken' },
  longcat:   { title: '美团 LongCat',      quota: '5000 万 tokens/天，LongCat-2.5 多模态', verify: 'realname', network: '直连', apply: 'https://longcat.chat' },
  zhipu:     { title: '智谱 GLM',          quota: 'GLM-Flash 永久免费 · 约 100 万 tokens/天', verify: 'realname', network: '直连', apply: 'https://open.bigmodel.cn/usercenter/apikeys' },
  sensenova: { title: '商汤 SenseNova',    quota: '1500 次/5 小时 · 60 RPM（公测）', verify: 'realname', network: '直连', apply: 'https://platform.sensenova.cn' },
  gemini:    { title: 'Google Gemini',     quota: 'Flash 约 1500 RPD；4 系仅 Hydrogen 免费', verify: 'none', network: '需代理', apply: 'https://aistudio.google.com/apikey' },
  openrouter:{ title: 'OpenRouter',        quota: '20 RPM / 50 RPD（充 $10 → 1000/天）', verify: 'none', network: '直连慢', apply: 'https://openrouter.ai/keys' },
  groq:      { title: 'Groq',              quota: '30 RPM / 1000 RPD / 200K TPD', verify: 'none', network: '需代理', apply: 'https://console.groq.com/keys' },
  sambanova: { title: 'SambaNova',         quota: '20 RPM / 20 RPD 每模型', verify: 'none', network: '直连', apply: 'https://cloud.sambanova.ai/apis' },
  unorouter: { title: 'UnoRouter',         quota: '每模型约 1 RPM，125 个 :free 可轮换', verify: 'none', network: '直连', apply: 'https://unorouter.com/en/token' },
  cloudflare:{ title: 'Cloudflare AI',     quota: '10,000 Neurons/天（需填 ACCOUNT_ID 到 endpoint）', verify: 'none', network: '需代理', apply: 'https://dash.cloudflare.com/profile/api-tokens' },
  ofm:       { title: '本机 OFM 网关',     quota: '端口 18900，自带 kilo-auto/free', verify: 'none', network: '本机', apply: null },
  deepseek:  { title: 'DeepSeek',          quota: '付费（key 已失效）', verify: 'card', network: '直连', apply: 'https://platform.deepseek.com/api_keys' },
  volc:      { title: '火山方舟',          quota: 'CodingPlan 订阅已过期', verify: 'realname', network: '直连', apply: 'https://console.volcengine.com/ark' },
  xai:       { title: 'xAI Grok',          quota: '付费 · 无 credits', verify: 'card', network: '需代理', apply: 'https://console.x.ai' },
};

function maskKey(k) {
  if (!k) return '';
  if (k.length <= 12) return '••••••';
  return `${k.slice(0, 7)}…${k.slice(-4)}`;
}

function saveConfig() {
  const tmp = `${CONFIG_PATH}.tmp`;
  writeFileSync(tmp, JSON.stringify(CFG, null, 2), { mode: 0o600 });
  renameSync(tmp, CONFIG_PATH);
}

/** 改完 config 就地重建队列：不重启进程，请求路径零中断 */
function reloadConfig({ keepTier } = {}) {
  CFG = JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));
  PROVIDERS = { ...(CFG.providers || {}) };
  DEFAULT_PROVIDER = CFG.defaultProvider
    || Object.keys(PROVIDERS).find((k) => PROVIDERS[k].apiKey)
    || Object.keys(PROVIDERS)[0];
  TIER = (keepTier && TIER) || CFG.tier || TIER;
  TIERS = CFG.tiers || {};
  rebuildQueue();
}

function providerStats(name) {
  const now = Date.now();
  let ok = 0, fail = 0, cooling = 0, active = 0, cooldownLeftMs = 0;
  for (const e of QUEUE) {
    if (e.pname !== name) continue;
    active++;
    const s = stat(e.raw);
    ok += s.ok || 0;
    fail += s.fail || 0;
    if (s.cooldownUntil > now) {
      cooling++;
      cooldownLeftMs = Math.max(cooldownLeftMs, s.cooldownUntil - now);
    }
  }
  return { ok, fail, cooling, active, cooldownLeftMs };
}

function adminState() {
  const list = Object.entries(PROVIDERS).map(([name, p]) => {
    const cat = ADMIN_CATALOG[name] || {};
    return {
      name,
      title: cat.title || name,
      builtin: Boolean(cat.title),
      endpoint: p.endpoint || '',
      proxyAddr: p.proxy || '',
      keyPresent: Boolean(p.apiKey),
      apiKeyMasked: maskKey(p.apiKey),
      allowNoKey: Boolean(p.allowNoKey),
      disabled: Boolean(p.disabled),
      note: p.note || '',
      login: p.login || null,
      quota: cat.quota || '',
      verify: cat.verify || 'none',
      network: cat.network || '',
      apply: p.login || cat.apply || null,
      // limits 的 key 就是模型 id
      models: Object.keys(p.limits || {}),
      modelLabels: p.modelNames || {},
      inTier: providerStats(name),
      stats: { ok: providerStats(name).ok, fail: providerStats(name).fail },
    };
  });
  // 缺 key 的排前面：那才是需要动手的
  list.sort((a, b) => (a.keyPresent - b.keyPresent)
    || (a.disabled - b.disabled)
    || a.name.localeCompare(b.name));
  return {
    ok: true,
    tier: TIER,
    tiers: Object.keys(TIERS),
    orcaterm: {
      models: CFG.orcaterm?.models || [],
      injectSystem: CFG.orcaterm?.injectSystem || '',
      autoLock: CFG.orcaterm?.autoLock !== false,
      credential: Boolean((PROVIDERS.orcaterm?.cookie || '').includes('skey=')),
      uin: PROVIDERS.orcaterm?.uin || '',
      // 腾讯云不提供额度查询接口，只能给「这个渠道调了多少次 / 最近延迟」当用量参考
      stats: providerStats('orcaterm'),
    },
    doubao: {
      models: CFG.doubao?.models || [],
      // 官方下发的全部可选模型（面板按这份渲染，模型清单不再写死）
      catalog: CFG.doubao?.catalog?.models || [],
      catalogAt: CFG.doubao?.catalog?.at || 0,
      autoCheckin: CFG.doubao?.autoCheckin !== false,
      credential: doubaoLoggedIn(),
      checkedToday: CFG.doubao?.checkin?.date === new Date(Date.now() + 8 * 3600 * 1000).toISOString().slice(0, 10),
      lastCheckin: CFG.doubao?.checkin?.result || '',
    },
    antigravity: {
      models: rotateOf('antigravity'),
      credential: antigravityLoggedIn(),
      email: PROVIDERS.antigravity?.email || '',
      plan: CFG.antigravity?.quota?.plan || '',
      labels: CFG.antigravity?.labels || {},
      updatedAt: CFG.antigravity?.quota?.updatedAt || 0,
    },
    // 「哪些模型写进 WorkBuddy」的开关：前端三个面板都读这一份
    inject: CFG.inject || {},
    queueSize: QUEUE.length,
    skipped: skippedEntries.length,
    uptimeSec: Math.round(process.uptime()),
    providers: list,
  };
}

/** 面板上的「测试」：真发一次请求，结论只回给面板，不写进网关记账 */
async function adminTest(name, modelHint) {
  const p = PROVIDERS[name];
  if (!p) throw new Error(`没有渠道「${name}」`);
  if (!p.endpoint) throw new Error('该渠道还没填 endpoint');
  if (!p.apiKey && !p.allowNoKey) throw new Error('该渠道还没填 apiKey');

  const isOrca = p.kind === 'orcaterm';
  const isDoubao = p.kind === 'doubao';
  const isAg = p.kind === 'antigravity';
  // 注意：model 必须在这里声明，豆包 / Antigravity 分支都要写它（原来的 let 写在后面会 TDZ）
  let model = modelHint;
  if (isDoubao) {
    model = model || (CFG.doubao?.models || [])[0];
    if (!model) throw new Error('豆包还没配模型，请在「豆包」标签里保存模型列表');
    if (!doubaoLoggedIn()) throw new Error(DOUBAO_LOGIN_HINT);
    const r = await doubaoChat('只回两个字：可用', model);
    return { ok: true, model, content: r.answer || '', thinking: r.thinking || '' };
  }
  if (isAg) {
    const m = modelHint || rotateOf('antigravity')[0];
    if (!m) throw new Error('Antigravity 还没有可用模型，请先登录后刷新额度');
    if (!antigravityLoggedIn()) throw new Error(ANTIGRAVITY_LOGIN_HINT);
    const r = await antigravityChat({ messages: [{ role: 'user', content: '只回两个字：可用' }], max_tokens: 64 }, m);
    return { ok: true, model: m, content: r.answer || '', thinking: r.thinking || '' };
  }
  // OrcaTerm 的模型列表在 inject.orcaterm.rotate（auto 轮换那份），不在 p.limits；
  // 用 resolveEntry 构造 entry，才能带上 cookie/csrf/kind 走私有协议。
  if (isOrca) {
    model = model || (CFG.orcaterm?.models || [])[0];
    if (!model) throw new Error('OrcaTerm 还没配模型，请在面板的「OrcaTerm 免费模型」里添加');
    // 老版本面板里存的旧 ID 映射到实测可用的真实模型（config.json 不覆盖，靠这层自愈）
    const ORCA_LEGACY_TEST = {
      'deepseek-v4.1-flash': 'TokenHub/deepseek-v4.1-flash',
      'deepseek-v4-flash': 'TokenHub/deepseek-v4-flash',
      'deepseek-v4-pro': 'TokenHub/deepseek-v4-pro',
      'kimi-k3': 'TokenHub/kimi-k3', 'kimi-k2': 'TokenHub/kimi-k3',
      'glm-5.3': 'TokenHub/glm-5.3', 'glm-5.3-flash': 'TokenHub/glm-5.3-flash',
      'glm-5.2': 'TokenHub/glm-5.2', 'hy3': 'Hunyuan3/hy3',
      'GLM-5.3': 'TokenHub/glm-5.3', 'GLM-5.3-Flash': 'TokenHub/glm-5.3-flash',
      'Kimi-K3': 'TokenHub/kimi-k3', 'Hy3': 'Hunyuan3/hy3',
      'hunyuan-t1': 'TokenHub/deepseek-v4.1-flash', 'glm-4.7': 'TokenHub/glm-5.3',
    };
    model = ORCA_LEGACY_TEST[model] || model;
    if (!orcaLoggedIn()) throw new Error(ORCA_LOGIN_HINT);
  } else {
    model = model || Object.keys(p.limits || {})[0];
    if (!model) throw new Error('该渠道还没配模型 ID');
  }

  const entry = isOrca
    ? resolveEntry(`orcaterm::${model}`)
    : (() => {
        const lim = p.limits?.[model];
        return {
          raw: `${name}::${model}`, pname: name, model,
          endpoint: p.endpoint, apiKey: p.apiKey || '', proxy: p.proxy ?? '',
          headers: p.headers || {},
          timeoutMs: lim?.timeoutMs || p.timeoutMs || CFG.attemptTimeoutMs || 30000,
          limits: lim,
        };
      })();
  if (entry.skip) throw new Error(entry.skip);

  const payload = {
    model,
    messages: [{ role: 'user', content: '只回两个字：可用' }],
    max_tokens: isOrca ? 64 : (entry.limits?.minOut || 64),
  };
  // OrcaTerm 是私有协议：请求体要翻译，上游 SSE 响应要聚合成 OpenAI JSON。
  const bodyStr = isOrca ? JSON.stringify(buildOrcaBody(payload, entry)) : JSON.stringify(payload);
  const r = await requestOnce(entry, bodyStr, Math.ceil(entry.timeoutMs / 1000) + 5);
  let rBody = r.body;
  if (isOrca && r.code === 200) {
    try { rBody = orcaResponse(r.body, model); }
    catch (e) { return { ok: false, code: r.code, ms: r.ms, text: '', error: `OrcaTerm 响应解析失败：${e.message}`, model }; }
  }
  let text = '', error = '';
  try {
    const j = JSON.parse(rBody);
    const m = (j.choices || [{}])[0]?.message || {};
    text = String(m.content || m.reasoning_content || '').trim().replace(/\s+/g, ' ').slice(0, 60);
    if (!text) error = parseError(rBody) || '空回复';
  } catch {
    error = (rBody || '').slice(0, 120) || `HTTP ${r.code}`;
  }
  if (r.code === 0) error = r.error || '连接超时';
  return { ok: r.code === 200 && Boolean(text), code: r.code, ms: r.ms, text, error, model };
}

/** 模型列表文本 → limits 结构，同时保留已有模型的参数 */
function applyModels(p, lines) {
  const parsed = lines.map((line) => {
    const [id, label] = String(line).split('|').map((s) => s.trim());
    return { id, label: label || '' };
  }).filter((m) => m.id);
  const prevLimits = p.limits || {};
  const prevNames = p.modelNames || {};
  const limits = {};
  const names = {};
  for (const { id, label } of parsed) {
    limits[id] = prevLimits[id] || {};
    if (label) names[id] = label;
    else if (prevNames[id]) names[id] = prevNames[id];
  }
  p.limits = limits;
  if (Object.keys(names).length) p.modelNames = names;
  else delete p.modelNames;
  return parsed.map((m) => m.id);
}

function adminUpsert(body) {
  const name = String(body.name || '').trim();
  if (!/^[a-z0-9_-]{1,32}$/.test(name)) throw new Error('渠道标识只能用小写字母、数字、- 和 _');
  CFG.providers = CFG.providers || {};
  const isNew = !CFG.providers[name];
  if (isNew && !body.create) throw new Error(`没有渠道「${name}」`);
  const prev = CFG.providers[name] || {};
  const hadKey = Boolean(prev.apiKey);
  const patch = body.patch || {};
  const next = { ...prev };

  if (patch.endpoint !== undefined) {
    const ep = String(patch.endpoint).trim();
    if (ep && !/^https?:\/\//.test(ep)) throw new Error('endpoint 必须以 http:// 或 https:// 开头');
    next.endpoint = ep;
  }
  if (patch.apiKey) next.apiKey = String(patch.apiKey).trim();
  if (patch.proxy !== undefined) next.proxy = String(patch.proxy).trim();
  if (patch.note !== undefined) next.note = String(patch.note).trim();
  if (typeof patch.disabled === 'boolean') next.disabled = patch.disabled;
  if (!next.timeoutMs) next.timeoutMs = 40000;

  let modelIds = null;
  if (Array.isArray(patch.models)) modelIds = applyModels(next, patch.models);

  CFG.providers[name] = next;
  CFG.tiers = CFG.tiers || {};

  // 刚拿到 key（或新建）的渠道：把前两个模型放进当前分组，才能立刻被用上；
  // 其余只进 all，避免一次把十几个慢模型塞进 high。
  // 注意：清空旧分组必须只发生在「本次确实重写了模型清单」时，
  // 否则只改备注/改 endpoint 会把该渠道的模型从所有分组里静默删光。
  const justActivated = Boolean(next.apiKey) && (!hadKey || isNew);
  if (modelIds) {
    for (const t of Object.keys(CFG.tiers)) {
      CFG.tiers[t] = CFG.tiers[t].filter((raw) => !raw.startsWith(`${name}::`));
    }
    CFG.tiers.all = CFG.tiers.all || [];
    for (const id of modelIds) {
      const raw = `${name}::${id}`;
      if (!CFG.tiers.all.includes(raw)) CFG.tiers.all.push(raw);
    }
    if (justActivated && !next.disabled) {
      CFG.tiers[TIER] = CFG.tiers[TIER] || [];
      for (const id of modelIds.slice(0, 2)) {
        const raw = `${name}::${id}`;
        if (!CFG.tiers[TIER].includes(raw)) CFG.tiers[TIER].push(raw);
      }
    }
  }
  saveConfig();
  reloadConfig({ keepTier: true });
  log('INFO', `面板：${isNew ? '新增' : '更新'}渠道 ${name}${justActivated ? '（已带 key，前两个模型入当前分组）' : ''}`);
  return { ok: true, name, justActivated, queueSize: QUEUE.length };
}

function adminDelete(name) {
  if (!CFG.providers?.[name]) throw new Error(`没有渠道「${name}」`);
  delete CFG.providers[name];
  for (const t of Object.keys(CFG.tiers || {})) {
    CFG.tiers[t] = CFG.tiers[t].filter((raw) => !raw.startsWith(`${name}::`));
  }
  saveConfig();
  reloadConfig({ keepTier: true });
  log('INFO', `面板：删除渠道 ${name}`);
  return { ok: true };
}

function adminTier(tier) {
  if (!TIERS[tier]) throw new Error(`没有分组「${tier}」`);
  CFG.tier = tier;
  saveConfig();
  reloadConfig();
  log('INFO', `面板：分组切到 ${tier}（${QUEUE.length} 个候选）`);
  return { ok: true, tier, queueSize: QUEUE.length };
}

function readJsonBody(req) {
  return new Promise((resolve, reject) => {
    let buf = '';
    req.on('data', (d) => { buf += d; if (buf.length > 512 * 1024) { reject(new Error('请求体过大')); req.destroy(); } });
    req.on('end', () => { try { resolve(buf ? JSON.parse(buf) : {}); } catch { reject(new Error('JSON 解析失败')); } });
    req.on('error', reject);
  });
}

async function handleAdminApi(req, res, route) {
  try {
    if (route === '/admin/api/state') return sendJson(res, 200, adminState());
    if (route === '/admin/api/reload') {
      // 给 orcaterm_auth.mjs 写完 cookie 后调用：重读配置文件 + 重建队列，不重启进程
      try {
        CFG = JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));
        TIER = CFG.tier || 'high';
        TIERS = CFG.tiers || {};
        reloadConfig({ keepTier: false });
        log('INFO', '配置已从磁盘热重载');
        return sendJson(res, 200, { ok: true, queueSize: QUEUE.length });
      } catch (e) {
        return sendJson(res, 400, { ok: false, error: e.message });
      }
    }
    if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: '只接受 POST' });
    const body = await readJsonBody(req);
    if (route === '/admin/api/provider') return sendJson(res, 200, adminUpsert(body));
    if (route === '/admin/api/delete') return sendJson(res, 200, adminDelete(String(body.name || '')));
    if (route === '/admin/api/tier') return sendJson(res, 200, adminTier(String(body.tier || '')));
    if (route === '/admin/api/test') return sendJson(res, 200, await adminTest(String(body.name || ''), body.model));
    // 面板一键登录 OrcaTerm：后台拉起登录器，立即返回；面板轮询 status
    if (route === '/admin/api/orcaterm/login') {
      // force:true = app 面板的「重新登录」：即使已有 cookie 也必须重新拉扫码窗口。
      const force = body.force === true;
      if (orcaLoggedIn() && !force) return sendJson(res, 200, { ok: true, already: true, message: '已登录，无需重复登录' });
      ORCA_LOGIN_PENDING = true;
      ORCA_LOGIN_PENDING_AT = Date.now();
      try { rmSync(STATUS_PATH, { force: true }); } catch {}
      // process.execPath 可能是带 provenance 的 node（一跑就 137，登录器秒退、窗口不弹）。
      // 逐个实际探测候选 node，挑一个真的能跑的来启动登录器。
      const candidates = nodeCandidates();
      let nodeBin = '';
      for (const c of candidates) {
        if (!c || !existsSync(c)) continue;
        try {
          const probe = spawnSync(c, ['-e', ''], { timeout: 3000, stdio: 'ignore' });
          if (probe.status === 0) { nodeBin = c; break; }
        } catch {}
      }
      if (!nodeBin) {
        ORCA_LOGIN_PENDING = false;
        return sendJson(res, 500, { ok: false, error: '找不到可执行的 Node，无法启动扫码登录' });
      }
      const loginLog = (() => {
        try {
          const fd = openSync(path.join(DIR, 'orcaterm-login.log'), 'a');
          return ['ignore', fd, fd];
        } catch { return 'ignore'; }
      })();
      const child = spawn(nodeBin, [path.join(DIR, 'orcaterm_auth.mjs')], {
        cwd: DIR, detached: true, stdio: loginLog, env: process.env,
      });
      child.unref();
      log('INFO', 'OrcaTerm 登录已由面板触发（node=' + nodeBin + ' pid=' + child.pid + '）');
      return sendJson(res, 200, { ok: true, started: true, message: '登录窗口已弹出，请扫码' });
    }
    if (route === '/admin/api/orcaterm/loginStatus') {
      let st = {};
      try { st = JSON.parse(readFileSync(STATUS_PATH, 'utf8')); } catch {}
      // 重新登录期间旧 cookie 不能算「本次登录成功」；登录器落 success/error 后再放行。
      if (st.state === 'success' || st.state === 'error') ORCA_LOGIN_PENDING = false;
      if (ORCA_LOGIN_PENDING && Date.now() - ORCA_LOGIN_PENDING_AT > 120000) ORCA_LOGIN_PENDING = false;
      const pending = ORCA_LOGIN_PENDING;
      return sendJson(res, 200, {
        ok: true,
        state: st.state || (pending ? 'starting' : orcaLoggedIn() ? 'success' : 'idle'),
        message: st.message || '',
        uin: st.uin || PROVIDERS.orcaterm?.uin || '',
        credential: pending ? false : orcaLoggedIn(),
        pending,
      });
    }
    if (route === '/admin/api/orcaterm') {
      // 配置 OrcaTerm auto 轮换的模型列表 + 人设注入
      const models = Array.isArray(body.models)
        ? body.models.map((m) => String(m).trim()).filter(Boolean)
        : String(body.models || '').split(/[\n,|]+/).map((m) => m.trim()).filter(Boolean);
      if (!models.length) throw new Error('至少填一个模型 ID');
      CFG.orcaterm = CFG.orcaterm || {};
      CFG.orcaterm.models = models;
      CFG.inject = CFG.inject || {};
      CFG.inject.orcaterm = { ...(CFG.inject.orcaterm || {}), rotate: models };
      if (typeof body.injectSystem === 'string') CFG.orcaterm.injectSystem = body.injectSystem;
      if (body.autoLock !== undefined) CFG.orcaterm.autoLock = Boolean(body.autoLock);
      reloadConfig({ keepTier: true });
      log('INFO', `OrcaTerm 模型列表已更新：${models.join(', ')}`);
      return sendJson(res, 200, { ok: true, models, injectSystem: CFG.orcaterm.injectSystem || '' });
    }
    if (route === '/admin/api/doubao/login') {
      // 已在登录状态也强制重新拉窗口：重新登录要能换账号。
      try { rmSync(DOUBAO_STATUS_PATH, { force: true }); } catch {}
      const candidates = nodeCandidates();
      let nodeBin = '';
      for (const c of candidates) {
        if (!c || !existsSync(c)) continue;
        try {
          const probe = spawnSync(c, ['-e', ''], { timeout: 3000, stdio: 'ignore' });
          if (probe.status === 0) { nodeBin = c; break; }
        } catch {}
      }
      if (!nodeBin) return sendJson(res, 500, { ok: false, error: '找不到可执行的 Node，无法启动豆包登录' });
      // stderr 必须落文件：登录器是 detached 起的，崩了没人看得到（Chrome 不在、node 被杀），
      // 界面上只会一直是「未登录」，用户根本无从判断。诊断面板会读这个日志。
      const loginLog = (() => {
        try {
          const fd = openSync(path.join(DIR, 'doubao-login.log'), 'a');
          return ['ignore', fd, fd];
        } catch { return 'ignore'; }
      })();
      const child = spawn(nodeBin, [path.join(DIR, 'doubao_bridge.mjs'), '--login'], {
        cwd: DIR, detached: true, stdio: loginLog, env: process.env,
      });
      child.unref();
      log('INFO', `豆包登录已由面板触发（node=${nodeBin} pid=${child.pid}）`);
      return sendJson(res, 200, { ok: true, started: true, message: '浏览器已弹出，请登录豆包' });
    }
    if (route === '/admin/api/doubao/loginStatus') {
      let st = {};
      try { st = JSON.parse(readFileSync(DOUBAO_STATUS_PATH, 'utf8')); } catch {}
      const s = await doubaoStatus();
      return sendJson(res, 200, {
        ok: true,
        state: st.state || (s.credential ? 'success' : 'idle'),
        message: st.message || '',
        credential: s.credential,
        checkedToday: s.checkedToday,
      });
    }
    if (route === '/admin/api/doubao/checkin') {
      const r = await doubaoCheckin(body.force === true);
      return sendJson(res, 200, { ok: true, ...r });
    }
    if (route === '/admin/api/doubao') {
      const models = Array.isArray(body.models)
        ? body.models.map((m) => String(m).trim()).filter(Boolean)
        : String(body.models || '').split(/[\n,|]+/).map((m) => m.trim()).filter(Boolean);
      if (!models.length) throw new Error('至少填一个模型 ID');
      CFG.doubao = CFG.doubao || {};
      CFG.doubao.models = models;
      CFG.inject = CFG.inject || {};
      CFG.inject.doubao = { ...(CFG.inject.doubao || {}), rotate: models };
      if (body.autoCheckin !== undefined) CFG.doubao.autoCheckin = Boolean(body.autoCheckin);
      saveConfig();
      reloadConfig({ keepTier: true });
      log('INFO', `豆包模型列表已更新：${models.join(', ')}`);
      return sendJson(res, 200, { ok: true, models, autoCheckin: CFG.doubao.autoCheckin !== false });
    }
    if (route === '/admin/api/doubao/quota') {
      const q = await doubaoQuota();
      return sendJson(res, 200, q);
    }
    // ── 注入开关：决定哪些模型写进 WorkBuddy models.json ────────────────────
    // 关闭平台 = 从 models.json 清掉该平台全部条目；启用 = 按 inject 列表写回去。
    // 具体落盘由 app 端的 Rust 侧执行（它才持有 ~/.workbuddy/models.json 的写逻辑）。
    if (route === '/admin/api/inject') {
      const provider = String(body.provider || '').trim();
      if (!['orcaterm', 'doubao', 'antigravity'].includes(provider)) throw new Error('未知平台：' + provider);
      const cur = CFG.inject[provider] || { enabled: true, rotate: [], inject: ['auto'] };
      const next = { ...cur };
      if (body.enabled !== undefined) next.enabled = Boolean(body.enabled);
      if (Array.isArray(body.rotate)) {
        next.rotate = body.rotate.map((m) => String(m).trim()).filter(Boolean);
        if (!next.rotate.length) throw new Error('自动轮换至少要留一个模型');
      }
      if (Array.isArray(body.inject)) {
        next.inject = body.inject.map((m) => String(m).trim()).filter(Boolean);
        if (!next.inject.length) next.inject = ['auto'];
      }
      CFG.inject[provider] = next;
      // 兼容旧字段：CFG.<平台>.models 仍被队列 / 面板读
      if (provider === 'orcaterm') { CFG.orcaterm = CFG.orcaterm || {}; CFG.orcaterm.models = next.rotate; }
      if (provider === 'doubao') { CFG.doubao = CFG.doubao || {}; CFG.doubao.models = next.rotate; }
      if (provider === 'antigravity') { CFG.antigravity = CFG.antigravity || {}; CFG.antigravity.models = next.rotate; }
      saveConfig();
      reloadConfig({ keepTier: true });
      log('INFO', `注入开关 ${provider}：enabled=${next.enabled} rotate=${next.rotate.length} inject=${next.inject.join(',')}`);
      return sendJson(res, 200, { ok: true, provider, inject: next });
    }
    if (route === '/admin/api/antigravity/login') {
      try { rmSync(ANTIGRAVITY_STATUS_PATH, { force: true }); } catch {}
      const candidates = nodeCandidates();
      let nodeBin = '';
      for (const c of candidates) {
        if (!c || !existsSync(c)) continue;
        try {
          const probe = spawnSync(c, ['-e', ''], { timeout: 3000, stdio: 'ignore' });
          if (probe.status === 0) { nodeBin = c; break; }
        } catch {}
      }
      if (!nodeBin) return sendJson(res, 500, { ok: false, error: '找不到可执行的 Node，无法启动 Antigravity 登录' });
      const child = spawn(nodeBin, [path.join(DIR, 'antigravity.mjs'), '--login'], {
        cwd: DIR, detached: true, stdio: 'ignore', env: process.env,
      });
      child.unref();
      log('INFO', `Antigravity 登录已由面板触发（node=${nodeBin} pid=${child.pid}）`);
      return sendJson(res, 200, { ok: true, started: true, message: '浏览器已打开，请选择 Google 账号' });
    }
    if (route === '/admin/api/antigravity/loginStatus') {
      let st = {};
      try { st = JSON.parse(readFileSync(ANTIGRAVITY_STATUS_PATH, 'utf8')); } catch {}
      const s = await antigravityStatus();
      return sendJson(res, 200, {
        ok: true,
        state: st.state || (s.credential ? 'success' : 'idle'),
        message: st.message || '',
        url: st.url || '',
        credential: s.credential,
        email: s.email,
      });
    }
    if (route === '/admin/api/antigravity/quota') {
      const q = await antigravityQuota({ force: body.force === true });
      return sendJson(res, 200, q);
    }
    if (route === '/admin/api/antigravity') {
      // 只改「自动轮换内部用哪些模型」；可用模型清单由 fetchAvailableModels 决定
      const models = Array.isArray(body.models)
        ? body.models.map((m) => String(m).trim()).filter(Boolean)
        : String(body.models || '').split(/[\n,|]+/).map((m) => m.trim()).filter(Boolean);
      if (!models.length) throw new Error('至少填一个模型 ID');
      CFG.antigravity = CFG.antigravity || {};
      CFG.antigravity.models = models;
      CFG.inject = CFG.inject || {};
      CFG.inject.antigravity = { ...(CFG.inject.antigravity || {}), rotate: models };
      saveConfig();
      reloadConfig({ keepTier: true });
      log('INFO', `Antigravity 模型列表已更新：${models.join(', ')}`);
      return sendJson(res, 200, { ok: true, models });
    }
    // 老进程跑新界面时最典型的现象：新增接口一律 404。日志里留下接口名便于定位。
    log('WARN', `未知面板接口：${route}（当前脚本版本 ${BUNDLE_VERSION || '未知'}）`);
    return sendJson(res, 404, { ok: false, error: `未知接口 ${route}`, version: BUNDLE_VERSION });

  } catch (e) {
    log('WARN', `面板接口 ${route} 失败：${e.message}`);
    return sendJson(res, 400, { ok: false, error: e.message });
  }
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host || '127.0.0.1'}`);

  if (url.pathname === '/admin' || url.pathname === '/admin/') {
    // 面板本体每次现读，改完 HTML 刷新即可，不必重启
    const html = readFileSync(path.join(DIR, 'admin.html'), 'utf8');
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
    return res.end(html);
  }
  if (url.pathname.startsWith('/admin/api/')) {
    // 只放行本面板自己的来源：别的网站不能跨站改配置。
    const origin = req.headers.origin;
    if (origin && !/^http:\/\/(127\.0\.0\.1|localhost):\d+$/.test(origin)) {
      return sendJson(res, 403, { ok: false, error: '拒绝跨站请求' });
    }
    return handleAdminApi(req, res, url.pathname).catch((e) => sendJson(res, 500, { ok: false, error: String(e?.message || e) }));
  }
  if (req.method === 'GET' && (url.pathname === '/health' || url.pathname === '/')) return handleHealth(res);
  if (req.method === 'GET' && url.pathname === '/providers') return handleProviders(res);
  if (req.method === 'GET' && url.pathname.endsWith('/models')) return handleModels(res);
  if (req.method === 'OPTIONS') { res.writeHead(204); return res.end(); }

  const isChat = url.pathname.endsWith('/chat/completions');
  if (!isChat) {
    if (req.method === 'GET') return sendJson(res, 200, { ok: true, hint: 'POST /v1/chat/completions · GET /health · GET /providers · GET /admin 配置面板' });
    return openAiError(res, 404, `未知路径 ${url.pathname}`, 'not_found');
  }
  if (req.method !== 'POST') return openAiError(res, 405, '只接受 POST', 'invalid_request_error');

  let chunks = [];
  let size = 0;
  const LIMIT = 64 * 1024 * 1024;
  req.on('data', (d) => {
    size += d.length;
    if (size > LIMIT) { openAiError(res, 413, '请求体过大'); req.destroy(); return; }
    chunks.push(d);
  });
  req.on('end', () => {
    const raw = Buffer.concat(chunks).toString('utf8');
    handleCompletions(req, res, raw).catch((e) => {
      log('ERROR', '未捕获异常: ' + (e?.stack || e));
      if (!res.headersSent) openAiError(res, 500, String(e?.message || e));
      else try { res.end(); } catch {}
    });
  });
});

server.listen(PORT, BIND, () => {
  // Windows 上 app 靠这个 pid 精确杀掉自己的网关进程（taskkill /PID），
  // 不用 /IM node.exe —— 那会把用户自己跑的 node 一起杀掉。
  try { writeFileSync(path.join(DIR, '.gateway.pid'), String(process.pid)); } catch {}
  const provs = [...new Set(QUEUE.map((e) => e.pname))];
  log('INFO', `网关已启动 http://${BIND}:${PORT} · 分组 ${TIER} · ${QUEUE.length} 个候选 / ${provs.length} 家上游`);
  log('INFO', `上游: ${provs.join(', ')}`);
  if (skippedEntries.length) {
    log('WARN', `跳过 ${skippedEntries.length} 个候选（缺 key/禁用）：${skippedEntries.map((s) => s.entry).join(', ')}`);
  }
  log('INFO', `初始顺序: ${candidates('auto').slice(0, 6).map((e) => e.raw).join(' → ')}`);
  if (existsSync(LOG_FILE)) log('DEBUG', '日志文件就绪');
  startAutoCheckin();
});

const dropPid = () => { try { rmSync(path.join(DIR, '.gateway.pid'), { force: true }); } catch {} };
process.on('SIGTERM', () => { log('INFO', '收到 SIGTERM，退出'); dropPid(); server.close(() => process.exit(0)); setTimeout(() => process.exit(0), 2000); });
process.on('SIGINT', () => { log('INFO', '收到 SIGINT，退出'); dropPid(); server.close(() => process.exit(0)); setTimeout(() => process.exit(0), 2000); });

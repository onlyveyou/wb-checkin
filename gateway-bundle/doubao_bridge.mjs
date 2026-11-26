#!/usr/bin/env node
/**
 * 豆包网页额度桥：登录取 cookie、每日签到、在浏览器页内调 /chat/completion。
 *
 * 用法：
 *   node doubao_bridge.mjs --login       # 打开独立 Chrome，登录 doubao.com 后自动保存 cookie
 *   node doubao_bridge.mjs --status      # 查看登录 / 签到状态（JSON）
 *   node doubao_bridge.mjs --checkin     # 立即签到一次
 *
 * 被 gateway.mjs import：doubaoChat(text, model) / doubaoCheckin() / doubaoStatus() / startAutoCheckin()
 *
 * 原理：豆包网页会话 cookie 直打签到接口；聊天必须走doubao.com 页面内的 fetch，
 * 让站点 JS hook 自动注入 a_bogus / msToken。所有请求都发往 www.doubao.com，
 * 凭据只保存在本机 config.json 与独立 Chrome profile。
 */
import http from 'node:http';
import { spawn } from 'node:child_process';
import { readFileSync, writeFileSync, renameSync, existsSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const DIR = path.dirname(fileURLToPath(import.meta.url));
const CONFIG_PATH = path.join(DIR, 'config.json');
const PROFILE = path.join(DIR, '.doubao-chrome-profile');
const LOGIN_STATUS = path.join(DIR, '.doubao-login-status.json');
const LOGIN_URL = 'https://www.doubao.com/chat?channel=FnrXU';
const CHAT_PORT = 9234;
const LOGIN_PORT = 9233;
const BOT_ID = '7338286299411103781';
const SCENE = 'pc_incentive_task_mvp_v3';
const CHECKIN_URL = 'https://www.doubao.com/alice/ug/campaign/landing/checkin';
const DETAIL_URL = 'https://www.doubao.com/alice/ug/campaign/v3/detail/';
const QUOTA_OVERVIEW = 'https://www.doubao.com/alice/commerce/sale/subscription/overview/';
const QUOTA_DETAIL = 'https://www.doubao.com/alice/commerce/sale/subscription/quota_balance/detail/';

/**
 * 豆包对话模型（对应当前客户端模型选择器）。
 *
 * 网上老教程只改 need_deep_think（0/1/3），实际客户端是两条独立的轴：
 *   - 选哪个模型：option.conversation_init_ext.model_item_key + model_config +
 *     aggregate_params.model_item_key（三者要一致，服务端先读 conversation_init_ext）；
 *   - 要不要思考：option.need_deep_think 只有 0 / 3 两档，配套 ext.use_deep_think；
 *     思考档位另外走 reasoning_effort（3 低 / 4 中 / 5 高）。
 * 之前只传 need_deep_think=1/3、不传 model_item_key，所以「思考 / 专家」两条其实是同一个
 * 默认路由，用户看到的就是「模型不对」。
 */
/**
 * 官方模型清单（从 /alice/slot/action_bar_v3/brief_list 动态拉，缓存在 config.doubao.catalog）。
 *
 * 每项的字段都是官方下发的原值，别自己编：
 *   model_item_key  聊天请求里的模型标识（数字是内置模型，person-* 是第三方模型）
 *   item_id         新老两套 ID；聊天请求的 need_deep_think 传的就是它
 *   agent_mode      2 = 对话（快速 / 专家），1 = 走 office/creation 通道的模型
 *   default_mode    对应请求里的 mode_id（对话是 1，其他是 3）
 *   model_extra_params  原样塞回 model_config.model_extra_params
 */
const CATALOG_URL = 'https://www.doubao.com/alice/slot/action_bar_v3/brief_list';
const CATALOG_TTL = 6 * 3600 * 1000;

/** 固定的模型 ID（给 WorkBuddy 用，不能随官方改名变），key 来自官方 model_item_key */
const KEY_TO_ID = {
  0: 'doubao',
  3: 'doubao-turbo',
  5: 'doubao-pro',
  9: 'doubao-auto',
  'seed-lite-7b': 'doubao-lite',
  'person-deepseek-v4-1-flash-261017': 'deepseek-v4-1-flash',
  'person-deepseek-v4-pro-ga-260813': 'deepseek-v4-pro',
  'person-glm-5-4': 'glm-5-4',
  'person-glm-5-3-261111': 'glm-5-3',
  'person-glm-5-3-flash-261111': 'glm-5-3-flash',
  'person-qwen4-max': 'qwen4-max',
  'person-qwen3-8-max': 'qwen3-8-max',
  'person-qwen3-8-flash': 'qwen3-8-flash',
  'person-kimi-k3-1-pro-261111': 'kimi-k3-1-pro',
  'person-kimi-k3-1-pro-highspeed-261111': 'kimi-k3-1-pro-hs',
  'person-kimi-k3-261111': 'kimi-k3',
};

/** 官方接口没拉到时的兜底（只放内置两个，保证聊天不至于不可用） */
const FALLBACK_MODES = {
  doubao: { key: '0', name: '快速', agentMode: 2, modeId: '1', need: 0, effort: 3 },
  'doubao-turbo': { key: '3', name: '专家', agentMode: 2, modeId: '1', need: 3, effort: 3 },
};

function slugForKey(key) {
  if (KEY_TO_ID[key]) return KEY_TO_ID[key];
  return String(key)
    .replace(/^person-/, '')
    .replace(/-\d{6}$/, '')
    .replace(/[^a-z0-9]+/gi, '-')
    .replace(/^-|-$/g, '')
    .toLowerCase();
}

/** 把 brief_list 的响应解析成 [{ id, key, name, agentMode, modeId, need, extra, tag, window }] */
function parseCatalog(json) {
  const out = [];
  const seen = new Set();
  const walk = (node) => {
    if (!node || typeof node !== 'object') return;
    if (Array.isArray(node)) { node.forEach(walk); return; }
    if (node.model_item_key && node.name && !seen.has(node.model_item_key + '|' + node.name)) {
      // 只保留聊天/创作通道能用的；office 专用副本跳过（同一 key 已有）
      const modes = node.conversation_mode_list || [];
      if (!modes.length || modes.includes('chat') || modes.includes('creation')) {
        seen.add(node.model_item_key + '|' + node.name);
        const effort = node.reasoning_effort_config?.default_level || 3;
        const extra = node.model_extra_params || {};
        out.push({
          id: slugForKey(node.model_item_key),
          key: String(node.model_item_key),
          name: node.name,
          agentMode: Number(node.agent_mode) || (modes.includes('chat') ? 2 : 1),
          modeId: String(node.default_mode || (modes.includes('chat') ? '1' : '3')),
          need: Number(node.item_id) || 0,
          effort: Number(effort) || 3,
          extra,
          tag: (node.tag_list || []).map((t) => t.text).filter(Boolean).join(' · '),
          window: Number(extra.total_window_size) || 0,
        });
      }
    }
    for (const v of Object.values(node)) walk(v);
  };
  walk(json);
  // 同一 id 只留第一个（官方会同时下发 chat / office 两份）
  const byId = new Map();
  for (const m of out) if (!byId.has(m.id)) byId.set(m.id, m);
  return [...byId.values()];
}

/** 拉官方模型清单（带 6 小时缓存）。失败就回退到缓存 / 内置两条。 */
export async function doubaoCatalog(force = false) {
  const cfg = readCfg();
  const cached = cfg.doubao?.catalog;
  if (!force && cached?.models?.length && Date.now() - (cached.at || 0) < CATALOG_TTL) {
    return cached.models;
  }
  const cookie = cfg.providers?.doubao?.cookie || '';
  if (!cookie) return fallbackCatalog(cfg);
  const q = clientQuery();
  try {
    const r = await fetch(`${CATALOG_URL}?${q}`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Agw-Js-Conv': 'str',
        'Origin': 'https://www.doubao.com',
        'Referer': 'https://www.doubao.com/chat/',
        'User-Agent': UA_DESKTOP,
        'Cookie': cookie,
      },
      body: JSON.stringify({ bot_id: BOT_ID, language_code: 'zh' }),
      signal: AbortSignal.timeout(30000),
    });
    const j = await r.json();
    const models = parseCatalog(j);
    if (!models.length) throw new Error('清单为空');
    const next = readCfg();
    next.doubao = next.doubao || {};
    next.doubao.catalog = { at: Date.now(), models };
    writeCfgSafe(next);
    return models;
  } catch (e) {
    console.error('[doubao] 拉模型清单失败:', e.message);
    return fallbackCatalog(cfg);
  }
}

function fallbackCatalog(cfg) {
  const cached = cfg.doubao?.catalog?.models;
  if (Array.isArray(cached) && cached.length) return cached;
  return Object.entries(FALLBACK_MODES).map(([id, m]) => ({
    id, key: m.key, name: m.name, agentMode: m.agentMode, modeId: m.modeId,
    need: m.need, effort: m.effort, extra: {}, tag: '', window: 0,
  }));
}

/** config.doubao.models 里的 ID 列表（面板与网关用这一份） */
export async function doubaoModelIds(force = false) {
  const models = await doubaoCatalog(force);
  return models.map((m) => m.id);
}

/** 模型 ID → 模式参数。支持传 ID（doubao-pro）、官方 key（5）、或显示名。 */
function resolveMode(model) {
  const raw = String(model || '').split('/').pop().trim();
  let catalog = [];
  try { catalog = readCfg().doubao?.catalog?.models || []; } catch {}
  const hit = catalog.find((m) => m.id === raw || m.key === raw || m.name === raw)
    || catalog.find((m) => raw && (m.id.startsWith(raw) || m.name.startsWith(raw)));
  if (hit) {
    return {
      key: hit.key, name: hit.name, agentMode: hit.agentMode, modeId: hit.modeId,
      need: hit.need, effort: hit.effort || 3, extra: hit.extra || {},
    };
  }
  if (FALLBACK_MODES[raw]) {
    const m = FALLBACK_MODES[raw];
    return { ...m, extra: {} };
  }
  return { ...FALLBACK_MODES.doubao, extra: {} };
}

const UA_DESKTOP = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) DoubaoDesktop/3.19.4 Chrome/131.0.0.0 Safari/537.36';

/**
 * 浏览器可执行文件：macOS 与 Windows 各取常见安装位置，`CHROME_PATH` 可覆盖。
 * Windows 上必须走真实 Chrome/Edge —— 豆包风控按浏览器特征判定，随便一个壳子都不行。
 */
function browserCandidates() {
  const mac = [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
  ];
  const win = [];
  const push = (base, ...rest) => { if (base) win.push(path.join(base, ...rest)); };
  push(process.env.PROGRAMFILES, 'Google', 'Chrome', 'Application', 'chrome.exe');
  push(process.env['PROGRAMFILES(X86)'], 'Google', 'Chrome', 'Application', 'chrome.exe');
  push(process.env.LOCALAPPDATA, 'Google', 'Chrome', 'Application', 'chrome.exe');
  push(process.env.PROGRAMFILES, 'Microsoft', 'Edge', 'Application', 'msedge.exe');
  push(process.env['PROGRAMFILES(X86)'], 'Microsoft', 'Edge', 'Application', 'msedge.exe');
  push(process.env.LOCALAPPDATA, 'Microsoft', 'Edge', 'Application', 'msedge.exe');
  return process.platform === 'win32' ? [...win, ...mac] : [...mac, ...win];
}
const CHROME_CANDIDATES = browserCandidates();
const CHROME = process.env.CHROME_PATH || CHROME_CANDIDATES.find((p) => existsSync(p)) || CHROME_CANDIDATES[0];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const uuid = () => crypto.randomUUID ? crypto.randomUUID() : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
const readCfg = () => JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));
function writeCfgSafe(cfg) {
  const tmp = CONFIG_PATH + '.tmp';
  writeFileSync(tmp, JSON.stringify(cfg, null, 2) + '\n');
  renameSync(tmp, CONFIG_PATH);
}
function setLoginStatus(state, extra = {}) {
  try { writeFileSync(LOGIN_STATUS, JSON.stringify({ state, at: Date.now(), ...extra }, null, 2)); } catch {}
}

function cookieValue(cookie, name) {
  const m = String(cookie || '').match(new RegExp(`(?:^|;\\s*)${name}=([^;]*)`));
  return m ? decodeURIComponent(m[1]) : '';
}
function loggedIn() {
  try { return Boolean(cookieValue(readCfg().providers?.doubao?.cookie, 'sessionid')); } catch { return false; }
}

/**
 * 会话映射：WorkBuddy 的「一个会话」对应豆包的一个会话。
 *
 * 以前是全局复用同一个 conversation_id（config.doubao.conversation），结果是所有
 * WorkBuddy 会话都灌进同一条豆包对话里 —— 新建会话问一句「今天的财政动态」，
 * 豆包看到历史里已经问过，就回一句「用户重复查询，此前已完成答复。」，
 * 而且上下文互相污染。现在按客户端会话指纹分开存，指纹由网关算好传进来。
 *
 * 同一会话继续追问时消息数只增不减（客户端每次都发全量历史），新会话则从很小开始，
 * 所以「消息数比上次少」就等于「这是新会话，我要另开一条豆包对话」。
 */
const SESSION_TTL = 12 * 3600 * 1000;
const SESSION_MAX = 24;

function pickConversation(key, count) {
  if (!key) return '';
  let rec;
  try { rec = (readCfg().doubao?.sessions || {})[key]; } catch { return ''; }
  if (!rec || !rec.id) return '';
  if (Date.now() - (rec.at || 0) > SESSION_TTL) return '';
  if (Number.isFinite(count) && Number.isFinite(rec.count) && count + 1 < rec.count) return '';
  return rec.id;
}

function saveConversation(key, id, count) {
  if (!key || !id) return;
  try {
    const cfg = readCfg();
    cfg.doubao = cfg.doubao || {};
    const map = cfg.doubao.sessions || {};
    map[key] = { id, at: Date.now(), count: Number.isFinite(count) ? count : 0 };
    const keep = Object.keys(map).sort((a, b) => (map[b].at || 0) - (map[a].at || 0)).slice(0, SESSION_MAX);
    cfg.doubao.sessions = Object.fromEntries(keep.map((k) => [k, map[k]]));
    // 旧的全局会话键已废弃，留着会被误认为「还有一条共用的对话」
    delete cfg.doubao.conversation;
    writeCfgSafe(cfg);
  } catch {}
}

/** 极简 CDP over WebSocket（Node 22 全局 WebSocket，无第三方依赖） */
class CDP {
  constructor(ws) {
    this.ws = ws; this.id = 0; this.pending = new Map(); this.handlers = new Map();
    ws.addEventListener('message', (e) => {
      let m; try { m = JSON.parse(typeof e.data === 'string' ? e.data : String(e.data)); } catch { return; }
      if (!m.id && m.method) {
        const fn = this.handlers.get(m.method);
        if (fn) { try { fn(m.params || {}); } catch {} }
        return;
      }
      const p = this.pending.get(m.id); if (!p) return;
      this.pending.delete(m.id);
      m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result);
    });
  }
  /** 订阅 CDP 事件（Fetch.requestPaused 这类需要回话的，回调里直接 send 续行）。 */
  on(method, fn) { this.handlers.set(method, fn); }
  static async attach(base, preferDoubao = true) {
    const list = await fetch(`${base}/json/list`).then((r) => r.json());
    const page = (preferDoubao && list.find((t) => t.type === 'page' && t.url.includes('doubao.com')))
      || list.find((t) => t.type === 'page');
    if (!page) throw new Error('CDP 里没找到页面');
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', rej, { once: true });
    });
    return new CDP(ws);
  }
  send(method, params = {}, timeoutMs = 20000) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP ${method} 超时`)); }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }
  close() { try { this.ws.close(); } catch {} }
}

/**
 * 把窗口最小化。
 *
 * 豆包的风控（shark_admin）会按浏览器特征下发 verify：headless 环境直接回
 * 710022004 / verify_scene=doubao_message_web，换 cookie、换模型、过验证都没用 ——
 * 因为是「不像真人浏览器」被拒，不是账号问题。必须有真实窗口。
 * 但窗口弹出来打扰人，所以启动后立刻最小化；再关掉后台节流，保证 SSE 不被降频。
 */
async function minimizeWindow(port) {
  try {
    const version = await fetch(`http://127.0.0.1:${port}/json/version`).then((r) => r.json());
    const ws = new WebSocket(version.webSocketDebuggerUrl);
    const cdp = new CDP(ws);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', rej, { once: true });
    });
    const { targetInfos } = await cdp.send('Target.getTargets');
    const page = targetInfos.find((t) => t.type === 'page');
    if (!page) return;
    const { windowId } = await cdp.send('Browser.getWindowForTarget', { targetId: page.targetId });
    await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'minimized' } });
    cdp.close();
  } catch {}
}

/**
 * 挡掉页面自己发起的「恢复请求」。
 *
 * 我们是在 doubao.com 页面上下文里手动 fetch('/chat/completion') 的，绕过了页面自己的状态机。
 * 但页面是活的：它通过 IM 长连接看到这个会话里有一条「有问无答」的消息，就会按自己的
 * recovery 逻辑把同一条消息再发一次（抓包里 recovery_option.is_recovery=true、
 * unique_key 与我们那条完全一致、model_config.model_item_key 为空 → 走默认模型）。
 * 结果是一次客户端请求在豆包平台上留下两条记录、扣两次额度、风控计数也翻倍。
 *
 * 修法：CDP 打开 Fetch 域只监听 /chat/completion，带 is_recovery 的直接掐掉，
 * 其余（我们自己那条）原样放行。用常驻会话，页面重载后自动重挂。
 */
let guard = null;
async function ensureGuard() {
  if (guard && guard.ws.readyState === 1) return guard.cdp;
  const cdp = await CDP.attach(`http://127.0.0.1:${CHAT_PORT}`);
  cdp.on('Fetch.requestPaused', async (p) => {
    let abort = false;
    try {
      abort = /"is_recovery"\s*:\s*true/.test(p.request?.postData || '');
    } catch {}
    try {
      if (abort) {
        await cdp.send('Fetch.failRequest', { requestId: p.requestId, errorReason: 'Aborted' });
      } else {
        await cdp.send('Fetch.continueRequest', { requestId: p.requestId });
      }
    } catch {}
  });
  await cdp.send('Fetch.enable', {
    patterns: [{ urlPattern: '*://www.doubao.com/chat/completion*', requestStage: 'Request' }],
  });
  guard = { cdp, ws: cdp.ws };
  return cdp;
}

async function waitDebug(port, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (r.ok) return await r.json();
    } catch (e) { last = e; }
    await sleep(300);
  }
  throw new Error('调试端口未起来：' + (last?.message || '超时'));
}

/** 登录：独立可见 Chrome，用户扫码/登录后从 CDP 读 doubao.com cookie 与设备指纹 */
async function doLogin() {
  if (!existsSync(CHROME)) throw new Error(`Chrome 不存在：${CHROME}（可用 CHROME_PATH 指定）`);
  mkdirSync(PROFILE, { recursive: true });
  setLoginStatus('starting', { message: '正在启动浏览器…' });
  const child = spawn(CHROME, [
    `--user-data-dir=${PROFILE}`,
    `--remote-debugging-port=${LOGIN_PORT}`,
    '--no-first-run', '--no-default-browser-check',
    '--disable-features=TranslateUI',
    LOGIN_URL,
  ], { stdio: 'ignore' });
  const cleanup = () => { try { child.kill('SIGTERM'); } catch {} };
  process.on('exit', cleanup);
  try {
    await waitDebug(LOGIN_PORT);
    setLoginStatus('waiting', { message: '浏览器已打开，请登录豆包（手机号/抖音/扫码）…' });
    const deadline = Date.now() + 300000;
    let cookies = [];
    while (Date.now() < deadline) {
      await sleep(2500);
      try {
        const cdp = await CDP.attach(`http://127.0.0.1:${LOGIN_PORT}`);
        const got = await cdp.send('Network.getAllCookies');
        cdp.close();
        cookies = got.cookies || [];
        if (cookies.some((c) => c.name === 'sessionid' && /doubao\.com/.test(c.domain || ''))) break;
      } catch {}
    }
    const session = cookies.find((c) => c.name === 'sessionid' && /doubao\.com/.test(c.domain || ''));
    if (!session) throw new Error('没读到 sessionid，登录超时或调试端口被占用');

    let fpInfo = { device_id: '', web_id: '', fp: '' };
    try {
      const cdp = await CDP.attach(`http://127.0.0.1:${LOGIN_PORT}`);
      const r = await cdp.send('Runtime.evaluate', {
        expression: `(() => {
          const out = {device_id:'',web_id:'',fp:''};
          try { out.device_id = JSON.parse(localStorage.getItem('samantha_web_web_id')||'{}').web_id || ''; } catch(e){}
          try { out.web_id = JSON.parse(localStorage.getItem('__tea_cache_tokens_497858')||'{}').web_id || ''; } catch(e){}
          const c = document.cookie.split(';').map(s=>s.trim()).find(s=>s.startsWith('s_v_web_id='));
          out.fp = c ? decodeURIComponent(c.split('=')[1]) : '';
          return out;
        })()`,
        returnByValue: true,
      });
      fpInfo = r.result?.value || fpInfo;
      cdp.close();
    } catch {}

    // 只保留 doubao.com 域 cookie；按名字排序并非必需，但 sessionid 必须在前
    const names = new Set(cookies.filter((c) => /doubao\.com/i.test(c.domain || '')).map((c) => c.name));
    const order = ['sessionid', 'sessionid_ss', 'ttwid', 'passport_csrf_token', 's_v_web_id', 'odin_tt', 'msToken'];
    const pick = (name) => cookies.find((c) => c.name === name && /doubao\.com/i.test(c.domain || ''))?.value || '';
    const pairs = [];
    for (const n of [...order.filter((n) => names.has(n)), ...[...names].filter((n) => !order.includes(n))]) {
      const v = pick(n);
      if (v) pairs.push(`${n}=${v}`);
    }
    const cfg = readCfg();
    cfg.providers = cfg.providers || {};
    cfg.providers.doubao = {
      ...(cfg.providers.doubao || {}),
      kind: 'doubao',
      allowNoKey: true,
      apiKey: '',
      endpoint: 'https://www.doubao.com/chat/completion',
      cookie: pairs.join('; '),
      device_id: fpInfo.device_id,
      web_id: fpInfo.web_id,
      fp: fpInfo.fp || pick('s_v_web_id'),
      note: `豆包网页额度 · 登录于 ${new Date().toISOString().slice(0, 16)}`,
      timeoutMs: 180000,
    };
    cfg.doubao = cfg.doubao || {};
    cfg.doubao.models = cfg.doubao.models || ['doubao', 'doubao-turbo'];
    cfg.doubao.autoCheckin = cfg.doubao.autoCheckin !== false;
    writeCfgSafe(cfg);
    // 通知网关热重载，失败无妨
    try {
      const req = http.request(`http://127.0.0.1:${cfg.port || 8901}/admin/api/reload`, { method: 'POST' }, (r) => r.resume());
      req.on('error', () => {});
      req.end();
    } catch {}
    setLoginStatus('success', { at: Date.now() });
    return cfg.providers.doubao;
  } finally {
    cleanup();
  }
}

/** 聊天用浏览器：保持 profile 登录态，有头常驻（最小化），首次调用时拉起 */
let browserChild = null;

/**
 * 把 config 里的 cookie 灌进浏览器。
 *
 * 豆包的 sessionid 是会话 cookie，Chrome 正常退出就丢了，profile 里读回来往往是空的
 * （表现就是页面内请求回 710012001「登录已过期」）。所以不依赖 profile，每次连上都重灌一遍。
 */
async function injectCookies(cdp) {
  let cookie = '';
  try { cookie = readCfg().providers?.doubao?.cookie || ''; } catch { return 0; }
  if (!cookie) return 0;
  let n = 0;
  for (const pair of cookie.split(/;\s*/)) {
    const i = pair.indexOf('=');
    if (i <= 0) continue;
    const name = pair.slice(0, i);
    const value = pair.slice(i + 1);
    try {
      await cdp.send('Network.setCookie', {
        name, value, domain: '.doubao.com', path: '/', secure: true, sameSite: 'None',
      });
      n += 1;
    } catch {}
  }
  return n;
}

async function ensureBrowser() {
  try {
    await waitDebug(CHAT_PORT, 1200);
    const cdp = await CDP.attach(`http://127.0.0.1:${CHAT_PORT}`);
    cdp.close();
    return;
  } catch {}
  if (!existsSync(CHROME)) throw new Error(`Chrome 不存在：${CHROME}`);
  mkdirSync(PROFILE, { recursive: true });
  browserChild = spawn(CHROME, [
    `--user-data-dir=${PROFILE}`,
    `--remote-debugging-port=${CHAT_PORT}`,
    // 不能加 --headless：豆包 shark 风控按浏览器特征判定，headless 一律回 710022004
    // （decision type=verify, verify_scene=doubao_message_web），跟账号、额度、cookie 都无关。
    '--no-first-run', '--no-default-browser-check',
    // 不要加 --disable-blink-features=AutomationControlled：Chrome 会因此顶一条
    // 「您使用的是不受支持的命令行标记」提示条，还会把最小化的窗口重新顶出来。
    // 那个标记本来是给 playwright/puppeteer 用的（它们会带 --enable-automation 把
    // navigator.webdriver 变成 true）；我们直接 spawn 浏览器的，webdriver 本来就是 false。
    '--disable-features=TranslateUI',
    '--window-size=520,360', '--window-position=60,60',
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
    '--disable-backgrounding-occluded-windows',
    LOGIN_URL,
  ], { stdio: 'ignore' });
  await waitDebug(CHAT_PORT);
  await minimizeWindow(CHAT_PORT);
  // 先灌 cookie 再让页面加载，站点 JS 一上来就是登录态
  try {
    const cdp = await CDP.attach(`http://127.0.0.1:${CHAT_PORT}`);
    await cdp.send('Network.enable');
    await injectCookies(cdp);
    cdp.close();
  } catch {}
  // 给站点 JS 时间装 fetch hook
  for (let i = 0; i < 30; i += 1) {
    try {
      const cdp = await CDP.attach(`http://127.0.0.1:${CHAT_PORT}`);
      const r = await cdp.send('Runtime.evaluate', { expression: `({url:location.href, hooked:!String(window.fetch).includes('native code')})`, returnByValue: true });
      cdp.close();
      if (r.result?.value?.url?.includes('doubao.com') && r.result.value.hooked) return;
    } catch {}
    await sleep(1000);
  }
}

/** 窗口有没有被顶出来（Chrome 偶发会在页面跳转/提示条后自己 restore）。 */
async function windowState(port) {
  try {
    const version = await fetch(`http://127.0.0.1:${port}/json/version`).then((r) => r.json());
    const ws = new WebSocket(version.webSocketDebuggerUrl);
    const cdp = new CDP(ws);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', rej, { once: true });
    });
    const { targetInfos } = await cdp.send('Target.getTargets');
    const page = targetInfos.find((t) => t.type === 'page');
    if (!page) return '';
    const { windowId } = await cdp.send('Browser.getWindowForTarget', { targetId: page.targetId });
    const { bounds } = await cdp.send('Browser.getWindowBounds', { windowId });
    cdp.close();
    return bounds?.windowState || '';
  } catch {
    return '';
  }
}

function payloadFor(text, model, conversationId = '') {
  const mode = resolveMode(model);
  // 官方请求里 need_deep_think 直接就是该模型的 item_id（快速 0 / 专家 3 / 2.1 Pro 9 / 第三方 10001），
  // 不是网上教程写的 0/1/3 三档；传错会退回默认路由，表现就是「模型不对」。
  const needThink = mode.need;
  const agentMode = mode.agentMode || 2;
  const modeId = String(mode.modeId || (agentMode === 2 ? '1' : '3'));
  const convMode = agentMode === 2 ? '1' : '';
  const now = Date.now();
  const uuidMsg = uuid();
  const plainTaskParam = { server_do_compare: true };
  return {
    client_meta: {
      local_conversation_id: `local_${uuid().replace(/-/g, '').slice(0, 16)}`,
      conversation_id: conversationId || '',
      bot_id: BOT_ID,
      last_section_id: '',
      last_message_index: null,
      local_permissions: [
        { permission_name: 'ACCESS_COARSE_LOCATION', status: 3 },
        { permission_name: 'ACCESS_FINE_LOCATION', status: 3 },
        { permission_name: 'ACCESS_BACKGROUND_LOCATION', status: 3 },
      ],
    },
    messages: [{
      local_message_id: uuidMsg,
      content_block: [{
        block_type: 10000,
        content: { text_block: { text, icon_url: '', icon_url_dark: '', summary: '' }, pc_event_block: '' },
        block_id: uuid(),
        parent_id: '',
        meta_info: [],
        append_fields: [],
      }],
      message_status: 0,
    }],
    option: {
      send_message_scene: '', create_time_ms: now, collect_id: '', is_audio: false,
      answer_with_suggest: false, agent_mode: agentMode, tts_switch: false,
      need_deep_think: needThink,
      click_clear_context: false, from_suggest: false, is_regen: false, is_replace: false,
      is_from_click_option: false, is_from_click_softlink: false,
      disable_sse_cache: false, select_text_action: '', is_select_text: false,
      resend_for_regen: false, scene_type: 0,
      unique_key: uuid(), start_seq: 0, need_create_conversation: !conversationId,
      conversation_init_option: { need_ack_conversation: true },
      conversation_mode: convMode,
      // 模型选择真正生效的地方
      conversation_init_ext: {
        model_item_key: mode.key,
        reasoning_effort: String(mode.effort),
        mode_id: modeId,
      },
      regen_query_id: [], edit_query_id: [], regen_instruction: '', no_replace_for_regen: false,
      message_from: 0, shared_app_name: '', shared_app_id: '',
      sse_recv_event_options: { support_chunk_delta: true },
      support_lazy_fetch_stream: true,
      is_ai_playground: false, is_old_user: true,
      general_task_param: plainTaskParam,
      recovery_option: { is_recovery: false, req_create_time_sec: Math.floor(now / 1000), append_sse_event_scene: 0 },
      message_storage_type: 0,
      related_deleted_message_ids: {},
      connector_info_list: [],
      model_config: {
        model_item_key: mode.key,
        model_extra_params: mode.extra || {},
        reasoning_effort: mode.effort,
      },
      aggregate_params: {
        mention_skill_list: '[]',
        mention_plugin_list: '[]',
        mention_ext: '[{}]',
        // 深度思考时这里要是 "1"，否则服务端只开 10040 空壳、不产出思考内容
        conversation_mode: convMode,
        mode_id: modeId,
        model_item_key: mode.key,
        agent_mode: String(agentMode),
        reasoning_effort: String(mode.effort),
        provider_id: '',
      },
    },
    user_context: [],
    ext: {
      agent_mode: String(agentMode),
      use_deep_think: String(needThink),
      fp: '', // 由页面里从 cookie 补
      collection_id: '',
      is_finish: '1',
      conversation_init_option: JSON.stringify({ need_ack_conversation: true }),
      general_task_param: JSON.stringify(plainTaskParam),
      commerce_credit_config_enable: '0',
      sub_conv_firstmet_type: '1',
    },
  };
}

const EVAL_CHAT = `async ([payloadJson, model]) => {
  const p = JSON.parse(payloadJson);
  const csrf = (document.cookie.match(/passport_csrf_token=([^;]+)/)||[])[1] || '';
  const fp = (document.cookie.match(/s_v_web_id=([^;]+)/)||[])[1] || '';
  p.ext.fp = decodeURIComponent(fp);
  const q = {
    aid: '497858', device_id: '', device_platform: 'web', fp: decodeURIComponent(fp),
    language: 'zh', pc_version: '3.19.4', pkg_type: 'release_version', real_aid: '497858',
    region: '', samantha_web: '1', sys_region: '', 'use-olympus-account': '1',
    version_code: '20800', web_tab_id: crypto.randomUUID(),
  };
  const qs = Object.keys(q).sort().map(k => k + '=' + encodeURIComponent(q[k])).join('&');
  const headers = { 'Content-Type': 'application/json', 'agw-js-conv': 'str' };
  if (csrf) headers['x-tt-passport-csrf-token'] = decodeURIComponent(csrf);
  try {
    const res = await fetch('/chat/completion?' + qs, {
      method: 'POST', headers, body: JSON.stringify(p), credentials: 'include',
    });
    if (!res.ok) return JSON.stringify({ error: 'HTTP ' + res.status, body: (await res.text()).slice(0, 500) });
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '', answer = '', thinking = '', conv = '';
    let thinkId = '', curIsThinking = false;
    const types = new Set();
    const take = (obj) => {
      if (!obj || typeof obj !== 'object') return;
      if (obj.code && obj.code !== 0) { answer = ''; throw new Error('code=' + obj.code + ' ' + (obj.msg || obj.message || '')); }
      // 上游的错误帧：{"error_code":710022002,"error_msg":"当前服务访问频繁，请稍后重试"}
      if (obj.error_code) {
        const friendly = obj.error_code === 710022002 || obj.error_code === 710022004
          ? '豆包风控限流（' + obj.error_code + '）：这个账号当前被要求人机验证。'
            + '打开 www.doubao.com 或豆包客户端用同一账号聊一句、过掉验证后就会恢复；'
            + '官方网页此刻也是同样的返回，不是这边配置的问题'
          : obj.error_msg || '';
        throw new Error('code=' + obj.error_code + ' ' + friendly);
      }
      // 顶层 {"text": "..."} 是「当前正在写的那个文本块」的增量，归属要跟着块走：
      // 思考区的子块（parent_id = 10040 容器的 block_id）算思考，正文块算回答。
      // 以前靠 10040 出现次数的奇偶去猜，但 start/update/finish 会来好几次，奇偶必然错位，
      // 结果推理和正文互相串门 —— 客户端看到的就是截断或自说自话。
      if (typeof obj.text === 'string' && obj.text && !obj.error_code) {
        if (curIsThinking) thinking += obj.text; else answer += obj.text;
        return;
      }
      const blocks = [];
      for (const patch of (obj.patch_op || [])) {
        const pv = patch.patch_value || {};
        if (Array.isArray(pv.content_block)) blocks.push(...pv.content_block);
      }
      const dc = obj.content;
      if (dc && Array.isArray(dc.content_block)) blocks.push(...dc.content_block);
      for (const cb of blocks) {
        const bt = cb.block_type || 0;
        types.add(bt);
        const c = cb.content || {};
        if (bt === 10040) {
          // 思考区容器（只带标题/状态，正文在它的子块里）
          if (cb.block_id) thinkId = cb.block_id;
          curIsThinking = true;
          continue;
        }
        if (bt === 10000) {
          const pid = cb.parent_id || '';
          curIsThinking = Boolean(pid) && (!thinkId || pid === thinkId);
          const text = c.text_block && c.text_block.text;
          if (text) {
            if (curIsThinking) thinking += text; else answer += text;
          }
        }
      }
      const cid = obj.conversation_id || (obj.message && obj.message.conversation_id);
      if (cid) conv = cid;
    };
    let raw = '';
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split('\\n');
      buffer = lines.pop();
      for (const line of lines) {
        const t = line.trim();
        if (!t.startsWith('data:')) continue;
        const s = t.slice(5).trim();
        if (!s || s === '{}') continue;
        raw += s + '\\n';
        try { take(JSON.parse(s)); } catch (e) { if (String(e.message).startsWith('code=')) throw new Error(e.message); }
      }
    }
    return JSON.stringify({ answer: answer || (thinking ? thinking : ''), thinking, conversation_id: conv, model, types: [...types], raw: raw.slice(-2000) });
  } catch (e) {
    return JSON.stringify({ error: e.message });
  }
}`;

export async function doubaoChat(text, model = 'doubao', ctx = {}) {
  if (!loggedIn()) throw new Error('豆包未登录：请在「豆包」标签里先登录，或点「重新登录」');
  await ensureBrowser();
  // 这个窗口只是后台跑的「页面上下文」，不该出现在桌面上；Chrome 偶尔会自己 restore。
  if ((await windowState(CHAT_PORT)) !== 'minimized') await minimizeWindow(CHAT_PORT);
  // 先挂拦截：页面在我们这条请求跑完后会自己补一条 recovery，不挡住就是双倍扣额度
  await ensureGuard().catch(() => {});
  const key = String(ctx?.key || '');
  const count = Number(ctx?.count);
  let convId = pickConversation(key, count);
  let out = await runOnce(text, model, convId);
  // 复用的对话里回空：豆包对「已经问过的内容」会只回状态帧、不吐正文。
  // 换一条新对话再问一次，比把空串当成正常回答回给客户端要好。
  if (!out.answer && !out.thinking && convId) {
    console.error('[doubao] 复用会话空回答，改用新会话重试');
    out = await runOnce(text, model, '');
    convId = '';
  }
  recordUsage(model, out);
  if (!out.answer && !out.thinking) {
    console.error('[doubao] 空回答：块类型=' + JSON.stringify(out.types || []) + ' raw=' + String(out.raw || '').slice(-400));
  }
  if (out.conversation_id) saveConversation(key, out.conversation_id, count);
  return out;
}

/** 跑一次上游调用（= 某条豆包对话里的一问）。 */
async function runOnce(text, model, convId) {
  const cdp = await CDP.attach(`http://127.0.0.1:${CHAT_PORT}`);
  try {
    await cdp.send('Network.enable').catch(() => {});
    await injectCookies(cdp);
    // Runtime.evaluate 没有 args 参数，参数要以字面量拼进表达式并把函数真正调用一次，
    // 否则返回的是函数对象本身（表现为 "is not valid JSON"）。
    const payloadJson = JSON.stringify(payloadFor(String(text || ''), model, convId));
    const expression = `(${EVAL_CHAT})(${JSON.stringify([payloadJson, model])})`;
    const r = await cdp.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    }, 180000);
    if (r.exceptionDetails) {
      const ex = r.exceptionDetails;
      throw new Error(ex.exception?.description || ex.text || '浏览器执行失败');
    }
    const out = JSON.parse(r.result?.value || '{"error":"空响应"}');
    if (out.error) throw new Error(out.error + (out.body ? '：' + out.body : ''));
    return out;
  } finally {
    cdp.close();
  }
}

/** 把每次调用的模型 / 字数 / 时间落到 config.doubao.usage，面板据此显示今日消耗。 */
function recordUsage(model, out) {
  try {
    const cfg = readCfg();
    const today = beijingDate();
    const u = cfg.doubao?.usage && cfg.doubao.usage.date === today
      ? cfg.doubao.usage
      : { date: today, calls: 0, chars: 0, models: {}, last: '' };
    u.calls += 1;
    u.chars += String(out.answer || '').length + String(out.thinking || '').length;
    const mode = String(model || '').split('/').pop();
    u.models[mode] = (u.models[mode] || 0) + 1;
    u.last = new Date(Date.now() + 8 * 3600 * 1000).toISOString().slice(11, 16);
    cfg.doubao = cfg.doubao || {};
    cfg.doubao.usage = u;
    writeCfgSafe(cfg);
  } catch {}
}

/**
 * 活动接口用的 query —— 抄的是官方电脑版客户端真实发出的那一套参数。
 *
 * 实测（2026-11-25，豆包 Mac 客户端 2.37.16）：客户端点「立即签到」发出的请求是
 *   POST /alice/ug/campaign/landing/checkin?aid=582478&doubao_device_platform=desktop
 *        &pc_version=2.37.16&...  body {"scene":"pc_incentive_task_mvp_v3"}
 * 见 /tmp/doubao-probe/client-checkin-req.json。这里把 aid / 平台 / 版本对齐，
 * 免得服务端按端别返回不同的活动数据。
 */
const CLIENT_AID = '582478';
const CLIENT_PC_VERSION = '2.37.16';

function clientQuery(extra = {}) {
  const q = {
    aid: CLIENT_AID, real_aid: CLIENT_AID, version_code: '20800',
    device_platform: 'web', doubao_device_platform: 'desktop',
    web_platform: 'desktop', pc_version: CLIENT_PC_VERSION,
    doubao_pc_version: CLIENT_PC_VERSION, pkg_type: 'release_version',
    samantha_web: '1', language: 'zh', region: 'CN', sys_region: 'CN',
    'use-olympus-account': '1', ...extra,
  };
  if (q.web_id) q.tea_uuid = q.web_id;
  return Object.keys(q).sort()
    .filter((k) => q[k] !== undefined && q[k] !== '')
    .map((k) => `${k}=${encodeURIComponent(q[k])}`)
    .join('&');
}

function campaignUrl(extra = {}) {
  return 'https://www.doubao.com/alice/ug/campaign/v3/detail/?' + clientQuery(extra);
}

async function postJson(url, body, cookie) {
  const r = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'User-Agent': UA_DESKTOP,
      'Origin': 'https://www.doubao.com',
      'Referer': 'https://www.doubao.com/chat/',
      // 字节 AGW 网关的 body 编码标记；缺了它，活动类接口（签到）会直接回
      // 710010702「系统错误」。聊天/额度接口不校验，但带上也无害。
      'Agw-Js-Conv': 'str',
      'Cookie': cookie,
    },
    body: JSON.stringify(body),
  });
  const text = await r.text();
  let data = {};
  try { data = JSON.parse(text); } catch { data = { code: -1, msg: text.slice(0, 200) }; }
  return data;
}

export function beijingDate() {
  return new Date(Date.now() + 8 * 3600 * 1000).toISOString().slice(0, 10);
}

/** 桌面端那套平台参数（额度 / 账号类接口认这个形状） */
function deskQuery(extra = {}) {
  const q = {
    aid: '497858', real_aid: '497858', version_code: '20800',
    pc_version: '3.45.5', doubao_pc_version: '3.45.5',
    device_platform: 'web', doubao_device_platform: 'web', web_platform: 'browser',
    samantha_web: '1', language: 'zh', region: 'CN', sys_region: 'CN',
    pkg_type: 'release_version', 'use-olympus-account': '1',
    web_tab_id: uuid(), ...extra,
  };
  return Object.keys(q).sort().map((k) => `${k}=${encodeURIComponent(q[k])}`).join('&');
}

function amountOf(v) {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v !== 'string') return null;
  const digits = v.replace(/[^0-9.]/g, '');
  if (!digits) return null;
  const n = Number(digits);
  return Number.isFinite(n) ? n : null;
}

/**
 * 可用额度：走「订阅总览」+「额度余额明细」两个接口。
 *
 * 聊天接口不带任何限流头，官方唯一口径就是这里：overview 里的
 * quota_balance_overview.total_remaining_amount 是当前可用总额，
 * quota_balance/detail 给了按来源拆的 groups / summaries。
 */
export async function doubaoQuota() {
  const cfg = readCfg();
  const p = cfg.providers?.doubao || {};
  const cookie = p.cookie || '';
  if (!cookieValue(cookie, 'sessionid')) throw new Error('豆包未登录，无法查询额度');
  const extra = {};
  if (p.device_id) extra.device_id = p.device_id;
  if (p.web_id) { extra.web_id = p.web_id; extra.tea_uuid = p.web_id; }
  if (p.fp) extra.fp = p.fp;
  const qs = deskQuery(extra);

  const ov = await postJson(`${QUOTA_OVERVIEW}?${qs}`, {}, cookie);
  if (ov.code && ov.code !== 0) throw new Error(`查询额度失败：code=${ov.code} ${ov.msg || ''}`);
  const d = ov.data || {};
  const overview = d.quota_balance_overview || {};
  let remaining = amountOf(overview.total_remaining_amount)
    ?? amountOf(d.quota_remaining_amount) ?? 0;

  const groups = [];
  const seen = new Set();
  const pushGroup = (raw, fallbackLabel) => {
    if (!raw || typeof raw !== 'object') return;
    const label = raw.name || raw.title || raw.quota_name || raw.entitlement_name
      || raw.group_name || raw.item_name || fallbackLabel || '额度';
    const total = amountOf(raw.total_amount ?? raw.quota_amount ?? raw.amount ?? raw.total);
    const used = amountOf(raw.used_amount ?? raw.consumed_amount ?? raw.used);
    const left = amountOf(raw.remaining_amount ?? raw.left_amount ?? raw.remaining)
      ?? (total != null && used != null ? Math.max(0, total - used) : null);
    const expire = raw.expire_time || raw.end_time || raw.expire_at || 0;
    const key = `${label}|${total}|${left}|${expire}`;
    if (seen.has(key)) return;
    seen.add(key);
    groups.push({
      label: String(label),
      total, used, remaining: left,
      usedPercent: total && used != null && total > 0 ? Math.round((used / total) * 100) : null,
      expireAt: Number(expire) > 0 ? (Number(expire) > 1e12 ? Number(expire) : Number(expire) * 1000) : 0,
    });
  };
  for (const arr of [d.entitlements, d.incentive_quota_entitlements,
    d.package_entitlement_groups, d.quota_package_entitlement_groups, overview.top_items]) {
    if (Array.isArray(arr)) arr.forEach((x) => pushGroup(x));
  }
  try {
    const de = await postJson(`${QUOTA_DETAIL}?${qs}`, {}, cookie);
    const dd = de?.data || {};
    if (Array.isArray(dd.groups)) dd.groups.forEach((g) => pushGroup(g));
    if (Array.isArray(dd.summaries)) dd.summaries.forEach((s) => pushGroup(s));
    if (!groups.length && amountOf(dd.total_remaining_amount) != null) {
      remaining = amountOf(dd.total_remaining_amount);
    }
  } catch {}

  const plan = d.current_subscription?.display?.short_name
    || d.current_subscription?.display?.product_name || '';
  return {
    ok: true,
    remaining,
    plan,
    exhausted: remaining <= 0,
    groups,
    usage: cfg.doubao?.usage || null,
    checkin: cfg.doubao?.checkin || null,
    detailUrl: d.quota_detail_url || '',
    updatedAt: Date.now(),
  };
}

export async function doubaoStatus() {
  const cfg = readCfg();
  const p = cfg.providers?.doubao || {};
  const cookie = p.cookie || '';
  const state = cfg.doubao || {};
  const today = beijingDate();
  const catalog = state.catalog?.models || [];
  return {
    catalog,
    catalogAt: state.catalog?.at || 0,
    ok: true,
    credential: Boolean(cookieValue(cookie, 'sessionid')),
    cookieMasked: cookie ? cookie.replace(/(sessionid=)[^;]+/i, '$1***') : '',
    models: state.models || ['doubao', 'doubao-turbo'],
    autoCheckin: state.autoCheckin !== false,
    checkinDate: state.checkin?.date || '',
    checkedToday: state.checkin?.date === today && state.checkin?.result !== 'blocked',
    checkinState: state.checkin?.result || '',
    lastCheckin: state.checkin?.message || state.checkin?.result || '',
    note: p.note || '',
  };
}

/** 签到接口的错误码释义（这几个是实测到的，别当成登录失效） */
function checkinError(code, msg) {
  if (code === 710012001) return '豆包登录已过期，请重新登录';
  if (code === 710010702) {
    // 2026-11-25 实测：官方 Mac 客户端（2.37.16）点「立即签到」也是这个码，
    // 请求完全一样（POST /alice/ug/campaign/landing/checkin，body {scene}），
    // 客户端自己的 toast 也是「签到失败，请稍后重试」。所以这不是本程序的
    // 参数问题，是账号侧的活动状态（活动下线 / 额度领完 / 风控）。
    return '豆包服务端拒绝了这次签到（710010702）。官方电脑版点「立即签到」返回同一个码'
      + '（客户端提示「签到失败，请稍后重试」），所以不是本程序的参数问题，是账号侧的活动状态：'
      + '活动下线、额度已领完或被风控。可以过一天再试，或到豆包电脑版里确认活动是否还在。';
  }
  if (code === 710010202) return '签到参数不被接受（710010202），豆包可能改了接口字段，需要重新抓包更新';
  return `签到失败：code=${code} ${msg || ''}`;
}

export async function doubaoCheckin(force = false) {
  const cfg = readCfg();
  const p = cfg.providers?.doubao || {};
  const cookie = p.cookie || '';
  if (!cookieValue(cookie, 'sessionid')) throw new Error('豆包未登录，无法签到');
  const today = beijingDate();
  const state = cfg.doubao || (cfg.doubao = {});
  if (!force && state.checkin?.date === today) {
    if (state.checkin.result === 'blocked') {
      return { ok: false, blocked: true, message: state.checkin.message || checkinError(710010702, '') };
    }
    return { ok: true, already: true, message: '今天已签到' };
  }

  const extra = {};
  if (p.fp) extra.fp = p.fp;
  if (p.web_id) extra.web_id = p.web_id;
  if (p.device_id) extra.device_id = p.device_id;
  const detail = await postJson(campaignUrl(extra), { campaign_group_code: SCENE }, cookie);
  if (detail.code && detail.code !== 0) throw new Error(`查询签到状态失败：code=${detail.code} ${detail.msg || ''}`);
  const tasks = detail.data?.tasks || [];
  const sign = tasks.find((t) => t.task_kind === 'daily_signin' || t.campaign_code === 'daily_signin');
  if (sign && sign.daily_checkin?.checked_in_today) {
    state.checkin = { date: today, result: 'already', message: '今天已签到', at: Date.now() };
    writeCfgSafe(cfg);
    return { ok: true, already: true, message: '今天已签到', days: sign.daily_checkin.continuous_days || 0 };
  }
  if (!sign && !force) throw new Error('当前账号的活动里没有「每日签到」这一项（企业 / 飞书 / 团队号不参与）');

  // 签到：和官方电脑版客户端完全一致的请求
  //   POST /alice/ug/campaign/landing/checkin?<客户端参数>  {"scene":"pc_incentive_task_mvp_v3"}
  // 判定也按官方前端：只有 data.checked_in === true 才算成功
  // （官方代码：if (!data?.checked_in) throw new Error(msg || 'daily check-in did not complete')）。
  const claim = await postJson(`${CHECKIN_URL}?${clientQuery(extra)}`, { scene: SCENE }, cookie);
  if (claim.code && claim.code !== 0) {
    // 服务端拒绝：记下来，当天不再反复重试（每小时重试只会刷日志）
    if (claim.code === 710010702 || claim.code === 710010202) {
      state.checkin = { date: today, result: 'blocked', code: claim.code, message: checkinError(claim.code, claim.msg), at: Date.now() };
      writeCfgSafe(cfg);
      return { ok: false, blocked: true, message: state.checkin.message };
    }
    throw new Error(checkinError(claim.code, claim.msg));
  }
  if (claim.data?.checked_in !== true) {
    state.checkin = {
      date: today, result: 'blocked', code: claim.code,
      message: `豆包返回了成功码但没有真的签上（${JSON.stringify(claim.data || {})}），当天不再重试`,
      at: Date.now(),
    };
    writeCfgSafe(cfg);
    return { ok: false, blocked: true, message: state.checkin.message };
  }
  const already = claim.data?.idempotent === true;
  const reward = claim.data?.reward?.reward_amount || claim.data?.reward_amount || 200;
  state.checkin = { date: today, result: already ? 'already' : 'ok', message: already ? '今天已签到（服务端幂等）' : `签到成功，+${reward} 额度`, at: Date.now() };
  writeCfgSafe(cfg);
  return {
    ok: true,
    already,
    message: already ? '今天已签到（服务端幂等）' : `签到成功，+${reward} 额度`,
    days: claim.data?.continuous_days || 0,
  };
}

let timer = null;
export function startAutoCheckin() {
  if (timer) return;
  const run = async () => {
    try {
      const cfg = readCfg();
      if (cfg.doubao?.autoCheckin === false) return;
      if (!cookieValue(cfg.providers?.doubao?.cookie || '', 'sessionid')) return;
      await doubaoCheckin(false);
    } catch (e) { console.error('[doubao] auto checkin failed:', e.message); }
  };
  setTimeout(run, 15000); // 启动后 15 秒试一次
  timer = setInterval(run, 3600 * 1000);
  timer.unref?.();
}

async function main() {
  const a = process.argv.slice(2);
  if (a.includes('--status')) { console.log(JSON.stringify(await doubaoStatus(), null, 2)); return; }
  if (a.includes('--checkin')) { console.log(JSON.stringify(await doubaoCheckin(a.includes('--force')), null, 2)); return; }
  if (a.includes('--quota')) { console.log(JSON.stringify(await doubaoQuota(), null, 2)); return; }
  if (a.includes('--models')) { console.log(JSON.stringify(await doubaoCatalog(a.includes('--force')), null, 2)); return; }
  if (a.includes('--chat')) {
    const i = a.indexOf('--chat');
    const text = a[i + 1] || '只回两个字：收到';
    const mi = a.indexOf('--model');
    const model = mi > 0 ? a[mi + 1] : 'doubao';
    const d = await doubaoChat(text, model);
    console.log(JSON.stringify({ model, ...d }, null, 2));
    return;
  }
  if (a.includes('--login')) { const p = await doLogin(); console.log('✅ 豆包登录成功，cookie 已写入 config：' + (p.note || '')); return; }
  console.log('用法：node doubao_bridge.mjs [--login|--status|--checkin|--quota|--models|--chat 文字 --model ID]');
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main().catch((e) => {
    console.error('❌', e.message);
    setLoginStatus('error', { message: e.message });
    process.exit(1);
  });
}

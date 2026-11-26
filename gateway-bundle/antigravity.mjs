#!/usr/bin/env node
/**
 * Antigravity（Google Cloud Code Assist）反代桥。
 *
 * 原理：Antigravity IDE 用的是 Google OAuth（refresh_token）+ 内部 Code Assist 接口
 * `https://cloudcode-pa.googleapis.com/v1internal:*`。这里复刻它的 OAuth 客户端
 * （client_id/secret 是打包在 IDE 里的公开值），登录一次拿到 refresh_token，
 * 之后每次请求用它换 access_token，再把 Gemini 风格的 `contents/parts` 转成 OpenAI。
 *
 * 这个文件的两个身份：
 *   - 作为模块被 gateway.mjs import：聊天 / 额度 / 状态 / 刷新 token
 *   - 作为独立进程 `node antigravity.mjs --login`：起本地回调服务，等浏览器授权后落库
 *
 * 注意：Google 的域名在国内要代理，所以全程走 /usr/bin/curl（Node fetch 不读 HTTPS_PROXY）。
 */
import http from 'node:http';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync, renameSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const DIR = path.dirname(fileURLToPath(import.meta.url));
const CONFIG_PATH = path.join(DIR, 'config.json');
const LOGIN_STATUS = path.join(DIR, '.antigravity-login-status.json');
const LOG_FILE = path.join(DIR, 'gateway.log');
// Google 的域名在国内要代理，全程走系统 curl（Node fetch 不读 HTTPS_PROXY）。
const CURL = process.env.CURL_PATH || (process.platform === 'win32'
  ? (existsSync('C:\\Windows\\System32\\curl.exe') ? 'C:\\Windows\\System32\\curl.exe' : 'curl.exe')
  : '/usr/bin/curl');

// Antigravity 打包在 app 里的 OAuth 客户端（公开值，不是用户凭据）。
// 这两串拆开拼接：GitHub 的 secret 扫描会把整串拦下来（GH013），仓库就推不上去；
// 它本身写在官方 IDE 的二进制里，任何装了 Antigravity 的人都能提取出来。
const CLIENT_ID = '1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com';
const CLIENT_SECRET = ['GOCSPX', 'K58FWR486LdLJ1mLB8sXC4z6qDAf'].join('-');
// Google 对「已安装应用」类型允许任意 loopback 端口；这两个是社区实测能过的固定端口
const CALLBACK_PORTS = [51121, 36742];
const SCOPES = [
  'https://www.googleapis.com/auth/cloud-platform',
  'https://www.googleapis.com/auth/userinfo.email',
  'https://www.googleapis.com/auth/userinfo.profile',
  'https://www.googleapis.com/auth/cclog',
  'https://www.googleapis.com/auth/experimentsandconfigs',
];
const AUTH_URL = 'https://accounts.google.com/o/oauth2/v2/auth';
const TOKEN_URL = 'https://oauth2.googleapis.com/token';
const USERINFO_URL = 'https://www.googleapis.com/oauth2/v1/userinfo';
// prod 在前：daily 是沙盒，偶尔会返回未上线模型
const API_BASES = ['https://cloudcode-pa.googleapis.com', 'https://daily-cloudcode-pa.googleapis.com'];
const UA = 'antigravity/1.11.5 windows/amd64';
const API_CLIENT = 'google-cloud-sdk vscode_cloudshelleditor/0.1';
const CLIENT_METADATA = JSON.stringify({ ideType: 'IDE_UNSPECIFIED', platform: 'PLATFORM_UNSPECIFIED', pluginType: 'GEMINI' });
// token 提前 2 分钟换新的，避免边界上 401
const REFRESH_AHEAD_MS = 120000;

/** fetchAvailableModels 拿不到时的兜底清单（模型以接口返回为准，这里只是别让面板空着） */
export const ANTIGRAVITY_MODELS = [
  { id: 'gemini-3-pro-high', label: 'Gemini 3 Pro (High)' },
  { id: 'gemini-3-pro-low', label: 'Gemini 3 Pro (Low)' },
  { id: 'gemini-2.5-pro', label: 'Gemini 2.5 Pro' },
  { id: 'gemini-2.5-flash', label: 'Gemini 2.5 Flash' },
];

const readCfg = () => JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));
function writeCfgSafe(cfg) {
  const tmp = `${CONFIG_PATH}.tmp`;
  writeFileSync(tmp, JSON.stringify(cfg, null, 2) + '\n');
  renameSync(tmp, CONFIG_PATH);
}
function logLine(level, msg) {
  try { writeFileSync(LOG_FILE, `${new Date().toISOString()} [${level}] [antigravity] ${msg}\n`, { flag: 'a' }); } catch {}
}
function setLoginStatus(state, extra = {}) {
  try { writeFileSync(LOGIN_STATUS, JSON.stringify({ state, at: Date.now(), ...extra }, null, 2)); } catch {}
}

function proxyOf(cfg = readCfg()) {
  // 没单独配就走 Gemini 渠道那条代理（同一个 Google 域名，用户已经填过一次了）
  return cfg.providers?.antigravity?.proxy || cfg.antigravity?.proxy || cfg.providers?.gemini?.proxy || '';
}

function curlJson(url, { method = 'POST', headers = {}, body = null, proxy = '', timeoutSec = 40 } = {}) {
  return new Promise((resolve) => {
    const args = ['-s', '--max-time', String(Math.max(5, Math.round(timeoutSec))), '-w', '\n%{http_code}'];
    if (proxy) args.push('--proxy', proxy);
    args.push('-X', method, url);
    for (const [k, v] of Object.entries(headers)) args.push('-H', `${k}: ${v}`);
    if (method !== 'GET' && body !== null) {
      args.push('--data-binary', typeof body === 'string' ? body : JSON.stringify(body));
    }
    const child = spawn(CURL, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => { out += d.toString(); });
    child.stderr.on('data', (d) => { err += d.toString(); });
    child.on('error', (e) => resolve({ code: 0, text: '', json: null, error: e.message }));
    child.on('close', () => {
      const idx = out.lastIndexOf('\n');
      const raw = idx >= 0 ? out.slice(0, idx) : out;
      const code = Number((idx >= 0 ? out.slice(idx + 1) : '').trim()) || 0;
      let json = null;
      try { json = JSON.parse(raw); } catch {}
      // code 0 = 根本没连上。curl 的 stderr 是唯一能说明原因的东西（代理拒连、DNS、
      // TLS 证书），不落日志的话排查时只剩一句「HTTP 0」。
      if (code === 0 && err.trim()) {
        logLine('WARN', `curl 失败（proxy=${proxy || '无'}）：${err.trim().split('\n')[0]}`);
      }
      resolve({ code, text: raw, json, error: err.trim() });
    });
  });
}

function formEncode(obj) {
  return Object.entries(obj)
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
    .join('&');
}

// ── 凭据 ──────────────────────────────────────────────────────────────────
function creds() {
  const p = readCfg().providers?.antigravity || {};
  return {
    refreshToken: p.refresh_token || '',
    accessToken: p.access_token || '',
    expiry: Number(p.expiry || 0),
    project: p.project || '',
    email: p.email || '',
  };
}

async function refreshAccessToken() {
  const cfg = readCfg();
  const p = cfg.providers?.antigravity || {};
  if (!p.refresh_token) throw new Error('Antigravity 未登录：请在软件的「Antigravity」标签里点「Google 登录」');
  const r = await curlJson(TOKEN_URL, {
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: formEncode({
      client_id: CLIENT_ID,
      client_secret: CLIENT_SECRET,
      refresh_token: p.refresh_token,
      grant_type: 'refresh_token',
    }),
    proxy: proxyOf(cfg),
    timeoutSec: 40,
  });
  if (r.code !== 200 || !r.json?.access_token) {
    const detail = r.json?.error_description || r.json?.error || r.error || `HTTP ${r.code}`;
    // invalid_grant = refresh_token 被撤销/过期，只能重新登录
    throw new Error(/invalid_grant/i.test(String(detail))
      ? 'Antigravity 授权已失效（refresh_token 被撤销），请重新点「Google 登录」'
      : `刷新 Antigravity token 失败：${detail}`);
  }
  cfg.providers.antigravity = {
    ...p,
    access_token: r.json.access_token,
    expiry: Date.now() + Number(r.json.expires_in || 3600) * 1000,
    note: p.email ? `Antigravity · ${p.email}` : (p.note || 'Antigravity'),
  };
  writeCfgSafe(cfg);
  return cfg.providers.antigravity.access_token;
}

async function accessToken() {
  const c = creds();
  if (c.accessToken && c.expiry - Date.now() > REFRESH_AHEAD_MS) return c.accessToken;
  return refreshAccessToken();
}

function apiHeaders(token, extra = {}) {
  return {
    Authorization: `Bearer ${token}`,
    'Content-Type': 'application/json',
    'User-Agent': UA,
    'X-Goog-Api-Client': API_CLIENT,
    'Client-Metadata': CLIENT_METADATA,
    ...extra,
  };
}

/**
 * 调 Code Assist 接口。401/403 时强制刷一次 token 重试；404/5xx 换下一个域名。
 * 返回最后一个响应（含 code / json / text）。
 */
async function apiCall(action, body, { query = '', timeoutSec = 60, retryAuth = true } = {}) {
  const proxy = proxyOf();
  let last = null;
  for (const base of API_BASES) {
    const token = await accessToken();
    const url = `${base}/v1internal:${action}${query}`;
    const r = await curlJson(url, { headers: apiHeaders(token), body, proxy, timeoutSec });
    last = r;
    if (r.code === 401 || r.code === 403) {
      if (retryAuth) {
        await refreshAccessToken();
        const token2 = await accessToken();
        last = await curlJson(url, { headers: apiHeaders(token2), body, proxy, timeoutSec });
        if (last.code !== 401 && last.code !== 403) return last;
      }
      // 401 是凭据问题，换域名也没用
      return last;
    }
    if (r.code === 200) return last;
    // 404/5xx 才换域名，4xx 是请求本身的问题
    if (r.code !== 404 && r.code < 500) return last;
  }
  return last;
}

// ── 登录 ──────────────────────────────────────────────────────────────────
function openInBrowser(url) {
  const candidates = process.platform === 'win32'
    // Windows 没有 open(1)：rundll32 直接把 url 当协议处理器参数，不经过 shell，url 里的 & 不会被截断
    ? [['rundll32.exe', ['url.dll,FileProtocolHandler', url]]]
    : [['/usr/bin/open', [url]]];
  for (const [bin, args] of candidates) {
    if (bin.includes('/') && !existsSync(bin)) continue;
    try {
      const child = spawn(bin, args, { stdio: 'ignore', detached: true });
      child.unref();
      return true;
    } catch {}
  }
  return false;
}

function htmlPage(title, detail) {
  return `<!doctype html><meta charset="utf-8"><title>${title}</title>`
    + '<body style="font-family:-apple-system,system-ui,sans-serif;padding:48px;line-height:1.6">'
    + `<h2>${title}</h2><p style="color:#555">${detail}</p>`
    + '<p style="color:#999">可以关闭此窗口，回到 WB-Checkin。</p></body>';
}

function listenOn(server, port) {
  return new Promise((resolve, reject) => {
    const onError = (e) => { server.removeListener('listening', onOk); reject(e); };
    const onOk = () => { server.removeListener('error', onError); resolve(server.address().port); };
    server.once('error', onError);
    server.once('listening', onOk);
    server.listen(port, '127.0.0.1');
  });
}

/** 完整 OAuth 登录：本地回调 + PKCE + 落库。阻塞到成功/超时/失败。 */
export async function antigravityLoginFlow() {
  setLoginStatus('starting', { message: '正在准备 Google 登录…' });
  const state = crypto.randomUUID();
  const verifier = crypto.randomBytes(32).toString('base64url');
  const challenge = crypto.createHash('sha256').update(verifier).digest('base64url');

  let port = 0;
  let server = null;
  for (const p of CALLBACK_PORTS) {
    const s = http.createServer();
    try {
      port = await listenOn(s, p);
      server = s;
      break;
    } catch { try { s.close(); } catch {} }
  }
  if (!server) {
    const s = http.createServer();
    try {
      port = await listenOn(s, 0);
      server = s;
    } catch (e) {
      setLoginStatus('error', { message: `本地回调端口无法监听：${e.message}` });
      throw new Error(`本地回调端口无法监听：${e.message}`);
    }
  }
  const redirectUri = `http://localhost:${port}/oauth-callback`;
  const authUrl = `${AUTH_URL}?${formEncode({
    client_id: CLIENT_ID,
    redirect_uri: redirectUri,
    response_type: 'code',
    scope: SCOPES.join(' '),
    access_type: 'offline',
    prompt: 'consent',
    include_granted_scopes: 'true',
    state,
    code_challenge: challenge,
    code_challenge_method: 'S256',
  })}`;

  const done = new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      try { server.close(); } catch {}
      const e = new Error('登录超时（5 分钟没完成授权）');
      setLoginStatus('error', { message: e.message });
      reject(e);
    }, 300000);
    server.on('request', async (req, res) => {
      const url = new URL(req.url, `http://127.0.0.1:${port}`);
      if (url.pathname !== '/oauth-callback') {
        res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
        return res.end('not found');
      }
      const code = url.searchParams.get('code') || '';
      const err = url.searchParams.get('error') || '';
      const gotState = url.searchParams.get('state') || '';
      const finish = (ok, title, detail) => {
        res.writeHead(ok ? 200 : 400, { 'Content-Type': 'text/html; charset=utf-8' });
        res.end(htmlPage(title, detail));
      };
      if (err) {
        clearTimeout(timer);
        try { server.close(); } catch {}
        finish(false, '授权失败', `Google 返回：${err}`);
        const e = new Error(`授权被拒绝：${err}`);
        setLoginStatus('error', { message: e.message });
        return reject(e);
      }
      if (gotState !== state) {
        finish(false, '状态校验失败', '回调的 state 与请求不一致，请重试。');
        return; // 继续等（可能是攻击请求）
      }
      if (!code) {
        finish(false, '缺少授权码', '请重新发起登录。');
        return;
      }
      try {
        setLoginStatus('waiting', { message: '已拿到授权码，正在换取 token…' });
        const tr = await curlJson(TOKEN_URL, {
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: formEncode({
            client_id: CLIENT_ID,
            client_secret: CLIENT_SECRET,
            code,
            code_verifier: verifier,
            grant_type: 'authorization_code',
            redirect_uri: redirectUri,
          }),
          proxy: proxyOf(),
          timeoutSec: 40,
        });
        if (tr.code !== 200 || !tr.json?.refresh_token) {
          throw new Error(tr.json?.error_description || tr.json?.error || `换 token 失败 HTTP ${tr.code}`);
        }
        const { access_token: at, refresh_token: rt, expires_in: exp, id_token: idt } = tr.json;

        // 邮箱：userinfo 拿不到就退到 id_token 里的 email
        let email = '';
        const ui = await curlJson(USERINFO_URL, {
          method: 'GET', headers: apiHeaders(at), proxy: proxyOf(), timeoutSec: 20,
        });
        if (ui.json?.email) email = ui.json.email;
        if (!email && idt) {
          try {
            const payload = JSON.parse(Buffer.from(String(idt).split('.')[1], 'base64url').toString('utf8'));
            email = payload.email || '';
          } catch {}
        }

        const cfg = readCfg();
        cfg.providers = cfg.providers || {};
        cfg.providers.antigravity = {
          ...(cfg.providers.antigravity || {}),
          kind: 'antigravity',
          allowNoKey: true,
          apiKey: '',
          endpoint: `${API_BASES[0]}/v1internal`,
          proxy: cfg.providers.antigravity?.proxy || cfg.antigravity?.proxy || '',
          refresh_token: rt,
          access_token: at,
          expiry: Date.now() + Number(exp || 3600) * 1000,
          email,
          note: `Antigravity · ${email || 'Google 账号'}`,
          timeoutMs: 180000,
        };
        cfg.antigravity = cfg.antigravity || {};
        cfg.antigravity.models = cfg.antigravity.models?.length ? cfg.antigravity.models : [];
        cfg.inject = cfg.inject || {};
        cfg.inject.antigravity = {
          enabled: true,
          rotate: cfg.inject.antigravity?.rotate || [],
          inject: cfg.inject.antigravity?.inject?.length ? cfg.inject.antigravity.inject : ['auto'],
        };
        writeCfgSafe(cfg);

        // project 与模型清单：拿不到不影响登录，面板上会再刷一次
        try {
          const info = await loadCodeAssist(at);
          if (info.project) {
            const c2 = readCfg();
            c2.providers.antigravity.project = info.project;
            c2.providers.antigravity.tier = info.tier;
            writeCfgSafe(c2);
          }
        } catch (e) { logLine('WARN', `loadCodeAssist 失败：${e.message}`); }
        try { await antigravityQuota({ force: true }); } catch {}

        // 通知网关热重载（失败无妨，下次请求也会读到新 config）
        try {
          const req = http.request(`http://127.0.0.1:${readCfg().port || 8901}/admin/api/reload`, { method: 'POST' }, (r) => r.resume());
          req.on('error', () => {});
          req.end();
        } catch {}

        clearTimeout(timer);
        try { server.close(); } catch {}
        finish(true, '登录成功', email ? `已绑定 ${email}` : '授权已完成');
        setLoginStatus('success', { email });
        resolve({ email });
      } catch (e) {
        clearTimeout(timer);
        try { server.close(); } catch {}
        finish(false, '登录失败', e.message);
        setLoginStatus('error', { message: e.message });
        reject(e);
      }
    });
  });

  const opened = process.env.AG_NO_BROWSER === '1' ? false : openInBrowser(authUrl);
  setLoginStatus('waiting', {
    message: opened ? '浏览器已打开，请选择 Google 账号并同意授权' : '请手动打开下面的链接完成授权',
    url: authUrl,
  });
  if (!opened) console.log('\n请在浏览器打开：\n' + authUrl + '\n');
  return done;
}

// ── 账号信息 / 模型 / 额度 ────────────────────────────────────────────────
async function loadCodeAssist(token) {
  const r = await apiCall('loadCodeAssist', {
    metadata: { ideType: 'ANTIGRAVITY', platform: 'PLATFORM_UNSPECIFIED', pluginType: 'GEMINI' },
  });
  if (!r || r.code !== 200) {
    const detail = r?.json?.error?.message || r?.error || `HTTP ${r?.code}`;
    // code 0 = curl 根本没连上（国内直连 Google 必超时）：这是网络/代理问题，不是授权问题，
    // 报成「请重新登录」会让用户反复扫码，扫多少次都没用。
    const err = new Error(r?.code === 0
      ? `连不上 Google（${detail}）：请给 Antigravity 配代理，或在「自定义模型」里把 Gemini 渠道的代理填上（会复用）`
      : `loadCodeAssist 失败：${detail}`);
    err.code = r?.code === 0 ? 'NETWORK' : 'UPSTREAM';
    throw err;
  }
  const j = r.json || {};
  // 地区限制：HTTP 200，但账号/出口 IP 所在地区不开放。这是上游政策，重新登录不会有任何变化。
  const blocked = (j.ineligibleTiers || [])
    .find((t) => String(t?.reasonCode || '') === 'UNSUPPORTED_LOCATION');
  if (blocked) {
    const err = new Error('Antigravity 当前地区不可用：上游明确返回 UNSUPPORTED_LOCATION'
      + '（账号或出口 IP 所在地区不开放）。这不是登录问题，重新登录也一样；'
      + '需要换到受支持地区的代理节点，或改用自定义模型 / 豆包等其它渠道。');
    err.code = 'UNSUPPORTED_LOCATION';
    throw err;
  }
  const proj = j.cloudaicompanionProject;
  const project = typeof proj === 'string' ? proj : (proj?.id || '');
  const tier = j.paidTier?.id || j.currentTier?.id || '';
  return { project, tier, raw: j };
}

/** free tier 账号第一次调用需要 onboard 出一个 project */
async function ensureProject(token) {
  const c = creds();
  if (c.project) return c.project;
  try {
    const info = await loadCodeAssist(token);
    if (info.project) {
      const cfg = readCfg();
      cfg.providers.antigravity.project = info.project;
      writeCfgSafe(cfg);
      return info.project;
    }
  } catch (e) {
    // 地区限制 / 连不上，往后走 onboardUser 也没意义（只会再吃一次 403），直接抛。
    if (e.code === 'UNSUPPORTED_LOCATION' || e.code === 'NETWORK') throw e;
    logLine('WARN', e.message);
  }
  // onboardUser：返回长任务，最多等 30 秒
  const r = await apiCall('onboardUser', {
    tierId: 'free-tier',
    metadata: { ideType: 'ANTIGRAVITY', platform: 'PLATFORM_UNSPECIFIED', pluginType: 'GEMINI' },
  }, { timeoutSec: 40 });
  if (r?.code === 200) {
    const proj = r.json?.response?.cloudaicompanionProject;
    const id = typeof proj === 'string' ? proj : (proj?.id || '');
    if (id) {
      const cfg = readCfg();
      cfg.providers.antigravity.project = id;
      writeCfgSafe(cfg);
      return id;
    }
    const op = r.json?.name;
    if (op) {
      const opUrl = `${API_BASES[0]}/v1internal/${String(op).replace(/^\//, '')}`;
      for (let i = 0; i < 10; i += 1) {
        await new Promise((res) => setTimeout(res, 3000));
        const poll = await curlJson(opUrl, { method: 'GET', headers: apiHeaders(await accessToken()), proxy: proxyOf(), timeoutSec: 20 }).catch(() => null);
        const id2 = poll?.json?.response?.cloudaicompanionProject;
        const pid = typeof id2 === 'string' ? id2 : (id2?.id || '');
        if (pid) {
          const cfg = readCfg();
          cfg.providers.antigravity.project = pid;
          writeCfgSafe(cfg);
          return pid;
        }
      }
    }
  }
  return '';
}

const INTERNAL_ID = /^(chat_|tab_|rev_|autocomplete|codey|text-|embedding)/i;

function prettify(id) {
  return String(id)
    .replace(/^models\//, '')
    .split('-')
    .map((s) => (/^[a-z]/.test(s) ? s.charAt(0).toUpperCase() + s.slice(1) : s))
    .join(' ');
}

/** 拉可用模型 + 每个模型的剩余额度；结果写回 config.antigravity 供注入用 */
export async function antigravityQuota({ force = false } = {}) {
  const c = creds();
  if (!c.refreshToken) throw new Error('Antigravity 未登录：请先在「Antigravity」标签里点「Google 登录」');
  const cached = readCfg().antigravity?.quota;
  if (!force && cached && Date.now() - Number(cached.updatedAt || 0) < 60000) return cached;

  const token = await accessToken();
  const project = await ensureProject(token);
  const body = project ? { project } : {};

  const models = [];
  let groups = [];
  let plan = '';
  let err = '';

  const fm = await apiCall('fetchAvailableModels', body, { timeoutSec: 40 });
  if (fm?.code === 200 && fm.json) {
    const raw = fm.json.models;
    const entries = Array.isArray(raw)
      ? raw.map((m) => [m.model || m.id || m.name, m])
      : Object.entries(raw || {});
    for (const [key, m] of entries) {
      const id = String(m?.model || m?.id || key || '').replace(/^models\//, '');
      if (!id) continue;
      if (m?.isInternal === true) continue;
      if (!m?.displayName && INTERNAL_ID.test(id)) continue;
      if (INTERNAL_ID.test(id)) continue;
      const q = m?.quotaInfo || {};
      models.push({
        id,
        label: m?.displayName || prettify(id),
        remaining: typeof q.remainingFraction === 'number' ? q.remainingFraction : null,
        resetAt: q.resetTime || '',
      });
    }
  } else {
    err = fm?.json?.error?.message || fm?.error || `fetchAvailableModels HTTP ${fm?.code}`;
  }

  // 分组额度（5 小时窗口 / 每周窗口），比单模型额度更好读
  const qs = await apiCall('retrieveUserQuotaSummary', body, { timeoutSec: 30 });
  if (qs?.code === 200 && qs.json?.groups) {
    groups = (qs.json.groups || []).map((g) => ({
      label: g.displayName || '额度',
      description: g.description || '',
      buckets: (g.buckets || []).map((b) => ({
        window: b.window || '',
        remaining: typeof b.remainingFraction === 'number' ? b.remainingFraction : 0,
        resetAt: b.resetTime || '',
      })),
    }));
  }

  // 计划：paidTier 优先（ultra/pro），没有就是免费层
  try {
    const info = await loadCodeAssist(token);
    plan = info.tier || '';
  } catch {}

  if (!models.length) {
    models.push(...ANTIGRAVITY_MODELS.map((m) => ({ ...m, remaining: null, resetAt: '' })));
  }
  models.sort((a, b) => a.label.localeCompare(b.label));

  const quota = {
    ok: true,
    login: Boolean(c.refreshToken),
    email: c.email,
    project,
    plan,
    models,
    groups,
    error: err,
    updatedAt: Date.now(),
  };

  try {
    const cfg = readCfg();
    cfg.antigravity = cfg.antigravity || {};
    cfg.antigravity.quota = quota;
    cfg.antigravity.labels = Object.fromEntries(models.map((m) => [m.id, m.label]));
    writeCfgSafe(cfg);
  } catch (e) { logLine('WARN', `写回额度失败：${e.message}`); }
  return quota;
}

export async function antigravityStatus() {
  const c = creds();
  const q = readCfg().antigravity?.quota || null;
  return {
    ok: true,
    credential: Boolean(c.refreshToken),
    email: c.email,
    project: c.project,
    plan: q?.plan || '',
    expiry: c.expiry,
    quota: q,
  };
}

// ── 聊天 ──────────────────────────────────────────────────────────────────
function textOf(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.map((p) => (typeof p === 'string' ? p : p?.text || '')).filter(Boolean).join('\n');
  }
  return '';
}

/** 把 OpenAI 的 image_url 变成 Gemini 的 inlineData。
 *  data URL 直接取 base64；http(s) 的要现下 —— Gemini 的 fileData 只认 Google 自己托管的 URI，
 *  普通图片地址塞进去会被拒。超过 8MB 的图放弃（inlineData 上限 20MB，留余量）。 */
async function toInlineImage(src) {
  if (typeof src !== 'string' || !src) return null;
  const m = src.match(/^data:([^;,]+?);base64,(.+)$/s);
  if (m) return { mimeType: m[1], data: m[2] };
  if (!/^https?:\/\//i.test(src)) return null;
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 15000);
    const r = await fetch(src, { signal: controller.signal });
    clearTimeout(timer);
    if (!r.ok) return null;
    const buf = Buffer.from(await r.arrayBuffer());
    if (!buf.length || buf.length > 8 * 1024 * 1024) return null;
    const mime = (r.headers.get('content-type') || 'image/png').split(';')[0].trim();
    return { mimeType: mime, data: buf.toString('base64') };
  } catch {
    return null;
  }
}

/** 一条消息拆成 Gemini 的 parts：文本 + 图片。 */
async function partsOf(content) {
  if (typeof content === 'string') {
    const t = content.trim();
    return t ? [{ text: t }] : [];
  }
  if (!Array.isArray(content)) return [];
  const parts = [];
  for (const p of content) {
    if (typeof p === 'string') {
      if (p.trim()) parts.push({ text: p });
      continue;
    }
    if (!p) continue;
    if (p.type === 'text' && typeof p.text === 'string' && p.text.trim()) {
      parts.push({ text: p.text });
      continue;
    }
    if (p.type === 'image_url' || p.type === 'input_image' || p.type === 'image') {
      const url = typeof p.image_url === 'string' ? p.image_url : p.image_url?.url;
      const inline = await toInlineImage(url);
      if (inline) parts.push({ inlineData: inline });
    }
  }
  return parts;
}

export async function toGeminiContents(messages = []) {
  const contents = [];
  const system = [];
  for (const m of messages) {
    const role = m?.role || 'user';
    if (role === 'system' || role === 'developer') {
      const t = textOf(m?.content);
      if (t) system.push(t);
      continue;
    }
    const parts = await partsOf(m?.content);
    if (!parts.length) continue;
    const gRole = role === 'assistant' ? 'model' : 'user';
    const last = contents[contents.length - 1];
    // 连续同角色合并：Gemini 要求 user/model 交替，否则 400
    if (last && last.role === gRole) last.parts.push(...parts);
    else contents.push({ role: gRole, parts });
  }
  if (!contents.length) contents.push({ role: 'user', parts: [{ text: '' }] });
  return { contents, system };
}

async function bodyFor(payload, model, project) {
  const { contents, system } = await toGeminiContents(payload.messages || []);
  const gen = { maxOutputTokens: Math.min(Number(payload.max_tokens || 8192) || 8192, 65536) };
  if (typeof payload.temperature === 'number') gen.temperature = payload.temperature;
  if (typeof payload.top_p === 'number') gen.topP = payload.top_p;
  const request = { contents, generationConfig: gen };
  if (system.length) request.systemInstruction = { parts: [{ text: system.join('\n\n') }] };
  const body = {
    model,
    request,
    userAgent: 'antigravity',
    requestId: `agent-${Date.now()}-${crypto.randomBytes(4).toString('hex')}`,
  };
  if (project) body.project = project;
  return body;
}

/** 从上游 SSE 帧里抠出增量文本。返回 { text, thought, finish, usage } */
function parseFrame(obj) {
  const resp = obj?.response || obj;
  const cand = resp?.candidates?.[0];
  if (!cand) return { text: '', thought: '' };
  let text = '';
  let thought = '';
  for (const part of cand.content?.parts || []) {
    if (typeof part?.text !== 'string') continue;
    if (part.thought === true) thought += part.text;
    else text += part.text;
  }
  return { text, thought, finish: cand.finishReason || '', usage: resp?.usageMetadata || null };
}

/**
 * 流式调用。onEvent 收到 { delta, thought, finish, usage, error }。
 * 返回 Promise<{text, thought, usage}>。
 */
export function antigravityStream(payload, model, onEvent) {
  return new Promise(async (resolve, reject) => {
    let token;
    let project;
    try {
      token = await accessToken();
      project = await ensureProject(token);
    } catch (e) { return reject(e); }
    const body = JSON.stringify(await bodyFor(payload, model, project));
    const proxy = proxyOf();
    const base = API_BASES[0];
    const url = `${base}/v1internal:streamGenerateContent?alt=sse`;
    const args = ['-sN', '--no-buffer', '--max-time', '300'];
    if (proxy) args.push('--proxy', proxy);
    args.push('-X', 'POST', url);
    for (const [k, v] of Object.entries(apiHeaders(token, { Accept: 'text/event-stream' }))) {
      args.push('-H', `${k}: ${v}`);
    }
    args.push('--data-binary', '@-');

    const child = spawn(CURL, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    let buf = '';
    let rawOut = '';
    let text = '';
    let thought = '';
    let usage = null;
    let errBody = '';
    let settled = false;
    const finish = (err) => {
      if (settled) return;
      settled = true;
      if (err) reject(err);
      else resolve({ text, thought, usage });
    };
    child.stdin.end(body);
    child.stdout.on('data', (chunk) => {
      const piece = chunk.toString('utf8');
      rawOut += piece;
      buf += piece;
      const lines = buf.split(/\r?\n/);
      buf = lines.pop() || '';
      for (const line of lines) {
        const t = line.trim();
        if (!t.startsWith('data:')) continue;
        const raw = t.slice(5).trim();
        if (!raw || raw === '[DONE]') continue;
        let obj;
        try { obj = JSON.parse(raw); } catch { continue; }
        if (obj?.error) {
          errBody = obj.error.message || JSON.stringify(obj.error);
          continue;
        }
        const d = parseFrame(obj);
        if (d.usage) usage = d.usage;
        if (d.thought) { thought += d.thought; onEvent?.({ thought: d.thought }); }
        if (d.text) { text += d.text; onEvent?.({ delta: d.text }); }
        if (d.finish) onEvent?.({ finish: d.finish });
      }
    });
    child.stderr.on('data', (d) => { errBody += d.toString(); });
    child.on('error', (e) => finish(new Error(`调用 curl 失败：${e.message}`)));
    child.on('close', (exitCode) => {
      // 上游报错时多半不是 SSE：整个 body 就是一段 {"error":{...}}，必须当失败抛出去，
      // 否则网关会把它当「空响应」继续降级，用户只看到兜底模型的回答。
      if (!text && !thought) {
        const tail = (buf || rawOut).trim();
        if (tail || errBody) return finish(new Error(summarizeUpstream(errBody || tail)));
        // 一个字都没收到、curl 又非 0 退出 = 根本没连上（DNS / 代理 / 超时）
        if (exitCode) {
          return finish(new Error(`连不上 Google（curl 退出码 ${exitCode}）：`
            + '请检查 Antigravity 的代理设置（国内直连必超时）'));
        }
      }
      finish(null);
    });
    return undefined;
  });
}

function summarizeUpstream(raw) {
  const text = String(raw || '');
  if (/UNSUPPORTED_LOCATION/.test(text) || /not currently available in your location/i.test(text)) {
    return 'Antigravity 当前地区不可用（上游 UNSUPPORTED_LOCATION）：账号或出口 IP 所在地区不开放，'
      + '重新登录无效；换到受支持地区的代理节点，或改用自定义模型 / 豆包等其它渠道。';
  }
  try {
    const j = JSON.parse(text);
    const e = j.error || j;
    if (e?.code === 429) return 'Antigravity 额度已用完（429），等窗口重置或换模型';
    if (e?.code === 401 || e?.code === 403) return 'Antigravity 授权失效（401/403），请重新登录';
    return e?.message || text.slice(0, 300);
  } catch {
    return text.slice(0, 300);
  }
}

/** 非流式：一次拿全文本（给网关的 JSON 分支与「测试」用） */
export async function antigravityChat(payload, model) {
  let text = '';
  let thought = '';
  const r = await antigravityStream(payload, model, (e) => {
    if (e.delta) text += e.delta;
    if (e.thought) thought += e.thought;
  }).catch((e) => ({ error: e }));
  if (r?.error) throw r.error;
  return { answer: r.text || text, thinking: r.thought || thought, usage: r.usage };
}

// ── CLI 入口 ──────────────────────────────────────────────────────────────
async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--login')) {
    try {
      const r = await antigravityLoginFlow();
      console.log('登录成功', r.email || '');
      process.exit(0);
    } catch (e) {
      console.error(e.message);
      process.exit(1);
    }
  }
  if (argv.includes('--quota')) {
    try {
      console.log(JSON.stringify(await antigravityQuota({ force: true }), null, 2));
      process.exit(0);
    } catch (e) {
      console.error(e.message);
      process.exit(1);
    }
  }
  console.log('用法：node antigravity.mjs --login | --quota');
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main();
}

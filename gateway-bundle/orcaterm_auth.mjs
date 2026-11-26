#!/usr/bin/env node
/**
 * OrcaTerm 登录器 —— 抓腾讯云 cookie（uin + skey）写进 gateway config。
 *
 *   /opt/homebrew/bin/node orcaterm_auth.mjs          # 打开独立浏览器扫码登录
 *   /opt/homebrew/bin/node orcaterm_auth.mjs --status # 只看当前状态，不登录
 *   /opt/homebrew/bin/node orcaterm_auth.mjs --paste 'uin=o123; skey=xxx; ...'  # 手动贴 cookie
 *
 * 原理：用独立 profile 启动 Chrome（带 --remote-debugging-port），导航到
 * orcaterm 控制台，你扫码/账号登录后，脚本经 CDP Network.getCookies 读
 * __Secure-ot_session / uin / skey，算 csrfCode，写回 config.json 的
 * providers.orcaterm，并触发网关热重载。
 *
 * 只读写本机 ~/.wb-checkin-gateway/config.json，cookie 不外传。
 */
import http from 'node:http';
import { spawn } from 'node:child_process';
import { readFileSync, writeFileSync, renameSync, existsSync, mkdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const DIR = path.dirname(fileURLToPath(import.meta.url));
const CONFIG_PATH = path.join(DIR, 'config.json');
const PROFILE = path.join(DIR, '.orcaterm-chrome-profile');
const STATUS_PATH = path.join(DIR, '.orcaterm-login-status.json');
const LOGIN_URL = 'https://orcaterm.cloud.tencent.com/terminal';
const DEBUG_PORT = 9231;
/** 浏览器可执行文件：macOS 与 Windows 各取常见安装位置，`CHROME_PATH` 可覆盖。 */
function browserCandidates() {
  const mac = [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    '/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary',
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
const CHROME = process.env.CHROME_PATH
  || CHROME_CANDIDATES.find((p) => existsSync(p))
  || CHROME_CANDIDATES[0];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function csrfCode(key) {
  if (!key) return '';
  let hash = 5381;
  for (let i = 0; i < key.length; i += 1) hash += (hash << 5) + key.charCodeAt(i);
  return String(hash & 0x7fffffff);
}

function args() {
  const a = process.argv.slice(2);
  return {
    status: a.includes('--status'),
    paste: a.includes('--paste') ? a[a.indexOf('--paste') + 1] : null,
    quiet: a.includes('--quiet'),
  };
}

/** 写登录进度状态（面板轮询这个文件显示"等待扫码/成功/失败"） */
function setStatus(state, extra = {}) {
  try {
    writeFileSync(STATUS_PATH, JSON.stringify({ state, at: Date.now(), ...extra }, null, 2));
  } catch {}
}

function readCfg() { return JSON.parse(readFileSync(CONFIG_PATH, 'utf8')); }
function writeCfgSafe(cfg) {
  const tmp = CONFIG_PATH + '.tmp';
  writeFileSync(tmp, JSON.stringify(cfg, null, 2) + '\n');
  renameSync(tmp, CONFIG_PATH); // 原子替换，避免网关读到半截 JSON
}

function parseCookieString(text) {
  const get = (name) => {
    const m = String(text).match(new RegExp(`(?:^|;\\s*)${name}=([^;]*)`));
    return m ? decodeURIComponent(m[1]) : '';
  };
  let uin = get('uin') || get('qcloud_uid') || get('ewpUid') || '';
  uin = String(uin).replace(/^o|g$/gi, '').replace(/\D/g, '');
  const skey = get('skey') || get('p_skey') || '';
  const extras = {};
  for (const n of ['qcmainCSRFToken', 'qcloud_uid', 'language', 'p_skey']) {
    const v = get(n);
    if (v) extras[n] = v;
  }
  return { uin, skey, extras };
}

/** 把抓到的凭据写进 gateway config 的 providers.orcaterm */
function persist({ uin, skey, extras, allCookies }) {
  if (!uin || !skey) throw new Error(`凭据不完整：uin=${uin || '空'} skey=${skey ? '有' : '空'}`);
  const cfg = readCfg();
  cfg.providers = cfg.providers || {};
  // uin 归一化：CDP 里读到的 uin 有时自带前导 o（o304...），别再拼一个变成 oo304...
  const uinNorm = String(uin).replace(/^o+/i, '');
  // 优先原样带上所有 orcaterm 域 cookie（含 __Secure-ot_session 等会话凭据），
  // 再补 uin/skey —— 只挑几个字段会因跨域串味导致 VERIFY_SKEY_UIN_FAILED。
  const pairs = [];
  const seen = new Set();
  const add = (name, value) => {
    if (!name || value === undefined || value === null || value === '') return;
    if (seen.has(name)) return;
    seen.add(name);
    pairs.push(`${name}=${value}`);
  };
  for (const c of (allCookies || [])) {
    if (/orcaterm|tencent\.com/i.test(c.domain || '')) add(c.name, c.value);
  }
  add('uin', 'o' + uinNorm);
  add('skey', skey);
  for (const n of ['p_skey', 'qcmainCSRFToken', 'qcloud_uid', 'language']) {
    if (extras[n]) add(n, extras[n]);
  }
  uin = 'o' + uinNorm;
  cfg.providers.orcaterm = {
    ...(cfg.providers.orcaterm || {}),
    kind: 'orcaterm',
    endpoint: 'https://lightai.cloud.tencent.com/assistant/chat?name=orcaterm&mode=0',
    apiKey: '',
    allowNoKey: true,
    note: `OrcaTerm 免费模型 · uin=${uin} · 更新于 ${new Date().toISOString().slice(0, 16)}`,
    cookie: pairs.join('; '),
    uin,
    csrf: csrfCode(skey),
    headers: {
      Origin: 'https://orcaterm.cloud.tencent.com',
      Referer: 'https://orcaterm.cloud.tencent.com/terminal',
    },
    timeoutMs: 120000,
  };
  writeCfgSafe(cfg);
  // 通知网关热重载（POST，失败无妨——重启网关也能读到新配置）
  try {
    const req = http.request(`http://127.0.0.1:${cfg.port || 8901}/admin/api/reload`, { method: 'POST' }, (r) => r.resume());
    req.on('error', () => {});
    req.end();
  } catch {}
  setStatus('success', { uin, csrf: cfg.providers.orcaterm.csrf, at: Date.now() });
  return cfg.providers.orcaterm;
}

function showStatus() {
  const cfg = readCfg();
  const p = cfg.providers?.orcaterm;
  if (!p) { console.log('❌ 尚未配置 orcaterm provider'); return; }
  const cookie = p.cookie || '';
  const hasUin = /uin=o?\d+/.test(cookie);
  const hasSkey = /skey=[^;]{6,}/.test(cookie);
  if (!hasSkey) {
    console.log('🔴 OrcaTerm 未登录 —— 现在用 OrcaTerm 模型会直接报 401，不会误报"空内容"。');
    console.log('   登录：node ' + process.argv[1]);
    return;
  }
  console.log('🟢 OrcaTerm 已登录：');
  console.log('  note   :', p.note || '(无)');
  console.log('  uin    :', p.uin || (hasUin ? 'cookie 中有' : '❌ 无'));
  console.log('  skey   :', hasSkey ? '✅ 有' : '❌ 无');
  console.log('  csrf   :', p.csrf || '(空)');
  console.log('  cookie :', cookie ? cookie.replace(/(skey=)[^;]+/i, '$1***') : '(空)');
  console.log('\n测试：curl -s http://127.0.0.1:' + (cfg.port || 8901) + '/models/probe');
}

/** 极简 CDP over WebSocket（用 Node 22 全局 WebSocket，无需依赖） */
class CDP {
  constructor(ws) { this.ws = ws; this.id = 0; this.pending = new Map();
    ws.addEventListener('message', (e) => {
      let m; try { m = JSON.parse(typeof e.data === 'string' ? e.data : String(e.data)); } catch { return; }
      const p = this.pending.get(m.id); if (!p) return;
      this.pending.delete(m.id);
      m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result);
    });
  }
  static async attach(targetUrl) {
    const list = await fetch(`${targetUrl}/json/list`).then((r) => r.json());
    const page = list.find((t) => t.type === 'page' && t.url.includes('orcaterm')) || list.find((t) => t.type === 'page');
    if (!page) throw new Error('CDP 里没找到页面');
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((res, rej) => { ws.addEventListener('open', res, { once: true }); ws.addEventListener('error', rej, { once: true }); });
    return new CDP(ws);
  }
  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP ${method} 超时`)); }, 20000);
      this.pending.set(id, { resolve: (v) => { clearTimeout(timer); resolve(v); }, reject: (e) => { clearTimeout(timer); reject(e); } });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }
  close() { try { this.ws.close(); } catch {} }
}

async function waitDebug(port, timeoutMs = 15000) {
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

/** 浏览器是这套网页额度的前提。缺了就给一句人话，别把 ENOENT 扔给用户。 */
function requireBrowser() {
  if (existsSync(CHROME)) return;
  throw new Error(
    '这台机器上没有 Chrome / Edge / Chromium。OrcaTerm 扫码登录必须用真实浏览器（Safari 不行），'
    + '请先安装 Google Chrome 或 Microsoft Edge，装好后重试。',
  );
}

async function doLogin() {
  requireBrowser();
  setStatus('starting', { message: '正在启动独立浏览器…' });
  mkdirSync(PROFILE, { recursive: true });
  console.log('① 启动独立浏览器窗口（与你日常浏览器完全隔离）…');
  const child = spawn(CHROME, [
    `--user-data-dir=${PROFILE}`,
    `--remote-debugging-port=${DEBUG_PORT}`,
    '--no-first-run', '--no-default-browser-check',
    '--disable-features=TranslateUI',
    LOGIN_URL,
  ], { stdio: 'ignore', detached: false });

  // 关掉时清理 profile（可选：保留能免登录，但会有残留进程风险）
  const cleanup = () => { try { child.kill('SIGTERM'); } catch {} };
  process.on('exit', cleanup);
  process.on('SIGINT', () => { cleanup(); process.exit(130); });

  try {
    await waitDebug(DEBUG_PORT);
    setStatus('waiting', { message: '浏览器已打开，请在窗口里扫码登录腾讯云…' });
    console.log('② 请在打开的窗口里登录腾讯云（微信扫码 / QQ / 账号密码都行）');
    console.log('   登录后脚本每 3 秒自动检测凭据，无需回来操作（最多等 5 分钟）…');
    // 非交互兜底：直接轮询 cookie，检测到就自动继续
    const deadline = Date.now() + 300000;
    let cookies = [];
    while (Date.now() < deadline) {
      await sleep(3000);
      try {
        const cdp = await CDP.attach(`http://127.0.0.1:${DEBUG_PORT}`);
        const { cookies: got } = await cdp.send('Network.getAllCookies');
        cdp.close();
        cookies = got || [];
        const names = new Set(cookies.map((c) => c.name));
        if (names.has('skey') || names.has('p_skey')) break;
      } catch {}
    }
    if (!cookies.length) throw new Error('没读到 cookie，可能登录超时或调试端口被占用');
    // 按域优先：orcaterm 域下的 skey/uin 才可信，其他腾讯域的同名 cookie 会串味
    const pick = (name) => {
      const hit = cookies.find((c) => c.name === name && /orcaterm/i.test(c.domain || ''))
        || cookies.find((c) => c.name === name);
      return hit?.value || '';
    };
    const raw = {
      uin: pick('uin') || pick('qcloud_uid') || pick('ewpUid'),
      skey: pick('skey') || pick('p_skey'),
      extras: {
        p_skey: pick('p_skey'),
        qcmainCSRFToken: pick('qcmainCSRFToken'),
        qcloud_uid: pick('qcloud_uid'),
        language: pick('language'),
      },
      allCookies: cookies,
    };
    const p = persist(raw);
    console.log('\n✅ 登录成功，凭据已写入 gateway config');
    console.log('   uin =', p.uin, ' csrf =', p.csrf);
    console.log('   现在可以在 WorkBuddy 里用 orcaterm:: 开头的模型了');
  } finally {
    cleanup();
    // 等浏览器退出后删 profile（下次重新扫码，避免用旧 cookie）
    await sleep(1500);
    try { rmSync(PROFILE, { recursive: true, force: true }); } catch {}
  }
}

async function main() {
  const o = args();
  if (o.status) return showStatus();
  if (o.paste) {
    const parsed = parseCookieString(o.paste);
    const p = persist(parsed);
    console.log('✅ 已写入：uin =', p.uin, ' csrf =', p.csrf);
    return;
  }
  await doLogin();
}

main().catch((e) => {
  console.error('❌', e.message);
  setStatus('error', { message: e.message, at: Date.now() });
  process.exit(1);
});

#!/usr/bin/env node
/**
 * 校验菜单栏面板的刷新行为，全程不需要手点：
 *
 *   1. 面板「被弹出」后，数据是否在持续刷新（签到概览 + AI 状态）
 *   2. 面板「被收起」后，是否停下来（看不见的时候不该继续探端口/读温度）
 *
 * 面板是常驻窗口，显隐不重新挂载，所以「刷新」全靠事件与轮询驱动 ——
 * 这类逻辑光看代码看不出来，必须实际跑一遍看调用次数有没有涨。
 *
 * 做法：用无头 Chrome 直接以 `file://` 打开预览页（预览桩会补发一次
 * popover-shown，等价于用户点了托盘图标），再通过 CDP 读预览桩统计的命令调用次数。
 * 前置只需先构建预览产物：
 *
 *   node node_modules/vite/bin/vite.js build --config vite.preview.config.ts
 *   node scripts/verify-popover-refresh.mjs
 *
 * 为什么走 `file://` 而不是起静态服务：运行环境的 shell 里设了 HTTP_PROXY，
 * Chrome 会把 127.0.0.1 的请求也交给代理，导致本地服务「拒绝连接」。
 * 预览产物用的是相对路径（vite 配置里 base: "./"），所以 file:// 能直接跑。
 */

import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const DIST = path.join(ROOT, "preview-dist");
const DEBUG_PORT = 9333;
const PROFILE = "/tmp/chrome-verify-popover";
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/** 面板开着时的观察窗口：要足够长，让 5s / 10s 两档轮询都至少各跑一轮。 */
const OBSERVE_MS = 12_000;
/** 收起后的观察窗口：超过 10s 档的核心轮询，只要还在请求就说明没停下。 */
const QUIET_MS = 13_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 极简 CDP 客户端：够用就好，不引第三方依赖。 */
async function connect() {
  let target;
  for (let i = 0; i < 40 && !target; i += 1) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json();
      target = list.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
    } catch {
      /* 调试端口还没起来 */
    }
    if (!target) await sleep(250);
  }
  if (!target) throw new Error("连不上 Chrome 调试端口，页面可能没起来");

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", reject, { once: true });
  });

  const pending = new Map();
  let id = 0;
  ws.addEventListener("message", (event) => {
    const msg = JSON.parse(event.data);
    const entry = pending.get(msg.id);
    if (!entry) return;
    pending.delete(msg.id);
    entry(msg);
  });

  const send = (method, params = {}) =>
    new Promise((resolve) => {
      id += 1;
      pending.set(id, resolve);
      ws.send(JSON.stringify({ id, method, params }));
    });

  /** 求值并取回 JSON 结果；页面抛错时返回 null，由调用方判定。 */
  const evaluate = async (expression) => {
    const res = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (res.result?.exceptionDetails) return null;
    return res.result?.result?.value ?? null;
  };

  return { evaluate, close: () => ws.close() };
}

/** 取一次快照：面板时间戳 + 各命令累计调用次数。 */
const SNAPSHOT = `JSON.stringify({
  stamp: document.querySelector(".pop-stamp")?.textContent ?? null,
  calls: window.__previewCalls ?? {},
  listeners: window.__previewEmit ? true : false,
})`;

function diff(before, after, key) {
  return (after.calls[key] ?? 0) - (before.calls[key] ?? 0);
}

async function main() {
  const results = [];
  const check = (name, ok, detail) => {
    results.push({ name, ok, detail });
    console.log(`${ok ? "  PASS" : "  FAIL"}  ${name}  ${detail}`);
  };

  spawn("pkill", ["-f", PROFILE]).on("exit", () => undefined);
  await sleep(300);

  const page = `${pathToFileURL(path.join(DIST, "index.html")).href}?pop=1&ai=on`;
  const chrome = spawn(
    CHROME,
    [
      "--headless=new",
      // 这三个参数缺一不可，都是被运行环境的沙箱逼出来的：
      //   --no-sandbox             无头进程拿不到内核沙箱权限，不加会直接启动失败
      //   --no-proxy-server        shell 里的 HTTP_PROXY 会把本机请求也带偏
      //   --allow-file-access-from-files  file:// 下加载同目录的 module 脚本需要它
      "--no-sandbox",
      "--no-proxy-server",
      "--allow-file-access-from-files",
      "--disable-gpu",
      "--no-first-run",
      "--no-default-browser-check",
      `--remote-debugging-port=${DEBUG_PORT}`,
      `--user-data-dir=${PROFILE}`,
      page,
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );

  try {
    const cdp = await connect();
    await sleep(1200); // 首屏 + 预览桩补发 popover-shown

    const opened = JSON.parse(await cdp.evaluate(SNAPSHOT));
    if (!opened?.listeners) throw new Error("预览桩没挂上，检查 mock-ipc.ts 是否被改动");
    console.log(`  初始：${opened.stamp}`);
    console.log(`  初始调用：${JSON.stringify(opened.calls)}`);

    await sleep(OBSERVE_MS);
    const later = JSON.parse(await cdp.evaluate(SNAPSHOT));
    console.log(`  ${OBSERVE_MS / 1000}s 后：${later.stamp}`);
    console.log(`  之后调用：${JSON.stringify(later.calls)}`);

    const core = diff(opened, later, "list_accounts");
    const ai = diff(opened, later, "local_ai_status");
    check("面板开着时签到概览在刷新", core > 0, `list_accounts +${core}`);
    check("面板开着时 AI 状态在轮询", ai >= 2, `local_ai_status +${ai}`);
    check("时间戳跟着更新", opened.stamp !== later.stamp, `${opened.stamp} → ${later.stamp}`);

    const hidden = await cdp.evaluate(`window.__previewEmit("popover-hidden")`);
    check("popover-hidden 能触发前端收尾", hidden === true, `__previewEmit → ${hidden}`);

    await sleep(QUIET_MS);
    const quiet = JSON.parse(await cdp.evaluate(SNAPSHOT));
    const quietCore = diff(later, quiet, "list_accounts");
    const quietAi = diff(later, quiet, "local_ai_status");
    check("收起后签到概览停止轮询", quietCore === 0, `list_accounts +${quietCore}`);
    check("收起后 AI 状态停止轮询", quietAi === 0, `local_ai_status +${quietAi}`);

    cdp.close();
  } finally {
    chrome.kill();
  }

  const failed = results.filter((r) => !r.ok);
  console.log(failed.length === 0 ? "\n全部通过" : `\n${failed.length} 项未通过`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(String(error));
  process.exit(1);
});

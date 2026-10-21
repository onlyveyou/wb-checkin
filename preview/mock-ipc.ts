// 浏览器预览用的 IPC 桩：把 Tauri 的 invoke 换成静态演示数据，
// 让同一份 App 组件能在普通浏览器里渲染出来。仅用于预览，不参与应用打包。

type Handler = (command: string, args?: Record<string, unknown>) => unknown;

const today = (() => {
  const now = new Date();
  const month = `${now.getMonth() + 1}`.padStart(2, "0");
  const day = `${now.getDate()}`.padStart(2, "0");
  return `${now.getFullYear()}-${month}-${day}`;
})();

const accounts = [
  {
    id: "uid:10001",
    uid: "10001",
    nickname: "主账号",
    email: "main@example.com",
    domain: "www.codebuddy.cn",
    hasRefreshToken: true,
    needsRelogin: false,
    needsReloginReason: null,
    expiresAt: Date.now() + 3_600_000,
    refreshExpiresAt: Date.now() + 86_400_000 * 20,
    lastCheckinDate: today,
    lastCheckinResult: "ok",
    lastCheckinMessage: "签到成功",
    lastCheckinAt: Date.now() - 600_000,
    createdAt: Date.now() - 86_400_000 * 30,
  },
  {
    id: "uid:10002",
    uid: "10002",
    nickname: "小号 A",
    email: "second@example.com",
    domain: "www.codebuddy.cn",
    hasRefreshToken: true,
    needsRelogin: false,
    needsReloginReason: null,
    expiresAt: Date.now() + 1_200_000,
    refreshExpiresAt: Date.now() + 86_400_000 * 18,
    lastCheckinDate: null,
    lastCheckinResult: null,
    lastCheckinMessage: null,
    lastCheckinAt: null,
    createdAt: Date.now() - 86_400_000 * 12,
  },
  {
    id: "uid:10003",
    uid: "10003",
    nickname: "小号 B",
    email: "third@example.com",
    domain: "www.workbuddy.cn",
    hasRefreshToken: false,
    needsRelogin: true,
    needsReloginReason: "刷新失败：refresh token 已过期，需重新登录",
    expiresAt: Date.now() - 60_000,
    refreshExpiresAt: null,
    lastCheckinDate: today,
    lastCheckinResult: "error",
    lastCheckinMessage: "凭证已过期，请重新登录",
    lastCheckinAt: Date.now() - 300_000,
    createdAt: Date.now() - 86_400_000 * 5,
  },
];

const logs = [
  { ts: Date.now() - 300_000, date: today, accountId: "uid:10001", account: "主账号", result: "ok", message: "签到成功" },
  { ts: Date.now() - 640_000, date: today, accountId: "uid:10002", account: "小号 A", result: "already", message: "今日已签到" },
  { ts: Date.now() - 900_000, date: today, accountId: "uid:10003", account: "小号 B", result: "error", message: "凭证已过期，请重新登录" },
  { ts: Date.now() - 3_600_000 * 9, date: today, accountId: "uid:10001", account: "主账号", result: "ok", message: "签到成功" },
  { ts: Date.now() - 86_400_000, date: today, accountId: "uid:10002", account: "小号 A", result: "ok", message: "签到成功" },
];

// 独立授权登录的演示状态机：前两次轮询报「等待中」，第三次返回成功，
// 便于在浏览器里走完整个交互流程。
let oauthPolls = 0;

// 积分演示数据：覆盖「正常」「余额较低」「凭证失效」三种展示形态。
const creditFixtures: Record<string, Record<string, unknown>> = {
  "uid:10001": {
    ok: true,
    total: 4600,
    remain: 627.55,
    used: 3972.45,
    unit: "credits",
    nearestExpiry: "2026-10-16 13:20:45",
    packages: [
      {
        code: "TCACA_code_007_nzdH5h4Nl0",
        name: "腾讯云代码助手 (IDE) - 赠送包",
        total: 4100,
        remain: 127.55,
        used: 3972.45,
        unit: "credits",
        expiresAt: "2026-10-16 13:20:45",
      },
      {
        code: "TCACA_code_008_cfWoLwvjU4",
        name: "CodeBuddy 个人版新人包",
        total: 500,
        remain: 500,
        used: 0,
        unit: "credits",
        expiresAt: "2026-10-16 13:20:45",
      },
    ],
  },
  "uid:10002": {
    ok: true,
    total: 5000,
    remain: 4820.5,
    used: 179.5,
    unit: "credits",
    nearestExpiry: "2026-09-30 23:59:59",
    packages: [
      {
        code: "TCACA_code_002_AkiJS3ZHF5",
        name: "腾讯云代码助手 (IDE) - 包月套餐",
        total: 5000,
        remain: 4820.5,
        used: 179.5,
        unit: "credits",
        expiresAt: "2026-09-30 23:59:59",
      },
    ],
  },
  "uid:10003": { ok: false, message: "401 Unauthorized：凭证已过期，需重新登录" },
};

function creditFor(id: string) {
  const account = accounts.find((item) => item.id === id);
  return {
    id,
    label: account?.nickname ?? id,
    ...(creditFixtures[id] ?? { ok: false, message: "无演示数据" }),
  };
}

// 本地 AI 监控的演示数据：按「已部署」的样子给，便于在浏览器里复核完整版式。
const localAiServices = [
  {
    key: "gateway",
    port: 8080,
    name: "按需网关 (统一入口)",
    desc: "空闲 300s 自动关停，请求时自动拉起（冷启动 9-29s）",
    tcpOnly: true,
    state: "ready",
    model: null,
    tasks: [],
  },
  {
    key: "text",
    port: 8081,
    name: "文本后端",
    desc: "分级/翻译主力",
    tcpOnly: false,
    state: "ready",
    model: "Qwen3.8-27B-UD-Q4_K_M.gguf",
    tasks: [
      {
        task: 1509,
        promptTokens: 1692,
        processed: 1692,
        cache: 1383,
        predicted: 305,
        service: "文本后端",
        port: 8081,
      },
    ],
  },
  {
    key: "vision",
    port: 18080,
    name: "视觉后端",
    desc: "验证码 / 画面理解",
    tcpOnly: false,
    state: "down",
    model: null,
    tasks: [],
  },
];

const localAiFiles = [
  { name: "qwen_8081.log", size: 25_095_150, mtime: Date.now() - 64_000 },
  { name: "gateway_8080.log", size: 32_903, mtime: Date.now() - 128_000 },
  { name: "uitars_18080.log", size: 3_970_558, mtime: Date.now() - 9_200_000 },
  { name: "qwen_launchd_stderr.log", size: 0, mtime: Date.now() - 9_200_000 },
];

const localAiLogLines = [
  "3.47.080.433 I slot launch_slot_: id  1 | task 1509 | processing task, is_child = 0",
  "3.59.556.746 I slot print_timing: id  1 | task 1509 | n_gen =    100, tg =  11.80 t/s",
  "4.05.612.932 I slot print_timing: id  1 | task 1509 | n_gen =    172, tg =  11.84 t/s",
  "4.11.661.421 I slot print_timing: id  1 | task 1509 | n_gen =    244, tg =  11.86 t/s",
  "4.16.778.047 I slot print_timing: id  1 | task 1509 | prompt eval time =    3803.89 ms /   440 tokens",
  "4.16.778.055 I slot print_timing: id  1 | task 1509 |        eval time =   25609.87 ms /   305 tokens",
  "4.16.778.058 I slot print_timing: id  1 | task 1509 |       total time =   29413.76 ms /   745 tokens",
  "4.16.778.059 I slot print_timing: id  1 | task 1509 |    graphs reused =       1685",
  "4.16.778.229 I slot      release: id  1 | task 1509 | stop processing: n_tokens = 1692, truncated = 0",
];

const localAiConfig = {
  enabled: false,
  log_dir: "",
  smctemp_path: "",
  services: [],
};

// 预览专用：带 ?ai=on 直接进「已启用」状态，方便用无头浏览器截取完整版式复核。
localAiConfig.enabled = new URLSearchParams(window.location.search).get("ai") === "on";

let ferryRunning = true;

// 预览专用：?ferry=stopped 时以"未启动"状态渲染，用于复核空态版式。
if (new URLSearchParams(window.location.search).get("ferry") === "stopped") {
  ferryRunning = false;
}

const ferryModels = [
  { id: "opencode/glm-5.3-flash-free", name: "GLM-5.3-Flash Free", context: 200000, toolcall: true, reasoning: true },
  { id: "opencode/deepseek-v4.1-flash-free", name: "DeepSeek V4.1 Flash Free", context: 200000, toolcall: true, reasoning: true },
  { id: "opencode/mimo-v2.6-flash-free", name: "MiMo-V2.6-Flash Free", context: 200000, toolcall: true, images: true },
  { id: "opencode/exo-free", name: "Exo Free", context: 1048576, toolcall: true, images: true },
  { id: "opencode/longcat-2.5-preview-free", name: "LongCat 2.5 Preview Free", context: 262144, toolcall: true },
  { id: "opencode/minimax-m3.1-flash-free", name: "MiniMax M3.1 Flash Free", context: 200000, toolcall: true },
  { id: "opencode/nemotron-3.5-lightning-free", name: "Nemotron 3.5 Lightning Free", context: 131072, toolcall: false },
  { id: "opencode/beam-free", name: "Beam Free", context: 1048576, toolcall: true },
  { id: "opencode/cat-jam-free", name: "Cat Jam Free", context: 200000, toolcall: true },
  { id: "opencode/space-bunny-free", name: "Space Bunny Free", context: 200000, toolcall: true },
  { id: "opencode/ling-3.1-flash-free", name: "Ling 3.1 Flash Free", context: 131072, toolcall: true },
  { id: "opencode/step-5-preview-free", name: "Step 5 Preview Free", context: 131072, toolcall: true },
];

const ferryResults: Record<string, unknown> = {
  "opencode/glm-5.3-flash-free": { model: "opencode/glm-5.3-flash-free", ok: true, category: "available", durationMs: 12474, source: "request" },
  "opencode/deepseek-v4.1-flash-free": { model: "opencode/deepseek-v4.1-flash-free", ok: true, category: "available", durationMs: 6699, source: "request" },
  "opencode/mimo-v2.6-flash-free": { model: "opencode/mimo-v2.6-flash-free", ok: true, category: "available", durationMs: 6683, source: "cache" },
  "opencode/exo-free": { model: "opencode/exo-free", ok: true, category: "available", durationMs: 10413, source: "probe" },
  "opencode/longcat-2.5-preview-free": { model: "opencode/longcat-2.5-preview-free", ok: true, category: "available", durationMs: 27454, source: "cache" },
  "opencode/minimax-m3.1-flash-free": { model: "opencode/minimax-m3.1-flash-free", ok: true, category: "available", durationMs: 3171, source: "probe" },
  "opencode/nemotron-3.5-lightning-free": { model: "opencode/nemotron-3.5-lightning-free", ok: true, chatOnly: true, category: "available", durationMs: 2923, source: "cache", error: "探测时只返回文本" },
  "opencode/beam-free": { model: "opencode/beam-free", ok: false, category: "access", status: 403, durationMs: 3332, source: "probe", error: "This model is not available in your country" },
  "opencode/cat-jam-free": { model: "opencode/cat-jam-free", ok: false, category: "access", status: 403, durationMs: 8339, source: "probe", error: "This model is not available in your country" },
  "opencode/space-bunny-free": { model: "opencode/space-bunny-free", ok: false, category: "error", status: 410, durationMs: 411, source: "probe", error: "Model space-bunny-free has been deprecated" },
  "opencode/ling-3.1-flash-free": { model: "opencode/ling-3.1-flash-free", ok: false, category: "timeout", status: 504, durationMs: 30000, source: "probe", error: "Model probe timed out" },
};

const ferryStatus = {
  running: true,
  phase: "ready",
  message: "运行中 · 12 个免费模型",
  endpoint: "http://127.0.0.1:41980/v1",
  port: 41980,
  dataDir: "~/.wb-checkin/ferry",
  nodePath: "/opt/homebrew/bin/node",
  sidecarDir: "/Applications/WB-Checkin.app/Contents/Resources/sidecar",
  opencodeVersion: "1.19.0",
  models: ferryModels,
  modelResults: ferryResults,
  availableModels: [
    "opencode/glm-5.3-flash-free",
    "opencode/deepseek-v4.1-flash-free",
    "opencode/mimo-v2.6-flash-free",
    "opencode/exo-free",
    "opencode/longcat-2.5-preview-free",
    "opencode/minimax-m3.1-flash-free",
    "opencode/nemotron-3.5-lightning-free",
  ],
  excludedModels: ["opencode/space-bunny-free", "opencode/cat-jam-free"],
  sync: { count: 7, changed: true, time: new Date().toISOString() },
  probe: { running: false, current: null, active: [], pending: [] },
  modelsFile: "~/.workbuddy/models.json",
  useSystemProxy: false,
  systemProxy: { configured: false, detail: "请先在 macOS 中启用 HTTPS 系统代理；暂不支持仅 SOCKS 或 PAC 配置" },
};

const ferryLogLines = [
  "2026-10-20T04:17:42.976Z OC Ferry ready at http://127.0.0.1:41980/v1; 12 free models; probe cache hit 5, probing 7",
  "2026-10-20T04:17:43.120Z 检测轮开始：并发 6，超时 30 秒",
  "2026-10-20T04:17:46.221Z opencode/beam-free 判定为 access（HTTP 403）：This model is not available in your country",
  "2026-10-20T04:17:47.003Z 运行时已恢复：OpenCode 服务退出（code=null）",
  "2026-10-20T04:18:01.884Z 检测轮结束：判定缓存命中 5 个，实际探测 7 个",
  "2026-10-20T04:18:02.011Z 导入 WorkBuddy 完成：7 条（只动带 oc-ferry-v1 标记的条目）",
  "2026-10-20T04:21:19.402Z 请求 opencode/glm-5.3-flash-free：首字 1.3s，总计 4.8s",
];

const routes: Record<string, Handler> = {
  list_accounts: () => accounts,
  get_config: () => ({
    auto_enabled: true,
    backup_hour: 12,
    backup_minute: 0,
    local_ai: localAiConfig,
  }),
  set_config: (_c, args) => ({
    auto_enabled: Boolean(args?.autoEnabled),
    backup_hour: Number(args?.backupHour ?? 12),
    backup_minute: Number(args?.backupMinute ?? 0),
    local_ai: localAiConfig,
  }),
  get_logs: (_c, args) => logs.slice(0, Number(args?.limit ?? 80)),
  store_paths: () => ({
    dataDir: "~/.wb-checkin",
    accounts: "~/.wb-checkin/accounts.json",
    logs: "~/.wb-checkin/checkin-logs.json",
    config: "~/.wb-checkin/config.json",
  }),
  checkin_all: () => ({
    total: accounts.length,
    succeeded: 2,
    failed: 1,
    items: accounts.map((account) => ({
      id: account.id,
      label: account.nickname,
      ok: !account.needsRelogin,
      result: account.needsRelogin ? "error" : "ok",
      message: account.needsRelogin ? "凭证已过期，请重新登录" : "签到成功",
    })),
    finishedAt: Date.now(),
  }),
  checkin_one: () => ({ id: "uid:10002", label: "小号 A", ok: true, result: "ok", message: "签到成功" }),
  delete_account: () => null,
  add_account: () => "uid:new",
  import_local_accounts: () => ({
    ok: true,
    source: "~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info",
    imported: 2,
    merged: 1,
  }),
  oauth_login_start: () => ({
    loginId: "preview-login",
    authUrl: "https://www.codebuddy.cn/login?platform=workbuddy&state=preview-state-0000",
    expiresIn: 600,
  }),
  oauth_login_open: () => null,
  oauth_login_poll: () => {
    oauthPolls += 1;
    if (oauthPolls < 3) {
      return { done: false, status: "pending", notice: null, expiresIn: 600 - oauthPolls * 2 };
    }
    return { done: true, status: "ok", account: accounts[1], notice: null, expiresIn: 594 };
  },
  oauth_login_cancel: () => true,
  fetch_credits_all: () => ({
    total: accounts.length,
    items: accounts.map((account) => creditFor(account.id)),
    finishedAt: Date.now(),
  }),
  fetch_credits_one: (_command, args) => creditFor(String(args?.id ?? "")),
  switch_preflight: () => ({
    authFilePath:
      "~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info",
    authFileExists: true,
    authFileWritable: true,
    clientAppPath: "/Applications/WorkBuddy.app",
    clientInstalled: true,
    clientRunning: true,
    backupDir: "~/.wb-checkin/backups",
  }),
  switch_to_account: () => ({
    ok: true,
    account: "小号 A",
    authFilePath:
      "~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info",
    backup: "~/.wb-checkin/backups/workbuddy-desktop.20260916-133000.info",
    refreshed: true,
    refreshError: null,
    restarted: true,
    launchError: null,
  }),
  local_ai_status: () => ({
    enabled: localAiConfig.enabled,
    time: Date.now(),
    temps: { cpu: 56.1, gpu: 43.7, smctemp: "~/llama.cpp/llm-serve/smctemp" },
    // 32 GiB 装机内存、占用 38.7%，三个数与真机口径一致（usedPercent 由 used/total 得出）
    memory: localAiConfig.enabled
      ? { total: 34359738368, used: 13297218748, usedPercent: 38.7 }
      : null,
    services: localAiConfig.enabled ? localAiServices : [],
    tasks: localAiConfig.enabled
      ? localAiServices.flatMap((service) =>
          service.tasks.map((task) => ({
            ...task,
            service: service.name,
            port: service.port,
          })),
        )
      : [],
    logs: {
      dir: "~/llama.cpp/logs",
      dirExists: localAiConfig.enabled,
      files: localAiConfig.enabled ? localAiFiles : [],
    },
    notes: [],
  }),
  local_ai_detect: () => ({
    deployed: true,
    ports: [
      { key: "gateway", port: 8080, name: "按需网关 (统一入口)", tcpOnly: true, listening: true },
      { key: "text", port: 8081, name: "文本后端", tcpOnly: false, listening: true },
      { key: "vision", port: 18080, name: "视觉后端", tcpOnly: false, listening: false },
    ],
    logDir: "~/llama.cpp/logs",
    logDirExists: true,
    logFiles: localAiFiles.length,
    smctemp: "~/llama.cpp/llm-serve/smctemp",
  }),
  local_ai_tail: (_command, args) => {
    const name = String(args?.file ?? "");
    return {
      file: name,
      size: localAiFiles.find((file) => file.name === name)?.size ?? 0,
      lines: localAiLogLines,
    };
  },
  set_local_ai_config: (_command, args) => {
    localAiConfig.enabled = Boolean(args?.enabled);
    localAiConfig.log_dir = String(args?.logDir ?? "");
    localAiConfig.smctemp_path = String(args?.smctempPath ?? "");
    return {
      auto_enabled: true,
      backup_hour: 12,
      backup_minute: 0,
      local_ai: localAiConfig,
    };
  },
  // 模型摆渡：数据取自真机的一轮检测结果（12 个模型，含地区限制与超时），
  // 这样预览里看到的状态分布和实际一致。
  ferry_status: () => ({
    ...ferryStatus,
    running: ferryRunning,
    phase: ferryRunning ? "ready" : "stopped",
    message: ferryRunning ? "运行中 · 12 个免费模型" : "未在运行",
  }),
  ferry_start: () => {
    ferryRunning = true;
    return { ...ferryStatus, running: true, phase: "ready" };
  },
  ferry_stop: () => {
    ferryRunning = false;
    return { running: false, phase: "stopped", message: "已停止" };
  },
  ferry_probe: () => ({ started: true, probing: 12, reused: 0 }),
  ferry_refresh: () => ({ count: ferryStatus.models.length }),
  ferry_import: () => ({ count: 7, changed: true }),
  ferry_selection: (_command, args) => ({ excludedModels: [String(args?.model ?? "")] }),
  ferry_set_proxy: (_command, args) => ({ useSystemProxy: Boolean(args?.enabled) }),
  ferry_logs: () => ({ file: "~/.wb-checkin/ferry/ferry.log", lines: ferryLogLines }),
  // 积分趋势：预览里给空集，面板会显示"还没有用量数据"而不是一条报错横幅。
  usage_cached: () => ({ days: [], accounts: [] }),
  fetch_usage_all: () => ({ days: [], accounts: [] }),
  // 面板上的窗口动作在真实环境里由后端执行，预览里什么都不做。
  popover_hide: () => null,
  popover_open_main: () => null,
  popover_resize: () => null,
  quit_app: () => null,
};

let callbackId = 0;

/**
 * 预览期的事件通道。
 *
 * 真实环境里事件由 Rust 侧发出，浏览器里没有这条通道，于是面板的「被弹出 → 刷新」
 * 逻辑在预览里根本走不到。这里把监听器存下来，由 `window.__previewEmit()` 手动触发
 * （`preview/main.tsx` 在 `?pop=1` 时自动发一次 popover-shown，等价于用户点了图标）。
 * 只用于预览与无头校验，不参与应用打包。
 */
const eventListeners = new Map<string, number>();

/** 各命令被调用的次数，暴露给校验脚本，用来确认「面板开着时确实在轮询」。 */
const callCounts: Record<string, number> = {};

const internals = {
  transformCallback: (callback: unknown) => {
    callbackId += 1;
    (window as unknown as Record<string, unknown>)[`_${callbackId}`] = callback;
    return callbackId;
  },
  invoke: async (command: string, args?: Record<string, unknown>) => {
    callCounts[command] = (callCounts[command] ?? 0) + 1;
    if (command.startsWith("plugin:event|")) {
      const event = String(args?.event ?? "");
      const handler = args?.handler;
      if (command.endsWith("listen") && typeof handler === "number") {
        eventListeners.set(event, handler);
      }
      return callbackId + 1;
    }
    const handler = routes[command];
    if (!handler) {
      throw new Error(`预览模式未实现命令：${command}`);
    }
    return handler(command, args);
  },
};

declare global {
  interface Window {
    __previewEmit?: (event: string, payload?: unknown) => boolean;
    __previewCalls?: Record<string, number>;
  }
}

window.__previewEmit = (event: string, payload: unknown = null) => {
  const id = eventListeners.get(event);
  const fn = id != null ? (window as unknown as Record<string, unknown>)[`_${id}`] : undefined;
  // 补发也要计数：面板「没刷新」时，先分清是事件压根没发出去、
  // 还是发出去了但前端没接住 —— 这两件事的排查方向完全不同。
  callCounts[`__emit:${event}:${typeof fn === "function" ? "ok" : "miss"}`] =
    (callCounts[`__emit:${event}:${typeof fn === "function" ? "ok" : "miss"}`] ?? 0) + 1;
  if (typeof fn !== "function") return false;
  (fn as (e: unknown) => void)({ event, id, payload });
  return true;
};

window.__previewCalls = callCounts;

(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = internals;
(window as unknown as Record<string, unknown>).__TAURI__ = {};

export {};

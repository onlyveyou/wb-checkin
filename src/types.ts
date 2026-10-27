export interface Account {
  id: string;
  uid: string | null;
  nickname: string | null;
  email: string | null;
  domain: string;
  hasRefreshToken: boolean;
  /** token 链的持有方：`checkin` = 签到侧独立授权，写进客户端会消耗掉签到链。 */
  tokenOwner: "checkin" | "client";
  needsRelogin: boolean;
  needsReloginReason: string | null;
  expiresAt: number | null;
  refreshExpiresAt: number | null;
  lastCheckinDate: string | null;
  lastCheckinResult: "ok" | "already" | "error" | null;
  lastCheckinMessage: string | null;
  lastCheckinAt: number | null;
  createdAt: number | null;
}

/** 本地 AI 监控：被监控的服务定义（与后端配置一一对应）。 */
export interface LocalAiServiceConfig {
  key: string;
  port: number;
  name: string;
  desc: string;
  tcp_only: boolean;
}

/** 本地 AI 监控的开关与路径。路径留空表示使用约定位置。 */
export interface LocalAiConfig {
  enabled: boolean;
  log_dir: string;
  smctemp_path: string;
  services: LocalAiServiceConfig[];
}

export interface AppConfig {
  auto_enabled: boolean;
  backup_hour: number;
  backup_minute: number;
  /** 自动签到之后顺手处理成长中心（旅行 + 任务领奖，不含盲盒）。 */
  auto_growth: boolean;
  local_ai: LocalAiConfig;
  /** 菜单栏要显示的指标：net / disk / cpu / mem，空数组表示只留图标。 */
  menubar_items: string[];
  /** 界面自动化新建会话前先切到的模型，空字符串表示不切。 */
  free_model: string;
  /** 官方客户端登录态文件路径。留空 = 自动探测。 */
  client_auth_file: string;
  /** 客户端程序路径（macOS 是 .app，Windows 是 .exe）。留空 = 自动探测。 */
  client_app_path: string;
}

/** 系统指标。网速与 CPU 需要两次采样求差，启动后约一秒内为 null。 */
export interface SysMetrics {
  diskPercent: number | null;
  diskTotal: number | null;
  diskUsed: number | null;
  netDown: number | null;
  netUp: number | null;
  cpuPercent: number | null;
  memPercent: number | null;
  items: string[];
}

export interface CheckinItem {
  id: string;
  label: string;
  ok: boolean;
  result: "ok" | "already" | "error";
  message: string;
}

export interface CheckinSummary {
  total: number;
  succeeded: number;
  failed: number;
  items: CheckinItem[];
  finishedAt: number;
  skipped?: boolean;
}

export interface LogRecord {
  ts: number;
  date: string;
  accountId: string;
  account: string;
  /** growth 是自动成长中心（旅行/任务）的独立记录，不参与签到成功数统计。 */
  result: "ok" | "already" | "error" | "growth";
  message: string;
}

export interface StorePaths {
  dataDir: string;
  accounts: string;
  logs: string;
  config: string;
}

/** 独立授权登录：一次会话的开始。 */
export interface OAuthStart {
  loginId: string;
  authUrl: string;
  expiresIn: number;
}

/** 独立授权登录：轮询一次的结果。 */
export interface OAuthPoll {
  done: boolean;
  status: "pending" | "ok" | "error";
  account?: Account | null;
  error?: string | null;
  notice?: string | null;
  expiresIn: number;
}

/** 单个积分包的余额。`expiresAt` 是周期结束时间，形如 `2026-10-16 13:20:45`。 */
export interface CreditPackage {
  code: string;
  name: string;
  total: number;
  remain: number;
  used: number;
  unit: string;
  expiresAt: string;
}

/** 一个账号的积分查询结果。失败时只有 `ok` 与 `message`。 */
export interface CreditItem {
  id: string;
  label: string;
  ok: boolean;
  message?: string;
  total?: number;
  remain?: number;
  used?: number;
  unit?: string;
  nearestExpiry?: string | null;
  packages?: CreditPackage[];
}

export interface CreditSummary {
  total: number;
  items: CreditItem[];
  finishedAt: number;
}

/** 账号切换前的环境检查结果。 */
/** 客户端路径自动探测的结果（设置页「自动探测」按钮用）。 */
export interface ClientPathProbe {
  /** 扫描起点：macOS 是 Application Support，Windows 是 %APPDATA%。 */
  base: string;
  /** 扫到的、看起来像客户端数据目录的路径。 */
  dataRoots: string[];
  /** 扫到的登录态文件（主文件在前，备份按时间倒序在后）。 */
  authFiles: string[];
  /** 当前生效的登录态路径。 */
  effective: string;
  effectiveExists: boolean;
}

export interface SwitchPreflight {
  authFilePath: string;
  authFileExists: boolean;
  authFileWritable: boolean;
  clientAppPath: string | null;
  clientInstalled: boolean;
  clientRunning: boolean;
  backupDir: string;
}

/** 账号切换的执行结果。 */
export interface SwitchResult {
  ok: boolean;
  account: string;
  authFilePath: string;
  backup: string | null;
  refreshed: boolean;
  refreshError: string | null;
  restarted: boolean;
  launchError: string | null;
  /** 是否走了分层快照（保留会话）。 */
  keptSession: boolean;
  /** 是否只换了凭证（登录身份未变）。 */
  creditsOnly: boolean;
  /** 切换前为原账号存的现场；未存档时为 null。 */
  savedProfile: ProfileSnapshot | null;
  saveError: string | null;
  /** 目标账号的恢复结果。`restored` 为 false 时 `reason` 说明原因。 */
  profileRestore: {
    restored: boolean;
    layers?: string[];
    savedAt?: number | null;
    reason?: string;
  } | null;
}

/** 谁在登录 / 谁在付费。用「只换额度」切过之后两者会不同。 */
export interface CurrentIdentity {
  loginUid: string | null;
  /** accessToken 的 JWT sub —— 真正扣积分的账号。 */
  billingUid: string | null;
  billingName: string | null;
}

/** 一份账号现场快照。 */
export interface ProfileSnapshot {
  uid: string;
  label: string | null;
  savedAt: number | null;
  sessionBytes: number;
  layers: string[];
}

export interface ProfileList {
  root: string;
  items: ProfileSnapshot[];
}

/** 保存现场的结果：比快照元信息多一个「是否顺手重启过客户端」。 */
export interface ProfileSaveResult extends ProfileSnapshot {
  restarted: boolean;
}

/** 成长中心单个动作的结果。 */
export interface GrowthAction {
  state: "ok" | "skip" | "fail" | "auth";
  message: string;
}

export interface GrowthItem {
  id: string;
  label: string;
  ok: boolean;
  message: string;
  travel: GrowthAction | null;
  lottery: GrowthAction | null;
  tasks: GrowthAction | null;
}

export interface GrowthSummary {
  total: number;
  succeeded: number;
  failed: number;
  items: GrowthItem[];
  finishedAt: number;
}

/** 一条待体验任务：进度没达标、也还没领奖。 */
export interface PendingTask {
  code: string;
  title: string;
  description: string;
  jumpUrl: string;
  reward: number;
  current: number;
  target: number;
}

export interface PendingAccount {
  id: string | null;
  label: string;
  ok: boolean;
  message: string | null;
  tasks: PendingTask[];
}

/** 一天的积分消耗。 */
export interface UsageDay {
  date: string;
  credit: number;
}

export interface UsageAccount {
  id: string;
  label: string;
  ok: boolean;
  message?: string | null;
  today: number;
  week: number;
  month: number;
  requests: number;
  days: UsageDay[];
}

export interface UsageSummary {
  accounts: UsageAccount[];
  /** 所有账号按日汇总后的序列。 */
  days: UsageDay[];
  finishedAt?: number;
  cached?: boolean;
}

// ---------------------------------------------------------------------------
// 本地 AI 监控
// ---------------------------------------------------------------------------

/**
 * 服务状态三态。
 * - `ready` 运行中
 * - `cold` 网关空闲（按需启停，属正常状态，不是故障）
 * - `down` 端口在听但接口不响应，或端口未监听
 */
export type LocalAiState = "ready" | "cold" | "down";

/** 正在处理的一个推理任务（来自 llama.cpp 的 /slots）。 */
export interface LocalAiTask {
  task: number | null;
  promptTokens: number | null;
  processed: number | null;
  cache: number | null;
  predicted: number | null;
  /** 后端展平时补上，标明来自哪个服务。 */
  service?: string;
  port?: number;
}

export interface LocalAiServiceStatus {
  key: string;
  port: number;
  name: string;
  desc: string;
  /** 真时只做了 TCP 探测，`model` 与 `tasks` 必定为空。 */
  tcpOnly: boolean;
  state: LocalAiState;
  model: string | null;
  tasks: LocalAiTask[];
}

/** CPU / GPU 温度。读不到时为 null。 */
export interface LocalAiTemps {
  cpu: number | null;
  gpu: number | null;
  /** 实际使用的 smctemp 路径；未找到时为 null。 */
  smctemp: string | null;
}

/** 内存占用。三个字段同生同灭：读不到时都为 null。 */
export interface LocalAiMemory {
  /** 装机内存总字节数。 */
  total: number | null;
  /** 已用字节数，口径同「活动监视器」的 Memory Used。 */
  used: number | null;
  /** 已用占比，0–100，保留一位小数。 */
  usedPercent: number | null;
}

export interface LocalAiLogFile {
  name: string;
  size: number;
  /** 毫秒时间戳。 */
  mtime: number;
}

/** 一次探活的完整结果。 */
export interface LocalAiStatus {
  enabled: boolean;
  time: number;
  temps?: LocalAiTemps;
  memory?: LocalAiMemory | null;
  services: LocalAiServiceStatus[];
  tasks: LocalAiTask[];
  logs: {
    dir: string;
    dirExists: boolean;
    files: LocalAiLogFile[];
  };
  /** 「为什么看起来不对」的说明，例如日志目录不存在、未找到 smctemp。 */
  notes: string[];
}

/** 本机部署情况探测结果。只读，不改配置。 */
export interface LocalAiDetect {
  deployed: boolean;
  ports: { key: string; port: number; name: string; tcpOnly: boolean; listening: boolean }[];
  logDir: string;
  logDirExists: boolean;
  logFiles: number;
  smctemp: string | null;
}

/** 日志尾部内容。失败时只有 `error`。 */
export interface LocalAiLogTail {
  file?: string;
  size?: number;
  lines?: string[];
  error?: string;
}

// ---------------------------------------------------------------------------
// 模型摆渡（OC Ferry）
//
// 状态由 sidecar 直接产出，字段名沿用它的口径（phase / modelResults / sync）。
// 这里只声明界面真正读到的部分：多写一个字段就要多维护一处，读不到的按缺省处理。
// ---------------------------------------------------------------------------

export interface FerryModel {
  id: string;
  name: string;
  context?: number;
  output?: number;
  images?: boolean;
  toolcall?: boolean;
  reasoning?: boolean;
}

/** 一个模型的最近一次判定。source 是 `probe`（检测）或 `cache`（缓存）或 `request`（真实调用）。 */
export interface FerryVerdict {
  model: string;
  ok: boolean;
  category?: string;
  time?: string;
  source?: string;
  chatOnly?: boolean;
  durationMs?: number;
  error?: string;
  status?: number;
  code?: string;
}

export interface FerryStatus {
  running: boolean;
  phase: string;
  message?: string;
  endpoint?: string;
  port?: number;
  dataDir?: string;
  nodePath?: string;
  sidecarDir?: string;
  opencodeVersion?: string | null;
  models?: FerryModel[];
  availableModels?: string[];
  modelResults?: Record<string, FerryVerdict>;
  excludedModels?: string[];
  sync?: { count?: number; changed?: boolean; error?: string; time?: string; skipped?: boolean };
  probe?: { running: boolean; current?: string | null; active?: string[]; pending?: string[] };
  modelsFile?: string;
  useSystemProxy?: boolean;
  /** 「系统代理」开关能跟随的出口。configured=false 时开关没有意义，detail 是原因。 */
  systemProxy?: { configured: boolean; address?: string; detail?: string };
  updatedAt?: string;
}

export interface FerryLogTail {
  file?: string;
  lines: string[];
}

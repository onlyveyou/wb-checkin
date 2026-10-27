import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  AppConfig,
  CheckinItem,
  CheckinSummary,
  CreditItem,
  CurrentIdentity,
  CreditSummary,
  GrowthItem,
  GrowthSummary,
  LocalAiDetect,
  LocalAiLogTail,
  LocalAiStatus,
  LogRecord,
  FerryLogTail,
  FerryStatus,
  OAuthPoll,
  OAuthStart,
  PendingAccount,
  ProfileList,
  ProfileSaveResult,
  StorePaths,
  SwitchPreflight,
  SwitchResult,
  SysMetrics,
  UsageSummary,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");

export const getConfig = () => invoke<AppConfig>("get_config");

export const setConfig = (
  autoEnabled: boolean,
  backupHour: number,
  backupMinute: number,
  autoGrowth?: boolean,
  freeModel?: string,
  // 客户端路径：留空字符串 = 恢复自动探测。不传则保持原值不动。
  clientAuthFile?: string,
  clientAppPath?: string,
) =>
  invoke<AppConfig>("set_config", {
    autoEnabled,
    backupHour,
    backupMinute,
    autoGrowth,
    freeModel,
    clientAuthFile,
    clientAppPath,
  });

export const getLogs = (limit = 80) => invoke<LogRecord[]>("get_logs", { limit });

/** 最近一次系统采样。由后端每秒刷新，前端轮询取用。 */
export const sysMetrics = () => invoke<SysMetrics>("sys_metrics");

/** 改菜单栏显示项，立即生效并写回配置。 */
export const setMenubarItems = (items: string[]) =>
  invoke<AppConfig>("set_menubar_items", { items });

export const getStorePaths = () => invoke<StorePaths>("store_paths");

export const checkinAll = () => invoke<CheckinSummary>("checkin_all");

export const checkinOne = (id: string) => invoke<CheckinItem>("checkin_one", { id });

export const deleteAccount = (id: string) => invoke<void>("delete_account", { id });

export const importLocalAccounts = () =>
  invoke<{
    ok: boolean;
    error?: string;
    source?: string;
    imported?: number;
    merged?: number;
  }>("import_local_accounts");

export const addAccount = (payload: {
  uid?: string;
  nickname?: string;
  accessToken: string;
  refreshToken?: string;
  domain?: string;
}) =>
  invoke<string>("add_account", {
    uid: payload.uid || null,
    nickname: payload.nickname || null,
    accessToken: payload.accessToken,
    refreshToken: payload.refreshToken || null,
    domain: payload.domain || null,
  });

// 独立授权登录：每个账号各走一次官方授权，不受客户端登录位限制。

export const oauthLoginStart = () => invoke<OAuthStart>("oauth_login_start");

export const oauthLoginOpen = (loginId: string) => invoke<void>("oauth_login_open", { loginId });

export const oauthLoginPoll = (loginId: string) => invoke<OAuthPoll>("oauth_login_poll", { loginId });

export const oauthLoginCancel = (loginId: string) => invoke<boolean>("oauth_login_cancel", { loginId });

// 积分查询：只读，不消耗积分、不影响签到。

export const fetchCreditsAll = () => invoke<CreditSummary>("fetch_credits_all");

export const fetchCreditsOne = (id: string) => invoke<CreditItem>("fetch_credits_one", { id });

// 账号切换：会覆写官方客户端登录态，restart 为真时还会重启客户端。

export const switchPreflight = () => invoke<SwitchPreflight>("switch_preflight");

/**
 * 切换账号。
 *
 * `creditsOnly` 为真时只把登录态文件里的凭证换成目标账号的，登录身份与本地现场都不动
 * —— 客户端仍显示原来那个账号，但模型调用按目标账号的额度结算。
 *
 * `force` 为真时允许把签到侧独立持有的账号写进客户端。默认拒绝：写进去之后客户端刷新
 * 一次就会把 refresh token 轮换掉，该账号的自动签到随即失效。
 */
export const switchToAccount = (
  id: string,
  restart: boolean,
  keepSession: boolean,
  creditsOnly = false,
  force = false,
) => invoke<SwitchResult>("switch_to_account", { id, restart, keepSession, creditsOnly, force });

// 分层快照：一个账号一套现场，切换时成套换回去，会话才不会断。

export const profileList = () => invoke<ProfileList>("profile_list");

/** 保存"当前登录账号"的现场。客户端在跑时会先退出、存完再自动拉起。 */
export const profileSave = (autoClose = true) =>
  invoke<ProfileSaveResult>("profile_save", { autoClose });

export const profileDelete = (uid: string) => invoke<void>("profile_delete", { uid });

/** 谁在登录 / 谁在付费。用「只换额度」切过之后两者不同。 */
export const currentIdentity = () => invoke<CurrentIdentity>("current_identity");

// 成长中心：旅行 / 盲盒 / 任务领奖。三个动作各自独立容错。

export const growthRunOne = (id: string) => invoke<GrowthItem>("growth_run_one", { id });

export const growthRunAll = () => invoke<GrowthSummary>("growth_run_all");

/** 待体验任务（进度没达标的）。顺带把没接取的任务接取掉。 */
export const growthPending = () =>
  invoke<{ accounts: PendingAccount[] }>("growth_pending");

/** 用系统默认方式打开 deeplink / 站内地址。 */
export const openLink = (url: string) => invoke<void>("open_link", { url });

// 积分趋势：官方逐请求明细，本地按日汇总。

export const usageCached = () => invoke<UsageSummary>("usage_cached");

export const fetchUsageAll = () => invoke<UsageSummary>("fetch_usage_all");

// 本地 AI 监控：只读本机推理后端与日志，不改任何东西。功能默认关闭。

export const localAiStatus = () => invoke<LocalAiStatus>("local_ai_status");

export const localAiDetect = () => invoke<LocalAiDetect>("local_ai_detect");

export const localAiTail = (file: string, lines = 150) =>
  invoke<LocalAiLogTail>("local_ai_tail", { file, lines });

export const setLocalAiConfig = (enabled: boolean, logDir: string, smctempPath: string) =>
  invoke<AppConfig>("set_local_ai_config", { enabled, logDir, smctempPath });

// 菜单栏面板：窗口显隐一律走后端，前端不直接持有窗口句柄（也就不需要窗口权限）。

export const popoverHide = () => invoke<void>("popover_hide");

export const popoverOpenMain = () => invoke<void>("popover_open_main");

/** 面板高度贴合内容：面板是固定宽度的独立窗口，高度得由前端量出来回传。 */
export const popoverResize = (height: number) => invoke<void>("popover_resize", { height });

export const quitApp = () => invoke<void>("quit_app");

// 模型摆渡：把 OpenCode 的免费模型摆渡给 WorkBuddy。
// 这是全应用唯一会开本地端口的模块，所以启停与所有管理动作都留在后端。

export const ferryStatus = () => invoke<FerryStatus>("ferry_status");

export const ferryStart = () => invoke<FerryStatus>("ferry_start");

export const ferryStop = () => invoke<FerryStatus>("ferry_stop");

export const ferryProbe = (model?: string) =>
  invoke<{ started?: boolean; probing?: number; reused?: number; message?: string }>("ferry_probe", {
    model: model ?? null,
  });

export const ferryRefresh = () => invoke<{ count?: number }>("ferry_refresh");

export const ferryImport = () => invoke<{ count?: number; changed?: boolean; error?: string }>("ferry_import");

export const ferrySelection = (model: string, selected: boolean) =>
  invoke<{ excludedModels: string[] }>("ferry_selection", { model, selected });

export const ferrySetProxy = (enabled: boolean) =>
  invoke<{ useSystemProxy: boolean }>("ferry_set_proxy", { enabled });

export const ferryLogs = (lines = 120) => invoke<FerryLogTail>("ferry_logs", { lines });

// 辅助功能驱动：界面自动化只用于「必须发生在客户端里」的动作。
// 授权归用户在系统设置里给，这里的命令只回报状态、触发一次授权提示、dump 界面树。

/** 辅助功能的状态：`windows > 0` 才是真读得到界面，`trusted` 只表示系统设置里勾没勾。 */
export type AxStatus = {
  trusted: boolean;
  clientPid: number | null;
  windows: number;
  /** 平台标识：Windows 没有「辅助功能」授权这一步，界面据此隐藏授权入口。 */
  platform?: string;
};

export const axStatus = () => invoke<AxStatus>("ax_status");

export const axProbe = () =>
  invoke<{ path: string; lines: number; windows: number; head: string }>("ax_probe");

export const axRequestTrust = () => invoke<AxStatus>("ax_request_trust");

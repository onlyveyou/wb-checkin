import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "./api";
import type { AxStatus } from "./api";
import type {
  Account,
  AppConfig,
  CheckinSummary,
  CreditItem,
  CurrentIdentity,
  LogRecord,
  PendingAccount,
  StorePaths,
  SwitchPreflight,
  SysMetrics,
  UsageSummary,
} from "./types";
import AccountRow from "./components/AccountRow";
import AddAccountDialog from "./components/AddAccountDialog";
import FerryPanel from "./components/FerryPanel";
import LocalAiPanel from "./components/LocalAiPanel";
import SwitchDialog from "./components/SwitchDialog";

function dateKey(date: Date): string {
  const month = `${date.getMonth() + 1}`.padStart(2, "0");
  const day = `${date.getDate()}`.padStart(2, "0");
  return `${date.getFullYear()}-${month}-${day}`;
}

function todayLocal(): string {
  return dateKey(new Date());
}

/// 字节数按 1024 进制换算，硬盘容量与网速共用一套口径。
function formatBytes(value: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let size = value;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size.toFixed(unit <= 1 ? 0 : 1)} ${units[unit]}`;
}

/// 日志时间戳：当天只给时刻，跨天补上月日，避免昨天的记录看起来像今天的。
function formatStamp(ts: number, today: string): string {
  const date = new Date(ts);
  const hour = `${date.getHours()}`.padStart(2, "0");
  const minute = `${date.getMinutes()}`.padStart(2, "0");
  if (dateKey(date) === today) {
    return `${hour}:${minute}`;
  }
  const month = `${date.getMonth() + 1}`.padStart(2, "0");
  const day = `${date.getDate()}`.padStart(2, "0");
  return `${month}-${day} ${hour}:${minute}`;
}

/** `input[type=time]` 需要的 `HH:MM`。 */
function toTimeValue(hour: number, minute: number): string {
  return `${`${hour}`.padStart(2, "0")}:${`${minute}`.padStart(2, "0")}`;
}

/** 解析 `HH:MM`；非法输入返回 null，由调用方提示，不静默落库。 */
function parseTimeValue(value: string): { hour: number; minute: number } | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!match) {
    return null;
  }
  const hour = Number(match[1]);
  const minute = Number(match[2]);
  return hour <= 23 && minute <= 59 ? { hour, minute } : null;
}

const INITIAL_CONFIG: AppConfig = {
  auto_enabled: true,
  backup_hour: 12,
  backup_minute: 0,
  auto_growth: true,
  local_ai: { enabled: false, log_dir: "", smctemp_path: "", services: [] },
  menubar_items: ["net", "disk"],
  free_model: "hy3",
  client_auth_file: "",
  client_app_path: "",
};

/** 菜单栏可勾选的指标。顺序与后端 `sysmon::ITEMS` 一致。 */
const MENUBAR_OPTIONS: { key: string; label: string; hint: string }[] = [
  { key: "net", label: "网速", hint: "独占一个图标，上下两行显示 ↓下行 ↑上行" },
  { key: "disk", label: "硬盘", hint: "根分区已用百分比，写作 HD 72%" },
  { key: "cpu", label: "CPU", hint: "整机占用率，写作 CPU 35%" },
  { key: "mem", label: "内存", hint: "已用物理内存比例，写作 MEM 61%" },
];

type Tab = "checkin" | "ai" | "ferry";

export default function App() {
  const [tab, setTab] = useState<Tab>("checkin");
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [config, setConfigState] = useState<AppConfig>(INITIAL_CONFIG);
  /** 后端当前认定的客户端路径（自动探测结果，只读展示）。 */
  const [clientPaths, setClientPaths] = useState<SwitchPreflight | null>(null);
  /** 手填草稿：不直接写 config，避免每敲一个字就存一次盘。 */
  const [clientPathDraft, setClientPathDraft] = useState({ authFile: "", app: "" });
  const [logs, setLogs] = useState<LogRecord[]>([]);
  const [paths, setPaths] = useState<StorePaths | null>(null);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState<{ index: number; total: number } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [banner, setBanner] = useState<{ kind: "ok" | "danger"; text: string } | null>(null);

  // 积分：按账号 id 缓存查询结果；未查询过的账号没有条目
  const [credits, setCredits] = useState<Record<string, CreditItem>>({});
  const [creditsBusyId, setCreditsBusyId] = useState<string | null>(null);
  const [creditsLoadingAll, setCreditsLoadingAll] = useState(false);
  const [switchTarget, setSwitchTarget] = useState<Account | null>(null);

  // 成长中心（旅行/盲盒/任务领奖）与积分趋势
  const [growthRunning, setGrowthRunning] = useState(false);
  /**
   * 待体验任务：进度没达标的体验类任务。这类任务要靠客户端里的真实行为推进，
   * 接口代不了，所以这里只做「列出来 + 一键跳过去」。
   */
  const [pending, setPending] = useState<PendingAccount[]>([]);
  const [pendingLoading, setPendingLoading] = useState(false);
  /** 防并发：窗口聚焦与签到回调可能在拉取途中再次触发。 */
  const pendingInflight = useRef(false);
  /** 待体验面板当前看的是哪个账号。null = 用清单里第一个有任务的账号。 */
  const [pendingTab, setPendingTab] = useState<string | null>(null);
  const [usage, setUsage] = useState<UsageSummary | null>(null);
  const [usageLoading, setUsageLoading] = useState(false);
  const [profileSaving, setProfileSaving] = useState(false);
  /**
   * 谁在登录、谁在付费。用「只换额度」切过之后登录身份不变、付费账号变成另一个，
   * 所以列表里要分别标出来。
   */
  const [identity, setIdentity] = useState<CurrentIdentity | null>(null);

  /**
   * 系统指标。后端每秒采一次并缓存，这里按同样节奏取用 —— 菜单栏与面板显示
   * 的是同一份读数，不会出现两处数字对不上。
   */
  const [sys, setSys] = useState<SysMetrics | null>(null);

  /**
   * 界面自动化的授权状态。辅助功能只能在系统设置里授，所以这里只显示实况 +
   * 提供「去授权」按钮，不在前端做任何猜测。
   */
  const [ax, setAx] = useState<AxStatus | null>(null);
  const [axProbing, setAxProbing] = useState(false);
  /** Windows 没有「辅助功能」授权这一步，界面文案与按钮要跟着变。 */
  const isWindows = ax?.platform === "windows";
  /** 授权期间的轮询句柄。用户勾完系统设置那一刻我们要知道。 */
  const axPollRef = useRef<number | null>(null);

  // 传给子组件的回调要稳定，否则会反复触发依赖它的 effect
  const showError = useCallback((text: string) => setBanner({ kind: "danger", text }), []);

  const refresh = useCallback(async () => {
    const [list, cfg, records, storePaths, who] = await Promise.all([
      api.listAccounts(),
      api.getConfig(),
      api.getLogs(80),
      api.getStorePaths(),
      api.currentIdentity().catch(() => null),
    ]);
    setAccounts(list);
    setConfigState(cfg);
    // 手填框跟着配置走：首次加载用它填，之后不再覆盖用户正在输入的内容。
    setClientPathDraft((draft) =>
      draft.authFile === "" && draft.app === ""
        ? { authFile: cfg.client_auth_file, app: cfg.client_app_path }
        : draft,
    );
    setLogs(records);
    setPaths(storePaths);
    setIdentity(who);
  }, []);

  /** 拉一次待体验任务清单。失败不打扰：它只是辅助信息，签到本身不受影响。 */
  const loadPending = useCallback(async (silent = false) => {
    // 聚焦与签到回调可能撞在一起，同一次没回来之前不重复打接口
    if (pendingInflight.current) return;
    pendingInflight.current = true;
    if (!silent) setPendingLoading(true);
    try {
      const result = await api.growthPending();
      setPending(result.accounts);
    } catch {
      // 忽略：账号不可用或网络异常时保持上一次的清单
    } finally {
      pendingInflight.current = false;
      if (!silent) setPendingLoading(false);
    }
  }, []);

  /**
   * 待体验面板只显示一个账号：顶部一排账号胶囊切换，避免两个号的清单连成一片。
   * 没选过（或选中的账号已无待办）时落到第一个有任务的账号。
   */
  const pendingKeyOf = (item: PendingAccount) => item.id ?? item.label;
  const pendingAccounts = pending.filter((item) => item.tasks.length > 0);
  const activePending =
    pendingAccounts.find((item) => pendingKeyOf(item) === pendingTab) ?? pendingAccounts[0] ?? null;
  const activePendingKey = activePending ? pendingKeyOf(activePending) : null;
  const pendingRewardTotal = (item: PendingAccount) =>
    item.tasks.reduce((sum, task) => sum + (task.reward || 0), 0);

  /** 一次查完所有账号。只读操作，不消耗积分、不影响签到。 */
  const loadCreditsAll = useCallback(async () => {
    setCreditsLoadingAll(true);
    try {
      const summary = await api.fetchCreditsAll();
      const next: Record<string, CreditItem> = {};
      summary.items.forEach((item) => {
        next[item.id] = item;
      });
      setCredits(next);
    } catch (error) {
      setBanner({ kind: "danger", text: `积分查询失败：${String(error)}` });
    } finally {
      setCreditsLoadingAll(false);
    }
  }, []);

  const loadCreditsOne = async (id: string) => {
    setCreditsBusyId(id);
    try {
      const item = await api.fetchCreditsOne(id);
      setCredits((prev) => ({ ...prev, [id]: item }));
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setCreditsBusyId(null);
    }
  };

  /** 拉一次最近 30 天的用量明细。接口返回逐请求记录，日汇总在后端做。 */
  const refreshUsage = useCallback(async () => {
    setUsageLoading(true);
    try {
      setUsage(await api.fetchUsageAll());
    } catch (error) {
      setBanner({ kind: "danger", text: `积分趋势查询失败：${String(error)}` });
    } finally {
      setUsageLoading(false);
    }
  }, []);

  /** 成长中心：旅行领奖/盲盒/任务领取，三个动作由后端串起来跑。 */
  const runGrowth = async () => {
    setGrowthRunning(true);
    setBanner(null);
    try {
      const summary = await api.growthRunAll();
      if (summary.total === 0) {
        setBanner({ kind: "danger", text: "还没有账号" });
      } else if (summary.failed === 0) {
        setBanner({ kind: "ok", text: `${summary.total} 个账号的成长中心已处理完。` });
      } else {
        const failed = summary.items.filter((item) => !item.ok).map((item) => item.label);
        setBanner({
          kind: "danger",
          text: `成长中心：成功 ${summary.succeeded} 个，失败 ${summary.failed} 个（${failed.join("、")}）。`,
        });
      }
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setGrowthRunning(false);
      await refresh().catch(() => undefined);
      await loadPending().catch(() => undefined);
    }
  };

  /** 保存"当前登录账号"的现场，供切换时恢复。客户端在跑时自动退出，存完再拉起。 */
  const saveProfile = async () => {    setProfileSaving(true);
    setBanner({ kind: "ok", text: "正在保存现场：客户端若在运行，会先退出、存完自动重新打开…" });
    try {
      const meta = await api.profileSave(true);
      const tail = meta.restarted ? "，客户端已重新打开" : "";
      setBanner({
        kind: "ok",
        text: `已保存「${meta.label ?? meta.uid}」的现场（${meta.layers.join(" / ")}）${tail}。`,
      });
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setProfileSaving(false);
    }
  };

  /**
   * 切换菜单栏显示项。
   *
   * 先改本地状态再等后端返回：勾选框要跟手，不能等一次 IPC 往返才亮。
   * 失败时把配置拉回来，避免界面显示的和实际存的对不上。
   */
  const toggleMenubar = async (key: string, on: boolean) => {
    const current = config.menubar_items ?? [];
    const next = on
      ? [...current.filter((item) => item !== key), key]
      : current.filter((item) => item !== key);
    setConfigState({ ...config, menubar_items: next });
    try {
      const saved = await api.setMenubarItems(next);
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
      await refresh().catch(() => undefined);
    }
  };

  useEffect(() => {
    // 每秒取一次后端缓存的采样值：网速天然是「每秒增量」，采样再快也没有意义。
    let alive = true;
    const tick = () => {
      api
        .sysMetrics()
        .then((value) => {
          if (alive) setSys(value);
        })
        .catch(() => undefined);
    };
    tick();
    const timer = window.setInterval(tick, 1000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    api.axStatus().then(setAx).catch(() => undefined);
    return () => {
      if (axPollRef.current !== null) window.clearInterval(axPollRef.current);
      axPollRef.current = null;
    };
  }, []);

  useEffect(() => {
    // 先用上次的缓存渲染，避免首屏空着；随后再拉一次最新
    api
      .usageCached()      .then((cached) => {
        if (cached.days.length > 0) {
          setUsage(cached);
        }
      })
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    // 先渲染账号列表，再在后台补积分：一次积分查询要打三个官方接口，
    // 串在首屏之前会明显拖慢启动。
    refresh()
      .then(() => loadCreditsAll())
      .catch((error) => setBanner({ kind: "danger", text: String(error) }));
    refreshUsage().catch(() => undefined);
    loadPending().catch(() => undefined);
  }, [refresh, loadCreditsAll, refreshUsage]);

  useEffect(() => {
    const subscriptions = [
      listen<{ index: number; total: number }>("checkin://progress", (event) => {
        setProgress({ index: event.payload.index, total: event.payload.total });
      }),
      listen<CheckinSummary>("checkin://finished", () => {
        setProgress(null);
        refresh().catch(() => undefined);
        // 签到会顺手把已完成任务的奖励领掉，领完立刻同步待体验，已领的不再挂着
        loadPending().catch(() => undefined);
      }),
    ];
    return () => {
      subscriptions.forEach((pending) => pending.then((unlisten) => unlisten()));
    };
  }, [refresh, loadPending]);

  /**
   * 回到面板就静默同步一次待体验。
   *
   * 领奖可能发生在客户端里（点「去体验」做完后在客户端领，或直接领），wb-checkin
   * 感知不到那一刻；等窗口重新拿到焦点再拉一次，已领取的任务自然从清单里消失。
   */
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (focused) loadPending(true).catch(() => undefined);
      })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadPending]);

  const today = todayLocal();

  const stats = useMemo(() => {
    const done = accounts.filter(
      (account) => account.lastCheckinDate === today && account.lastCheckinResult !== "error",
    ).length;
    const failed = accounts.filter(
      (account) => account.lastCheckinDate === today && account.lastCheckinResult === "error",
    ).length;
    return { done, failed, pending: accounts.length - done - failed };
  }, [accounts, today]);

  const runAll = async () => {
    setRunning(true);
    setBanner(null);
    try {
      const summary = await api.checkinAll();
      if (summary.skipped) {
        setBanner({ kind: "danger", text: "已有一轮自动签到在进行中，请稍候再试。" });
      } else {
        setBanner(
          summary.failed === 0
            ? { kind: "ok", text: `${summary.total} 个账号处理完成，全部成功。` }
            : {
                kind: "danger",
                text: `${summary.total} 个账号：成功 ${summary.succeeded} 个，失败 ${summary.failed} 个。`,
              },
        );
      }
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setRunning(false);
      setProgress(null);
      await refresh().catch(() => undefined);
      // 签到顺手领奖后，待体验里已领的条目要立即消失
      await loadPending().catch(() => undefined);
    }
  };

  const runOne = async (id: string) => {
    setBusyId(id);
    setBanner(null);
    try {
      const item = await api.checkinOne(id);
      if (!item.ok) {
        setBanner({ kind: "danger", text: `${item.label}：${item.message || "签到失败"}` });
      }
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setBusyId(null);
      await refresh().catch(() => undefined);
      await loadPending().catch(() => undefined);
    }
  };

  const remove = async (account: Account) => {
    const label = account.nickname || account.email || account.uid || "该账号";
    if (!window.confirm(`确认删除「${label}」？本地保存的凭证会一并移除。`)) {
      return;
    }
    try {
      await api.deleteAccount(account.id);
      await refresh();
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  const toggleAuto = async (next: boolean) => {
    try {
      const saved = await api.setConfig(
        next,
        config.backup_hour,
        config.backup_minute,
        config.auto_growth,
      );
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  /** 自动签到之后再顺手跑成长中心（旅行 + 任务），可以单独关掉。 */
  const toggleGrowth = async (next: boolean) => {
    try {
      const saved = await api.setConfig(
        config.auto_enabled,
        config.backup_hour,
        config.backup_minute,
        next,
      );
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  /** 自动处理新建会话前先切到这个模型 —— 走流程的对话没必要花额度。 */
  const changeFreeModel = async (value: string) => {
    try {
      const saved = await api.setConfig(
        config.auto_enabled,
        config.backup_hour,
        config.backup_minute,
        config.auto_growth,
        value,
      );
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  /** 重新问一次后端：它现在认定的登录态文件与客户端位置在哪。 */
  const refreshClientPaths = useCallback(async () => {
    try {
      setClientPaths(await api.switchPreflight());
    } catch {
      // 客户端没装、目录不存在都会走到这里，界面显示「未找到」即可
      setClientPaths(null);
    }
  }, []);

  /** 保存手填的客户端路径。两个都留空 = 恢复自动探测。 */
  const saveClientPaths = async () => {
    try {
      const saved = await api.setConfig(
        config.auto_enabled,
        config.backup_hour,
        config.backup_minute,
        config.auto_growth,
        config.free_model,
        clientPathDraft.authFile.trim(),
        clientPathDraft.app.trim(),
      );
      setConfigState(saved);
      await refreshClientPaths();
      setBanner({ kind: "ok", text: "客户端路径已保存" });
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  // 首屏问一次自动探测结果；重新打开设置页时也会用到最新的值。
  useEffect(() => {
    refreshClientPaths().catch(() => undefined);
  }, [refreshClientPaths]);

  /** 重新读一次辅助功能授权状态。授权之后不会自动生效，得用户点一下刷新。 */
  const refreshAx = async () => {
    try {
      setAx(await api.axStatus());
    } catch {
      // 忽略：状态查不到不影响其他功能
    }
  };

  /** 弹系统授权提示，并把系统设置停在「辅助功能」页。 */
  const grantAx = async () => {
    try {
      setAx(await api.axRequestTrust());
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
    // 用户是在系统设置里勾选，勾完那一刻程序并不知道。这里最多盯 60 秒，
    // 一旦授权到位就顺手探一次界面 —— 否则还得让人回来再点两下。
    if (axPollRef.current !== null) return;
    let left = 30;
    axPollRef.current = window.setInterval(async () => {
      left -= 1;
      const next = await api.axStatus().catch(() => null);
      if (next && next.windows > 0) {
        if (axPollRef.current !== null) window.clearInterval(axPollRef.current);
        axPollRef.current = null;
        setAx(next);
        await probeAx();
        return;
      }
      if (left <= 0) {
        if (axPollRef.current !== null) window.clearInterval(axPollRef.current);
        axPollRef.current = null;
      }
    }, 2000);
  };

  /** dump 客户端界面树，用于确认自动化能认到哪些控件。 */
  const probeAx = async () => {
    setAxProbing(true);
    try {
      const result = await api.axProbe();
      setBanner({
        kind: result.windows > 0 ? "ok" : "danger",
        text:
          result.windows > 0
            ? `认到 ${result.windows} 个窗口，界面树 ${result.lines} 行已写到 ${result.path}`
            : `读不到客户端窗口（界面树 ${result.lines} 行）。通常是辅助功能授权还没生效。`,
      });
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    } finally {
      setAxProbing(false);
    }
  };

  const changeBackupTime = async (value: string) => {
    const parsed = parseTimeValue(value);
    if (!parsed) {
      setBanner({ kind: "danger", text: "兜底时刻格式应为 HH:MM" });
      return;
    }
    try {
      const saved = await api.setConfig(
        config.auto_enabled,
        parsed.hour,
        parsed.minute,
        config.auto_growth,
      );
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  return (
    <div className="shell">
      <div className="titlebar" data-tauri-drag-region>
        <div data-tauri-drag-region>
          <h1 style={{ display: "inline" }}>WB 签到</h1>
          <span className="sub">多账号每日自动签到</span>
        </div>
        {tab === "checkin" ? (
          <button className="btn sm" onClick={() => setDialogOpen(true)}>
            添加账号
          </button>
        ) : tab === "ferry" ? (
          <span className="sub">可选功能 · 需要 Node 22+</span>
        ) : (
          <span className="sub">可选功能 · 默认关闭</span>
        )}
      </div>

      <div className="content">
        {banner && (
          <div className={`banner${banner.kind === "danger" ? " danger" : ""}`}>
            <span>{banner.text}</span>
          </div>
        )}

        <div className="tabs">
          <button
            className={`tab${tab === "checkin" ? " on" : ""}`}
            onClick={() => setTab("checkin")}
          >
            账号签到
          </button>
          <button className={`tab${tab === "ai" ? " on" : ""}`} onClick={() => setTab("ai")}>
            本地 AI
          </button>
          <button
            className={`tab${tab === "ferry" ? " on" : ""}`}
            onClick={() => setTab("ferry")}
          >
            模型摆渡
          </button>
        </div>

        {tab === "ferry" ? (
          <FerryPanel active={tab === "ferry"} onError={showError} />
        ) : tab === "ai" ? (
          <LocalAiPanel
            config={config.local_ai}
            active={tab === "ai"}
            onConfig={setConfigState}
            onError={showError}
          />
        ) : (
          <>
        <div className="overview">
          <div className="stat">
            <div className="value">{accounts.length}</div>
            <div className="label">账号总数</div>
          </div>
          <div className="stat">
            <div className="value ok">{stats.done}</div>
            <div className="label">今日已签</div>
          </div>
          <div className="stat">
            <div className="value pending">{stats.pending}</div>
            <div className="label">待签到</div>
          </div>
          {stats.failed > 0 && (
            <div className="stat">
              <div className="value" style={{ color: "var(--danger)" }}>
                {stats.failed}
              </div>
              <div className="label">失败</div>
            </div>
          )}
          <div className="spacer" />
          <button
            className="btn"
            style={{ marginRight: 8 }}
            onClick={runGrowth}
            disabled={growthRunning || accounts.length === 0}
            title="旅行领奖 / 盲盒 / 任务领取（三个动作各自容错）"
          >
            {growthRunning && <span className="spin" />}
            {growthRunning ? "领取中" : "今日全领"}
          </button>
          <button
            className="btn"
            style={{ marginRight: 8 }}
            onClick={loadCreditsAll}
            disabled={creditsLoadingAll || accounts.length === 0}
            title="查询所有账号的积分余额（只读，不消耗积分）"
          >
            {creditsLoadingAll && <span className="spin" />}
            {creditsLoadingAll ? "查询中" : "刷新积分"}
          </button>
          <button className="btn primary" onClick={runAll} disabled={running || accounts.length === 0}>
            {running && <span className="spin" />}
            {running
              ? progress
                ? `签到中 ${progress.index}/${progress.total}`
                : "签到中"
              : "全部签到"}
          </button>
        </div>

        <div className="section-head">
          <h2>账号</h2>
        </div>
        <div className="panel">
          {accounts.length === 0 ? (
            <div className="empty">
              <div className="big">还没有账号</div>
              <div className="hint">点右上角「添加账号」，用独立登录逐个授权多个账号</div>
            </div>
          ) : (
            accounts.map((account) => (
              <AccountRow
                key={account.id}
                account={account}
                active={Boolean(account.uid) && account.uid === identity?.loginUid}
                billing={Boolean(account.uid) && account.uid === identity?.billingUid}
                today={today}
                busy={busyId === account.id}
                disabled={running}
                credits={credits[account.id] ?? null}
                creditsBusy={creditsBusyId === account.id}
                switching={switchTarget?.id === account.id}
                onCheckin={() => runOne(account.id)}
                onDelete={() => remove(account)}
                onLoadCredits={() => loadCreditsOne(account.id)}
                onSwitch={() => setSwitchTarget(account)}
              />
            ))
          )}
        </div>

        {pendingAccounts.length > 0 && (
          <>
            <div className="section-head">
              <h2>待体验</h2>
              <div className="head-tools">
                {activePending && (
                  <span className="head-meta">
                    {activePending.tasks.length} 项 · +{pendingRewardTotal(activePending)} 积分
                  </span>
                )}
                <button
                  className="btn sm"
                  onClick={() => loadPending()}
                  disabled={pendingLoading}
                  title="重新拉取各账号的待体验任务"
                >
                  {pendingLoading && <span className="spin" />}
                  {pendingLoading ? "刷新中" : "刷新"}
                </button>
              </div>
            </div>
            <div className="panel task-panel">
              {pendingAccounts.length > 1 && (
                <div className="task-tabs" role="tablist">
                  {pendingAccounts.map((item) => {
                    const key = item.id ?? item.label;
                    return (
                      <button
                        key={key}
                        role="tab"
                        aria-selected={activePendingKey === key}
                        className={`task-tab${activePendingKey === key ? " active" : ""}`}
                        onClick={() => setPendingTab(key)}
                      >
                        {item.label}
                        <span className="task-tab-count">{item.tasks.length}</span>
                      </button>
                    );
                  })}
                </div>
              )}

              {activePending ? (
                <div className="task-list">
                  {activePending.tasks.map((task) => (
                    <div className="task-row" key={task.code}>
                      <div className="task-main">
                        <div className="task-title">
                          <span className="task-name">{task.title}</span>
                          {task.target > 1 && (
                            <span className="task-progress">
                              {task.current}/{task.target}
                            </span>
                          )}
                        </div>
                        {task.description && (
                          <div className="task-desc" title={task.description}>
                            {task.description}
                          </div>
                        )}
                      </div>
                      <span className="task-reward">+{task.reward}</span>
                      <button
                        className="btn sm"
                        disabled={!task.jumpUrl}
                        onClick={() =>
                          api.openLink(task.jumpUrl).catch((error) => showError(String(error)))
                        }
                      >
                        去体验
                      </button>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="hint">这个账号暂时没有待体验任务。</div>
              )}

              <div className="task-foot">
                这些任务要在客户端里真用一次才算完成（召唤专家对话、读文档…），接口代不了。
                点「去体验」直接跳到对应页面，做完后下一轮签到会自动把奖励领掉。
              </div>
            </div>
          </>
        )}

        <div className="section-head">
          <h2>积分趋势</h2>
          <button
            className="btn sm"
            onClick={refreshUsage}
            disabled={usageLoading || accounts.length === 0}
            title="拉取最近 30 天的逐请求用量，本地按日汇总"
          >
            {usageLoading && <span className="spin" />}
            {usageLoading ? "查询中" : "刷新"}
          </button>
        </div>
        <div className="panel">
          <UsageTrend usage={usage} loading={usageLoading} />
        </div>

        <div className="section-head">
          <h2>系统</h2>
        </div>
        <div className="panel sys">
          <div className="sys-row">
            <div className="sys-block">
              <div className="sys-label">硬盘已用</div>
              <div className="sys-value">
                {sys?.diskPercent == null ? "—" : `${Math.round(sys.diskPercent)}%`}
              </div>
              <div className="sys-bar">
                <span
                  style={{ width: `${Math.min(100, Math.max(0, sys?.diskPercent ?? 0))}%` }}
                />
              </div>
              <div className="sys-sub">
                {sys?.diskUsed == null || sys?.diskTotal == null
                  ? "读取中"
                  : `${formatBytes(sys.diskUsed)} / ${formatBytes(sys.diskTotal)}`}
              </div>
            </div>

            <div className="sys-block">
              <div className="sys-label">实时网速</div>
              <div className="sys-net">
                <span className="sys-net-item">
                  <span className="arrow">↓</span>
                  <span className="value">
                    {sys?.netDown == null ? "—" : `${formatBytes(sys.netDown)}/s`}
                  </span>
                </span>
                <span className="sys-net-item">
                  <span className="arrow">↑</span>
                  <span className="value">
                    {sys?.netUp == null ? "—" : `${formatBytes(sys.netUp)}/s`}
                  </span>
                </span>
              </div>
              <div className="sys-sub">最近一秒的平均速率，回环流量不计入</div>
            </div>
          </div>
        </div>

        <div className="section-head">
          <h2>菜单栏显示</h2>
        </div>
        <div className="panel settings">
          <div className="row">
            <div>
              <div className="row-label">在菜单栏显示</div>
              <div className="row-hint">
                勾选即时生效，不用重启。网速自己占一个图标（上下两行），硬盘、CPU、内存
                挂在主图标上；网速不勾时那个图标会整个收起，不在菜单栏占位。
              </div>
            </div>
          </div>
          <div className="menu-picks">
            {MENUBAR_OPTIONS.map((option) => {
              const on = (config.menubar_items ?? []).includes(option.key);
              return (
                <label className={`pick${on ? " on" : ""}`} key={option.key}>
                  <input
                    type="checkbox"
                    checked={on}
                    onChange={(event) => toggleMenubar(option.key, event.target.checked)}
                  />
                  <span className="pick-text">
                    <span className="pick-label">{option.label}</span>
                    <span className="pick-hint">{option.hint}</span>
                  </span>
                </label>
              );
            })}
          </div>
        </div>

        <div className="section-head">
          <h2>自动签到</h2>
        </div>
        <div className="panel settings">
          <div className="row">
            <div>
              <div className="row-label">开启自动签到</div>
              <div className="row-hint">
                App 启动时立即签到一次；此后每天在下面的兜底时刻再签一次。关闭后只能手动签到。
              </div>
            </div>
            <button
              className="switch"
              data-on={config.auto_enabled}
              aria-label="切换自动签到"
              onClick={() => toggleAuto(!config.auto_enabled)}
            />
          </div>
          <div className="row">
            <div>
              <div className="row-label">每日兜底时刻</div>
              <div className="row-hint">
                每天到达该时刻再签一次，用于覆盖当天尚未签到的情况。签到接口幂等，当天已签到
                时重复调用只会返回「已签到」，不会重复提交。
              </div>
            </div>
            <input
              className="select time"
              type="time"
              disabled={!config.auto_enabled}
              value={toTimeValue(config.backup_hour, config.backup_minute)}
              onChange={(event) => changeBackupTime(event.target.value)}
            />
          </div>
          <div className="row">
            <div>
              <div className="row-label">顺手处理成长中心</div>
              <div className="row-hint">
                自动签到之后，为该账号领取旅行奖励并再次出发、接取未报名的任务、领取可领的
                任务奖励；没做完的体验任务会列在上方「待体验」里，点一下就能跳过去。
                只做这些，「盲盒」不在此范围内 —— 抽奖次数有限，留给你自己在「今日全领」里点。
              </div>
            </div>
            <button
              className="switch"
              data-on={config.auto_growth}
              aria-label="切换成长中心自动处理"
              disabled={!config.auto_enabled}
              onClick={() => toggleGrowth(!config.auto_growth)}
            />
          </div>
          <div className="row">
            <div>
              <div className="row-label">界面自动化</div>
              <div className="row-hint">
                「召唤专家」这类体验任务只有在客户端界面里真的说上一句才算数，接口代不了。
                {isWindows
                  ? "Windows 上直接用系统自带的 UI Automation，不需要额外授权，只要客户端开着窗口就能用。"
                  : "授权「辅助功能」之后，程序可以替你打开专家页并发送一句话。"}
                <br />
                当前：
                {ax === null
                  ? "检查中"
                  : ax.windows > 0
                    ? "可用"
                    : isWindows
                      ? "没读到客户端窗口，可能还缩在托盘里，点开它再试"
                      : ax.trusted
                        ? "已勾选但读不到界面，重启客户端再试"
                        : "未授权"}
                {ax?.clientPid ? `　客户端 ${ax.clientPid} 正在运行` : "　客户端未运行"}
              </div>
            </div>
            <div className="row-actions">
              <button className="btn sm" onClick={probeAx} disabled={axProbing}>
                {axProbing ? "探测中" : "探测界面"}
              </button>
              {isWindows || (ax && ax.windows > 0) ? (
                <button className="btn sm ghost" onClick={refreshAx}>
                  刷新
                </button>
              ) : (
                <button className="btn sm primary" onClick={grantAx}>
                  去授权
                </button>
              )}
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">新建会话用免费模型</div>
              <div className="row-hint">
                体验类任务只是走个流程，用付费模型纯属浪费额度。自动处理开始前会先切到这一档，
                切完再新建会话。Hy3 标价 0.00x（限时免费）；Hy4 preview 只在夜间免费，白天照样计费。
              </div>
            </div>
            <div className="row-actions">
              <select
                className="select"
                value={config.free_model}
                onChange={(event) => changeFreeModel(event.target.value)}
              >
                <option value="hy3">Hy3（限时免费）</option>
                <option value="hy4">Hy4 preview（夜间免费）</option>
                <option value="">不切换</option>
              </select>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">客户端路径</div>
              <div className="row-hint">
                切换账号要改写官方客户端的登录态文件，重启时还要拉起客户端本身。
                留空由程序自动探测；Windows 上客户端的数据目录名无从预知，
                探测不到时在这里手填（两个都留空即恢复自动探测）。
              </div>
              <div className="path">登录态：{clientPaths?.authFilePath ?? "未找到"}</div>
              <div className="path">客户端：{clientPaths?.clientAppPath ?? "未找到"}</div>
              <div style={{ display: "grid", gap: 8, marginTop: 10 }}>
                <input
                  className="select"
                  placeholder="登录态文件路径（留空 = 自动探测）"
                  value={clientPathDraft.authFile}
                  onChange={(event) =>
                    setClientPathDraft((draft) => ({ ...draft, authFile: event.target.value }))
                  }
                />
                <input
                  className="select"
                  placeholder="客户端程序路径（留空 = 自动探测）"
                  value={clientPathDraft.app}
                  onChange={(event) =>
                    setClientPathDraft((draft) => ({ ...draft, app: event.target.value }))
                  }
                />
              </div>
            </div>
            <div className="row-actions">
              <button className="btn sm" onClick={saveClientPaths}>
                保存
              </button>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">后台常驻</div>
              <div className="row-hint">
                点窗口的关闭按钮不会退出程序，窗口会收起来、程序继续在菜单栏 / 托盘里跑，
                到点照常签到。要把界面调回来，点托盘图标 →「显示主窗口」；要真正退出，
                点托盘菜单或右键菜单里的「退出 WB 签到」。
              </div>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">数据目录</div>
              <div className="path">{paths?.dataDir ?? "—"}</div>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">账号现场快照</div>
              <div className="row-hint">
                切换账号时用来保留会话（登录态 + 用户目录 + 浏览器会话 + 侧边栏）。
                每个账号需各存一次：在客户端里登录该账号 → 退出 WorkBuddy → 点右侧按钮。
              </div>
            </div>
            <button className="btn sm" onClick={saveProfile} disabled={profileSaving}>
              {profileSaving && <span className="spin" />}
              {profileSaving ? "保存中" : "保存现场"}
            </button>
          </div>
        </div>

        <div className="section-head">
          <h2>签到日志</h2>
        </div>
        <div className="panel logs">
          {logs.length === 0 ? (
            <div className="empty">
              <div className="hint">暂无记录</div>
            </div>
          ) : (
            logs.map((record, index) => (
              <div className="log" key={`${record.ts}-${index}`}>
                <span className="time">{formatStamp(record.ts, today)}</span>
                <span className="who">{record.account}</span>
                <span className="msg">{record.message}</span>
                <span
                  className={`verdict ${
                    record.result === "error"
                      ? "danger"
                      : record.result === "already"
                        ? "warn"
                        : "ok"
                  }`}
                >
                  {record.result === "error"
                    ? "失败"
                    : record.result === "already"
                      ? "已签过"
                      : record.result === "growth"
                        ? "成长"
                        : "成功"}
                </span>
              </div>
            ))
          )}
        </div>
          </>
        )}
      </div>

      {dialogOpen && (
        <AddAccountDialog
          onClose={() => setDialogOpen(false)}
          onDone={async (text) => {
            setDialogOpen(false);
            setBanner({ kind: "ok", text });
            await refresh().catch(() => undefined);
            await loadCreditsAll().catch(() => undefined);
          }}
        />
      )}

      {switchTarget && (
        <SwitchDialog
          account={switchTarget}
          currentName={accounts.find((item) => item.uid === identity?.loginUid)?.nickname ?? null}
          onClose={() => setSwitchTarget(null)}
          onDone={async (text) => {
            setSwitchTarget(null);
            setBanner({ kind: "ok", text });
            await refresh().catch(() => undefined);
          }}
        />
      )}
    </div>
  );
}

/** 积分消耗：14 根日柱 + 每账号今日/7 日/30 日汇总。 */
function UsageTrend({ usage, loading }: { usage: UsageSummary | null; loading: boolean }) {
  if (!usage || usage.days.length === 0) {
    return (
      <div className="empty">
        <div className="hint">
          {loading ? "正在拉取用量…" : "还没有用量数据，点右上「刷新」拉取最近 30 天记录"}
        </div>
      </div>
    );
  }

  const recent = usage.days.slice(-14);
  const max = Math.max(...recent.map((day) => day.credit), 1);
  const sum = (key: "today" | "week" | "month") =>
    usage.accounts.reduce((total, account) => total + (account[key] || 0), 0);

  return (
    <div className="trend">
      <div className="trend-stats">
        <Stat label="今日消耗" value={sum("today")} />
        <Stat label="近 7 天" value={sum("week")} />
        <Stat label="近 30 天" value={sum("month")} />
      </div>

      <div className="trend-bars">
        {recent.map((day) => (
          <div className="trend-col" key={day.date} title={`${day.date}：${formatCredit(day.credit)}`}>
            <div
              className={`trend-bar${day.credit > 0 ? "" : " zero"}`}
              style={{ height: `${Math.max(2, (day.credit / max) * 100)}%` }}
            />
          </div>
        ))}
      </div>
      <div className="trend-axis">
        {recent.map((day, index) => (
          <span key={day.date}>{index % 3 === 0 ? day.date.slice(5) : ""}</span>
        ))}
      </div>

      <div className="trend-rows">
        <div className="trend-row head">
          <span className="who">账号</span>
          <span className="num">今日</span>
          <span className="num">7 日</span>
          <span className="num">30 日</span>
        </div>
        {usage.accounts.map((account) => (
          <div className="trend-row" key={account.id}>
            <span className="who">{account.label}</span>
            <span className="num">{account.ok ? formatCredit(account.today) : "—"}</span>
            <span className="num">{account.ok ? formatCredit(account.week) : "—"}</span>
            <span className="num">{account.ok ? formatCredit(account.month) : "—"}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number }) {

  return (
    <div className="trend-stat">
      <div className="value">{formatCredit(value)}</div>
      <div className="label">{label}</div>
    </div>
  );
}

/** 积分是小数（官方按精确值返回），整数时不显示多余的小数位。 */
function formatCredit(value: number): string {
  if (!Number.isFinite(value)) {
    return "—";
  }
  return Number.isInteger(value) ? `${value}` : value.toFixed(1);
}

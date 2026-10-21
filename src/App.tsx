import { useCallback, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import * as api from "./api";
import type {
  Account,
  AppConfig,
  CheckinSummary,
  CreditItem,
  CurrentIdentity,
  LogRecord,
  StorePaths,
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
  local_ai: { enabled: false, log_dir: "", smctemp_path: "", services: [] },
  menubar_items: ["net", "disk"],
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
    setLogs(records);
    setPaths(storePaths);
    setIdentity(who);
  }, []);

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
  }, [refresh, loadCreditsAll, refreshUsage]);

  useEffect(() => {
    const subscriptions = [
      listen<{ index: number; total: number }>("checkin://progress", (event) => {
        setProgress({ index: event.payload.index, total: event.payload.total });
      }),
      listen<CheckinSummary>("checkin://finished", () => {
        setProgress(null);
        refresh().catch(() => undefined);
      }),
    ];
    return () => {
      subscriptions.forEach((pending) => pending.then((unlisten) => unlisten()));
    };
  }, [refresh]);

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
      const saved = await api.setConfig(next, config.backup_hour, config.backup_minute);
      setConfigState(saved);
    } catch (error) {
      setBanner({ kind: "danger", text: String(error) });
    }
  };

  const changeBackupTime = async (value: string) => {
    const parsed = parseTimeValue(value);
    if (!parsed) {
      setBanner({ kind: "danger", text: "兜底时刻格式应为 HH:MM" });
      return;
    }
    try {
      const saved = await api.setConfig(config.auto_enabled, parsed.hour, parsed.minute);
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
              <div className="row-label">后台常驻</div>
              <div className="row-hint">
                点窗口的关闭按钮不会退出程序，窗口会收起来、程序继续在右上角菜单栏跑，
                到点照常签到。要把界面调回来，点菜单栏图标 →「显示主窗口」；要真正退出，
                点「退出 WB 签到」或按 ⌘Q。
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
                    record.result === "error" ? "danger" : record.result === "already" ? "warn" : "ok"
                  }`}
                >
                  {record.result === "error" ? "失败" : record.result === "already" ? "已签过" : "成功"}
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

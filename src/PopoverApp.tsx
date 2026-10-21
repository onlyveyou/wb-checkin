import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import * as api from "./api";
import type { Account, AppConfig, LocalAiStatus } from "./types";

/**
 * 菜单栏弹出面板。
 *
 * 信息量按「点开一眼看完」来定：签到进度、AI 温度与服务状态、三个动作。
 * 深挖（改配置、看完整日志、切账号）一律留给主窗口 —— 面板塞太多就失去意义了。
 */

/** 时间戳是否落在今天。用时间戳而非字符串，免得受后端日期格式影响。 */
function isToday(ts: number | null): boolean {
  if (!ts) return false;
  const d = new Date(ts);
  const now = new Date();
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  );
}

const pad = (n: number) => String(n).padStart(2, "0");
const clock = (h: number, m: number) => `${pad(h)}:${pad(m)}`;
/** 带秒：面板是靠「值在动」证明自己活着，只到分钟的话用户没法判断有没有刷新。 */
const timeOf = (ts: number) => {
  const d = new Date(ts);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
};

const STATE_TEXT: Record<string, string> = {
  ready: "运行中",
  cold: "空闲",
  down: "未响应",
};

/** 字节转 GB。内存容量基本都是整数 GB，整数就不带小数，免得写成「32.0G」。 */
const gb = (bytes: number) => {
  const value = bytes / 1024 ** 3;
  return Number.isInteger(value) ? String(value) : value.toFixed(1);
};

/** 内存占用分档：<70 正常 / 70–90 偏紧 / >90 吃紧。 */
const memTone = (percent: number) =>
  percent < 70 ? "is-ok" : percent < 90 ? "is-warn" : "is-danger";

/** 面板开着时 AI 状态的轮询间隔。温度这类指标要「看着在动」才有意义。 */
const AI_POLL_MS = 5_000;

/** 面板开着时签到概览的轮询间隔。数据只在兜底签到跑完后会变，慢一点无妨。 */
const CORE_POLL_MS = 10_000;

/**
 * 面板关着时的兜底轮询间隔。
 *
 * 正常路径是「后端弹出 → 发 popover-shown → 前端刷新」，但这条链路一旦哑掉
 * （事件没到、焦点没变化），用户看到的就是一块永远不动的内容 —— 面板不像普通
 * 窗口，它没有「重新打开就重新加载」这层保险。所以留一条最慢的兜底：
 * 只读两个本地 JSON 文件，一分钟一次，代价可以忽略，但能保证内容迟早是新的。
 */
const WATCHDOG_MS = 60_000;

/** 一次刷新要拉哪几块数据。签到概览和 AI 状态不是一个量级，分开控频。 */
type Slot = "core" | "ai";

export default function PopoverApp() {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [ai, setAi] = useState<LocalAiStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState(0);
  const [stale, setStale] = useState(false);

  /** 面板是否「开着且在前台」。关着的时候除了兜底轮询，不做任何后台请求。 */
  const openRef = useRef(false);
  /** 同类请求没回来之前不再发新的，免得慢探测（AI 探端口最长几秒）叠成一堆。 */
  const slotBusyRef = useRef<Record<Slot, boolean>>({ core: false, ai: false });
  /** 轮询用得上「AI 开着吗」，但轮询闭包里读不到最新的 config，用 ref 同步一份。 */
  const aiOnRef = useRef(false);

  useEffect(() => {
    aiOnRef.current = Boolean(config?.local_ai.enabled);
  }, [config]);

  /**
   * 拉一次数据。
   *
   * `silent` 是自动轮询用的：网络抖一下不该把红色报错糊在面板上，只把时间戳标成
   * 「读取失败」，下一次成功自然就恢复了。用户主动点刷新/签到走的非静默路径，报错
   * 照旧显示，而且不会被轮询悄悄清掉 —— 失败提示得留得住。
   */
  const pull = useCallback(async (what: "full" | Slot, silent: boolean) => {
    const slots: Slot[] = what === "full" ? ["core", "ai"] : [what];
    if (slots.some((s) => slotBusyRef.current[s])) return;
    slots.forEach((s) => {
      slotBusyRef.current[s] = true;
    });
    try {
      let aiEnabled = aiOnRef.current;
      if (what !== "ai") {
        const [list, cfg] = await Promise.all([api.listAccounts(), api.getConfig()]);
        setAccounts(list);
        setConfig(cfg);
        aiEnabled = cfg.local_ai.enabled;
      }
      // AI 监控是可选功能：关着的时候后端只回空壳，没必要每次都去探一遍端口
      if (what !== "core") setAi(aiEnabled ? await api.localAiStatus() : null);
      setUpdatedAt(Date.now());
      setStale(false);
    } catch (e) {
      if (silent) setStale(true);
      else setNotice(String(e));
    } finally {
      slots.forEach((s) => {
        slotBusyRef.current[s] = false;
      });
    }
  }, []);

  useEffect(() => {
    void pull("full", false);

    // 面板是常驻窗口：显隐不会让它重新挂载，所以「刷新」得自己造触发点。
    // 三条独立路径互为备份 —— 任何一条哑掉，内容都不会停在旧值上：
    //   1) 后端每次弹出时发的 popover-shown（主路径）
    //   2) 窗口焦点变化（弹出时后端会 set_focus）
    //   3) 定时轮询（见下面的 effect）
    // 浏览器预览环境没有 Tauri 事件通道，listen 失败就退化成 2) + 3)。
    const enter = (full: boolean) => {
      openRef.current = true;
      void pull(full ? "full" : "core", !full);
    };
    const onFocus = () => enter(false);
    const onBlur = () => {
      // 面板失焦就等于后端会把它收起来，此时停工，别在看不见的地方空转
      openRef.current = false;
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") enter(false);
    };
    window.addEventListener("focus", onFocus);
    window.addEventListener("blur", onBlur);
    document.addEventListener("visibilitychange", onVisible);

    const subs = [
      listen("popover-shown", () => enter(true)),
      // 后端明确告知「已收起」：轮询停在这里，不靠系统失焦事件碰运气
      listen("popover-hidden", () => {
        openRef.current = false;
      }),
      // 后台兜底签到跑完时主窗口会收到这个事件，面板同样得跟上
      listen("checkin://finished", () => void pull("core", true)),
    ].map((p) => p.catch(() => () => {}));

    return () => {
      window.removeEventListener("focus", onFocus);
      window.removeEventListener("blur", onBlur);
      document.removeEventListener("visibilitychange", onVisible);
      subs.forEach((p) => void p.then((f) => f()));
    };
  }, [pull]);

  // 轮询：AI 温度要「在动」，签到概览只需要不错过兜底那一轮的变更。
  useEffect(() => {
    const ai = window.setInterval(() => {
      if (openRef.current && aiOnRef.current) void pull("ai", true);
    }, AI_POLL_MS);
    const core = window.setInterval(() => {
      if (openRef.current) void pull("core", true);
    }, CORE_POLL_MS);
    const watchdog = window.setInterval(() => void pull("core", true), WATCHDOG_MS);
    return () => {
      window.clearInterval(ai);
      window.clearInterval(core);
      window.clearInterval(watchdog);
    };
  }, [pull]);

  const rootRef = useRef<HTMLDivElement>(null);

  // 面板是「固定宽度、高度自适应」的窗口：内容多少不定（AI 开着比关着高一截），
  // 高度得量出来回传给后端。用 ResizeObserver 而不是渲染后手量一次，
  // 是因为数据到位（比如 AI 状态回来）会让内容二次长高。
  useEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const push = () => {
      // 向上取整再 +1：避免亚像素舍入把最后一行裁掉
      void api.popoverResize(Math.ceil(el.getBoundingClientRect().height) + 1);
    };
    push();
    const ro = new ResizeObserver(push);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const done = accounts.filter((a) => isToday(a.lastCheckinAt)).length;
  const failed = accounts.filter(
    (a) => isToday(a.lastCheckinAt) && a.lastCheckinResult === "error",
  ).length;

  const runCheckin = async () => {
    setBusy(true);
    setNotice(null);
    try {
      const summary = await api.checkinAll();
      if (summary.failed > 0) setNotice(`${summary.failed} 个账号签到失败`);
      // 静默刷新：失败提示已经写在上面了，别被这次拉数据的报错顶掉
      await pull("full", true);
    } catch (e) {
      setNotice(String(e));
    } finally {
      setBusy(false);
    }
  };

  const manualRefresh = () => {
    setNotice(null);
    void pull("full", false);
  };

  const aiOn = Boolean(config?.local_ai.enabled);
  const services = ai?.services ?? [];

  /**
   * 内存行的两个数：占用率（夹到 0–100，防后端异常值把进度条撑出格）与
   * 「已用/总量」。任一字段缺失就整行不显示 —— 宁可不显示，也不显示半截。
   */
  const mem = (() => {
    const raw = ai?.memory;
    if (!raw || raw.usedPercent == null || raw.used == null || raw.total == null) {
      return null;
    }
    return {
      percent: Math.min(100, Math.max(0, raw.usedPercent)),
      text: `${gb(raw.used)}/${gb(raw.total)}G`,
    };
  })();

  return (
    <div className="pop" ref={rootRef}>
      <header className="pop-head">
        <span className="pop-dot" />
        <span className="pop-title">WB 签到</span>
        <span className="pop-stamp">
          {updatedAt ? `更新于 ${timeOf(updatedAt)}` : "读取中…"}
          {stale && <em className="pop-bad"> · 读取失败</em>}
        </span>
        <button className="pop-refresh" onClick={manualRefresh} title="刷新">
          ↻
        </button>
      </header>

      <section className="pop-block">
        <div className="pop-block-head">今日签到</div>
        <div className="pop-kpi">
          <span className="pop-kpi-num">
            {done}
            <i>/{accounts.length}</i>
          </span>
          <span className="pop-kpi-note">
            {accounts.length === 0
              ? "还没有账号"
              : done === accounts.length
                ? "已全部完成"
                : `${accounts.length - done} 个待签`}
            {failed > 0 && <em className="pop-bad"> · {failed} 个失败</em>}
          </span>
        </div>
        <div className="pop-meta">
          {config
            ? `下次兜底 ${clock(config.backup_hour, config.backup_minute)} · 自动签到${
                config.auto_enabled ? "已开启" : "已关闭"
              }`
            : "—"}
        </div>
      </section>

      <section className="pop-block">
        <div className="pop-block-head">本地 AI</div>
        {!aiOn ? (
          <div className="pop-empty">
            监控未启用。这台机器不需要就保持关闭；要开在主窗口「本地 AI」页里开。
          </div>
        ) : (
          <>
            <div className="pop-temps">
              <div className="pop-temp">
                <b>{ai?.temps?.cpu != null ? `${ai.temps.cpu}°` : "—"}</b>
                <span>CPU</span>
              </div>
              <div className="pop-temp">
                <b>{ai?.temps?.gpu != null ? `${ai.temps.gpu}°` : "—"}</b>
                <span>GPU</span>
              </div>
            </div>
            {mem && (
              <div className="pop-mem">
                <span className="pop-mem-label">内存</span>
                <span className="pop-mem-bar">
                  <i className={memTone(mem.percent)} style={{ width: `${mem.percent}%` }} />
                </span>
                <span className="pop-mem-val">
                  {Math.round(mem.percent)}%
                  <em>{mem.text}</em>
                </span>
              </div>
            )}
            <ul className="pop-svc">
              {services.map((s) => (
                <li key={s.key}>
                  <span className={`pop-svc-dot is-${s.state}`} />
                  <span className="pop-svc-name">{s.name}</span>
                  <span className="pop-svc-port">{s.port}</span>
                  <span className={`pop-svc-state is-${s.state}`}>
                    {STATE_TEXT[s.state] ?? s.state}
                  </span>
                </li>
              ))}
            </ul>
            {ai && ai.tasks.length > 0 && (
              <div className="pop-meta">正在推理 {ai.tasks.length} 个任务</div>
            )}
            {services.length === 0 && <div className="pop-meta">正在探测…</div>}
          </>
        )}
      </section>

      {notice && <div className="pop-notice">{notice}</div>}

      <footer className="pop-actions">
        <button
          className="pop-btn primary"
          disabled={busy || accounts.length === 0}
          onClick={() => void runCheckin()}
        >
          {busy ? "签到中…" : "立即签到"}
        </button>
        <button className="pop-btn" onClick={() => void api.popoverOpenMain()}>
          主窗口
        </button>
        <button className="pop-btn ghost" onClick={() => void api.quitApp()}>
          退出
        </button>
      </footer>
    </div>
  );
}

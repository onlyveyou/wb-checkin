import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../api";
import type { FerryModel, FerryStatus, FerryVerdict } from "../types";

const POLL_MS = 2500;
const LOG_POLL_MS = 6000;

const PHASE_TEXT: Record<string, string> = {
  stopped: "未运行",
  starting: "正在启动",
  reading: "正在读取模型目录",
  ready: "运行中",
  recovering: "正在恢复运行时",
  error: "启动失败",
  unreachable: "转发口无响应",
};

const PHASE_TONE: Record<string, "ok" | "warn" | "danger" | "plain"> = {
  ready: "ok",
  starting: "warn",
  reading: "warn",
  recovering: "warn",
  stopped: "plain",
  error: "danger",
  unreachable: "danger",
};

/** 失败原因的中文口径。上游的原文照抄，别自作主张改写——用户要能对得上官方返回。 */
function verdictText(verdict?: FerryVerdict): { text: string; tone: "ok" | "warn" | "danger" | "plain" } {
  if (!verdict) {
    return { text: "未检测", tone: "plain" };
  }
  if (verdict.ok) {
    return verdict.chatOnly
      ? { text: "可用 · 仅对话", tone: "warn" }
      : { text: "可用", tone: "ok" };
  }
  const message = verdict.error ?? "";
  if (/not available in your country|country/i.test(message)) {
    return { text: "地区限制", tone: "danger" };
  }
  // 已下线在状态码上是 410，模型目录给的分类只有笼统的 error，按文案认一下。
  if (/deprecat|has been retired|no longer available/i.test(message)) {
    return { text: "已下线", tone: "danger" };
  }
  switch (verdict.category) {
    case "access":
      return { text: "无权限", tone: "danger" };
    case "quota":
      return { text: "额度用尽", tone: "danger" };
    // 出口 IP 被免费池临时限流：不是模型的问题，会自己恢复，所以用 warn 而不是 danger。
    case "throttled":
      return { text: "IP 受限", tone: "warn" };
    case "rate_limit":
      return { text: "限流", tone: "warn" };
    case "timeout":
      return { text: "超时", tone: "warn" };
    case "deprecated":
      return { text: "已下线", tone: "danger" };
    default:
      return { text: "不可用", tone: "danger" };
  }
}

function formatDuration(ms?: number): string {
  if (!ms || !Number.isFinite(ms)) {
    return "—";
  }
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
}

function formatContext(tokens?: number): string {
  if (!tokens) {
    return "—";
  }
  if (tokens >= 1_000_000) {
    return `${Math.round(tokens / 100_000) / 10}M`;
  }
  return `${Math.round(tokens / 1000)}K`;
}

export default function FerryPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [status, setStatus] = useState<FerryStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const activeRef = useRef(active);
  activeRef.current = active;

  const pull = useCallback(async () => {
    try {
      setStatus(await api.ferryStatus());
    } catch (error) {
      onError(String(error));
    }
  }, [onError]);

  useEffect(() => {
    if (!active) {
      return;
    }
    pull();
    const timer = window.setInterval(() => {
      if (activeRef.current) {
        pull();
      }
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, pull]);

  useEffect(() => {
    if (!active) {
      return;
    }
    let alive = true;
    const load = async () => {
      try {
        const tail = await api.ferryLogs(80);
        if (alive) {
          setLogs(tail.lines ?? []);
        }
      } catch {
        // 日志读不到不是错误路径：面板其余部分照常工作。
      }
    };
    load();
    const timer = window.setInterval(load, LOG_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [active]);

  const run = async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    try {
      await action();
      await pull();
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  };

  const running = status?.running === true;
  const phase = status?.phase ?? "stopped";
  const models: FerryModel[] = status?.models ?? [];
  const results = status?.modelResults ?? {};
  const excluded = new Set(status?.excludedModels ?? []);
  const available = new Set(status?.availableModels ?? []);
  const probing = status?.probe?.running === true;
  const geoBlocked = models.filter((model) => /country/i.test(results[model.id]?.error ?? "")).length;
  const proxy = status?.systemProxy;
  const proxyReady = proxy?.configured === true;

  return (
    <>
      <div className="ferry-head">
        <div>
          <div className="ferry-title">
            <span className={`dot ${PHASE_TONE[phase] ?? "plain"}`} />
            {PHASE_TEXT[phase] ?? phase}
            {probing && status?.probe?.pending?.length ? (
              <span className="ferry-sub">检测中 · 还剩 {status.probe.pending.length}</span>
            ) : null}
          </div>
          <div className="ferry-sub">
            {running
              ? `${models.length} 个免费模型 · ${available.size} 个可用 · ${status?.endpoint ?? ""}`
              : status?.message && status.message !== "未在运行"
                ? status.message
                : "启动后只监听 127.0.0.1；不启动时不占用任何端口"}
          </div>
        </div>
        <div className="ferry-actions">
          {running ? (
            <button
              className="btn sm"
              disabled={busy !== null}
              onClick={() => run("stop", api.ferryStop)}
            >
              {busy === "stop" && <span className="spin" />}
              停止
            </button>
          ) : (
            <button
              className="btn sm primary"
              disabled={busy !== null}
              onClick={() => run("start", api.ferryStart)}
            >
              {busy === "start" && <span className="spin" />}
              {busy === "start" ? "启动中" : "启动"}
            </button>
          )}
          <button
            className="btn sm"
            disabled={!running || probing || busy !== null}
            onClick={() => run("probe", () => api.ferryProbe())}
            title="强制重测全部模型：忽略判定缓存"
          >
            {probing && <span className="spin" />}
            {probing ? "检测中" : "检测全部"}
          </button>
          <button
            className="btn sm"
            disabled={!running || busy !== null || available.size === 0}
            onClick={() => run("import", api.ferryImport)}
            title="把可用模型写进 WorkBuddy 的 models.json"
          >
            {busy === "import" && <span className="spin" />}
            导入 WorkBuddy
          </button>
        </div>
      </div>

      {!running && (
        <div className="panel settings">
          <div className="row">
            <div>
              <div className="row-label">这是什么</div>
              <div className="row-hint">
                在内置的隔离运行时里调 OpenCode 的免费模型，再以 OpenAI 兼容接口暴露给
                WorkBuddy。启动后只监听 127.0.0.1，访问口令一次一发，浏览器来源一律拒绝。
              </div>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">运行时依赖</div>
              <div className="row-hint">
                需要一个 Node.js 22 及以上的运行时。没有找到时启动会直接报错，不会留下半死的进程。
              </div>
              <div className="path">{status?.nodePath || "未找到 Node.js 运行时"}</div>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">数据目录</div>
              <div className="path">{status?.dataDir ?? "—"}</div>
            </div>
          </div>
        </div>
      )}

      {running && (
        <>
          <div className="section-head">
            <h2>模型</h2>
            <button
              className="btn sm"
              disabled={busy !== null || probing}
              onClick={() => run("refresh", api.ferryRefresh)}
              title="重读 OpenCode 的模型目录"
            >
              {busy === "refresh" && <span className="spin" />}
              刷新目录
            </button>
          </div>
          <div className="panel">
            {models.length === 0 ? (
              <div className="empty">
                <div className="hint">正在读取模型目录…</div>
              </div>
            ) : (
              <div className="ferry-list">
                <div className="ferry-row head">
                  <span className="who">模型</span>
                  <span className="num">上下文</span>
                  <span className="num">耗时</span>
                  <span className="state">状态</span>
                  <span className="switch-cell">导入</span>
                </div>
                {models.map((model) => {
                  const verdict = results[model.id];
                  const info = verdictText(verdict);
                  return (
                    <div className="ferry-row" key={model.id}>
                      <span className="who">
                        <span className="ferry-name">{model.name}</span>
                        {!model.toolcall && <span className="ferry-tag">无工具</span>}
                        {model.images && <span className="ferry-tag">图片</span>}
                        {verdict?.source === "cache" && <span className="ferry-tag quiet">缓存</span>}
                      </span>
                      <span className="num">{formatContext(model.context)}</span>
                      <span className="num">{formatDuration(verdict?.durationMs)}</span>
                      <span className={`state ${info.tone}`} title={verdict?.error ?? ""}>
                        {info.text}
                      </span>
                      <span className="switch-cell">
                        <button
                          className="switch tiny"
                          data-on={!excluded.has(model.id)}
                          aria-label={`切换 ${model.name}`}
                          disabled={busy !== null}
                          onClick={() =>
                            run(`sel:${model.id}`, () =>
                              api.ferrySelection(model.id, excluded.has(model.id)),
                            )
                          }
                        />
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {geoBlocked > 0 && (
            <div className="ferry-note">
              {geoBlocked} 个模型返回了「该地区不可用」。这是上游按出口 IP 判定的策略，
              换客户端或改请求头都不会改变结论 —— 关掉它们的导入，把请求落到能用的模型上。
            </div>
          )}

          <div className="section-head">
            <h2>接入状态</h2>
          </div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">WorkBuddy 配置</div>
                <div className="row-hint">
                  {status?.sync?.error
                    ? `上次写入失败：${status.sync.error}`
                    : `已导入 ${status?.sync?.count ?? 0} 条。只增删本工具标记过的条目，你手写的模型一条都不动。`}
                </div>
                <div className="path">{status?.modelsFile ?? "—"}</div>
              </div>
            </div>
            <div className="row">
              <div>
                <div className="row-label">系统代理</div>
                <div className="row-hint">
                  {proxyReady
                    ? `跟随 macOS 的手动 HTTPS 代理（${proxy?.address ?? "—"}）。只改本模块的出口，不动系统设置。`
                    : `${proxy?.detail ?? "没有检测到可跟随的系统代理"}。这个开关只负责「跟随」，不会替你配代理。`}
                </div>
                <div className="path">
                  它不改变上游对出口 IP 的地区与频次判定：被 IP 挡住时，开关开与关都不影响结果。
                </div>
              </div>
              <button
                className="switch"
                data-on={status?.useSystemProxy === true}
                aria-label="切换系统代理"
                disabled={busy !== null || (!proxyReady && status?.useSystemProxy !== true)}
                onClick={() =>
                  run("proxy", () => api.ferrySetProxy(status?.useSystemProxy !== true))
                }
              />
            </div>
            <div className="row">
              <div>
                <div className="row-label">运行时版本</div>
                <div className="row-hint">
                  内置 OpenCode {status?.opencodeVersion ?? "—"} · 转发口 {status?.port ?? "—"}
                </div>
              </div>
            </div>
          </div>
        </>
      )}

      <div className="section-head">
        <h2>运行日志</h2>
        <span className="sub">{logs.length} 行</span>
      </div>
      <div className="panel logs">
        {logs.length === 0 ? (
          <div className="empty">
            <div className="hint">{running ? "还没有日志" : "启动后这里会显示摆渡与运行时的日志"}</div>
          </div>
        ) : (
          logs.map((line, index) => (
            <div className="log raw" key={`${index}-${line.slice(0, 16)}`}>
              {line}
            </div>
          ))
        )}
      </div>
    </>
  );
}

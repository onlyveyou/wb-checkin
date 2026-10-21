import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../api";
import type {
  AppConfig,
  LocalAiConfig,
  LocalAiDetect,
  LocalAiState,
  LocalAiStatus,
  LocalAiTemps,
} from "../types";

const POLL_MS = 2000;
const LOG_LINES = 150;

const STATE_TEXT: Record<LocalAiState, string> = {
  ready: "运行中",
  cold: "空闲（按需 · 正常）",
  down: "未响应",
};

const STATE_TONE: Record<LocalAiState, "ok" | "warn" | "danger"> = {
  ready: "ok",
  cold: "warn",
  down: "danger",
};

function pad(value: number): string {
  return `${value}`.padStart(2, "0");
}

/** 只给时刻，日志按天轮转，跨天的日期交给文件名去表达。 */
function formatClock(ms: number): string {
  const date = new Date(ms);
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function formatBytes(size: number): string {
  if (size < 1024) {
    return `${size} B`;
  }
  if (size < 1024 * 1024) {
    return `${(size / 1024).toFixed(1)} KB`;
  }
  return `${(size / 1024 / 1024).toFixed(1)} MB`;
}

function formatTemp(value: number | null | undefined): string {
  return value == null ? "--" : `${value.toFixed(1)}°C`;
}

/** 温度分档：<60 正常 / 60–75 偏热 / >75 高温。读不到时不给判定，免得误报。 */
function tempTone(value: number | null | undefined): "ok" | "warn" | "danger" | "plain" {
  if (value == null) {
    return "plain";
  }
  if (value < 60) {
    return "ok";
  }
  if (value < 75) {
    return "warn";
  }
  return "danger";
}

function TempsCard({ temps }: { temps?: LocalAiTemps }) {
  const missing = !temps?.smctemp;
  return (
    <div className="ai-card">
      <div className="ai-card-title">电脑温度</div>
      <div className="ai-card-values">
        <span className={`pill ${tempTone(temps?.cpu)}`}>CPU {formatTemp(temps?.cpu)}</span>
        <span className={`pill ${tempTone(temps?.gpu)}`}>GPU {formatTemp(temps?.gpu)}</span>
      </div>
      <div className="ai-card-desc">
        {missing
          ? "未找到 smctemp，温度不可读。可在下方「监控设置」里指定它的路径。"
          : "smctemp 直读 SMC · <60 正常 / 60–75 偏热 / >75 高温"}
      </div>
    </div>
  );
}

interface Props {
  config: LocalAiConfig;
  /** 当前标签页是否可见。不可见时不轮询，避免后台空转。 */
  active: boolean;
  onConfig: (next: AppConfig) => void;
  onError: (message: string) => void;
}

export default function LocalAiPanel({ config, active, onConfig, onError }: Props) {
  const [status, setStatus] = useState<LocalAiStatus | null>(null);
  const [detect, setDetect] = useState<LocalAiDetect | null>(null);
  const [detecting, setDetecting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [logFile, setLogFile] = useState<string | null>(null);
  const [logText, setLogText] = useState("");
  const [logError, setLogError] = useState<string | null>(null);
  const [draftLogDir, setDraftLogDir] = useState(config.log_dir);
  const [draftSmctemp, setDraftSmctemp] = useState(config.smctemp_path);

  const logRef = useRef<HTMLPreElement | null>(null);
  // 用户往上翻看历史时不要把他拽回底部
  const stickyRef = useRef(true);
  const logTextRef = useRef("");
  const autoDetectRef = useRef(false);

  const enabled = config.enabled;

  useEffect(() => {
    setDraftLogDir(config.log_dir);
  }, [config.log_dir]);

  useEffect(() => {
    setDraftSmctemp(config.smctemp_path);
  }, [config.smctemp_path]);

  // 关闭时清空上一次的残留，下次打开从干净状态开始
  useEffect(() => {
    if (!enabled) {
      setStatus(null);
      setLogFile(null);
      setLogText("");
      setLogError(null);
      logTextRef.current = "";
    }
  }, [enabled]);

  const runDetect = useCallback(async () => {
    setDetecting(true);
    try {
      setDetect(await api.localAiDetect());
    } catch (error) {
      onError(String(error));
    } finally {
      setDetecting(false);
    }
  }, [onError]);

  // 第一次切到本标签页时自动探一次本机部署情况，用户不用先点按钮才知道该不该打开
  useEffect(() => {
    if (enabled || !active || autoDetectRef.current) {
      return;
    }
    autoDetectRef.current = true;
    void runDetect();
  }, [enabled, active, runDetect]);

  // 状态轮询
  useEffect(() => {
    if (!enabled || !active) {
      return;
    }
    let cancelled = false;
    const run = async () => {
      if (document.hidden) {
        return;
      }
      try {
        const next = await api.localAiStatus();
        if (!cancelled) {
          setStatus(next);
        }
      } catch {
        // 探活偶发失败不值得弹横幅，保留上一帧，下一轮自愈
      }
    };
    void run();
    const timer = window.setInterval(run, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, active]);

  // 选定日志文件：优先沿用当前选择，否则默认挑文本后端的主日志
  useEffect(() => {
    if (!enabled) {
      return;
    }
    const files = status?.logs.files ?? [];
    if (files.length === 0) {
      setLogFile(null);
      return;
    }
    setLogFile((previous) => {
      if (previous && files.some((file) => file.name === previous)) {
        return previous;
      }
      const preferred = files.find((file) => file.name === "qwen_8081.log");
      return (preferred ?? files[0]).name;
    });
  }, [enabled, status]);

  // 日志跟随
  useEffect(() => {
    if (!enabled || !active || !logFile) {
      return;
    }
    let cancelled = false;
    const run = async () => {
      if (document.hidden) {
        return;
      }
      try {
        const tail = await api.localAiTail(logFile, LOG_LINES);
        if (cancelled) {
          return;
        }
        if (tail.error) {
          setLogError(tail.error);
          logTextRef.current = "";
          setLogText("");
          return;
        }
        const text = (tail.lines ?? []).join("\n");
        // 内容没变就不 setState，否则每 2 秒一次空渲染会打断选中与滚动
        if (text === logTextRef.current) {
          return;
        }
        logTextRef.current = text;
        setLogError(null);
        setLogText(text);
      } catch {
        // 同上：轮询失败静默，下一轮重试
      }
    };
    void run();
    const timer = window.setInterval(run, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, active, logFile]);

  useEffect(() => {
    const node = logRef.current;
    if (node && stickyRef.current) {
      node.scrollTop = node.scrollHeight;
    }
  }, [logText]);

  const save = useCallback(
    async (nextEnabled: boolean, nextLogDir: string, nextSmctemp: string) => {
      setSaving(true);
      try {
        onConfig(await api.setLocalAiConfig(nextEnabled, nextLogDir, nextSmctemp));
      } catch (error) {
        onError(String(error));
      } finally {
        setSaving(false);
      }
    },
    [onConfig, onError],
  );

  const commitSettings = () => {
    if (draftLogDir === config.log_dir && draftSmctemp === config.smctemp_path) {
      return;
    }
    void save(enabled, draftLogDir, draftSmctemp);
  };

  const toggle = () => {
    void save(!enabled, draftLogDir, draftSmctemp);
  };

  const tasks = status?.tasks ?? [];
  const files = status?.logs.files ?? [];

  if (!enabled) {
    const summary = (() => {
      if (detecting) {
        return { tone: "warn" as const, text: "正在检测本机部署情况…" };
      }
      if (!detect) {
        return null;
      }
      const listening = detect.ports.filter((item) => item.listening);
      if (detect.deployed) {
        const parts = [
          listening.length > 0
            ? `${listening.map((item) => `${item.port}`).join(" / ")} 端口在监听`
            : "没有端口在监听",
          detect.logDirExists
            ? `日志目录存在，共 ${detect.logFiles} 个文件`
            : `日志目录不存在（${detect.logDir}）`,
          detect.smctemp ? "已找到 smctemp" : "未找到 smctemp",
        ];
        return { tone: "ok" as const, text: `检测到本机部署过本地 AI：${parts.join("；")}。` };
      }
      const missing = [
        detect.ports.map((item) => item.port).join(" / ") + " 端口均未监听",
        `日志目录不存在（${detect.logDir}）`,
      ];
      return {
        tone: "warn" as const,
        text: `本机未检测到本地 AI 部署：${missing.join("；")}。这台电脑保持关闭即可。`,
      };
    })();

    return (
      <div className="panel ai-intro">
        <div className="ai-intro-title">本地 AI 监控未启用</div>
        <div className="ai-intro-hint">
          启用后可查看本机 llama.cpp 推理后端与按需网关的实时状态、CPU / GPU 温度和运行日志。
          这个功能是为部署了本地 AI 的机器准备的：在没有部署的电脑上保持关闭即可，
          关闭状态下后端不做任何探测。
        </div>

        {summary && <div className={`note ${summary.tone === "ok" ? "ok" : "warn"}`}>{summary.text}</div>}

        <div className="ai-intro-actions">
          <button className="btn" onClick={runDetect} disabled={detecting || saving}>
            {detecting && <span className="spin" />}
            {detecting ? "检测中" : "重新检测"}
          </button>
          <button className="btn primary" onClick={toggle} disabled={saving || detecting}>
            {saving && <span className="spin" />}
            启用监控
          </button>
        </div>
      </div>
    );
  }

  return (
    <>
      {status && status.notes.length > 0 && (
        <div className="note warn">{status.notes.join("；")}</div>
      )}

      <div className="ai-cards">
        <TempsCard temps={status?.temps} />
        {(status?.services ?? []).map((service) => (
          <div className="ai-card" key={service.key}>
            <div className="ai-card-title">
              <span className={`ai-state-dot ${service.state}`} />
              {service.name}
            </div>
            <div className="ai-card-values">
              <span className={`pill ${STATE_TONE[service.state]}`}>
                {STATE_TEXT[service.state]}
              </span>
              <span className="pill">:{service.port}</span>
              {service.tcpOnly && <span className="pill">仅 TCP 探测</span>}
              {service.tasks.length > 0 && (
                <span className="pill ok">{service.tasks.length} 个任务</span>
              )}
            </div>
            <div className="ai-card-desc">{service.model ?? service.desc}</div>
          </div>
        ))}
      </div>

      <div className="section-head">
        <h2>正在处理的推理任务</h2>
        {status && <span className="sub">刷新于 {formatClock(status.time)}</span>}
      </div>
      <div className="panel">
        {tasks.length === 0 ? (
          <div className="empty">
            <div className="hint">当前没有正在进行的推理任务</div>
          </div>
        ) : (
          <table className="ai-table">
            <thead>
              <tr>
                <th>服务</th>
                <th>任务</th>
                <th>prompt tokens</th>
                <th>进度</th>
                <th>命中缓存</th>
                <th>已生成</th>
              </tr>
            </thead>
            <tbody>
              {tasks.map((task, index) => {
                const percent = task.promptTokens
                  ? Math.min(100, Math.round((100 * (task.processed ?? 0)) / task.promptTokens))
                  : 0;
                return (
                  <tr key={`${task.service}-${task.task}-${index}`}>
                    <td>{task.service ?? "—"}</td>
                    <td>#{task.task ?? "—"}</td>
                    <td>{task.promptTokens ?? "—"}</td>
                    <td>
                      <div className="ai-bar">
                        <i style={{ width: `${percent}%` }} />
                      </div>
                    </td>
                    <td>{task.cache ?? "—"}</td>
                    <td>{task.predicted ?? "—"}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      <div className="section-head">
        <h2>运行日志</h2>
        <span className="sub">{status?.logs.dir ?? "—"}</span>
      </div>
      <div className="panel">
        {files.length === 0 ? (
          <div className="empty">
            <div className="hint">
              {status?.logs.dirExists
                ? "日志目录里没有 .log / .txt 文件"
                : "日志目录不存在，可在下方「监控设置」里改路径"}
            </div>
          </div>
        ) : (
          <>
            <div className="ai-logtabs">
              {files.map((file) => (
                <button
                  key={file.name}
                  className={`ai-tab${file.name === logFile ? " on" : ""}`}
                  title={`${formatBytes(file.size)} · 更新于 ${formatClock(file.mtime)}`}
                  onClick={() => {
                    stickyRef.current = true;
                    setLogFile(file.name);
                  }}
                >
                  {file.name}
                </button>
              ))}
            </div>
            <pre
              className="ai-log"
              ref={logRef}
              onScroll={(event) => {
                const node = event.currentTarget;
                stickyRef.current =
                  node.scrollTop + node.clientHeight >= node.scrollHeight - 30;
              }}
            >
              {logError ? `读取失败：${logError}` : logText || "（暂无内容）"}
            </pre>
          </>
        )}
      </div>

      <div className="section-head">
        <h2>监控设置</h2>
        <span className="sub">留空表示使用约定位置</span>
      </div>
      <div className="panel settings">
        <div className="row">
          <div>
            <div className="row-label">日志目录</div>
            <div className="row-hint">
              约定位置是 ~/llama.cpp/logs。只读取该目录下的 .log / .txt 文件。
            </div>
          </div>
          <input
            className="select ai-input"
            placeholder="~/llama.cpp/logs"
            value={draftLogDir}
            onChange={(event) => setDraftLogDir(event.target.value)}
            onBlur={commitSettings}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                commitSettings();
                event.currentTarget.blur();
              }
            }}
          />
        </div>

        <div className="row">
          <div>
            <div className="row-label">smctemp 路径</div>
            <div className="row-hint">
              读取 CPU / GPU 温度的小工具。找不到时温度会显示为「--」，其余功能不受影响。
            </div>
          </div>
          <input
            className="select ai-input"
            placeholder="自动查找"
            value={draftSmctemp}
            onChange={(event) => setDraftSmctemp(event.target.value)}
            onBlur={commitSettings}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                commitSettings();
                event.currentTarget.blur();
              }
            }}
          />
        </div>

        <div className="row">
          <div>
            <div className="row-label">关闭监控</div>
            <div className="row-hint">
              关闭后不再探活、不读日志。配置会保留，下次打开时沿用。
            </div>
          </div>
          <button
            className="switch"
            data-on={true}
            aria-label="关闭本地 AI 监控"
            disabled={saving}
            onClick={toggle}
          />
        </div>
      </div>
    </>
  );
}

import { useEffect, useMemo, useRef, useState } from "react";
import type { InjectEntry } from "../types";

/**
 * 「哪些模型写进 WorkBuddy」的统一勾选器，三个平台面板共用。
 *
 * 两个维度别混：
 *   - 轮换（rotate）：WorkBuddy 里选「自动轮换」时，网关内部依次试哪些模型
 *   - 单独（inject）：这个模型要不要作为独立条目出现在 WorkBuddy 的模型列表里
 *
 * 交互上有一条硬规则：**勾选即写入**。以前的版本改完勾选还要滚到最底部点「保存并写入」，
 * 结果就是「我点了单独，模型列表里还是没有」—— 勾选和落盘之间不该有第二个动作。
 */

export interface ModelRow {
  id: string;
  label: string;
  /** 行下方的补充说明（额度、重置时间等） */
  hint?: string;
  /** 0~1 剩余比例，有值时画一条进度条 */
  ratio?: number | null;
  danger?: boolean;
}

/** 连续点几个勾只写一次盘：等手停 350ms 再提交 */
const COMMIT_DELAY = 350;

function ratioColor(ratio: number | null | undefined, danger?: boolean) {
  if (danger) return "var(--red)";
  if (typeof ratio !== "number") return "var(--muted)";
  if (ratio <= 0.1) return "var(--red)";
  if (ratio <= 0.3) return "var(--warn)";
  return "var(--green)";
}

export default function ModelInjectPicker({
  provider,
  rows,
  entry,
  busy,
  autoHint,
  onSave,
  children,
}: {
  provider: "orcaterm" | "doubao" | "antigravity";
  rows: ModelRow[];
  entry: InjectEntry;
  busy?: boolean;
  autoHint?: string;
  onSave: (next: InjectEntry) => void | Promise<void>;
  children?: React.ReactNode;
}) {
  const [enabled, setEnabled] = useState(entry.enabled !== false);
  const [rotate, setRotate] = useState<string[]>(entry.rotate || []);
  const [inject, setInject] = useState<string[]>(entry.inject?.length ? entry.inject : ["auto"]);
  /** 让「点了到底有没生效」一眼可见：写入中 / 已写入 / 失败原因 */
  const [status, setStatus] = useState<"idle" | "saving" | "done" | "error">("idle");
  const [statusText, setStatusText] = useState("");

  const timer = useRef<number | null>(null);
  const pending = useRef<InjectEntry | null>(null);
  /** 本地刚改过的时间戳：面板每 5s 轮询会把旧值推回来，别把用户刚点的勾覆盖掉 */
  const localUntil = useRef(0);

  const key = `${entry.enabled}|${(entry.rotate || []).join(",")}|${(entry.inject || []).join(",")}`;
  useEffect(() => {
    if (Date.now() < localUntil.current) return;
    setEnabled(entry.enabled !== false);
    setRotate(entry.rotate || []);
    setInject(entry.inject?.length ? entry.inject : ["auto"]);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  useEffect(() => () => {
    if (timer.current) window.clearTimeout(timer.current);
  }, []);

  // rotate 里可能有当前清单没有的旧模型：也要列出来，否则保存一次就被静默删掉
  const allRows = useMemo(() => {
    const known = new Set(rows.map((r) => r.id));
    const extra: ModelRow[] = rotate.filter((id) => !known.has(id)).map((id) => ({ id, label: id }));
    return [...rows, ...extra];
  }, [rows, rotate]);

  const toggle = (list: string[], id: string) =>
    list.includes(id) ? list.filter((x) => x !== id) : [...list, id];

  /** 把当前勾选落盘（写 config + 写 models.json）；失败必须说出来，不能默默吞掉 */
  async function flush(next: InjectEntry) {
    setStatus("saving");
    setStatusText("写入中…");
    try {
      await onSave(next);
      setStatus("done");
      setStatusText("已写入");
      window.setTimeout(() => setStatus((s) => (s === "done" ? "idle" : s)), 3000);
    } catch (error) {
      setStatus("error");
      setStatusText(String(error));
    }
  }

  /** 勾选变化 → 先更新界面，再合并 350ms 内所有改动写一次 */
  function apply(next: InjectEntry) {
    setEnabled(next.enabled);
    setRotate(next.rotate);
    setInject(next.inject);
    localUntil.current = Date.now() + 2500;
    pending.current = next;
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      const payload = pending.current;
      pending.current = null;
      if (payload) void flush(payload);
    }, COMMIT_DELAY);
  }

  /** 落盘前补两条常识：rotate 至少留一个，inject 永远非空 */
  function normalize(next: { enabled?: boolean; rotate?: string[]; inject?: string[] }): InjectEntry {
    const nextRotate = allRows.map((r) => r.id).filter((id) => (next.rotate ?? rotate).includes(id));
    const nextInject = (next.inject ?? inject).filter(Boolean);
    return {
      enabled: next.enabled ?? enabled,
      rotate: nextRotate.length ? nextRotate : ([allRows[0]?.id].filter(Boolean) as string[]),
      inject: nextInject.length ? nextInject : ["auto"],
    };
  }

  return (
    <>
      <div className="row">
        <div>
          <div className="row-label">启用这个平台</div>
          <div className="row-hint">
            开启后按下面的勾选写入 WorkBuddy 的模型列表；关掉会立即把该平台的模型
            从 <code>~/.workbuddy/models.json</code> 清掉，列表立刻变干净。
          </div>
        </div>
        <button
          className="switch"
          data-on={enabled}
          aria-label={`切换 ${provider} 启用状态`}
          onClick={() => apply(normalize({ enabled: !enabled }))}
        />
      </div>

      <div className="row">
        <div style={{ flex: 1 }}>
          <div className="row-label">模型清单</div>
          <div className="row-hint">
            「轮换」= 自动轮换时依次试哪些；「单独」= 作为独立模型出现在 WorkBuddy 里。
            <b>点了就写入，不用再找保存按钮。</b>
            WorkBuddy 只在启动时读一次模型列表，写入后要重启它才看得到。
            （当前 {allRows.length} 个可选，默认只写自动轮换）
          </div>
        </div>
      </div>

      <div className="row">
        <div style={{ flex: 1 }}>
          <div className="row-label">自动轮换</div>
          <div className="row-hint">{autoHint || "一个模型失败/限流时自动换下一个"}</div>
        </div>
        <button
          className={`pill${inject.includes("auto") ? " on" : ""}`}
          style={{ cursor: "pointer" }}
          onClick={() => apply(normalize({ inject: toggle(inject, "auto") }))}
        >
          单独
        </button>
      </div>

      {allRows.map((row) => {
        const inRotate = rotate.includes(row.id);
        const inInject = inject.includes(row.id);
        return (
          <div className="row" key={row.id}>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="row-label" style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {row.label}
                </span>
                {typeof row.ratio === "number" && (
                  <span style={{ fontSize: 11, color: ratioColor(row.ratio, row.danger), flex: "none" }}>
                    {Math.round(row.ratio * 100)}%
                  </span>
                )}
              </div>
              {row.hint && <div className="row-hint">{row.hint}</div>}
              {typeof row.ratio === "number" && (
                <div style={{ height: 4, borderRadius: 2, marginTop: 5, background: "rgba(127,127,127,.22)", overflow: "hidden" }}>
                  <div
                    style={{
                      height: "100%",
                      width: `${Math.min(100, Math.max(0, row.ratio * 100))}%`,
                      background: ratioColor(row.ratio, row.danger),
                    }}
                  />
                </div>
              )}
            </div>
            <div style={{ display: "flex", gap: 6, flex: "none" }}>
              <button
                className={`pill${inRotate ? " on" : ""}`}
                style={{ cursor: "pointer" }}
                onClick={() => apply(normalize({ rotate: toggle(rotate, row.id) }))}
              >
                轮换
              </button>
              <button
                className={`pill${inInject ? " on" : ""}`}
                style={{ cursor: "pointer" }}
                onClick={() => apply(normalize({ inject: toggle(inject, row.id) }))}
              >
                单独
              </button>
            </div>
          </div>
        );
      })}

      {children}

      <div className="row">
        <div className="row-hint" style={{ flex: 1 }}>
          轮换 {allRows.filter((r) => rotate.includes(r.id)).length} 个 ·
          单独写入 {inject.length} 个（含自动）
          {status !== "idle" && (
            <>
              {" · "}
              <span style={{ color: status === "error" ? "var(--red)" : "var(--accent)" }}>
                {statusText}
              </span>
            </>
          )}
          {status === "done" && "（重启 WorkBuddy 后才会出现在模型列表）"}
        </div>
        <button
          className="btn sm"
          disabled={busy || status === "saving" || !allRows.length}
          onClick={() => void flush(normalize({}))}
        >
          {status === "saving" ? "写入中…" : "重新写入"}
        </button>
      </div>
    </>
  );
}

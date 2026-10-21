import { useEffect, useRef, useState } from "react";
import * as api from "../api";

interface Props {
  onDone: (text: string) => void;
}

type Phase = "idle" | "waiting" | "failed";

const POLL_INTERVAL_MS = 2000;

function formatRemaining(seconds: number): string {
  const safe = Math.max(0, seconds);
  const minutes = Math.floor(safe / 60);
  const rest = safe % 60;
  return `${minutes}:${`${rest}`.padStart(2, "0")}`;
}

/**
 * 独立授权登录面板。
 *
 * 与「从本机导入」的关键差别：不读官方客户端的登录态文件，每个账号各走一次官方
 * 授权，所以不受「客户端同一时刻只能激活一个账号」的限制。
 */
export default function OAuthLogin({ onDone }: Props) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [loginId, setLoginId] = useState<string | null>(null);
  const [authUrl, setAuthUrl] = useState("");
  const [remaining, setRemaining] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  // 轮询循环只依赖 loginId。用 ref 取最新回调，避免父组件重渲染打断循环。
  const doneRef = useRef(onDone);
  doneRef.current = onDone;

  const activeRef = useRef<string | null>(null);
  activeRef.current = loginId;

  useEffect(() => {
    if (!loginId) return;
    let stopped = false;
    let timer: number | undefined;
    const sessionId = loginId;

    const tick = async () => {
      if (stopped) return;
      try {
        const result = await api.oauthLoginPoll(sessionId);
        if (stopped) return;
        setRemaining(result.expiresIn ?? 0);
        setNotice(result.notice ?? null);

        if (result.done) {
          if (result.status === "ok") {
            const account = result.account;
            const label = account?.nickname || account?.email || account?.uid || "新账号";
            doneRef.current(`已通过独立授权登录「${label}」。`);
          } else {
            setPhase("failed");
            setError(result.error || "登录失败");
            setLoginId(null);
          }
          return;
        }
      } catch (caught) {
        if (stopped) return;
        // 单次请求失败不终止轮询，后端有自己的超时兜底
        setNotice(String(caught));
      }
      timer = window.setTimeout(tick, POLL_INTERVAL_MS);
    };

    tick();
    return () => {
      stopped = true;
      if (timer) window.clearTimeout(timer);
    };
  }, [loginId]);

  // 离开面板时通知后端丢弃会话，避免留下无主的 state
  useEffect(
    () => () => {
      const sessionId = activeRef.current;
      if (sessionId) api.oauthLoginCancel(sessionId).catch(() => undefined);
    },
    [],
  );

  const start = async () => {
    setStarting(true);
    setError(null);
    setNotice(null);
    try {
      const session = await api.oauthLoginStart();
      setAuthUrl(session.authUrl);
      setRemaining(session.expiresIn);
      setPhase("waiting");
      setLoginId(session.loginId);
      try {
        await api.oauthLoginOpen(session.loginId);
      } catch (caught) {
        setNotice(`未能自动打开浏览器，请手动打开下方链接。（${String(caught)}）`);
      }
    } catch (caught) {
      setPhase("failed");
      setError(String(caught));
    } finally {
      setStarting(false);
    }
  };

  const reopen = async () => {
    if (!loginId) return;
    try {
      await api.oauthLoginOpen(loginId);
      setNotice(null);
    } catch (caught) {
      setNotice(String(caught));
    }
  };

  if (phase === "idle") {
    return (
      <>
        <div className="field">
          <div className="hint" style={{ lineHeight: 1.75 }}>
            每个账号各走一次官方授权，彼此独立，所以不受「客户端同一时刻只能激活一个账号」的限制，
            可以连续登录多个账号。登录在系统浏览器里完成，本工具不接触你的密码。
          </div>
        </div>
        <div>
          <button className="btn primary" onClick={start} disabled={starting}>
            {starting && <span className="spin" />}
            {starting ? "正在申请" : "开始登录"}
          </button>
        </div>
      </>
    );
  }

  if (phase === "failed") {
    return (
      <>
        <div className="note danger">{error}</div>
        <div>
          <button className="btn" onClick={start} disabled={starting}>
            重新开始
          </button>
        </div>
      </>
    );
  }

  return (
    <>
      <div className="field">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span className="spin" style={{ color: "var(--accent)" }} />
          <span>等待浏览器完成登录</span>
          <span
            style={{
              marginLeft: "auto",
              color: "var(--faint)",
              fontVariantNumeric: "tabular-nums",
            }}
          >
            {formatRemaining(remaining)}
          </span>
        </div>
        <div className="hint">
          已在浏览器中打开官方登录页。若没弹出，点下方「重新打开登录页」。
        </div>
      </div>

      <div className="field">
        <label>登录链接</label>
        <div className="path" style={{ userSelect: "all", cursor: "text" }}>
          {authUrl}
        </div>
      </div>

      {notice && <div className="note warn">{notice}</div>}

      <div>
        <button className="btn sm" onClick={reopen}>
          重新打开登录页
        </button>
      </div>
    </>
  );
}

import { useEffect, useState } from "react";
import * as api from "../api";
import type { Account, ProfileSnapshot, SwitchPreflight } from "../types";

interface Props {
  account: Account;
  /** 当前客户端登录的账号名，用于说明「登录身份会保持谁」。 */
  currentName: string | null;
  onClose: () => void;
  onDone: (text: string) => void;
}

function name(account: Account): string {
  return account.nickname || account.email || account.uid || "未命名账号";
}

function formatBytes(bytes: number): string {
  if (!bytes) {
    return "—";
  }
  const mb = bytes / 1024 / 1024;
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${Math.round(mb)} MB`;
}

function formatTime(ms: number | null): string {
  if (!ms) {
    return "—";
  }
  const date = new Date(ms);
  const pad = (value: number) => `${value}`.padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

export default function SwitchDialog({ account, currentName, onClose, onDone }: Props) {
  const [preflight, setPreflight] = useState<SwitchPreflight | null>(null);
  const [snapshot, setSnapshot] = useState<ProfileSnapshot | null>(null);
  const [restart, setRestart] = useState(true);
  const [keepSession, setKeepSession] = useState(true);
  /** 默认只换额度：登录身份不动，这是最常用的场景。 */
  const [creditsOnly, setCreditsOnly] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 签到侧独立持有凭证的账号默认不允许切换：写进客户端会把签到链消耗掉。 */
  const [force, setForce] = useState(false);
  const checkinOwned = account.tokenOwner === "checkin";

  useEffect(() => {
    api
      .switchPreflight()
      .then(setPreflight)
      .catch((cause) => setError(String(cause)));
    api
      .profileList()
      .then((list) => {
        const uid = account.uid;
        setSnapshot(uid ? list.items.find((item) => item.uid === uid) ?? null : null);
      })
      .catch(() => setSnapshot(null));
  }, [account.uid]);

  // 两个硬性前提：客户端装了（要重启它）、登录态目录可写（要改它）
  const blocked =
    preflight !== null && (!preflight.clientInstalled || !preflight.authFileWritable);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api.switchToAccount(account.id, restart, keepSession, creditsOnly, force);
      const parts = creditsOnly
        ? [`已把额度换成「${result.account}」，客户端登录身份仍是 ${currentName ?? "原账号"}`]
        : [`已把「${result.account}」写入客户端登录态`];

      if (!creditsOnly && result.keptSession) {
        if (result.profileRestore?.restored) {
          parts.push(`，并恢复了该账号的本地现场（${(result.profileRestore.layers ?? []).join(" / ")}）`);
        } else if (result.profileRestore?.reason) {
          parts.push(`。注意：${result.profileRestore.reason}`);
        }
        if (result.saveError) {
          parts.push(`。原账号现场未能存档：${result.saveError}`);
        }
      }

      if (result.restarted) {
        parts.push("，已重新启动 WorkBuddy");
      } else if (result.launchError) {
        parts.push(`，但自动启动失败：${result.launchError}`);
      } else if (!restart) {
        parts.push("。请手动重启 WorkBuddy 使登录态生效");
      }
      if (result.refreshed) {
        parts.push("。写入前已刷新凭证");
      } else if (result.refreshError) {
        parts.push(`。注意：刷新凭证失败（${result.refreshError}），写入的是原凭证`);
      }
      onDone(parts.join(""));
    } catch (cause) {
      setError(String(cause));
      setBusy(false);
    }
  };

  return (
    <div className="mask" onClick={busy ? undefined : onClose}>
      <div className="modal" onClick={(event) => event.stopPropagation()}>
        <div className="modal-head">
          <h3>{creditsOnly ? `用「${name(account)}」的积分` : `切换到「${name(account)}」`}</h3>
        </div>

        <div className="modal-body">
          <div className="mode-group">
            <label className={`mode-item${creditsOnly ? " active" : ""}`}>
              <input
                type="radio"
                name="switch-mode"
                checked={creditsOnly}
                disabled={busy}
                onChange={() => setCreditsOnly(true)}
              />
              <div>
                <div className="mode-title">只换额度</div>
                <div className="mode-desc">
                  客户端仍登录 {currentName ?? "当前账号"}，界面、会话、侧边栏都不变，
                  只是模型调用按这个账号的积分结算
                </div>
              </div>
            </label>
            <label className={`mode-item${creditsOnly ? "" : " active"}`}>
              <input
                type="radio"
                name="switch-mode"
                checked={!creditsOnly}
                disabled={busy}
                onChange={() => setCreditsOnly(false)}
              />
              <div>
                <div className="mode-title">整套换号</div>
                <div className="mode-desc">
                  登录身份也换成这个账号，并恢复它自己的本地现场（登录态 / 用户目录 / 浏览器会话 / 侧边栏）
                </div>
              </div>
            </label>
          </div>

          <div className="warn-box">
            <strong>
              {creditsOnly
                ? "这个操作只改登录态文件里的凭证部分。"
                : "这个操作会改动 WorkBuddy 的登录状态。"}
            </strong>
            {restart ? (
              <div>
                执行时会<strong>先关闭再重新启动 WorkBuddy</strong>。
                {creditsOnly
                  ? "正在进行的对话会被打断，重启后会回到原来那条会话。"
                  : "正在进行的对话会被打断，开启「保留会话」后重启会回到原来那条会话。"}
              </div>
            ) : (
              <div>
                本次只写入登录态、<strong>不重启客户端</strong>，需要你之后手动重启 WorkBuddy
                才会生效。
              </div>
            )}
            <div>
              本应用是独立程序，切换过程中<strong>不会被关闭</strong>，会继续完成写入与重启。
            </div>
          </div>

          {preflight === null && !error && <div className="hint">正在检查环境…</div>}

          {checkinOwned && (
            <>
              <div className="note danger">
                该账号的 token 由签到侧独立持有。服务端每刷新一次就换发新的 refresh
                token，写进客户端后客户端刷新一次就会把它轮换掉，<strong>这个账号的自动签到会立即失效</strong>。
                要正常切它，请先在官方客户端登录该账号（登录后的链归客户端持有），或直接确认下面的强制切换。
              </div>
              <label className="toggle-row">
                <input
                  type="checkbox"
                  checked={force}
                  disabled={busy}
                  onChange={(event) => setForce(event.target.checked)}
                />
                <span>强制切换（接受该账号自动签到失效，需要重新授权才能恢复）</span>
              </label>
            </>
          )}

          {preflight && (
            <div className="checklist">
              <CheckLine
                ok={preflight.clientInstalled}
                label="已安装 WorkBuddy 客户端"
                detail={preflight.clientAppPath || "未在 /Applications 找到"}
              />
              <CheckLine
                ok={preflight.authFileWritable}
                label="登录态目录可写"
                detail={
                  preflight.authFileWritable
                    ? preflight.authFilePath
                    : "无写入权限，请到「系统设置 → 隐私与安全性 → App 管理」授权"
                }
              />
              <CheckLine
                ok
                label={preflight.clientRunning ? "WorkBuddy 正在运行（将被重启）" : "WorkBuddy 当前未运行"}
                detail={preflight.clientRunning ? undefined : "切换后会自动打开"}
              />
              <CheckLine ok label="切换前会自动备份当前登录态" detail={preflight.backupDir} />
            </div>
          )}

          {!creditsOnly && (
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={keepSession}
                disabled={busy}
                onChange={(event) => setKeepSession(event.target.checked)}
              />
              <span>保留会话（分层快照：登录态 + 用户目录 + 浏览器会话 + 侧边栏）</span>
            </label>
          )}

          {!creditsOnly && keepSession && (
            <div className="checklist">
              <CheckLine
                ok={snapshot !== null}
                label={snapshot ? `该账号已有现场快照` : "该账号还没有现场快照"}
                detail={
                  snapshot
                    ? `保存于 ${formatTime(snapshot.savedAt)}，会话数据 ${formatBytes(
                        snapshot.sessionBytes,
                      )}，含 ${snapshot.layers.join(" / ")}`
                    : "切换仍会写入登录态，但会话可能中断。先登录该账号，退出客户端后点「保存现场」"
                }
              />
            </div>
          )}

          <label className="toggle-row">
            <input
              type="checkbox"
              checked={restart}
              disabled={busy}
              onChange={(event) => setRestart(event.target.checked)}
            />
            <span>切换后自动重启 WorkBuddy（推荐，否则需手动重启才生效）</span>
          </label>

          {error && <div className="note danger">{error}</div>}
        </div>

        <div className="modal-foot">
          <button className="btn" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            className="btn danger"
            onClick={submit}
            disabled={busy || blocked || preflight === null || (checkinOwned && !force)}
          >
            {busy && <span className="spin" />}
            {busy ? "切换中，请稍候" : "确认切换"}
          </button>
        </div>
      </div>
    </div>
  );
}

function CheckLine({
  ok,
  label,
  detail,
}: {
  ok: boolean;
  label: string;
  detail?: string;
}) {
  return (
    <div className="check-line">
      <span className={`check-mark${ok ? " ok" : " bad"}`}>{ok ? "✓" : "!"}</span>
      <div className="check-body">
        <div className="check-label">{label}</div>
        {detail && <div className="check-detail">{detail}</div>}
      </div>
    </div>
  );
}

import { useState } from "react";
import * as api from "../api";
import OAuthLogin from "./OAuthLogin";

interface Props {
  onClose: () => void;
  onDone: (text: string) => void;
}

type Mode = "oauth" | "local" | "manual";

const DOMAINS = ["www.codebuddy.cn", "www.workbuddy.cn"];

const MODES: { key: Mode; label: string }[] = [
  { key: "oauth", label: "独立登录" },
  { key: "local", label: "从本机导入" },
  { key: "manual", label: "手动添加" },
];

export default function AddAccountDialog({ onClose, onDone }: Props) {
  const [mode, setMode] = useState<Mode>("oauth");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [uid, setUid] = useState("");
  const [nickname, setNickname] = useState("");
  const [accessToken, setAccessToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [domain, setDomain] = useState(DOMAINS[0]);

  const doImport = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api.importLocalAccounts();
      if (!result.ok) {
        setError(result.error || "导入失败");
        return;
      }
      onDone(`已从本机导入：新增 ${result.imported ?? 0} 个，更新 ${result.merged ?? 0} 个。`);
    } catch (caught) {
      setError(String(caught));
    } finally {
      setBusy(false);
    }
  };

  const doAdd = async () => {
    if (!accessToken.trim()) {
      setError("access_token 不能为空");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.addAccount({ uid, nickname, accessToken, refreshToken, domain });
      onDone(`账号「${nickname || uid || "已添加"}」保存成功。`);
    } catch (caught) {
      setError(String(caught));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mask" onClick={onClose}>
      <div className="modal" onClick={(event) => event.stopPropagation()}>
        <div className="modal-head">添加账号</div>

        <div className="modal-body">
          <div style={{ display: "flex", gap: 6 }}>
            {MODES.map((item) => (
              <button
                key={item.key}
                className={`btn sm${mode === item.key ? " primary" : ""}`}
                onClick={() => {
                  setMode(item.key);
                  setError(null);
                }}
              >
                {item.label}
              </button>
            ))}
          </div>

          {mode === "oauth" ? (
            <OAuthLogin onDone={onDone} />
          ) : mode === "local" ? (
            <div className="field">
              <div className="hint" style={{ lineHeight: 1.6 }}>
                读取官方客户端在本机的登录态文件，只能导入当前登录的那一个账号（历史账号记录里不含凭证）。
                需要本机已登录过官方客户端。
              </div>
            </div>
          ) : (
            <>
              <div className="field">
                <label>access_token（必填）</label>
                <input
                  type="password"
                  value={accessToken}
                  onChange={(event) => setAccessToken(event.target.value)}
                  placeholder="粘贴 token"
                />
              </div>
              <div className="field">
                <label>refresh_token（建议填写）</label>
                <input
                  type="password"
                  value={refreshToken}
                  onChange={(event) => setRefreshToken(event.target.value)}
                  placeholder="用于 token 过期后自动续期"
                />
                <div className="hint">不填则每次到期都需重新粘贴，无法长期无人值守。</div>
              </div>
              <div className="field">
                <label>UID（可选）</label>
                <input value={uid} onChange={(event) => setUid(event.target.value)} placeholder="账号 uid" />
              </div>
              <div className="field">
                <label>备注名（可选）</label>
                <input
                  value={nickname}
                  onChange={(event) => setNickname(event.target.value)}
                  placeholder="用于区分多个账号"
                />
              </div>
              <div className="field">
                <label>域名</label>
                <select
                  className="select"
                  style={{ width: "100%" }}
                  value={domain}
                  onChange={(event) => setDomain(event.target.value)}
                >
                  {DOMAINS.map((item) => (
                    <option key={item} value={item}>
                      {item}
                    </option>
                  ))}
                </select>
                <div className="hint">仅支持官方域名，其他域名会被拒绝。</div>
              </div>
            </>
          )}

          {error && <div className="note danger">{error}</div>}
        </div>

        <div className="modal-foot">
          <button className="btn" onClick={onClose} disabled={busy}>
            {mode === "oauth" ? "关闭" : "取消"}
          </button>
          {mode === "local" && (
            <button className="btn primary" onClick={doImport} disabled={busy}>
              {busy ? "导入中" : "开始导入"}
            </button>
          )}
          {mode === "manual" && (
            <button className="btn primary" onClick={doAdd} disabled={busy}>
              {busy ? "保存中" : "保存"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

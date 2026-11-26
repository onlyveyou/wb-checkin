import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { AntigravityLoginStatus, AntigravityQuota, AntigravityState, InjectEntry } from "../types";
import ModelInjectPicker, { type ModelRow } from "./ModelInjectPicker";

/**
 * Antigravity：本地网关反代 Google Cloud Code Assist。
 *
 * 走的是 Antigravity IDE 自己的 OAuth 客户端：登录一次拿 refresh_token，
 * 存在网关 config.json，之后网关用它换 access_token 直接调
 * `cloudcode-pa.googleapis.com/v1internal:streamGenerateContent`，
 * 模型与额度来自 `fetchAvailableModels` / `retrieveUserQuotaSummary`（动态，不是写死的）。
 */

const POLL_MS = 8000;
const LOGIN_POLL_MS = 2500;
const QUOTA_POLL_MS = 120000;
const FALLBACK: InjectEntry = { enabled: true, rotate: [], inject: ["auto"] };

function pct(v: number | null | undefined) {
  if (typeof v !== "number") return "—";
  return `${Math.round(v * 100)}%`;
}

function resetText(iso: string) {
  if (!iso) return "";
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "";
  const left = t - Date.now();
  if (left <= 0) return "已重置";
  const h = Math.floor(left / 3600000);
  const m = Math.round((left % 3600000) / 60000);
  return h > 0 ? `${h} 小时 ${m} 分后重置` : `${m} 分钟后重置`;
}

export default function AntigravityPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [state, setState] = useState<AntigravityState | null>(null);
  const [login, setLogin] = useState<AntigravityLoginStatus | null>(null);
  const [quota, setQuota] = useState<AntigravityQuota | null>(null);
  const [inject, setInject] = useState<InjectEntry>(FALLBACK);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);
  /** 静默轮询失败时也要留痕：不然面板只剩「已登录 + 剩余 —」，看着像还没加载。 */
  const [quotaError, setQuotaError] = useState("");

  const credential = state?.credential === true || login?.credential === true;
  // 上游明确拒过这份 token（地区限制 / 401）：此时 credential 也是 false，
  // 但和「没登录」要区别对待 —— 重新登录解决不了地区限制，得照着 error 说。
  const expired = state?.expired === true;
  const expiredReason = state?.error || login?.message || "";

  const refresh = useCallback(async () => {
    try {
      const next = await api.antigravityState();
      setState(next);
      if (!next.offline && next.inject && typeof next.inject === "object") {
        setInject({
          enabled: next.inject.enabled !== false,
          rotate: Array.isArray(next.inject.rotate) ? next.inject.rotate : [],
          inject: next.inject.inject?.length ? next.inject.inject : ["auto"],
        });
      }
    } catch (error) {
      onError(String(error));
    }
  }, [onError]);

  const refreshQuota = useCallback(async (force = false, silent = false) => {
    if (!silent) setBusy((b) => b ?? "quota");
    try {
      const next = await api.antigravityQuota(force);
      setQuota(next);
      setQuotaError(next.error || "");
      if (next.error && !silent) setNotice(`额度接口返回：${next.error}`);
    } catch (error) {
      setQuotaError(String(error));
      if (!silent) setNotice(`额度查询失败：${String(error)}`);
    } finally {
      if (!silent) setBusy((b) => (b === "quota" ? null : b));
    }
  }, []);

  useEffect(() => {
    if (!active) return;
    void refresh();
    const timer = window.setInterval(() => void refresh(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, refresh]);

  useEffect(() => {
    if (!active || !credential) return;
    void refreshQuota(false, true);
    const timer = window.setInterval(() => void refreshQuota(false, true), QUOTA_POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, credential, refreshQuota]);

  // 登录中加密轮询：扫码/授权完成后自动停
  useEffect(() => {
    if (!active || !login || !["starting", "waiting"].includes(login.state)) return;
    const timer = window.setInterval(async () => {
      try {
        const next = await api.antigravityLoginStatus();
        setLogin(next);
        if (next.credential || next.state === "success" || next.state === "error") {
          await refresh();
          if (next.credential) {
            // 登录完立刻把 antigravity/auto 写进 WorkBuddy，省掉再点一次「保存并写入」
            try { setNotice(await api.freehubInjectModels()); } catch { /* 写失败不挡登录 */ }
            void refreshQuota(true, true);
          }
        }
      } catch { /* 继续轮询 */ }
    }, LOGIN_POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, login, refresh, refreshQuota]);

  async function doLogin() {
    setBusy("login");
    setNotice(null);
    try {
      const started = await api.antigravityLogin();
      setLogin({ ok: true, state: "starting", message: started.message, credential: false, email: "" });
      setNotice("浏览器已打开，请选择 Google 账号并同意授权（授权完窗口会自动关）");
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doSave(next: InjectEntry) {
    setBusy("save");
    setNotice(null);
    try {
      await api.freehubSetInject("antigravity", next);
      const msg = await api.freehubInjectModels();
      setInject(next);
      setNotice(msg);
      await refresh();
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doTest() {
    setBusy("test");
    setTestResult(null);
    try {
      const r = await api.antigravityTest(quota?.models?.[0]?.id);
      const text = (r as unknown as { content?: string })?.content || r.text || "";
      setTestResult(`测试成功：${String(text).slice(0, 160)}`);
      void refreshQuota(true, true);
    } catch (error) {
      setTestResult(`测试失败：${String(error)}`);
    } finally {
      setBusy(null);
    }
  }

  const rows: ModelRow[] = (quota?.models?.length
    ? quota.models
    : (state?.models || []).map((id) => ({ id, label: state?.labels?.[id] || id, remaining: null as number | null, resetAt: "" }))
  ).map((m) => ({
    id: m.id,
    label: m.label || m.id,
    hint: [
      m.id,
      typeof m.remaining === "number" ? `剩余 ${pct(m.remaining)}` : "额度未知",
      resetText(m.resetAt || ""),
    ].filter(Boolean).join(" · "),
    ratio: typeof m.remaining === "number" ? m.remaining : null,
    danger: m.remaining === 0,
  }));

  const offline = state?.offline === true;
  const usedRatio = (() => {
    const list = quota?.models?.filter((m) => typeof m.remaining === "number") || [];
    if (!list.length) return null;
    const avg = list.reduce((s, m) => s + (m.remaining as number), 0) / list.length;
    return avg;
  })();

  return (
    <>
      <div className="ferry-head">
        <div>
          <div className="ferry-title">Antigravity（Google）</div>
          <div className="ferry-sub">
            用 Antigravity IDE 的 Google 账号额度反代 Gemini / Claude。本机网关换 token、转协议，
            登录一次就一直能用；WorkBuddy 里选 <code>Antigravity · 自动轮换</code> 或单独模型。
          </div>
        </div>
        <div className="ferry-actions">
          <button className="btn" disabled={busy === "quota" || !credential} onClick={() => void refreshQuota(true)}>
            {busy === "quota" ? "查询中…" : "刷新额度"}
          </button>
          <button className="btn primary" disabled={offline || busy === "login"} onClick={() => void doLogin()}>
            {busy === "login" ? "处理中…" : credential ? "重新登录" : "Google 登录"}
          </button>
        </div>
      </div>

      {notice && <div className="ferry-note">{notice}</div>}
      {expired && !credential && expiredReason && <div className="banner">{expiredReason}</div>}
      {!expired && quotaError && <div className="banner">额度查询失败：{quotaError}</div>}

      {offline ? (
        <div className="empty">
          <div className="big">网关未运行</div>
          <div className="hint">{state?.message || "到其他标签点「启动网关」再试"}</div>
        </div>
      ) : (
        <>
          <div className="section-head"><h2>账号与额度</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">
                  <span style={{ color: credential ? "var(--green)" : "var(--red)" }}>{credential ? "●" : "○"}</span>{" "}
                  {credential
                    ? (state?.email || login?.email || "已登录")
                    : expired ? "不可用（原因见下）" : "未登录"}
                </div>
                <div className="row-hint">
                  {credential
                    ? `方案 ${quota?.plan || state?.plan || "—"} · project ${quota?.project || "—"}`
                    : expired
                      ? expiredReason
                      : "未登录时调用 Antigravity 模型会直接报错，不会静默降级到别家"}
                </div>
              </div>
              <div style={{ textAlign: "right", flex: "none" }}>
                <div className="row-label">模型平均剩余</div>
                <div
                  style={{
                    fontSize: 26,
                    fontWeight: 700,
                    color: usedRatio === null ? "var(--fg)" : usedRatio <= 0.1 ? "var(--red)" : usedRatio <= 0.3 ? "var(--warn)" : "var(--green)",
                  }}
                >
                  {!credential && expired ? "—" : usedRatio === null ? "—" : pct(usedRatio)}
                </div>
              </div>
            </div>

            {quota?.groups?.length ? (
              quota.groups.map((g) => (
                <div className="row" key={g.label}>
                  <div style={{ flex: 1 }}>
                    <div className="row-label">{g.label}</div>
                    <div className="row-hint">{g.description || "按窗口统计的剩余额度"}</div>
                    {g.buckets.map((b) => (
                      <div key={`${b.window}-${b.resetAt}`} style={{ marginTop: 6 }}>
                        <div className="row-hint">
                          {b.window || "额度"}：{pct(b.remaining)}
                          {b.resetAt ? ` · ${resetText(b.resetAt)}` : ""}
                        </div>
                        <div style={{ height: 5, borderRadius: 3, marginTop: 4, background: "rgba(127,127,127,.22)", overflow: "hidden" }}>
                          <div
                            style={{
                              height: "100%",
                              width: `${Math.min(100, Math.max(0, b.remaining * 100))}%`,
                              background: b.remaining <= 0.1 ? "var(--red)" : b.remaining <= 0.3 ? "var(--warn)" : "var(--green)",
                            }}
                          />
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              ))
            ) : (
              <div className="ferry-note">
                {credential
                  ? (quota?.error ? `额度接口没给数据：${quota.error}` : "登录后自动拉取每个模型的剩余额度（按 5 小时 / 每周窗口）。")
                  : "登录后可查每个模型的剩余额度（走 Google 自己的 quota 接口，不是本地估算）。"}
              </div>
            )}
          </div>

          <div className="section-head"><h2>模型与 WorkBuddy 注入</h2></div>
          <div className="panel settings">
            <ModelInjectPicker
              provider="antigravity"
              rows={rows}
              entry={inject}
              busy={busy === "save"}
              autoHint="按「轮换」勾选的模型依次尝试，成功率和速度看上面的剩余额度"
              onSave={doSave}
            />
          </div>

          <div className="section-head"><h2>连通测试</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">真发一次请求</div>
                <div className="row-hint">用轮换列表里第一个模型问一句「只回两个字：可用」，不走降级。</div>
              </div>
              <button className="btn" disabled={busy === "test" || !credential} onClick={() => void doTest()}>
                {busy === "test" ? "测试中…" : "测试"}
              </button>
            </div>
            {testResult && <div className="ferry-note">{testResult}</div>}
          </div>

          <div className="ferry-note">
            这是「反代」方案：用的是 Antigravity IDE 内置的公开 OAuth 客户端和内部 Code Assist 接口，
            不是官方 API，Google 改协议就可能失效；token 只存本机网关，不会上传。
          </div>
        </>
      )}
    </>
  );
}

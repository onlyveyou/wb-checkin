import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { InjectEntry, OrcaTermLoginStatus, OrcaTermState } from "../types";
import ModelInjectPicker, { type ModelRow } from "./ModelInjectPicker";

/**
 * OrcaTerm 免费模型配置。
 *
 * 凭据的唯一事实来源是网关（127.0.0.1:8901）的 config.json，WorkBuddy 也读它。
 * 本面板不持有 cookie、不直接发 HTTP：登录走「网关拉独立浏览器扫码 → 自动写凭据
 * → 热重载」，这里只负责触发和轮询进度。这样软件里登录一次，WorkBuddy 立刻能用。
 *
 * 两个概念别混：
 *   - 登录态（uin + skey）：决定能不能用，由扫码得到，不在这里编辑
 *   - 模型清单：轮换用哪些、哪些单独写进 WorkBuddy，都在下面的勾选器里改
 */

const POLL_MS = 8000;
const LOGIN_POLL_MS = 2000;
const FALLBACK: InjectEntry = { enabled: true, rotate: [], inject: ["auto"] };

const ORCA_ROWS: ModelRow[] = [
  { id: "TokenHub/deepseek-v4.1-flash", label: "DeepSeek V4.1 Flash" },
  { id: "TokenHub/deepseek-v4-flash", label: "DeepSeek V4 Flash" },
  { id: "TokenHub/deepseek-v4-pro", label: "DeepSeek V4 Pro" },
  { id: "TokenHub/kimi-k3", label: "Kimi K3" },
  { id: "TokenHub/glm-5.3-flash", label: "GLM 5.3 Flash" },
  { id: "TokenHub/glm-5.3", label: "GLM 5.3" },
  { id: "TokenHub/glm-5.2", label: "GLM 5.2" },
  { id: "Hunyuan3/hy3", label: "Hunyuan 3" },
];

function loginText(status: OrcaTermLoginStatus | null, state: OrcaTermState | null): string {
  // credential 为 true 但上游已经拒过这份 cookie → 说「已失效」，别说「已登录」：
  // 这两个词对应的动作不一样（一个直接用，一个去重新扫码）。
  if (state?.expired) return "登录已失效，需要重新登录";
  if (state?.credential) return "已登录";
  if (!status) return "检查中…";
  switch (status.state) {
    case "starting": return "正在启动浏览器…";
    case "waiting": return "等待扫码…";
    case "success": return "登录成功";
    case "error": return "登录失败";
    default: return "未登录";
  }
}

export default function OrcaTermPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [state, setState] = useState<OrcaTermState | null>(null);
  const [login, setLogin] = useState<OrcaTermLoginStatus | null>(null);
  const [injectSystem, setInjectSystem] = useState("");
  const [autoLock, setAutoLock] = useState(true);
  const [inject, setInject] = useState<InjectEntry>(FALLBACK);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const next = await api.orcatermState();
      setState(next);
      if (!next.offline) {
        setInjectSystem((prev) => prev || next.injectSystem || "");
        setAutoLock(next.autoLock !== false);
        if (next.inject && typeof next.inject === "object") {
          setInject({
            enabled: next.inject.enabled !== false,
            rotate: Array.isArray(next.inject.rotate) ? next.inject.rotate : [],
            inject: next.inject.inject?.length ? next.inject.inject : ["auto"],
          });
        }
      }
    } catch (error) {
      onError(String(error));
    }
  }, [onError]);

  useEffect(() => { void refresh(); }, [refresh]);

  // 面板可见时才轮询，别的标签页不为配置页持续打请求
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => void refresh(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, refresh]);

  // 登录中时加密轮询进度：扫码完成后自动停
  useEffect(() => {
    if (!active || !login || !["starting", "waiting"].includes(login.state)) return;
    const timer = window.setInterval(async () => {
      try {
        const next = await api.orcatermLoginStatus();
        setLogin(next);
        if (next.credential || next.state === "success" || next.state === "error") {
          await refresh();
        }
      } catch { /* 轮询失败继续等 */ }
    }, LOGIN_POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, login, refresh]);

  const offline = state?.offline === true;
  const credential = state?.credential === true || login?.credential === true;
  const stats = state?.stats || {};

  async function doLogin() {
    setBusy("login");
    setNotice(null);
    try {
      const started = await api.orcatermLogin();
      if (started.already) {
        setNotice("已经登录了，无需重复扫码");
        await refresh();
      } else {
        setLogin({ ok: true, state: "starting", message: started.message, uin: "", credential: false });
        setNotice("浏览器已弹出，请在窗口里扫码登录腾讯云");
      }
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doStart() {
    setStarting(true);
    setNotice(null);
    try {
      await api.orcatermStart();
      setNotice("网关已启动");
      await refresh();
    } catch (error) {
      onError(String(error));
    } finally {
      setStarting(false);
    }
  }

  /** 勾选器保存：先写注入开关，再覆盖式写 models.json（关掉平台 = 当场清掉条目） */
  async function doSaveInject(next: InjectEntry) {
    setBusy("save");
    setNotice(null);
    try {
      await api.freehubSetInject("orcaterm", next);
      setInject(next);
      setNotice(await api.freehubInjectModels());
      await refresh();
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doSaveExtra() {
    setBusy("extra");
    setNotice(null);
    try {
      // 人设 / 降级设置单独保存，不动模型勾选（否则会把用户在勾选器里的改动冲掉）
      const list = inject.rotate.length ? inject.rotate : ORCA_ROWS.map((r) => r.id);
      await api.orcatermSave(list, injectSystem, autoLock);
      setNotice("人设与降级设置已保存，网关已热重载");
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
      const r = await api.orcatermTest();
      setTestResult(
        r.ok
          ? `连通正常 · ${r.ms}ms · ${(r.text || "").slice(0, 80)}`
          : `测试失败：${r.error || r.text || "未知错误"}`,
      );
    } catch (error) {
      setTestResult(`测试失败：${String(error)}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <>
      <div className="ferry-head">
        <div>
          <div className="ferry-title">OrcaTerm 免费模型</div>
          <div className="ferry-sub">
            腾讯云 OrcaTerm 面板里的限时免费模型。登录一次，WorkBuddy 里选
            <code> OrcaTerm · 自动轮换 </code>
            或勾选出来的单独模型即可使用。
          </div>
        </div>
        <div className="ferry-actions">
          {offline && (
            <button className="btn" disabled={starting} onClick={() => void doStart()}>
              {starting ? "启动中…" : "启动网关"}
            </button>
          )}
          <button
            className="btn primary"
            disabled={offline || busy === "login"}
            onClick={() => void doLogin()}
          >
            {busy === "login" ? "处理中…" : credential ? "重新登录" : "扫码登录"}
          </button>
        </div>
      </div>

      {notice && <div className="ferry-note">{notice}</div>}

      {offline ? (
        <div className="empty">
          <div className="big">网关未运行</div>
          <div className="hint">{state?.message || "点右上角「启动网关」再试"}</div>
        </div>
      ) : (
        <>
          <div className="section-head"><h2>账号与用量</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">
                  <span style={{ color: credential ? "var(--green)" : "var(--red)" }}>
                    {credential ? "●" : "○"}
                  </span>{" "}
                  {loginText(login, state)}
                </div>
                <div className="row-hint">
                  {credential
                    ? `腾讯云 uin ${state?.uin || login?.uin || "—"} · 凭据由网关保存，本应用只读取状态`
                    : (state?.expired
                      ? (state.error || "登录态已失效：点右上角「扫码登录」重新授权一次")
                      : (login?.message || "未登录时调用 OrcaTerm 模型会直接报错，不会误报「空内容」"))}
                </div>
              </div>
              <div style={{ textAlign: "right", flex: "none" }}>
                <div className="row-label">累计调用</div>
                <div style={{ fontSize: 26, fontWeight: 700 }}>
                  {(stats.ok || 0).toLocaleString("zh-CN")}
                  <span style={{ fontSize: 13, color: "var(--muted)", fontWeight: 400 }}>
                    {" "}成功 / {stats.fail || 0} 失败
                  </span>
                </div>
                <div className="row-hint">
                  {stats.cooling ? `${stats.cooling} 个模型冷却中` : "额度按腾讯云账号发放，无公开查询接口"}
                </div>
              </div>
            </div>
            <div className="row">
              <div>
                <div className="row-label">连通测试</div>
                <div className="row-hint">真发一次请求验证凭据和上游是否正常（不写进网关记账）。</div>
              </div>
              <button className="btn" disabled={busy === "test" || !credential} onClick={() => void doTest()}>
                {busy === "test" ? "测试中…" : "测试"}
              </button>
            </div>
            {testResult && <div className="ferry-note">{testResult}</div>}
          </div>

          <div className="section-head"><h2>模型与 WorkBuddy 注入</h2></div>
          <div className="panel settings">
            <ModelInjectPicker
              provider="orcaterm"
              rows={ORCA_ROWS}
              entry={inject}
              busy={busy === "save"}
              autoHint="限流/失败自动降级下一个，不会串到别的免费池"
              onSave={doSaveInject}
            />
          </div>

          <div className="section-head"><h2>人设与降级</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">锁定在 OrcaTerm 内部降级</div>
                <div className="row-hint">关掉后允许在失败时降级到其他免费渠道（默认开）。</div>
              </div>
              <button
                className="switch"
                data-on={autoLock}
                aria-label="切换锁定在 OrcaTerm 内部降级"
                onClick={() => setAutoLock(!autoLock)}
              />
            </div>
            <div className="row">
              <div style={{ flex: 1 }}>
                <div className="row-label">人设注入</div>
                <div className="row-hint">
                  上游把模型塑造成「云服务器运维助手」，通用/编程任务会拒答；这里覆盖成通用助手。
                </div>
                <textarea
                  className="path"
                  value={injectSystem}
                  onChange={(e) => setInjectSystem(e.target.value)}
                  rows={3}
                  style={{ width: "100%", minHeight: 64, marginTop: 8, resize: "vertical" }}
                />
              </div>
            </div>
            <div className="row">
              <div className="row-hint" style={{ flex: 1 }}>
                只保存人设 / 降级设置，不影响上面的模型勾选。
              </div>
              <button className="btn" disabled={busy === "extra"} onClick={() => void doSaveExtra()}>
                {busy === "extra" ? "保存中…" : "保存人设"}
              </button>
            </div>
          </div>

          <div className="ferry-note">
            额度是腾讯云账号侧的限时免费权益，无固定截止日期；用完只影响 OrcaTerm 这一组模型，
            不影响你的其他免费渠道。cookie 只存本机网关配置，不上传任何地方。
          </div>
        </>
      )}
    </>
  );
}

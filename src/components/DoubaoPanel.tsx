import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { DoubaoLoginStatus, DoubaoQuota, DoubaoState, InjectEntry } from "../types";
import ModelInjectPicker, { type ModelRow } from "./ModelInjectPicker";

const POLL_MS = 5000;
const LOGIN_POLL_MS = 2500;
const FALLBACK: InjectEntry = { enabled: true, rotate: ["doubao", "doubao-turbo"], inject: ["auto"] };

/** 几个常用模型的固定说明（其余只显示官方倍率与上下文） */
const MODEL_NOTE: Record<string, string> = {
  doubao: "不思考，秒回，适合日常问答",
  "doubao-turbo": "更深思考，慢一些但更强",
  "doubao-auto": "自动挑一个合适的模型",
  "doubao-lite": "轻量版，最快最省",
  "doubao-pro": "能力更强的豆包主力模型",
};

/** 官方清单还没拉回来时的兜底（就两条内置模型） */
const FALLBACK_ROWS: ModelRow[] = [
  { id: "doubao", label: "快速", hint: "不思考，秒回，适合日常问答" },
  { id: "doubao-turbo", label: "专家", hint: "更深思考，慢一些但更强" },
];

export default function DoubaoPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [state, setState] = useState<DoubaoState | null>(null);
  const [login, setLogin] = useState<DoubaoLoginStatus | null>(null);
  const [autoCheckin, setAutoCheckin] = useState(true);
  const [inject, setInject] = useState<InjectEntry>(FALLBACK);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [checkinMsg, setCheckinMsg] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [quota, setQuota] = useState<DoubaoQuota | null>(null);
  const [quotaMsg, setQuotaMsg] = useState<string | null>(null);
  const [quotaDelta, setQuotaDelta] = useState<number | null>(null);

  const hasCred = state?.credential === true || login?.credential === true;

  // 模型清单来自官方接口（/alice/slot/action_bar_v3/brief_list），官方上新就会自动多出来
  const catalogRows: ModelRow[] = (state?.catalog ?? []).map((m) => ({
    id: m.id,
    label: m.name,
    hint: [
      MODEL_NOTE[m.id] || "",
      m.tag,
      m.window ? `${Math.round(m.window / 1000)}K 上下文` : "",
    ].filter(Boolean).join(" · "),
  }));
  const modelRows = catalogRows.length ? catalogRows : FALLBACK_ROWS;

  const refresh = useCallback(async () => {
    try {
      const next = await api.doubaoState();
      setState(next);
      if (!next.offline) {
        setAutoCheckin(next.autoCheckin !== false);
        if (next.inject && typeof next.inject === "object") {
          setInject({
            enabled: next.inject.enabled !== false,
            rotate: next.inject.rotate?.length ? next.inject.rotate : FALLBACK.rotate,
            inject: next.inject.inject?.length ? next.inject.inject : ["auto"],
          });
        }
      }
    } catch (error) {
      onError(String(error));
    }
  }, [onError]);

  const refreshQuota = useCallback(async (silent = false) => {
    if (!silent) setBusy((b) => b ?? "quota");
    try {
      const next = await api.doubaoQuota();
      setQuota((prev) => {
        if (prev && typeof prev.remaining === "number") {
          const diff = prev.remaining - next.remaining;
          setQuotaDelta(diff > 0 ? diff : diff < 0 ? diff : 0);
        }
        return next;
      });
      setQuotaMsg(null);
    } catch (error) {
      if (!silent) setQuotaMsg(`额度查询失败：${String(error)}`);
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
    if (!active || !hasCred) return;
    void refreshQuota(true);
    const timer = window.setInterval(() => void refreshQuota(true), 60000);
    return () => window.clearInterval(timer);
  }, [active, hasCred, refreshQuota]);

  useEffect(() => {
    if (!active || !login || !["starting", "waiting"].includes(login.state)) return;
    const timer = window.setInterval(async () => {
      try {
        const next = await api.doubaoLoginStatus();
        setLogin(next);
        if (next.credential || next.state === "success" || next.state === "error") await refresh();
      } catch { /* 继续轮询 */ }
    }, LOGIN_POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, login, refresh]);

  const offline = state?.offline === true;
  const credential = hasCred;

  async function doLogin() {
    setBusy("login");
    setNotice(null);
    try {
      const started = await api.doubaoLogin();
      setLogin({ ok: true, state: "starting", message: started.message, credential: false, checkedToday: false });
      setNotice("浏览器已弹出，请登录豆包（登录后 cookie 自动写入本机网关）");
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doCheckin(force: boolean) {
    setBusy(force ? "checkin-force" : "checkin");
    setCheckinMsg(null);
    try {
      const r = await api.doubaoCheckin(force);
      const blocked = (r as unknown as { blocked?: boolean })?.blocked === true;
      setCheckinMsg(blocked ? r.message : r.already ? "今天已签到，服务端幂等，不会重复发放" : r.message);
      await refresh();
      void refreshQuota(true);
    } catch (error) {
      setCheckinMsg(`签到失败：${String(error)}`);
    } finally {
      setBusy(null);
    }
  }

  /** 勾选器保存：注入开关 + models.json 一起落盘；autoCheckin 保持当前值 */
  async function doSaveInject(next: InjectEntry) {
    setBusy("save");
    setNotice(null);
    try {
      await api.freehubSetInject("doubao", next);
      setInject(next);
      const msg = await api.freehubInjectModels();
      setNotice(msg);
      await refresh();
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function doSaveCheckin() {
    setBusy("checkin-save");
    setNotice(null);
    try {
      const r = await api.doubaoSave(inject.rotate.length ? inject.rotate : FALLBACK.rotate, autoCheckin);
      setNotice(`自动签到已${r.autoCheckin ? "开启" : "关闭"}`);
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
      const r = await api.doubaoTest();
      const text = (r as unknown as { content?: string })?.content
        ?? (r as unknown as { message?: string })?.message
        ?? JSON.stringify(r);
      setTestResult(`测试成功：${String(text).slice(0, 200)}`);
      void refreshQuota(true);
    } catch (error) {
      setTestResult(`测试失败：${String(error)}`);
    } finally {
      setBusy(null);
    }
  }

  const loginText = () => {
    if (credential && !login) return "已登录";
    switch (login?.state) {
      case "starting": return "正在启动浏览器…";
      case "waiting": return "等待登录…";
      case "success": return "登录成功";
      case "error": return "登录失败";
      default: return credential ? "已登录" : "未登录";
    }
  };

  return (
    <>
      <div className="ferry-head">
        <div>
          <div className="ferry-title">豆包网页额度</div>
          <div className="ferry-sub">
            直接用你豆包账号里的免费额度（含每日签到送的额度）。登录一次，
            WorkBuddy 里选 <code> 豆包 · 自动轮换 </code>
            或「豆包 快速 / 2.1 Turbo（专家）」即可使用。
          </div>
        </div>
        <div className="ferry-actions">
          <button
            className="btn"
            disabled={offline || busy === "checkin"}
            onClick={() => void doCheckin(false)}
          >
            {busy === "checkin" ? "签到中…" : state?.checkedToday ? "今日已签" : "立即签到"}
          </button>
          <button
            className="btn"
            disabled={offline}
            title="账号被要求人机验证时（710022004），去网页聊一句过掉验证，再点重新登录抓新 cookie"
            onClick={() => void api.freehubOpen("https://www.doubao.com/chat/")}
          >
            网页验证
          </button>
          <button
            className="btn primary"
            disabled={offline || busy === "login"}
            onClick={() => void doLogin()}
          >
            {busy === "login" ? "处理中…" : credential ? "重新登录" : "登录豆包"}
          </button>
        </div>
      </div>

      {notice && <div className="ferry-note">{notice}</div>}
      {checkinMsg && <div className="ferry-note">{checkinMsg}</div>}

      <div className="section-head"><h2>额度与消耗</h2></div>
      <div className="panel settings">
        <div className="row">
          <div>
            <div className="row-label">当前可用额度</div>
            <div
              style={{
                fontSize: 30,
                fontWeight: 700,
                lineHeight: 1.25,
                marginTop: 2,
                color: quota
                  ? quota.exhausted ? "var(--red)" : "var(--green)"
                  : "var(--fg)",
              }}
            >
              {quota ? quota.remaining.toLocaleString("zh-CN") : credential ? "…" : "—"}
            </div>
            <div className="row-hint">
              {quota?.plan ? `套餐：${quota.plan}` : "套餐：—"}
              {" · "}今日调用 {quota?.usage?.calls ?? 0} 次 / {quota?.usage?.chars ?? 0} 字
              {quotaDelta !== null && quotaDelta !== 0
                ? ` · 较上次刷新 ${quotaDelta > 0 ? "−" : "+"}${Math.abs(quotaDelta).toLocaleString("zh-CN")}`
                : ""}
            </div>
          </div>
          <button
            className="btn"
            disabled={offline || !credential || busy === "quota"}
            onClick={() => void refreshQuota(false)}
          >
            {busy === "quota" ? "查询中…" : "刷新额度"}
          </button>
        </div>
        {quota?.groups?.length ? (
          quota.groups.map((g) => (
            <div className="row" key={`${g.label}-${g.expireAt}-${g.total}`}>
              <div style={{ flex: 1 }}>
                <div className="row-label">{g.label}</div>
                <div className="row-hint">
                  剩余 {g.remaining ?? "—"} / 共 {g.total ?? "—"}
                  {g.expireAt ? ` · ${new Date(g.expireAt).toLocaleDateString("zh-CN")} 到期` : ""}
                </div>
                <div
                  style={{
                    height: 6,
                    borderRadius: 3,
                    marginTop: 6,
                    background: "rgba(127,127,127,.22)",
                    overflow: "hidden",
                  }}
                >
                  <div
                    style={{
                      height: "100%",
                      width: `${Math.min(100, Math.max(0, g.usedPercent ?? 0))}%`,
                      background: "var(--green)",
                    }}
                  />
                </div>
              </div>
            </div>
          ))
        ) : (
          <div className="ferry-note">
            {quota
              ? "额度明细为空：豆包只展示未到期的额度，当前账号没有任何未到期额度。签到拿到的额度会出现在这里。"
              : "登录后可查可用额度（走豆包自己的订阅总览 / 额度余额接口，不是本地估算）。"}
          </div>
        )}
        {quotaMsg && <div className="ferry-note">{quotaMsg}</div>}
      </div>

      {offline ? (
        <div className="empty">
          <div className="big">网关未运行</div>
          <div className="hint">{state?.message || "点其他标签里的「启动网关」再试"}</div>
        </div>
      ) : (
        <>
          <div className="section-head"><h2>登录与签到</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">
                  <span style={{ color: credential ? "var(--green)" : "var(--red)" }}>
                    {credential ? "●" : "○"}
                  </span>{" "}
                  {loginText()}
                </div>
                <div className="row-hint">
                  签到：{state?.checkedToday ? "今日已领" : "今日未领"}
                  {state?.lastCheckin ? ` · 最近：${state.lastCheckin}` : ""}
                  {" · "}自动签到：{autoCheckin ? "开启" : "关闭"}
                </div>
              </div>
              <button
                className="btn"
                disabled={busy === "checkin-force" || !credential}
                onClick={() => void doCheckin(true)}
              >
                {busy === "checkin-force" ? "重试中…" : "重新签到"}
              </button>
            </div>
            <div className="row">
              <div>
                <div className="row-label">每天自动签到</div>
                <div className="row-hint">每小时检查一次，北京时间当天未领才请求签到。走的是电脑版同一个接口（landing/checkin），服务端幂等。</div>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                <button
                  className="switch"
                  data-on={autoCheckin}
                  aria-label="切换每天自动签到"
                  onClick={() => setAutoCheckin(!autoCheckin)}
                />
                <button className="btn sm" disabled={busy === "checkin-save"} onClick={() => void doSaveCheckin()}>
                  {busy === "checkin-save" ? "保存中…" : "保存"}
                </button>
              </div>
            </div>
          </div>

          <div className="section-head"><h2>模型与 WorkBuddy 注入</h2></div>
          <div className="panel settings">
            <ModelInjectPicker
              provider="doubao"
              rows={modelRows}
              entry={inject}
              busy={busy === "save"}
              autoHint="按「轮换」勾选的模型依次尝试；勾选越多越快不了，但某个被限流能自动换下一个"
              onSave={doSaveInject}
            />
          </div>

          <div className="section-head"><h2>连通测试</h2></div>
          <div className="panel settings">
            <div className="row">
              <div>
                <div className="row-label">真发一次请求</div>
                <div className="row-hint">走当前账号额度问一句「只回两个字：可用」。</div>
              </div>
              <button className="btn" disabled={busy === "test" || !credential} onClick={() => void doTest()}>
                {busy === "test" ? "测试中…" : "测试"}
              </button>
            </div>
            {testResult && <div className="ferry-note">{testResult}</div>}
          </div>
        </>
      )}
    </>
  );
}

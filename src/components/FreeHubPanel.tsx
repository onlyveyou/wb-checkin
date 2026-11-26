import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "../api";
import type { FreeHubDiagnostics, FreeHubProvider, FreeHubSelfcheck, FreeHubState } from "../types";

/**
 * 自定义模型（免费渠道）。
 *
 * 数据源是本机的免费池网关（127.0.0.1:8901），它把十几家「有免费额度」的平台
 * 收在一个 OpenAI 兼容端点后面。这里做的事只有三件：看清每家的额度与验证门槛、
 * 把 key 填进去、试一次通不通。
 *
 * 两个视图，靠 selected 切换：
 *   - 列表（selected === null）：一眼看全部渠道谁缺 key、谁在队列里
 *   - 配置页（selected === 渠道名）：只处理这一家，Endpoint / key / 代理 / 模型清单齐全
 * 缺 key 的渠道同样能进配置页 —— Cloudflare 这种要把 ACCOUNT_ID 拼进 URL 的，
 * 光在行内填个 key 是配不完的。
 *
 * 与「模型摆渡」的区别：那边从 OpenCode 的免费池往外捞模型，模型目录掌握在它手里；
 * 这边是各家平台自己的 key，渠道清单随网关的 config.json 走，本页不做任何缓存。
 */

const POLL_MS = 8000;

const VERIFY_TEXT: Record<FreeHubProvider["verify"], { text: string; tone: "plain" | "warn" | "danger" }> = {
  phone: { text: "需手机验证", tone: "warn" },
  realname: { text: "需实名", tone: "warn" },
  card: { text: "需绑卡", tone: "danger" },
  none: { text: "无需验证", tone: "plain" },
};

const TIER_TEXT: Record<string, string> = {
  high: "高质量",
  standard: "标准",
  all: "全部",
};

const EMPTY_DRAFT = { endpoint: "", proxy: "", key: "", models: "", note: "" };

function seconds(ms?: number): string {
  if (!ms) return "—";
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
}

function uptime(sec?: number): string {
  if (!sec) return "—";
  if (sec >= 3600) return `${(sec / 3600).toFixed(1)} 小时`;
  return `${Math.max(1, Math.round(sec / 60))} 分钟`;
}

export default function FreeHubPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [state, setState] = useState<FreeHubState | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState(EMPTY_DRAFT);
  const [adding, setAdding] = useState(false);
  const [form, setForm] = useState({ name: "", endpoint: "", key: "", proxy: "", models: "", note: "" });
  const [diag, setDiag] = useState<FreeHubDiagnostics | null>(null);
  const [showDiag, setShowDiag] = useState(false);
  const [check, setCheck] = useState<FreeHubSelfcheck | null>(null);

  const refresh = useCallback(async () => {
    try {
      const next = await api.freehubState();
      setState(next);
    } catch (error) {
      onError(String(error));
    } finally {
      setLoading(false);
    }
  }, [onError]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 进页面拉一次诊断：顶部那条「没有可用渠道 / 记得重启 WorkBuddy」靠它判断
  useEffect(() => {
    if (!active) return;
    void api.freehubDiagnostics().then(setDiag).catch(() => undefined);
  }, [active]);

  // 只在面板可见时轮询：别的标签页不该为一个配置页持续打请求
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => void refresh(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, refresh]);

  const offline = state?.offline === true;
  const providers = useMemo(() => state?.providers ?? [], [state]);
  const missing = providers.filter((p) => !p.keyPresent && !p.allowNoKey).length;
  const ready = providers.filter((p) => p.keyPresent && !p.disabled).length;

  const current = selected ? providers.find((p) => p.name === selected) ?? null : null;

  // 渠道被删掉后配置页没了落点，退回列表
  useEffect(() => {
    if (selected && state?.providers && !state.providers.some((p) => p.name === selected)) {
      setSelected(null);
    }
  }, [selected, state]);

  async function run(key: string, action: () => Promise<unknown>, done?: string) {
    setBusy(key);
    setNotice(null);
    try {
      await action();
      if (done) setNotice(done);
      await refresh();
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  /** 诊断面板：换台电脑出问题时，这块能直接说清「网关旧了 / 没登录 / 没写进 models.json」。 */
  async function toggleDiag() {
    if (showDiag) {
      setShowDiag(false);
      return;
    }
    try {
      setDiag(await api.freehubDiagnostics());
      setShowDiag(true);
    } catch (error) {
      onError(String(error));
    }
  }

  /**
   * 渠道自检：真发请求，代价是慢（豆包要拉有头浏览器，最坏两分钟）。
   * 走 run 的话每项都会连带 refresh 状态，这里单独一个 busy 键就够了。
   */
  async function runSelfcheck() {
    setBusy("diag:check");
    setNotice(null);
    try {
      const result = await api.freehubSelfcheck();
      setCheck(result);
      if (result.offline) onError(result.message || "网关没有在运行");
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  }

  async function copyDiag() {
    try {
      await navigator.clipboard.writeText(JSON.stringify(diag ?? {}, null, 2));
      setNotice("诊断信息已复制到剪贴板");
    } catch (error) {
      onError(`复制失败：${String(error)}`);
    }
  }

  function enterConfig(p: FreeHubProvider) {
    setSelected(p.name);
    setNotice(null);
    setDraft({
      endpoint: p.endpoint,
      proxy: p.proxyAddr,
      key: "",
      models: p.models.map((id) => (p.modelLabels[id] ? `${id} | ${p.modelLabels[id]}` : id)).join("\n"),
      note: p.note,
    });
  }

  async function saveKey(p: FreeHubProvider) {
    const value = (keys[p.name] ?? "").trim();
    if (!value) {
      onError("先粘贴这家平台的 API Key");
      return;
    }
    await run(
      `key:${p.name}`,
      () => api.freehubSave(p.name, { apiKey: value, disabled: false }),
      `${p.title} 已保存，key 就位后它已经进队列`,
    );
    setKeys((prev) => ({ ...prev, [p.name]: "" }));
  }

  async function testOne(p: FreeHubProvider) {
    const result = await api.freehubTest(p.name).catch((error) => {
      onError(String(error));
      return null;
    });
    if (!result) return;
    setNotice(
      result.ok
        ? `${p.title} 通了 · ${seconds(result.ms)} · 回「${result.text}」`
        : `${p.title} 失败：${result.error || `HTTP ${result.code}`}`,
    );
  }

  // 申请 key 走 freehubOpen：openLink 只放行 workbuddy 站内地址，用它开各家平台会被拒。
  // 拒绝是异步的，必须 catch —— 否则界面上就是「点了没反应」。
  async function openApply(p: FreeHubProvider) {
    if (!p.apply) return;
    // 先在本地给一句，再去开浏览器：open 是异步拉起的，万一系统那头没反应，
    // 界面上至少能看到「点收到了」，不会像按了空按钮。
    setNotice(`正在打开 ${p.title} 的申请页…`);
    try {
      await api.freehubOpen(p.apply);
      setNotice(`已在浏览器打开 ${p.title} 的申请页：${p.apply}`);
    } catch (error) {
      onError(`${p.title} 的申请页打不开：${String(error)}`);
    }
  }

  async function saveDraft(p: FreeHubProvider) {
    const patch: Record<string, unknown> = {
      endpoint: draft.endpoint.trim(),
      proxy: draft.proxy.trim(),
      note: draft.note.trim(),
      models: draft.models
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean),
    };
    if (draft.key.trim()) patch.apiKey = draft.key.trim();
    await run(`edit:${p.name}`, () => api.freehubSave(p.name, patch), `${p.title} 已更新`);
    setDraft((prev) => ({ ...prev, key: "" }));
  }

  async function addChannel() {
    const name = form.name.trim();
    if (!name || !form.endpoint.trim()) {
      onError("渠道标识和 Endpoint 都是必填");
      return;
    }
    const models = form.models
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean);
    if (!models.length) {
      onError("至少填一个模型 ID");
      return;
    }
    await run(
      "add",
      () =>
        api.freehubSave(
          name,
          {
            endpoint: form.endpoint.trim(),
            apiKey: form.key.trim(),
            proxy: form.proxy.trim(),
            note: form.note.trim(),
            models,
          },
          true,
        ),
      `${name} 已加入渠道清单`,
    );
    setForm({ name: "", endpoint: "", key: "", proxy: "", models: "", note: "" });
    setAdding(false);
  }

  // ---------------------------------------------------------------- 头部

  const head = current ? (
    <div className="ferry-head">
      <div className="fh-pagehead">
        <button className="fh-back" onClick={() => setSelected(null)} title="返回渠道列表">
          ←
        </button>
        <div>
          <div className="ferry-title">
            {current.title}
            <span className="ferry-sub fh-mono">{current.name}</span>
            {current.disabled ? (
              <span className="pill danger">已停用</span>
            ) : current.inTier.active > 0 ? (
              <span className="pill ok">队列 {current.inTier.active}</span>
            ) : null}
            {!current.keyPresent && !current.allowNoKey && <span className="pill warn">缺 key</span>}
          </div>
          <div className="ferry-sub">{current.quota || current.note || "—"}</div>
        </div>
      </div>
      <div className="ferry-actions">
        <button
          className="btn sm"
          disabled={busy !== null || (!current.keyPresent && !current.allowNoKey)}
          title={!current.keyPresent && !current.allowNoKey ? "这家还没有 key，先在上面填上再测" : "真发一次请求，看通不通、多快"}
          onClick={() => void testOne(current)}
        >
          测试连接
        </button>
        {current.apply && (
          <button className="btn sm ghost" onClick={() => void openApply(current)}>
            申请 key ↗
          </button>
        )}
        <button className="btn sm" onClick={() => setSelected(null)}>
          返回列表
        </button>
      </div>
    </div>
  ) : (
    <div className="ferry-head">
      <div>
        <div className="ferry-title">
          <span className={`dot ${offline ? "danger" : "ok"}`} />
          {offline ? "网关未运行" : "免费渠道网关运行中"}
          {!offline && state?.tier ? (
            <span className="ferry-sub">
              {TIER_TEXT[state.tier] ?? state.tier} 分组 · 队列 {state.queueSize ?? 0} · 跳过{" "}
              {state.skipped ?? 0}
            </span>
          ) : null}
        </div>
        <div className="ferry-sub">
          {loading && !state
            ? "正在读取渠道清单…"
            : offline
              ? state?.message ?? "本机网关（127.0.0.1:8901）没有响应"
              : `${providers.length} 家渠道 · ${ready} 家已就绪 · ${missing} 家缺 key · 已跑 ${uptime(state?.uptimeSec)}`}
        </div>
      </div>
      <div className="ferry-actions">
        {offline ? (
          <button
            className="btn sm primary"
            disabled={busy !== null}
            onClick={() => run("start", api.freehubStart, "网关已拉起")}
          >
            {busy === "start" && <span className="spin" />}
            启动网关
          </button>
        ) : (
          state?.tiers?.map((tier) => (
            <button
              key={tier}
              className={`btn sm${state.tier === tier ? " primary" : ""}`}
              disabled={busy !== null}
              onClick={() => run("tier", () => api.freehubTier(tier), `已切到「${TIER_TEXT[tier] ?? tier}」`)}
            >
              {TIER_TEXT[tier] ?? tier}
            </button>
          ))
        )}
        <button className={`btn sm${showDiag ? " primary" : ""}`} onClick={() => void toggleDiag()}>
          诊断
        </button>
        <button className="btn sm" disabled={busy !== null} onClick={() => void refresh()}>
          刷新
        </button>
      </div>
    </div>
  );

  // 没有任何可用渠道时，WorkBuddy 里选了模型也只会 401 —— 与其让人自己试，不如直接说清下一步。
  const noChannel =
    !loading && !offline && providers.length > 0 && ready === 0
    && !state?.orcaterm?.credential && !state?.doubao?.credential && !state?.antigravity?.credential;
  const needRestart = diag?.workbuddyRunning === true && diag?.inject?.ok === true;
  const guidance = (
    <>
      {noChannel && (
        <div className="banner">
          现在没有任何可用渠道：十几家 API 渠道都没填 key，豆包 / OrcaTerm / Antigravity 也都未登录。
          最快的是去「豆包」标签点登录（免费额度、扫码即可），或去「OrcaTerm」扫码。
          在登录之前，WorkBuddy 里选到本应用的模型都会返回 401。
        </div>
      )}
      {needRestart && (
        <div className="banner">
          模型已写入 WorkBuddy 的配置，但 WorkBuddy 正在运行 —— 它只在启动时读一次模型列表，
          <b>重启 WorkBuddy</b> 之后新模型才会出现。
        </div>
      )}
    </>
  );

  // ---------------------------------------------------------------- 配置页

  if (current) {
    const verify = VERIFY_TEXT[current.verify] ?? VERIFY_TEXT.none;
    const cooling = current.inTier.cooldownLeftMs > 0;
    return (
      <>
        {head}
        {guidance}
        {notice && <div className="banner">{notice}</div>}

        <div className="panel settings">
          <div className="fh-facts">
            <div className="fh-fact">
              <span>免费额度</span>
              <b>{current.quota || "—"}</b>
            </div>
            <div className="fh-fact">
              <span>验证门槛</span>
              <b>
                <span className={`pill ${verify.tone === "plain" ? "" : verify.tone}`}>{verify.text}</span>
              </b>
            </div>
            <div className="fh-fact">
              <span>网络</span>
              <b>{current.network || "—"}</b>
            </div>
            <div className="fh-fact">
              <span>分组归属</span>
              <b>
                {TIER_TEXT[state?.tier ?? ""] ?? state?.tier ?? "—"} · 队列 {current.inTier.active}
              </b>
            </div>
            <div className="fh-fact">
              <span>试连统计</span>
              <b>
                成功 {current.stats.ok} · 失败 {current.stats.fail}
                {cooling ? ` · 冷却 ${Math.ceil(current.inTier.cooldownLeftMs / 1000)}s` : ""}
              </b>
            </div>
            <div className="fh-fact">
              <span>模型</span>
              <b>{current.models.length} 个</b>
            </div>
          </div>

          <div className="fh-block">
            <div className="fh-block-head">
              <div className="row-label">连接设置</div>
              <div className="row-hint">
                改完点「保存设置」才写入网关的 config.json，随即重建队列，不需要重启。
              </div>
            </div>
            <div className="field">
              <label>Endpoint（OpenAI 兼容，完整路径）</label>
              <input
                className="fh-mono"
                placeholder="https://api.example.com/v1/chat/completions"
                value={draft.endpoint}
                onChange={(event) => setDraft({ ...draft, endpoint: event.target.value })}
              />
              <div className="row-hint">
                要把账号标识拼进 URL 的上游（Cloudflare 的 ACCOUNT_ID、Azure 的 deployment）就写在这里，整条填满。
              </div>
            </div>
            <div className="fh-grid2">
              <div className="field">
                <label>API Key（留空 = 不修改）</label>
                <input
                  type="password"
                  placeholder={current.keyPresent ? current.apiKeyMasked : "未填写"}
                  value={draft.key}
                  onChange={(event) => setDraft({ ...draft, key: event.target.value })}
                />
              </div>
              <div className="field">
                <label>代理（留空为直连）</label>
                <input
                  className="fh-mono"
                  placeholder="http://127.0.0.1:10808"
                  value={draft.proxy}
                  onChange={(event) => setDraft({ ...draft, proxy: event.target.value })}
                />
              </div>
            </div>
          </div>

          <div className="fh-block">
            <div className="fh-block-head">
              <div className="row-label">模型清单</div>
              <div className="row-hint">
                每行一个模型 ID，可写成「id | 显示名」。这里的清单就是网关往这家平台发请求时认的模型；
                改完保存后，新加的模型同时进「全部」分组。
              </div>
            </div>
            <div className="field">
              <textarea
                className="fh-textarea"
                rows={Math.min(12, Math.max(4, draft.models.split("\n").length + 1))}
                placeholder={"z-ai/glm-5.3-flash | GLM 5.3 Flash\nminimaxai/minimax-m3.1-flash"}
                value={draft.models}
                onChange={(event) => setDraft({ ...draft, models: event.target.value })}
              />
            </div>
          </div>

          <div className="fh-block">
            <div className="fh-block-head">
              <div className="row-label">备注</div>
              <div className="row-hint">额度口径、验证方式、注意事项，列表里也会显示这一行。</div>
            </div>
            <div className="field">
              <input
                value={draft.note}
                onChange={(event) => setDraft({ ...draft, note: event.target.value })}
              />
            </div>
          </div>

          <div className="fh-editor-foot">
            <button
              className="btn sm primary"
              disabled={busy !== null}
              onClick={() => void saveDraft(current)}
            >
              {busy === `edit:${current.name}` ? <span className="spin" /> : null}
              保存设置
            </button>
            <button className="btn sm" onClick={() => setSelected(null)}>
              取消
            </button>
            <button
              className="btn sm"
              disabled={busy !== null}
              onClick={() =>
                void run(
                  `toggle:${current.name}`,
                  () => api.freehubSave(current.name, { disabled: !current.disabled }),
                  `${current.title} 已${current.disabled ? "启用" : "停用"}`,
                )
              }
            >
              {current.disabled ? "启用此渠道" : "停用此渠道"}
            </button>
            {!current.builtin && (
              <button
                className="btn sm danger"
                disabled={busy !== null}
                onClick={() => void run(`del:${current.name}`, () => api.freehubDelete(current.name), `${current.title} 已删除`)}
              >
                删除渠道
              </button>
            )}
          </div>
        </div>
      </>
    );
  }

  // ---------------------------------------------------------------- 列表

  return (
    <>
      {head}
      {notice && <div className="banner">{notice}</div>}

      {offline ? (
        <div className="panel settings">
          <div className="row">
            <div>
              <div className="row-label">这个面板管的是什么</div>
              <div className="row-hint">
                十几家「有免费额度但要验证」的平台（魔搭、美团 LongCat、商汤、Groq……）
                收在一个 OpenAI 兼容端点后面，key 就在本机网关的 config.json 里。
                网关没起来时，这一步先跳过，不影响签到与摆渡。
              </div>
            </div>
          </div>
          <div className="row">
            <div>
              <div className="row-label">手动启动</div>
              <div className="row-hint">
                终端里执行 <code>cd ~/.wb-checkin-gateway &amp;&amp; ./run.sh</code>
                （Windows 双击同目录下的 <code>run.cmd</code>），回来点刷新即可。
              </div>
            </div>
          </div>
        </div>
      ) : (
        <>
          {showDiag && diag && (
            <div className="panel settings">
              <div className="fh-block">
                <div className="fh-block-head">
                  <div className="row-label">运行诊断</div>
                  <div className="row-hint">
                    换台电脑不好用先看这里：网关跑的是哪版脚本、模型有没有写进 WorkBuddy、平台登没登录。
                  </div>
                </div>
                <div className="fh-facts">
                  <div className="fh-fact">
                    <span>网关</span>
                    <b>{diag.gateway.online ? `在线 · ${uptime(diag.gateway.uptimeSec ?? 0)}` : "未运行"}</b>
                  </div>
                  <div className="fh-fact">
                    <span>运行 / 脚本版本</span>
                    <b>
                      {diag.gateway.runningVersion || "—"} / {diag.gateway.bundleVersion || "—"}
                    </b>
                  </div>
                  <div className="fh-fact">
                    <span>models.json</span>
                    <b>{diag.paths.modelsExists ? `${diag.paths.modelsCount} 条` : "文件不存在"}</b>
                  </div>
                  <div className="fh-fact">
                    <span>上次注入</span>
                    <b>{diag.inject ? (diag.inject.ok ? "成功" : "失败") : "还没执行过"}</b>
                  </div>
                  <div className="fh-fact">
                    <span>有 key 的渠道</span>
                    <b>{diag.providersWithKey} 家</b>
                  </div>
                  <div className="fh-fact">
                    <span>内置 node</span>
                    <b>{diag.gateway.nodeRunnable ? "可执行" : "跑不起来"}</b>
                  </div>
                </div>
                {diag.gateway.online &&
                  diag.gateway.runningVersion &&
                  diag.gateway.bundleVersion &&
                  diag.gateway.runningVersion !== diag.gateway.bundleVersion && (
                    <div className="banner">
                      网关进程跑的还是旧脚本（运行 {diag.gateway.runningVersion} ≠ 脚本 {diag.gateway.bundleVersion}），
                      新增接口会全部 404 —— 点下面的「重启网关」。
                    </div>
                  )}
                <div className="fh-facts">
                  <div className="fh-fact">
                    <span>豆包</span>
                    <b>
                      {diag.platforms.doubao.cookie
                        ? `已登录 · 模型 ${diag.platforms.doubao.catalog || diag.platforms.doubao.fallbackModels} 个`
                        : "未登录（模型清单要登录后才拉得到）"}
                    </b>
                  </div>
                  <div className="fh-fact">
                    <span>OrcaTerm</span>
                    <b>
                      {diag.platforms.orcaterm.expired
                        ? "登录已失效"
                        : (diag.platforms.orcaterm.loggedIn ?? diag.platforms.orcaterm.cookie)
                          ? "已登录"
                          : "未登录"}
                    </b>
                  </div>
                  <div className="fh-fact">
                    <span>Antigravity</span>
                    <b>
                      {diag.platforms.antigravity.expired
                        ? "不可用（地区/授权）"
                        : (diag.platforms.antigravity.loggedIn ?? diag.platforms.antigravity.refreshToken)
                          ? "已授权"
                          : "未登录"}
                    </b>
                  </div>
                  <div className="fh-fact">
                    <span>注入结果</span>
                    <b>{diag.inject?.message || "—"}</b>
                  </div>
                </div>
                <div className="row-hint fh-mono">{diag.paths.models}</div>
                <div className="ferry-actions">
                  <button
                    className="btn sm"
                    disabled={busy !== null}
                    onClick={() => run("diag:restart", api.freehubStart, "网关已重启").then(() => void toggleDiag())}
                  >
                    {busy === "diag:restart" && <span className="spin" />}
                    重启网关
                  </button>
                  <button
                    className="btn sm"
                    disabled={busy !== null}
                    onClick={() => run("diag:inject", api.freehubInjectModels, "模型已重新写入").then(() => void toggleDiag())}
                  >
                    {busy === "diag:inject" && <span className="spin" />}
                    重写模型
                  </button>
                  <button className="btn sm" onClick={() => void copyDiag()}>
                    复制诊断
                  </button>
                </div>

                <div className="row-label">渠道自检</div>
                <div className="row-hint">
                  面板写「已登录」不代表上游认这份凭据。这里四个渠道各真发一次最小对话，通了才算能用。
                </div>
                <div className="ferry-actions">
                  <button className="btn sm" disabled={busy !== null} onClick={() => void runSelfcheck()}>
                    {busy === "diag:check" && <span className="spin" />}
                    {busy === "diag:check" ? "自检中…" : check ? "重新自检" : "开始自检"}
                  </button>
                  {check && !check.offline && (
                    <span className="row-hint">
                      {check.passed}/{check.total} 通过
                    </span>
                  )}
                </div>
                {check?.offline && <div className="banner">{check.message}</div>}
                {check && check.items.length > 0 && (
                  <>
                    <div className="fh-facts">
                      {check.items.map((item) => (
                        <div className="fh-fact" key={item.key}>
                          <span>{item.title}</span>
                          <b>
                            {item.status === "ok"
                              ? `通过 · ${item.ms}ms${item.upstream ? ` · ${item.upstream}` : ""}`
                              : item.status === "skipped"
                                ? "跳过"
                                : `失败${item.code ? ` ${item.code}` : ""} · ${item.ms}ms`}
                          </b>
                        </div>
                      ))}
                    </div>
                    {check.items.some((item) => item.status !== "ok") && (
                      <div>
                        {check.items
                          .filter((item) => item.status !== "ok")
                          .map((item) => (
                            <div className="row-hint" key={item.key}>
                              <b>{item.title}</b>：{item.message}
                            </div>
                          ))}
                      </div>
                    )}
                  </>
                )}
                {(() => {
                  const text = [
                    diag.logs.stderr && `── 网关 stderr ──\n${diag.logs.stderr}`,
                    diag.logs.doubaoLogin && `── 豆包登录器 ──\n${diag.logs.doubaoLogin}`,
                    diag.logs.orcatermLogin && `── OrcaTerm 登录器 ──\n${diag.logs.orcatermLogin}`,
                  ]
                    .filter(Boolean)
                    .join("\n\n");
                  return text ? <pre className="fh-mono diag-log">{text}</pre> : null;
                })()}
              </div>
            </div>
          )}
          <div className="panel">
            {providers.map((p) => {
              const verify = VERIFY_TEXT[p.verify] ?? VERIFY_TEXT.none;
              const needsKey = !p.keyPresent && !p.allowNoKey;
              return (
                <div
                  key={p.name}
                  className="fh-row"
                  onClick={() => enterConfig(p)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      enterConfig(p);
                    }
                  }}
                >
                  <div className="fh-main">
                    <div className="row-label">
                      {p.title}
                      <span className="ferry-sub">{p.name}</span>
                      {p.disabled ? (
                        <span className="pill danger">已停用</span>
                      ) : p.inTier.active > 0 ? (
                        <span className="pill ok">队列 {p.inTier.active}</span>
                      ) : null}
                    </div>
                    <div className="row-hint">
                      {p.quota || p.note || "—"}
                      <span className="fh-tags">
                        <span className={`pill ${verify.tone === "plain" ? "" : verify.tone}`}>{verify.text}</span>
                        {p.network && <span className="pill">{p.network}</span>}
                        <span className="pill">{p.models.length} 个模型</span>
                        {p.inTier.cooling > 0 && <span className="pill warn">冷却 {p.inTier.cooling}</span>}
                      </span>
                    </div>
                  </div>
                  <div className="row-actions fh-actions" onClick={(event) => event.stopPropagation()}>
                    {needsKey && (
                      <>
                        <input
                          className="fh-input"
                          type="password"
                          placeholder="粘贴 API Key"
                          value={keys[p.name] ?? ""}
                          onChange={(event) => setKeys((prev) => ({ ...prev, [p.name]: event.target.value }))}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") void saveKey(p);
                          }}
                        />
                        <button
                          className="btn sm primary"
                          disabled={busy !== null}
                          onClick={() => void saveKey(p)}
                        >
                          {busy === `key:${p.name}` ? <span className="spin" /> : null}
                          保存
                        </button>
                      </>
                    )}
                    <button
                      className="btn sm"
                      disabled={busy !== null || needsKey}
                      title={needsKey ? "这家还没有 key，填上再测" : "真发一次请求，看通不通、多快"}
                      onClick={() => void testOne(p)}
                    >
                      测试
                    </button>
                    <button className="btn sm" disabled={busy !== null} onClick={() => enterConfig(p)}>
                      配置 ›
                    </button>
                  </div>
                </div>
              );
            })}
          </div>

          <div className="panel settings" style={{ marginTop: 12 }}>
            <div className="row">
              <div>
                <div className="row-label">
                  新增自定义渠道
                  <span className="ferry-sub">任意 OpenAI 兼容上游，保存即入队</span>
                </div>
                <div className="row-hint">
                  模型会自动进「全部」分组；带 key 保存的，前两个模型同时进当前分组。
                </div>
              </div>
              <div className="row-actions">
                <button className="btn sm" onClick={() => setAdding((value) => !value)}>
                  {adding ? "收起" : "展开填写"}
                </button>
              </div>
            </div>
            {adding && (
              <div className="fh-editor">
                <div className="fh-grid2">
                  <div className="field">
                    <label>渠道标识（英文小写）</label>
                    <input
                      placeholder="如 moonshot"
                      value={form.name}
                      onChange={(event) => setForm({ ...form, name: event.target.value })}
                    />
                  </div>
                  <div className="field">
                    <label>Endpoint</label>
                    <input
                      placeholder="https://api.example.com/v1/chat/completions"
                      value={form.endpoint}
                      onChange={(event) => setForm({ ...form, endpoint: event.target.value })}
                    />
                  </div>
                  <div className="field">
                    <label>API Key</label>
                    <input
                      type="password"
                      value={form.key}
                      onChange={(event) => setForm({ ...form, key: event.target.value })}
                    />
                  </div>
                  <div className="field">
                    <label>代理（可选）</label>
                    <input
                      placeholder="http://127.0.0.1:10808"
                      value={form.proxy}
                      onChange={(event) => setForm({ ...form, proxy: event.target.value })}
                    />
                  </div>
                </div>
                <div className="field">
                  <label>模型 ID（每行一个）</label>
                  <textarea
                    className="fh-textarea"
                    rows={3}
                    value={form.models}
                    onChange={(event) => setForm({ ...form, models: event.target.value })}
                  />
                </div>
                <div className="field">
                  <label>备注（额度 / 验证说明）</label>
                  <input
                    value={form.note}
                    onChange={(event) => setForm({ ...form, note: event.target.value })}
                  />
                </div>
                <div className="fh-editor-foot">
                  <button className="btn sm primary" disabled={busy !== null} onClick={() => void addChannel()}>
                    {busy === "add" ? <span className="spin" /> : null}
                    保存并加入队列
                  </button>
                  <button className="btn sm" onClick={() => setAdding(false)}>
                    取消
                  </button>
                </div>
              </div>
            )}
          </div>
        </>
      )}
    </>
  );
}

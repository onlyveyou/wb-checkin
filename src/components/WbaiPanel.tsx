import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../api";
import type { WbaiModels, WbaiStatus, WbaiVerdict } from "../types";

const POLL_MS = 2500;
const LOG_POLL_MS = 6000;
/// 登录轮询比状态轮询快：用户点完浏览器授权回来，等 2 秒内就该看到「已登录」。
const LOGIN_POLL_MS = 2000;

/** 探测结论的中文口径。后端已经归好类，这里只负责显示与配色。 */
function verdictText(verdict?: WbaiVerdict | null): {
  text: string;
  tone: "ok" | "warn" | "danger" | "plain";
} {
  if (!verdict) {
    return { text: "未检测", tone: "plain" };
  }
  if (verdict.ok) {
    return { text: "可用", tone: "ok" };
  }
  switch (verdict.status) {
    case "not_allowed":
      return { text: "账号不支持", tone: "danger" };
    case "quota":
      return { text: "额度不足", tone: "danger" };
    case "rate_limit":
      return { text: "限流", tone: "warn" };
    case "timeout":
      return { text: "超时", tone: "warn" };
    case "unreachable":
      return { text: "连不上", tone: "danger" };
    case "no_login":
      return { text: "未登录", tone: "danger" };
    case "not_found":
      return { text: "无此模型", tone: "danger" };
    case "illegal":
      return { text: "被判非法", tone: "danger" };
    default:
      return { text: "不可用", tone: "danger" };
  }
}

function formatContext(tokens?: number | null): string {
  if (!tokens) {
    return "—";
  }
  if (tokens >= 1_000_000) {
    return `${Math.round(tokens / 100_000) / 10}M`;
  }
  return `${Math.round(tokens / 1000)}K`;
}

/**
 * 国际版摆渡面板。
 *
 * 与「模型摆渡」并列但互不影响：那边借 opencode 的免费模型，这边借的是**国际版账号
 * 自己的模型额度**。两者端口不同，可以同时开。
 *
 * 交互照抄模型摆渡（状态轮询 + 日志轮询 + 一个 busy 状态管住所有按钮），
 * 登录照抄国内版账号登录（申请 → 打开系统浏览器 → 前端轮询），用户不需要学新东西。
 */
export default function WbaiPanel({
  active,
  onError,
}: {
  active: boolean;
  onError: (text: string) => void;
}) {
  const [status, setStatus] = useState<WbaiStatus | null>(null);
  const [catalog, setCatalog] = useState<WbaiModels | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  /// 「模型摆渡」是否也在跑。两者可共存，但摆渡那套 node + opencode 很吃内存，
  /// 同时开着时得让用户看得见这件事，否则只会觉得「电脑怎么变卡了」。
  const [ferryOn, setFerryOn] = useState(false);
  /// 正在等待浏览器登录时的会话。为 null 表示没有进行中的登录。
  const [login, setLogin] = useState<{ id: string; expiresIn: number } | null>(null);
  const activeRef = useRef(active);
  activeRef.current = active;

  const pull = useCallback(async () => {
    try {
      setStatus(await api.wbaiStatus());
    } catch (error) {
      onError(String(error));
    }
  }, [onError]);

  /// 目录只在切页和操作后拉，不跟着 2.5 秒的状态轮询走。
  const pullCatalog = useCallback(async () => {
    try {
      setCatalog(await api.wbaiModels());
    } catch (error) {
      onError(String(error));
    }
    try {
      setFerryOn((await api.ferryStatus()).running === true);
    } catch {
      // 摆渡状态读不到不影响本页任何功能，静默当作没开。
    }
  }, [onError]);

  useEffect(() => {
    if (!active) {
      return;
    }
    pull();
    pullCatalog();
    const timer = window.setInterval(() => {
      if (activeRef.current) {
        pull();
      }
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [active, pull, pullCatalog]);

  useEffect(() => {
    if (!active) {
      return;
    }
    let alive = true;
    const load = async () => {
      try {
        const tail = await api.wbaiLogs(80);
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

  // 登录轮询。只在有活跃会话时跑，登录一结束就停，不在后台空转。
  const loginId = login?.id ?? null;
  useEffect(() => {
    if (!loginId) {
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const poll = await api.wbaiLoginPoll(loginId);
        if (!alive) {
          return;
        }
        if (!poll.done) {
          setLogin((prev) => (prev ? { ...prev, expiresIn: poll.expiresIn } : prev));
          return;
        }
        setLogin(null);
        if (poll.status === "ok") {
          setNotice(poll.account ? `已登录 ${poll.account}` : "已登录");
          await pull();
        } else {
          onError(poll.error ?? "登录失败");
        }
      } catch (error) {
        if (alive) {
          onError(String(error));
        }
      }
    };
    tick();
    const timer = window.setInterval(tick, LOGIN_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [loginId, pull, onError]);

  const run = async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    setNotice(null);
    try {
      await action();
      await Promise.all([pull(), pullCatalog()]);
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  };

  const startLogin = async () => {
    setBusy("login");
    setNotice(null);
    try {
      const started = await api.wbaiLoginStart();
      await api.wbaiLoginOpen(started.loginId);
      setLogin({ id: started.loginId, expiresIn: started.expiresIn });
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  };

  const cancelLogin = async () => {
    if (!login) {
      return;
    }
    try {
      await api.wbaiLoginCancel(login.id);
    } catch {
      // 取消失败无所谓：服务端超时后会话自己会结束。
    }
    setLogin(null);
  };

  const probe = async (scope: "all" | "selected", force: boolean) => {
    setBusy(scope === "all" ? "probe-all" : "probe");
    setNotice(null);
    try {
      const result = await api.wbaiProbe(scope, force);
      if (result.ok === false) {
        throw new Error(result.error ?? "检测失败");
      }
      const list = Object.values(result.results ?? {});
      const passed = list.filter((verdict) => verdict.ok).length;
      setNotice(`检测 ${list.length} 个模型：${passed} 个可用`);
      await Promise.all([pull(), pullCatalog()]);
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(null);
    }
  };

  const loggedIn = status?.loggedIn === true;
  const running = status?.running === true;
  const models = catalog?.models ?? [];
  const selected = catalog?.selected ?? 0;

  return (
    <>
      <div className="ferry-head">
        <div>
          <div className="ferry-title">
            <span className={`dot ${running ? "ok" : loggedIn ? "warn" : "plain"}`} />
            {running ? "国际版服务运行中" : loggedIn ? "已登录，服务未启动" : "未登录国际版账号"}
          </div>
          <div className="ferry-sub">
            {running
              ? `${status?.endpoint ?? ""} · ${status?.account ?? "未知账号"}`
              : loggedIn
                ? "启动后只监听 127.0.0.1；不启动时不占用任何端口"
                : "登录后即可把国际版的模型接进国内版，国内账户与积分都不受影响"}
            {catalog
              ? ` · 共 ${catalog.total} 个模型 · ${catalog.free} 个免费 · 已选 ${catalog.selected}`
              : null}
          </div>
        </div>
        <div className="ferry-actions">
          {running ? (
            <button
              className="btn sm"
              disabled={busy !== null}
              onClick={() => run("stop", api.wbaiStop)}
            >
              {busy === "stop" && <span className="spin" />}
              停止
            </button>
          ) : (
            <button
              className="btn sm primary"
              disabled={busy !== null || !loggedIn}
              onClick={() => run("start", api.wbaiStart)}
            >
              {busy === "start" && <span className="spin" />}
              {busy === "start" ? "启动中" : "启动"}
            </button>
          )}
        </div>
      </div>

      {notice && <div className="ferry-note">{notice}</div>}

      {running && ferryOn && (
        <div className="ferry-note">
          「模型摆渡」也在运行。实测占用：它那套 Node 与 opencode 加起来约 860 MB，
          本模块全在应用进程内、不新增进程（应用整体约 100 MB）。两个转发口各占一个端口
          （41980 / 8799），互不抢占；内存真紧张时先停摆渡，不必停这个。
        </div>
      )}

      <div className="section-head">
        <h2>国际版账号</h2>
      </div>
      <div className="panel settings">
        <div className="row">
          <div>
            <div className="row-label">
              {login ? "等待浏览器登录" : loggedIn ? status?.account ?? "已登录" : "未登录"}
            </div>
            <div className="row-hint">
              {login
                ? `登录页已在系统浏览器打开，完成后这里会自动变成「已登录」。剩余 ${login.expiresIn} 秒。`
                : loggedIn
                  ? "登录态由本应用自己走一次独立授权拿到，和官方客户端的登录互不影响，不会把它挤下线。"
                  : "点右侧按钮会在浏览器打开国际版登录页。这一步只需要你的账号密码，不需要任何客户端里的文件。"}
            </div>
            {loggedIn && !login && (
              <div className="path">令牌只存在本机，权限 0600；有效期到期前可自动续期。</div>
            )}
          </div>
          {login ? (
            <button className="btn sm" disabled={busy !== null} onClick={cancelLogin}>
              取消
            </button>
          ) : loggedIn ? (
            <button
              className="btn sm"
              disabled={busy !== null}
              onClick={() =>
                run("logout", async () => {
                  await api.wbaiLogout();
                })
              }
            >
              {busy === "logout" && <span className="spin" />}
              退出登录
            </button>
          ) : (
            <button className="btn sm primary" disabled={busy !== null} onClick={startLogin}>
              {busy === "login" && <span className="spin" />}
              登录国际版
            </button>
          )}
        </div>
        <div className="row">
          <div>
            <div className="row-label">转发地址</div>
            <div className="row-hint">
              {running ? status?.endpoint ?? "—" : "服务未启动时这个地址不可用"}
            </div>
            <div className="path">{status?.dataDir ?? "—"}</div>
          </div>
        </div>
      </div>

      <div className="section-head">
        <h2>模型</h2>
        <span className="sub">{catalog?.source ?? "正在读取模型目录…"}</span>
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
              <span className="num">倍率</span>
              <span className="num">上下文</span>
              <span className="state">状态</span>
              <span className="switch-cell">导入</span>
            </div>
            {models.map((model) => {
              const info = verdictText(model.verdict);
              return (
                <div className="ferry-row" key={model.id}>
                  <span className="who">
                    <span className="ferry-name">{model.name}</span>
                    {model.free && <span className="ferry-tag">免费</span>}
                    {!model.tools && <span className="ferry-tag quiet">无工具</span>}
                    {model.images && <span className="ferry-tag quiet">图片</span>}
                  </span>
                  <span className="num">{model.credits}</span>
                  <span className="num">{formatContext(model.input)}</span>
                  <span className={`state ${info.tone}`} title={model.verdict?.error ?? ""}>
                    {info.text}
                  </span>
                  <span className="switch-cell">
                    <button
                      className="switch tiny"
                      data-on={model.selected}
                      aria-label={`切换 ${model.name}`}
                      disabled={busy !== null}
                      onClick={() =>
                        run(`sel:${model.id}`, () => api.wbaiSelection(model.id, !model.selected))
                      }
                    />
                  </span>
                </div>
              );
            })}
          </div>
        )}
      </div>

      <div className="ferry-actions" style={{ marginTop: 10 }}>
        <button className="btn sm" disabled={busy !== null} onClick={() => pullCatalog()}>
          刷新目录
        </button>
        <button
          className="btn sm"
          disabled={busy !== null || selected === 0}
          onClick={() => probe("selected", false)}
          title="只测已勾选的模型，10 分钟内的结论直接用缓存"
        >
          {busy === "probe" && <span className="spin" />}
          检测勾选
        </button>
        <button
          className="btn sm"
          disabled={busy !== null || models.length === 0}
          onClick={() => probe("all", true)}
          title="强制重测全部模型：忽略缓存，一眼看清哪些真能用"
        >
          {busy === "probe-all" && <span className="spin" />}
          检测全部
        </button>
      </div>

      <div className="path" style={{ marginTop: 8 }}>
        检测会真的调用一次模型（约 16 token）：免费模型不花积分，付费模型只花很少。
        结论缓存 10 分钟，「检测全部」会忽略缓存重测。
      </div>

      {selected > 0 && catalog && catalog.ready === 0 && (
        <div className="ferry-note">
          还没检测过。倍率为「免费」的模型也不一定都能用 —— 上游会按账号类型放行，
          点一次「检测勾选」就能把不能用的挑出来，避免导进去后发现调不通。
        </div>
      )}

      <div className="section-head">
        <h2>接入国内版</h2>
      </div>
      <div className="panel settings">
        <div className="row">
          <div>
            <div className="row-label">写入模型列表</div>
            <div className="row-hint">
              把勾选的 {selected} 个模型写进国内版的模型配置，每个模型一条，可以单独启用。
              只增删本模块自己标记过的条目，你自己加的模型一条都不改。写完需要完全退出并重开国内版才生效。
            </div>
          </div>
          <button
            className="btn sm"
            disabled={busy !== null || selected === 0}
            onClick={() =>
              run("import", async () => {
                const result = await api.wbaiImport();
                if (result.ok === false) {
                  throw new Error(result.error ?? "写入失败");
                }
                const skipped = result.skipped?.length
                  ? `，跳过 ${result.skipped.length} 个（id 和你已有配置冲突）`
                  : "";
                setNotice(`已写入 ${result.count ?? 0} 个模型${skipped}`);
              })
            }
          >
            {busy === "import" && <span className="spin" />}
            导入 {selected} 个
          </button>
        </div>
        <div className="row">
          <div>
            <div className="row-label">它和「模型摆渡」的关系</div>
            <div className="row-hint">
              模型摆渡借的是 opencode 的免费模型，要跑 Node 子进程（实测 Node 179 MB + opencode 678 MB）；
              这里全部在本进程内完成，不起子进程，实测不给应用增加可观测的内存。两者端口不同
              （41980 / 8799），可以同时开。
            </div>
          </div>
        </div>
        <div className="row">
          <div>
            <div className="row-label">模型目录来源</div>
            <div className="row-hint">
              读的是国际版客户端自己缓存的产品配置（含积分倍率与能力位），不联网、不解密。
              官方上下架模型或你换套餐，打开一次国际版客户端再点「刷新目录」就会跟着变。
            </div>
            <div className="path">{catalog?.catalogDir ?? "—"}</div>
          </div>
        </div>
      </div>

      <div className="section-head">
        <h2>运行日志</h2>
        <span className="sub">{logs.length} 行</span>
      </div>
      <div className="panel logs">
        {logs.length === 0 ? (
          <div className="empty">
            <div className="hint">{running ? "还没有日志" : "启动后这里会显示转发与登录的日志"}</div>
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

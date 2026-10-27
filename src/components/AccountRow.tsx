import { useState } from "react";
import type { Account, CreditItem } from "../types";

interface Props {
  account: Account;
  /** 在账号列表里的位置（从 1 开始），显示在头像上。 */
  index: number;
  /** 该账号是否就是客户端当前登录的账号。 */
  active: boolean;
  /** 该账号是否在承担当前消耗（用「只换额度」切过之后可能与登录账号不是同一个）。 */
  billing: boolean;
  today: string;
  busy: boolean;
  disabled: boolean;
  credits: CreditItem | null;
  creditsBusy: boolean;
  switching: boolean;
  onCheckin: () => void;
  onDelete: () => void;
  onLoadCredits: () => void;
  onSwitch: () => void;
}

/** 积分数量级从个位到几千，保留两位小数并加千分位便于扫读。 */
function formatAmount(value: number): string {
  return value.toLocaleString("zh-CN", {
    minimumFractionDigits: 0,
    maximumFractionDigits: 2,
  });
}

/** 到期时间只到日即可，官方给的是 `YYYY-MM-DD HH:MM:SS`。 */
function formatExpiry(value: string): string {
  return value.slice(0, 10);
}

export default function AccountRow({
  account,
  index,
  active,
  billing,
  today,
  busy,
  disabled,
  credits,
  creditsBusy,
  switching,
  onCheckin,
  onDelete,
  onLoadCredits,
  onSwitch,
}: Props) {
  const [expanded, setExpanded] = useState(false);

  const name = account.nickname || account.email || account.uid || "未命名账号";

  const checkedToday = account.lastCheckinDate === today;
  const failedToday = checkedToday && account.lastCheckinResult === "error";
  const doneToday = checkedToday && !failedToday;

  const metaParts = [
    account.uid ? `uid ${account.uid}` : null,
    account.email,
    account.domain,
  ].filter(Boolean);

  const meta = failedToday
    ? account.lastCheckinMessage || "签到失败"
    : metaParts.join(" · ");

  const hasBreakdown = Boolean(credits?.ok && credits.packages && credits.packages.length > 0);
  const creditsFailed = credits !== null && credits.ok === false;

  // 只让「有余额可看」的结果可点开，避免点出一个空面板
  const creditsLabel = creditsBusy
    ? "查询中"
    : credits === null
      ? "查积分"
      : credits.ok
        ? `剩余 ${formatAmount(credits.remain ?? 0)}`
        : "积分失败";

  const creditsTitle = creditsBusy
    ? "正在查询积分"
    : creditsFailed
      ? credits.message || "积分查询失败"
      : credits?.ok
        ? hasBreakdown
          ? "点击查看各积分包明细"
          : "该账号当前没有可用积分包"
        : "点击查询该账号积分";

  return (
    <div className="account-wrap">
      <div
        className={`account${active ? " active" : ""}${
          billing && !active ? " billing" : ""
        }`}
      >
        {/* 头像显示序号而不是昵称首字：账号名多是 uid/手机号打头的一串数字，
            取了首字常常每个账号都是同一个数字，反而看不出第几个。 */}
        <div className="avatar">{index}</div>

        <div className="identity">
          <div className="name">
            <span className="text">{name}</span>
            {active && <span className="tag-current">当前登录</span>}
            {billing && <span className="tag-billing">当前额度</span>}
            {account.tokenOwner === "checkin" && (
              <span className="tag-independent" title="凭证来自本应用内的独立登录，不随客户端刷新而失效">
                独立凭证
              </span>
            )}
          </div>
          <div className="meta" style={failedToday ? { color: "var(--danger)" } : undefined}>
            {meta || "—"}
          </div>
        </div>

        <div className="actions">
          {account.needsRelogin && (
            <span className="pill danger">
              <span className="dot" />
              需重新登录
            </span>
          )}
          {!account.needsRelogin && !account.hasRefreshToken && (
            <span className="pill warn">无刷新凭证</span>
          )}

          {doneToday ? (
            <span className="pill ok">
              <span className="dot" />
              今日已签
            </span>
          ) : failedToday ? (
            <span className="pill danger">
              <span className="dot" />
              签到失败
            </span>
          ) : (
            <span className="pill warn">
              <span className="dot" />
              待签到
            </span>
          )}

          <button
            className={`btn sm credits${creditsFailed ? " danger" : ""}${
              expanded && hasBreakdown ? " active" : ""
            }`}
            onClick={() => {
              if (hasBreakdown) {
                setExpanded((value) => !value);
              } else {
                setExpanded(false);
                onLoadCredits();
              }
            }}
            onDoubleClick={onLoadCredits}
            disabled={disabled || creditsBusy}
            title={creditsTitle}
          >
            {creditsBusy && <span className="spin" />}
            {creditsLabel}
          </button>

          <button className="btn sm" onClick={onCheckin} disabled={disabled || busy}>
            {busy ? "签到中" : "签到"}
          </button>
          <button className="btn sm" onClick={onSwitch} disabled={disabled || switching}>
            {switching ? "切换中" : "切换"}
          </button>
          <button className="btn sm ghost danger" onClick={onDelete} disabled={disabled}>
            删除
          </button>
        </div>
      </div>

      {expanded && hasBreakdown && credits?.packages && (
        <div className="credits-detail">
          {credits.packages.map((item) => (
            <div className="credits-line" key={item.code || item.name}>
              <span className="credits-name">{item.name || item.code}</span>
              <span className="credits-bar" aria-hidden>
                <span
                  className="credits-fill"
                  style={{
                    width: `${
                      item.total > 0 ? Math.max(0, Math.min(100, (item.remain / item.total) * 100)) : 0
                    }%`,
                  }}
                />
              </span>
              <span className="credits-value">
                {formatAmount(item.remain)} / {formatAmount(item.total)}
              </span>
              <span className="credits-expiry">
                {item.expiresAt ? `${formatExpiry(item.expiresAt)} 到期` : "长期"}
              </span>
            </div>
          ))}
          {credits.nearestExpiry && (
            <div className="credits-note">
              最近到期：{formatExpiry(credits.nearestExpiry)}（到期未用完的部分会作废）
            </div>
          )}
        </div>
      )}
    </div>
  );
}

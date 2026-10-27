//! 独立授权登录：不经过官方客户端的登录态，直接向官方申请 state 并在浏览器完成登录。
//!
//! 全流程均已用真实请求探明（伪造路径返回 404 Route Not Found 作对照）：
//!   1. `POST /v2/plugin/auth/state?platform=workbuddy`
//!        -> `{ code: 0, data: { state, authUrl } }`
//!   2. 浏览器打开 authUrl（官方登录页），由用户自行完成登录
//!   3. `GET  /v2/plugin/auth/token?state=<state>`
//!        -> 未完成 `{ code: 11217, msg: "11217:login ing..." }`
//!        -> 已完成 `{ code: 0, data: { accessToken, refreshToken, expiresAt, ... } }`
//!   4. `GET  /v2/plugin/login/account?state=<state>`（Bearer 鉴权）-> 账号资料，尽力而为
//!
//! 存在的意义：官方客户端同一时刻只能激活一个账号，而本模块让每个账号各走一次
//! 授权，彼此独立，因此可以在同一台机器上堆任意多个账号。
//!
//! 安全约束：交给浏览器打开的地址必须先通过官方域名白名单，避免官方响应被篡改后
//! 把用户导向钓鱼页。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::store;

const API_BASE: &str = "https://www.codebuddy.cn";
const API_PREFIX: &str = "/v2/plugin";
const PLATFORM: &str = "workbuddy";

/// 登录会话有效期，与官方 state 的存活时间一致。
const SESSION_TIMEOUT_SECONDS: i64 = 600;
/// 连续多少次「非登录中」的错误才判定失败。偶发网络抖动不应该打断扫码。
const ERROR_STREAK_LIMIT: u32 = 10;
/// 会话结束后保留多久，供前端再读一次最终结果。
const SESSION_LINGER_MS: i64 = 300_000;
/// 官方表示「还没登录完成」的状态码。
const CODE_LOGIN_PENDING: i64 = 11217;

const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

struct Session {
    state: String,
    auth_url: String,
    expires_at_ms: i64,
    done: bool,
    result: Option<Value>,
    error: Option<String>,
    notice: Option<String>,
    error_streak: u32,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, Session>> {
    sessions().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("构建 HTTP 客户端失败")
    })
}

fn now_ms() -> i64 {
    crate::paths::now_ms()
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

fn code_of(response: &Value) -> i64 {
    response.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

fn message_of(response: &Value) -> String {
    response
        .get("message")
        .or_else(|| response.get("msg"))
        .or_else(|| response.get("error_msg"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误")
        .to_string()
}

fn is_success(response: &Value) -> bool {
    let code = code_of(response);
    code == 0 || code == 200
}

/// 官方「仍在等待用户完成登录」的判定。
pub fn is_pending(response: &Value) -> bool {
    code_of(response) == CODE_LOGIN_PENDING
        || message_of(response).to_lowercase().contains("login ing")
}

/// 时间戳归一化：秒级自动升为毫秒。
pub fn norm_ts(value: Option<&Value>) -> Option<i64> {
    let raw = match value? {
        Value::String(text) => text.trim().parse::<f64>().ok()? as i64,
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f as i64))?,
        _ => return None,
    };
    Some(if raw < 10_000_000_000 { raw * 1000 } else { raw })
}

fn str_of(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// 地址白名单
// ---------------------------------------------------------------------------

/// 从 URL 里取出主机名（去 scheme、去 userinfo、去端口）。
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = host_port.split(':').next()?.trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// 登录地址必须是 https 且落在官方域名白名单内。
pub fn is_allowed_login_url(url: &str) -> bool {
    url.starts_with("https://")
        && host_of(url)
            .map(|host| crate::api::is_allowed_domain(&host))
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn call(method: &str, url: &str, bearer: Option<&str>, domain: Option<&str>) -> Value {
    let mut request = if method == "GET" {
        client().get(url)
    } else {
        client().post(url).json(&json!({}))
    };
    request = request
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if let Some(token) = bearer {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(domain) = domain {
        request = request.header("X-Domain", domain);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or_else(|_| {
                    json!({
                        "code": status.as_u16(),
                        "message": text.chars().take(300).collect::<String>(),
                    })
                })
            }
        }
        Err(error) => json!({ "code": -1, "message": error.to_string() }),
    }
}

// ---------------------------------------------------------------------------
// 会话负载
// ---------------------------------------------------------------------------

fn remaining_seconds(session: &Session) -> i64 {
    ((session.expires_at_ms - now_ms()) / 1000).max(0)
}

fn status_of(session: &Session) -> &'static str {
    if !session.done {
        "pending"
    } else if session.result.is_some() {
        "ok"
    } else {
        "error"
    }
}

fn snapshot(session: &Session) -> Value {
    json!({
        "done": session.done,
        "status": status_of(session),
        "account": session.result,
        "error": session.error,
        "notice": session.notice,
        "expiresIn": remaining_seconds(session),
    })
}

fn pending(login_id: &str, notice: Option<String>) -> Value {
    let mut map = lock_sessions();
    match map.get_mut(login_id) {
        Some(session) => {
            session.notice = notice;
            let mut payload = snapshot(session);
            // done 一定是 false，这里只是保留同一份结构
            payload["done"] = json!(false);
            payload
        }
        None => json!({
            "done": true, "status": "error",
            "error": "登录会话不存在或已取消", "expiresIn": 0,
        }),
    }
}

fn finish(login_id: &str, account: Option<Value>, error: Option<String>) -> Value {
    let mut map = lock_sessions();
    match map.get_mut(login_id) {
        Some(session) => {
            session.done = true;
            session.result = account;
            session.error = error;
            snapshot(session)
        }
        None => json!({
            "done": true,
            "status": if account.is_some() { "ok" } else { "error" },
            "account": account, "error": error, "expiresIn": 0,
        }),
    }
}

// ---------------------------------------------------------------------------
// 流程
// ---------------------------------------------------------------------------

/// 第一步：向官方申请 state，返回给前端去打开浏览器。
pub async fn start() -> Result<Value, String> {
    {
        let mut map = lock_sessions();
        let now = now_ms();
        map.retain(|_, session| session.expires_at_ms + SESSION_LINGER_MS > now);
    }

    let url = format!("{API_BASE}{API_PREFIX}/auth/state?platform={PLATFORM}");
    let response = call("POST", &url, None, None).await;
    if !is_success(&response) {
        return Err(format!(
            "申请登录 state 失败：code={} {}",
            code_of(&response),
            message_of(&response)
        ));
    }

    let data = response
        .get("data")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let state = str_of(&data, "state").ok_or_else(|| "登录响应缺少 state 字段".to_string())?;
    let auth_url = str_of(&data, "authUrl")
        .or_else(|| str_of(&data, "auth_url"))
        .or_else(|| str_of(&data, "url"))
        .ok_or_else(|| "登录响应缺少 authUrl 字段".to_string())?;

    if !is_allowed_login_url(&auth_url) {
        return Err(format!(
            "官方返回的登录地址不在白名单内，已拒绝使用：{auth_url}"
        ));
    }

    let login_id = uuid::Uuid::new_v4().to_string();
    lock_sessions().insert(
        login_id.clone(),
        Session {
            state,
            auth_url: auth_url.clone(),
            expires_at_ms: now_ms() + SESSION_TIMEOUT_SECONDS * 1000,
            done: false,
            result: None,
            error: None,
            notice: None,
            error_streak: 0,
        },
    );

    Ok(json!({
        "loginId": login_id,
        "authUrl": auth_url,
        "expiresIn": SESSION_TIMEOUT_SECONDS,
    }))
}

/// 打开登录页。只接受本进程自己申请的地址，前端无法诱导打开任意 URL。
pub fn open_login_page(login_id: &str) -> Result<(), String> {
    let url = {
        let map = lock_sessions();
        let session = map
            .get(login_id)
            .ok_or_else(|| "登录会话不存在或已取消".to_string())?;
        session.auth_url.clone()
    };

    if !is_allowed_login_url(&url) {
        return Err(format!("登录地址不在官方域名白名单内，已拒绝打开：{url}"));
    }

    // 两平台都交给系统默认浏览器；命令差异收在 `open_external` 里。
    crate::open_external(&url).map_err(|error| format!("打开浏览器失败：{error}"))
}

pub fn cancel(login_id: &str) -> bool {
    lock_sessions().remove(login_id).is_some()
}

/// 第二步：轮询一次。前端每 2 秒调一次。
pub async fn poll(login_id: &str) -> Value {
    let state = {
        let mut map = lock_sessions();
        let Some(session) = map.get_mut(login_id) else {
            return json!({
                "done": true, "status": "error",
                "error": "登录会话不存在或已取消", "expiresIn": 0,
            });
        };
        if session.done {
            return snapshot(session);
        }
        if now_ms() > session.expires_at_ms {
            session.done = true;
            session.error = Some("登录超时，请重新发起".to_string());
            return snapshot(session);
        }
        session.state.clone()
    };

    let url = format!("{API_BASE}{API_PREFIX}/auth/token?state={state}");
    let response = call("GET", &url, None, None).await;

    if is_success(&response) {
        let data = response
            .get("data")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let access_token = str_of(&data, "accessToken").or_else(|| str_of(&data, "access_token"));

        let Some(token) = access_token else {
            // 结构像是成功了但还没带 token，继续等
            return pending(login_id, None);
        };

        let domain = str_of(&data, "domain");
        let profile = fetch_profile(&state, &token, domain.as_deref()).await;
        let account = build_account(&data, &profile);
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if let Err(error) = store::upsert(account.clone()) {
            return finish(login_id, None, Some(format!("保存账号失败：{error}")));
        }
        // 新 token 到手，清掉此前可能残留的「需要重新登录」标记
        let _ = store::apply_patch(
            &id,
            &json!({ "needs_relogin": false, "needs_relogin_reason": Value::Null }),
        );

        return finish(login_id, Some(store::view(&account)), None);
    }

    if is_pending(&response) {
        return pending(login_id, None);
    }

    // 既不是成功也不是「登录中」：可能是网关抖动，也可能是这个 state 已经废了。
    // 先当作抖动继续轮询，连续多次才判定失败。
    let notice = format!(
        "官方返回 code={} {}",
        code_of(&response),
        message_of(&response)
    );
    let give_up = {
        let mut map = lock_sessions();
        match map.get_mut(login_id) {
            Some(session) => {
                session.error_streak += 1;
                session.error_streak >= ERROR_STREAK_LIMIT
            }
            None => true,
        }
    };

    if give_up {
        return finish(login_id, None, Some(notice));
    }
    pending(login_id, Some(notice))
}

/// 拉账号资料。失败不阻断登录 —— token 才是必需的，资料只是让列表更好认。
async fn fetch_profile(state: &str, access_token: &str, domain: Option<&str>) -> Value {
    let url = format!("{API_BASE}{API_PREFIX}/login/account?state={state}");
    let response = call("GET", &url, Some(access_token), domain).await;
    if !is_success(&response) {
        return json!({});
    }
    response
        .get("data")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

/// 把 token 响应与账号资料合成一条账号记录。
///
/// id 统一用 `uid:<uid>`，与「从本机导入」保持一致，同一账号重复登录只会合并更新。
pub fn build_account(data: &Value, profile: &Value) -> Value {
    let access_token =
        str_of(data, "accessToken").or_else(|| str_of(data, "access_token")).unwrap_or_default();
    let refresh_token = str_of(data, "refreshToken").or_else(|| str_of(data, "refresh_token"));

    let domain = str_of(data, "domain")
        .filter(|candidate| crate::api::is_allowed_domain(candidate))
        .unwrap_or_else(|| "www.codebuddy.cn".to_string());

    // 账号资料接口可能拿不到（偶发失败/权限差异）。这时退回 token 自身的 sub 认人：
    // 否则 uid 为空会生成随机 id，同一个账号会被记成两条，独立凭证的迁移就乱了。
    let (jwt_sub, jwt_name) = match crate::switch::token_owner(&access_token) {
        Some((sub, name)) => (Some(sub), name),
        None => (None, None),
    };

    let uid = str_of(profile, "uid").or_else(|| str_of(data, "uid")).or(jwt_sub);
    let nickname = str_of(profile, "nickname")
        .or_else(|| str_of(data, "nickname"))
        .or(jwt_name);
    let email = str_of(profile, "email").or_else(|| str_of(data, "email"));
    let enterprise_id = str_of(profile, "enterpriseId")
        .or_else(|| str_of(profile, "enterprise_id"))
        .or_else(|| str_of(data, "enterpriseId"));

    let expires_at = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at"))).or_else(|| {
        data.get("expiresIn")
            .and_then(Value::as_i64)
            .map(|seconds| now_ms() + seconds * 1000)
    });
    let refresh_expires_at =
        norm_ts(data.get("refreshExpiresAt").or_else(|| data.get("refresh_expires_at"))).or_else(
            || {
                data.get("refreshExpiresIn")
                    .and_then(Value::as_i64)
                    .map(|seconds| now_ms() + seconds * 1000)
            },
        );

    let id = uid
        .as_deref()
        .map(|value| format!("uid:{value}"))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 留存官方账号资料原件。切换账号时要靠它构造客户端能识别的 account 对象——
    // 只凭上面这几个字段重建会丢掉 uin / oneidAccountId / idp 等，导致切过去之后
    // 客户端的账号卡片资料残缺。资料为空时不写该字段（upsert 会跳过空值）。
    let profile_raw = if profile.as_object().is_some_and(|object| !object.is_empty()) {
        profile.clone()
    } else {
        Value::Null
    };

    json!({
        "id": id,
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "enterpriseId": enterprise_id,
        "domain": domain,
        "token_owner": store::OWNER_CHECKIN,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": str_of(data, "tokenType")
            .or_else(|| str_of(data, "token_type"))
            .unwrap_or_else(|| "Bearer".to_string()),
        "expiresAt": expires_at,
        "refreshExpiresAt": refresh_expires_at,
        // 独立登录刚刚签发了一张新 token，签发时刻就是有效的「刷新时间」。
        // 不写这个字段会让界面上的刷新时间一直停在旧值（导入路径遗留）。
        "refreshedAt": now_ms(),
        "profile_raw": profile_raw,
        "source": "oauth",
        "createdAt": now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn login_url_must_be_https_and_whitelisted() {
        let good = "https://www.codebuddy.cn/login?platform=workbuddy&state=abc";
        assert!(is_allowed_login_url(good));
        assert_eq!(host_of(good).as_deref(), Some("www.codebuddy.cn"));
        assert!(is_allowed_login_url("https://www.workbuddy.cn/login?state=a"));

        // 明文 http 一律拒绝
        assert!(!is_allowed_login_url("http://www.codebuddy.cn/login"));
        // 伪装域名
        assert!(!is_allowed_login_url("https://www.codebuddy.cn.evil.com/login"));
        // 用户信息混淆
        assert!(!is_allowed_login_url("https://www.codebuddy.cn@evil.com/login"));
        // 完全无关的域名
        assert!(!is_allowed_login_url("https://evil.com/login"));
        // 带端口的官方域名仍然通过
        assert!(is_allowed_login_url("https://www.codebuddy.cn:443/login"));
    }

    #[test]
    fn pending_detection_covers_code_and_message() {
        assert!(is_pending(&json!({ "code": 11217, "msg": "11217:login ing..." })));
        assert!(is_pending(&json!({ "code": 0, "msg": "LOGIN ING" })));
        assert!(!is_pending(&json!({ "code": 0, "data": {} })));
        assert!(!is_pending(&json!({ "code": 500, "msg": "internal error" })));
    }

    #[test]
    fn norm_ts_promotes_seconds_to_millis() {
        assert_eq!(norm_ts(Some(&json!(1_700_000_000))), Some(1_700_000_000_000));
        assert_eq!(norm_ts(Some(&json!(1_700_000_000_000i64))), Some(1_700_000_000_000));
        assert_eq!(norm_ts(Some(&json!("1700000000"))), Some(1_700_000_000_000));
        assert_eq!(norm_ts(None), None);
    }

    #[test]
    fn build_account_keeps_uid_identity_and_whitelists_domain() {
        let data = json!({
            "accessToken": "at-1",
            "refreshToken": "rt-1",
            "tokenType": "Bearer",
            "domain": "www.codebuddy.cn",
            "expiresIn": 3600,
        });
        let profile = json!({ "uid": "u-9", "nickname": "小号", "email": "a@b.com" });

        let account = build_account(&data, &profile);
        assert_eq!(account["id"], json!("uid:u-9"));
        assert_eq!(account["access_token"], json!("at-1"));
        assert_eq!(account["refresh_token"], json!("rt-1"));
        assert_eq!(account["domain"], json!("www.codebuddy.cn"));
        assert!(account["expiresAt"].as_i64().unwrap() > now_ms());
    }

    #[test]
    fn build_account_rejects_foreign_domain() {
        let data = json!({ "accessToken": "at", "domain": "attacker.example" });
        let account = build_account(&data, &json!({ "uid": "u-1" }));
        assert_eq!(account["domain"], json!("www.codebuddy.cn"));
    }

    #[test]
    fn build_account_falls_back_to_random_id_without_uid() {
        let data = json!({ "accessToken": "at" });
        let account = build_account(&data, &json!({}));
        let id = account["id"].as_str().unwrap();
        assert!(!id.starts_with("uid:"));
        assert!(!id.is_empty());
    }
}

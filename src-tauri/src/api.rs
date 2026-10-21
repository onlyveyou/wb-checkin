//! 官方接口调用：签到、签到状态、token 刷新。
//!
//! 出站目标只有官方域名。账号记录里的 domain 会先过白名单，未命中一律回落到
//! 默认域名 —— 避免账号记录被篡改后把 Bearer token 发往攻击者主机。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://www.codebuddy.cn";
const ALLOWED_HOSTS: [&str; 2] = ["www.codebuddy.cn", "www.workbuddy.cn"];
const PLUGIN_PREFIX: &str = "/v2/plugin";
const METER_PREFIX: &str = "/v2/billing/meter";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

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

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 解析账号应使用的域名；白名单外一律回落默认域名。
pub fn resolve_base(account: &Value) -> String {
    match str_field(account, "domain") {
        Some(domain) if ALLOWED_HOSTS.contains(&domain.as_str()) => format!("https://{domain}"),
        _ => DEFAULT_BASE.to_string(),
    }
}

/// 域名是否在白名单内（外部传入的 domain 需要先过这一关）。
pub fn is_allowed_domain(domain: &str) -> bool {
    ALLOWED_HOSTS.contains(&domain.trim())
}

fn auth_headers(account: &Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if let Some(token) = str_field(account, "access_token") {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if let Some(uid) = str_field(account, "uid") {
        headers.insert("X-User-Id".to_string(), uid);
    }
    if let Some(enterprise) =
        str_field(account, "enterpriseId").or_else(|| str_field(account, "enterprise_id"))
    {
        headers.insert("X-Enterprise-Id".to_string(), enterprise.clone());
        headers.insert("X-Tenant-Id".to_string(), enterprise);
    }
    if let Some(domain) = str_field(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    headers
}

/// 发一次 POST。网络层失败返回 `{"code": -1, "message": ...}`，与官方错误结构统一。
async fn post(account: &Value, path: &str, extra: &[(&str, String)]) -> Value {
    post_body(account, path, json!({}), extra).await
}

/// 带自定义请求体的 POST。积分接口需要按商品码传参，不能固定发空对象。
async fn post_body(account: &Value, path: &str, body: Value, extra: &[(&str, String)]) -> Value {
    let url = format!("{}{}", resolve_base(account), path);
    let mut request = client().post(&url);
    for (key, value) in auth_headers(account) {
        request = request.header(key, value);
    }
    for (key, value) in extra {
        request = request.header(*key, value.clone());
    }
    match request.json(&body).send().await {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or_else(|_| {
                    json!({ "code": status.as_u16(), "message": text.chars().take(300).collect::<String>() })
                })
            }
        }
        Err(error) => json!({ "code": -1, "message": error.to_string() }),
    }
}

/// 发一次 GET。与 POST 同一套信封归一化：网络失败返回 `{"code":-1}`。
///
/// 成长中心的查询类端点用 GET，且不需要请求体。
pub async fn get_json(account: &Value, path: &str, extra: &[(&str, String)]) -> Value {
    let url = format!("{}{}", resolve_base(account), path);
    let mut request = client().get(&url);
    for (key, value) in auth_headers(account) {
        request = request.header(key, value);
    }
    for (key, value) in extra {
        request = request.header(*key, value.clone());
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                serde_json::from_str(&text).unwrap_or(Value::Null)
            } else {
                serde_json::from_str(&text).unwrap_or_else(|_| {
                    json!({ "code": status.as_u16(), "message": text.chars().take(300).collect::<String>() })
                })
            }
        }
        Err(error) => json!({ "code": -1, "message": error.to_string() }),
    }
}

fn code_of(response: &Value) -> i64 {
    response.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

fn message_of(response: &Value) -> String {
    response
        .get("message")
        .or_else(|| response.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误")
        .to_string()
}

fn is_success(response: &Value) -> bool {
    let code = code_of(response);
    code == 0 || code == 200
}

/// 判断是否属于「凭证失效」，用于触发一次刷新重试。
pub fn is_unauthorized(response: &Value) -> bool {
    let code = code_of(response);
    if code == 401 || code == 403 {
        return true;
    }
    let message = message_of(response).to_lowercase();
    ["unauthorized", "401", "登录", "失效", "过期", "token"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// 响应是否成功（供积分等模块判断，不必各自重写 0/200 判定）。
pub fn success(response: &Value) -> bool {
    is_success(response)
}

/// 错误描述：优先取官方 `message`/`msg`，网关返回 HTML 时截取片段。
pub fn describe(response: &Value) -> String {
    message_of(response)
}

/// 带额外请求头与自定义请求体的 POST（积分接口需要 `Origin`/`X-Client-Platform` 等头）。
pub async fn post_json(
    account: &Value,
    path: &str,
    body: Value,
    extra: &[(&str, String)],
) -> Value {
    post_body(account, path, body, extra).await
}

/// 账号是否具备可用的 refresh token。
pub fn can_refresh(account: &Value) -> bool {
    str_field(account, "refresh_token").is_some()
}

fn checked_in_flag(data: &Value) -> bool {
    data.get("today_checked_in")
        .or_else(|| data.get("todayCheckedIn"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 查询今日签到状态。先试 checkin-activity-status，失败回退 checkin-status。
pub async fn fetch_status(account: &Value) -> Value {
    let primary = post(
        account,
        &format!("{METER_PREFIX}/checkin-activity-status"),
        &[],
    )
    .await;
    if is_success(&primary) {
        let data = primary.get("data").cloned().unwrap_or_else(|| json!({}));
        return json!({ "ok": true, "todayCheckedIn": checked_in_flag(&data) });
    }

    let fallback = post(account, &format!("{METER_PREFIX}/checkin-status"), &[]).await;
    if is_success(&fallback) {
        let data = fallback.get("data").cloned().unwrap_or_else(|| json!({}));
        return json!({ "ok": true, "todayCheckedIn": checked_in_flag(&data) });
    }

    json!({ "ok": false, "error": message_of(&fallback), "code": code_of(&fallback) })
}

/// 执行签到。服务端返回「已签到」视为成功。
pub async fn submit(account: &Value) -> Value {
    let response = post(account, &format!("{METER_PREFIX}/daily-checkin"), &[]).await;
    if is_success(&response) {
        return json!({ "ok": true, "already": false });
    }
    let message = message_of(&response);
    if message.contains("已签到") || message.to_lowercase().contains("repeat") {
        return json!({ "ok": true, "already": true, "message": message });
    }
    json!({ "ok": false, "error": message, "code": code_of(&response) })
}

/// 刷新 token。成功返回可直接合并进账号记录的字段。
pub async fn refresh(account: &Value) -> Result<Value, String> {
    let refresh_token =
        str_field(account, "refresh_token").ok_or_else(|| "缺少 refresh token，需重新登录".to_string())?;

    let response = post(
        account,
        &format!("{PLUGIN_PREFIX}/auth/token/refresh"),
        &[("X-Refresh-Token", refresh_token)],
    )
    .await;

    if !is_success(&response) {
        return Err(format!("刷新失败：{}", message_of(&response)));
    }

    let data = response.get("data").cloned().unwrap_or_else(|| json!({}));
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .or_else(|| data.get("access_token").and_then(Value::as_str))
        .map(String::from);
    let new_refresh_token = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .or_else(|| data.get("refresh_token").and_then(Value::as_str))
        .map(String::from);

    let mut patch = serde_json::Map::new();
    if let Some(token) = access_token {
        patch.insert("access_token".to_string(), json!(token));
    }
    if let Some(token) = new_refresh_token {
        patch.insert("refresh_token".to_string(), json!(token));
    }
    for key in ["expiresAt", "refreshExpiresAt", "refreshExpiresIn", "expiresIn"] {
        if let Some(value) = data.get(key) {
            if !value.is_null() {
                patch.insert(key.to_string(), value.clone());
            }
        }
    }
    patch.insert("refreshedAt".to_string(), json!(crate::paths::now_ms()));

    if !patch.contains_key("access_token") {
        return Err("刷新响应缺少 accessToken".to_string());
    }
    Ok(Value::Object(patch))
}

/// 单账号签到：状态 → 提交；遇凭证失效刷新一次后重试。
///
/// 返回值：`{ "ok": bool, "result": "ok"|"already"|"error", "message": string, "patch": object|null }`
/// `patch` 是需要合并回账号记录的字段（新 token 等）。
pub async fn checkin_account(account: &Value) -> Value {
    let mut working = account.clone();

    for attempt in 0..2 {
        let status = fetch_status(&working).await;
        if status.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = status.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let retryable = attempt == 0
                && str_field(&working, "refresh_token").is_some()
                && (status.get("error").is_some()
                    && (is_unauthorized(&status) || code == -1));
            if retryable {
                match refresh(&working).await {
                    Ok(patch) => {
                        if let (Some(target), Some(source)) = (working.as_object_mut(), patch.as_object())
                        {
                            for (key, value) in source {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                        continue;
                    }
                    Err(error) => {
                        return json!({ "ok": false, "result": "error", "message": error, "patch": Value::Null });
                    }
                }
            }
            let message = status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败")
                .to_string();
            return json!({ "ok": false, "result": "error", "message": message, "patch": Value::Null });
        }

        if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
            return json!({ "ok": true, "result": "already", "message": "今日已签到", "patch": Value::Null });
        }

        let result = submit(&working).await;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            let already = result.get("already").and_then(Value::as_bool).unwrap_or(false);
            return json!({
                "ok": true,
                "result": if already { "already" } else { "ok" },
                "message": if already { "今日已签到" } else { "签到成功" },
                "patch": Value::Null
            });
        }

        let code = result.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if attempt == 0
            && str_field(&working, "refresh_token").is_some()
            && (is_unauthorized(&result) || code == -1)
        {
            match refresh(&working).await {
                Ok(patch) => {
                    if let (Some(target), Some(source)) = (working.as_object_mut(), patch.as_object()) {
                        for (key, value) in source {
                            target.insert(key.clone(), value.clone());
                        }
                    }
                    continue;
                }
                Err(error) => {
                    return json!({ "ok": false, "result": "error", "message": error, "patch": Value::Null });
                }
            }
        }

        let message = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("签到失败")
            .to_string();
        return json!({ "ok": false, "result": "error", "message": message, "patch": Value::Null });
    }

    json!({ "ok": false, "result": "error", "message": "重试后仍失败", "patch": Value::Null })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_or_missing_domain_falls_back_to_default() {
        assert_eq!(resolve_base(&json!({ "domain": "attacker.example" })), DEFAULT_BASE);
        assert_eq!(resolve_base(&json!({})), DEFAULT_BASE);
        assert_eq!(resolve_base(&json!({ "domain": "" })), DEFAULT_BASE);
        assert_eq!(
            resolve_base(&json!({ "domain": "www.workbuddy.cn" })),
            "https://www.workbuddy.cn"
        );

        assert!(!is_allowed_domain("evil.com"));
        assert!(!is_allowed_domain("www.codebuddy.cn.evil.com"));
        assert!(is_allowed_domain("www.codebuddy.cn"));
    }

    #[test]
    fn envelope_helpers_handle_msg_and_gateway_html() {
        // 官方刷新接口用的是 msg 字段
        assert_eq!(message_of(&json!({ "code": 10001, "msg": "refreshToken is empty" })), "refreshToken is empty");
        assert!(!is_success(&json!({ "code": 10001, "msg": "x" })));
        assert!(is_success(&json!({ "code": 0, "data": {} })));
        assert!(is_success(&json!({ "code": 200 })));

        // APISIX 网关拦截时返回的是 HTML，映射为 code=401 后必须能识别为凭证失效
        let gateway = json!({ "code": 401, "message": "<html>401 Authorization Required</html>" });
        assert!(is_unauthorized(&gateway));
        assert!(!is_unauthorized(&json!({ "code": 10085, "msg": "请求不合法" })));
    }

    #[test]
    fn checked_in_flag_accepts_both_casings() {
        assert!(checked_in_flag(&json!({ "today_checked_in": true })));
        assert!(checked_in_flag(&json!({ "todayCheckedIn": true })));
        assert!(!checked_in_flag(&json!({})));
    }
}

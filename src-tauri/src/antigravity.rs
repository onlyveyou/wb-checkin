//! Antigravity（Google Cloud Code Assist）：把本机网关的 /admin/api/antigravity/* 转成 IPC。
//!
//! 凭据（refresh_token / access_token）只存网关 config.json，本模块不持有任何 token。
//! 登录也是网关侧起独立进程拉浏览器，这里只触发 + 轮询进度。

use serde_json::{json, Value};
use std::time::Duration;

const PORT: u16 = 8901;

fn endpoint() -> String {
    format!("http://127.0.0.1:{PORT}/admin/api")
}

fn allowed(path: &str) -> bool {
    matches!(
        path,
        "/state" | "/antigravity" | "/antigravity/login" | "/antigravity/loginStatus"
            | "/antigravity/quota" | "/test"
    )
}

async fn call(method: reqwest::Method, path: &str, body: Option<Value>, timeout: u64) -> Result<Value, String> {
    if !allowed(path) {
        return Err(format!("不允许的网关路径：{path}"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()
        .map_err(|error| format!("构建 HTTP 客户端失败：{error}"))?;
    let mut request = client
        .request(method, format!("{}{}", endpoint(), path))
        .header("Accept", "application/json");
    if let Some(payload) = body {
        request = request.json(&payload);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("网关无响应：{error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("网关响应不是 JSON：{error}"))?;
    if !status.is_success() {
        let reason = parsed
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| parsed.get("message").and_then(Value::as_str))
            .unwrap_or("未知错误")
            .to_string();
        return Err(reason);
    }
    Ok(parsed)
}

pub async fn state() -> Value {
    match call(reqwest::Method::GET, "/state", None, 8).await {
        Ok(value) => {
            let a = value.get("antigravity").cloned().unwrap_or(json!({}));
            json!({
                "ok": true,
                "offline": false,
                "credential": a.get("credential").and_then(Value::as_bool).unwrap_or(false),
                "email": a.get("email").and_then(Value::as_str).unwrap_or(""),
                "plan": a.get("plan").and_then(Value::as_str).unwrap_or(""),
                "models": a.get("models").cloned().unwrap_or(json!([])),
                "labels": a.get("labels").cloned().unwrap_or(json!({})),
                "quotaAge": a.get("updatedAt").and_then(Value::as_u64).unwrap_or(0),
                "inject": value.get("inject").and_then(|v| v.get("antigravity")).cloned().unwrap_or(json!({})),
            })
        }
        Err(message) => json!({ "ok": false, "offline": true, "message": message }),
    }
}

pub async fn login() -> Result<Value, String> {
    call(reqwest::Method::POST, "/antigravity/login", Some(json!({})), 15).await
}

pub async fn login_status() -> Value {
    match call(reqwest::Method::POST, "/antigravity/loginStatus", Some(json!({})), 8).await {
        Ok(value) => value,
        Err(message) => json!({ "ok": false, "state": "error", "message": message }),
    }
}

/// 拉可用模型 + 每个模型剩余额度（走 fetchAvailableModels / retrieveUserQuotaSummary）
pub async fn quota(force: Option<bool>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/antigravity/quota",
        Some(json!({ "force": force.unwrap_or(false) })),
        60,
    )
    .await
}

pub async fn save(models: Value) -> Result<Value, String> {
    call(reqwest::Method::POST, "/antigravity", Some(json!({ "models": models })), 15).await
}

pub async fn test(model: Option<String>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/test",
        Some(json!({ "name": "antigravity", "model": model })),
        180,
    )
    .await
}

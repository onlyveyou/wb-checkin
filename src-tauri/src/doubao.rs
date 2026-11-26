//! 豆包网页额度：把本机网关的 /admin/api/doubao/* 转成 IPC。
//! 凭据只存在网关 config.json 与本机 Chrome profile，本模块不另存 cookie。

use serde_json::{json, Value};
use std::time::Duration;

const PORT: u16 = 8901;

fn endpoint() -> String {
    format!("http://127.0.0.1:{PORT}/admin/api")
}

fn allowed(path: &str) -> bool {
    matches!(
        path,
        "/state" | "/doubao" | "/doubao/login" | "/doubao/loginStatus" | "/doubao/checkin"
            | "/doubao/quota" | "/test"
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
            let d = value.get("doubao").cloned().unwrap_or(json!({}));
            let inject = value
                .get("inject")
                .and_then(|v| v.get("doubao"))
                .cloned()
                .unwrap_or(json!({}));
            json!({
                "ok": true,
                "offline": false,
                "credential": d.get("credential").and_then(Value::as_bool).unwrap_or(false),
                "checkedToday": d.get("checkedToday").and_then(Value::as_bool).unwrap_or(false),
                "lastCheckin": d.get("lastCheckin").and_then(Value::as_str).unwrap_or(""),
                "models": d.get("models").cloned().unwrap_or(json!([])),
                // 官方下发的全部模型，面板据此渲染（含第三方模型与倍率标签）
                "catalog": d.get("catalog").cloned().unwrap_or(json!([])),
                "catalogAt": d.get("catalogAt").and_then(Value::as_u64).unwrap_or(0),
                "autoCheckin": d.get("autoCheckin").and_then(Value::as_bool).unwrap_or(true),
                "inject": inject,
            })
        }
        Err(message) => json!({ "ok": false, "offline": true, "message": message }),
    }
}

pub async fn login() -> Result<Value, String> {
    // 豆包登录不做 already 短路：重新登录必须能换账号。
    call(reqwest::Method::POST, "/doubao/login", Some(json!({})), 15).await
}

pub async fn login_status() -> Value {
    match call(reqwest::Method::POST, "/doubao/loginStatus", Some(json!({})), 8).await {
        Ok(value) => value,
        Err(message) => json!({ "ok": false, "state": "error", "message": message }),
    }
}

pub async fn checkin(force: Option<bool>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/doubao/checkin",
        Some(json!({ "force": force.unwrap_or(false) })),
        60,
    )
    .await
}

pub async fn save(models: Value, auto_checkin: Option<bool>) -> Result<Value, String> {
    let mut payload = json!({ "models": models });
    if let Some(value) = auto_checkin {
        payload["autoCheckin"] = json!(value);
    }
    call(reqwest::Method::POST, "/doubao", Some(payload), 15).await
}

/// 可用额度 + 今日调用统计（走豆包自己的订阅总览 / 额度余额接口）
pub async fn quota() -> Result<Value, String> {
    call(reqwest::Method::POST, "/doubao/quota", Some(json!({})), 20).await
}

pub async fn test(model: Option<String>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/test",
        Some(json!({ "name": "doubao", "model": model })),
        180,
    )
    .await
}

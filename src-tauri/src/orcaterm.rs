//! OrcaTerm 免费模型：把本机网关（127.0.0.1:8901）的 OrcaTerm 管理接口转成 IPC。
//!
//! 网关里那套已经干完了所有重活：拉独立浏览器扫码、抓 uin+skey、算 CSRF、写 cookie、
//! 关闭时热重载。这里只做转发，不碰凭据本身，也不在本应用里另存一份 cookie —— 凭据的
//! 唯一事实来源是网关的 config.json，WorkBuddy 读的是同一份，不会出现两处登录态打架。
//!
//! 和 freehub.rs 一样守两条规矩：不新开端口、前端不直接发 HTTP；出站只有回环地址这一条，
//! 路径走白名单，外部输入不拼进 URL。

use serde_json::{json, Value};
use std::time::Duration;

const PORT: u16 = 8901;

fn endpoint() -> String {
    format!("http://127.0.0.1:{PORT}/admin/api")
}

/// 只放这五条：状态、一键登录、登录进度、模型配置、连通测试。别的路径一律拒绝。
fn allowed(path: &str) -> bool {
    matches!(
        path,
        "/state" | "/orcaterm/login" | "/orcaterm/loginStatus" | "/orcaterm" | "/test"
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
            .unwrap_or("未知错误")
            .to_string();
        return Err(reason);
    }
    Ok(parsed)
}

/// 从网关 state 里挑出 OrcaTerm 那块（登录状态 / uin / 模型列表 / 人设注入 / autoLock）。
/// 网关不在时回 offline，面板显示「网关未运行」而不是报错。
pub async fn state() -> Value {
    match call(reqwest::Method::GET, "/state", None, 8).await {
        Ok(value) => {
            let orca = value.get("orcaterm").cloned().unwrap_or(json!({}));
            let credential = orca.get("credential").and_then(Value::as_bool).unwrap_or(false);
            json!({
                "ok": true,
                "offline": false,
                "credential": credential,
                // 凭据被上游拒过（cookie 过期）：面板要显示「已失效，去重新登录」，
                // 不能因为 cookie 还在就继续写「已登录」——用户会以为能直接开用。
                "expired": orca.get("expired").and_then(Value::as_bool).unwrap_or(false),
                "error": orca.get("error").and_then(Value::as_str).unwrap_or(""),
                "uin": orca.get("uin").and_then(Value::as_str).unwrap_or(""),
                "models": orca.get("models").cloned().unwrap_or(json!([])),
                "injectSystem": orca.get("injectSystem").and_then(Value::as_str).unwrap_or(""),
                "autoLock": orca.get("autoLock").and_then(Value::as_bool).unwrap_or(true),
                "stats": orca.get("stats").cloned().unwrap_or(json!({})),
                "inject": value
                    .get("inject")
                    .and_then(|v| v.get("orcaterm"))
                    .cloned()
                    .unwrap_or(json!({})),
            })
        }
        Err(message) => json!({ "ok": false, "offline": true, "message": message }),
    }
}

/// 触发扫码登录：网关后台拉起独立浏览器，这里立即返回，前端轮询 login_status。
pub async fn login() -> Result<Value, String> {
    // 面板上的「重新登录」必须真弹出扫码窗口：旧 cookie 还在时网关默认会回 already，
    // 这里强制跳过该判断；登录器成功后会覆盖原凭据。
    call(reqwest::Method::POST, "/orcaterm/login", Some(json!({ "force": true })), 15).await
}

/// 登录进度：idle / starting / waiting / success / error。
pub async fn login_status() -> Value {
    match call(reqwest::Method::POST, "/orcaterm/loginStatus", Some(json!({})), 8).await {
        Ok(value) => value,
        Err(message) => json!({ "ok": false, "state": "error", "message": message }),
    }
}

/// 保存 auto 轮换模型列表（可传数组或换行/逗号分隔的字符串）+ 人设注入。
pub async fn save(models: Value, inject_system: Option<String>, auto_lock: Option<bool>) -> Result<Value, String> {
    let mut payload = json!({ "models": models });
    if let Some(text) = inject_system {
        payload["injectSystem"] = json!(text);
    }
    if let Some(value) = auto_lock {
        payload["autoLock"] = json!(value);
    }
    call(reqwest::Method::POST, "/orcaterm", Some(payload), 15).await
}

/// 真发一次请求测连通；上游慢模型给到 150 秒。
pub async fn test(model: Option<String>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/test",
        Some(json!({ "name": "orcaterm", "model": model })),
        150,
    )
    .await
}

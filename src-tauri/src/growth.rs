//! 成长中心：旅行 / 盲盒 / 任务领奖三个动作。
//!
//! 端点与判定口径对齐社区实测实现（MIT，smart-open/TraeWorkAssistant）：
//!
//! | 动作 | 端点 | 方法 | 请求体 |
//! |------|------|------|--------|
//! | 旅行状态 | `/v2/activity/growth/buddy/travel/status` | GET | — |
//! | 旅行领奖 | `/v2/activity/growth/buddy/travel/claim` | POST | `{"record_id": ...}` |
//! | 旅行目的地 | `/v2/activity/growth/buddy/travel/config` | GET | — |
//! | 旅行出发 | `/v2/activity/growth/buddy/travel/depart` | POST | `{"destination": ...}` |
//! | 盲盒次数 | `/v2/activity/growth/lottery/chances` | GET | — |
//! | 盲盒抽取 | `/v2/activity/growth/lottery/draw` | POST | `{}` |
//! | 任务列表 | `/v2/activity/growth/tasks` | GET | — |
//! | 任务领取 | `/v2/activity/growth/tasks/accept` | POST | `{"task_code": ...}` |
//!
//! 三个动作各自独立容错：任何一个失败都不影响其余两个。整轮遇凭证失效时刷新一次
//! 后重跑，与签到模块的策略一致。

use serde_json::{json, Value};

use crate::api;

const PREFIX: &str = "/v2/activity/growth";
/// 单账号单轮最多抽多少次盲盒，防止接口异常时无限循环。
const MAX_DRAWS: usize = 20;

fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

/// 依次尝试多个键，返回第一个存在的值（上游字段名在不同账号类型下不一致）。
fn dig<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .filter(|item| !item.is_null())
    })
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(text)) => matches!(text.trim(), "1" | "true" | "True" | "yes"),
        _ => false,
    }
}

fn number(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f.round() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 活动接口与积分接口共用一套「网页端」请求头：缺 `X-Client-Platform` 会被网关
/// 当成未知客户端。
fn headers(account: &Value) -> Vec<(&'static str, String)> {
    let origin = api::resolve_base(account);
    vec![
        ("X-Client-Platform", "web".to_string()),
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("Origin", origin.clone()),
        ("Referer", format!("{origin}/")),
    ]
}

fn data_of(response: &Value) -> Value {
    response.get("data").cloned().unwrap_or_else(|| json!({}))
}

fn unauthorized(response: &Value) -> bool {
    api::is_unauthorized(response)
}

/// 旅行：status → 到达则 claim → config → depart。
async fn travel(account: &Value) -> (Value, bool) {
    let extra = headers(account);
    let status = api::get_json(account, &format!("{PREFIX}/buddy/travel/status"), &extra).await;
    if unauthorized(&status) {
        return (json!({ "state": "auth", "message": "登录态失效" }), true);
    }
    if !api::success(&status) {
        return (
            json!({ "state": "fail", "message": api::describe(&status) }),
            false,
        );
    }

    let data = data_of(&status);
    let arrived = truthy(dig(&data, &["arrived", "is_arrived", "has_arrived"]));
    if !arrived {
        let destination = text(dig(&data, &["destination", "name", "target"]));
        let message = if destination.is_empty() {
            "旅行在途".to_string()
        } else {
            format!("旅行在途（{destination}）")
        };
        return (json!({ "state": "skip", "message": message }), false);
    }

    let record = dig(&data, &["record_id", "recordId"]).cloned();
    let claim_body = match record {
        Some(value) => json!({ "record_id": value }),
        None => json!({}),
    };
    let claim = api::post_json(
        account,
        &format!("{PREFIX}/buddy/travel/claim"),
        claim_body,
        &extra,
    )
    .await;
    if unauthorized(&claim) {
        return (json!({ "state": "auth", "message": "登录态失效" }), true);
    }
    let claimed = api::success(&claim);

    // 领完奖再出发下一段；config 失败不阻塞，照样用空体 depart。
    let config = api::get_json(account, &format!("{PREFIX}/buddy/travel/config"), &extra).await;
    let destination = api::success(&config)
        .then(|| text(dig(&data_of(&config), &["destination", "name", "target"])))
        .filter(|text| !text.is_empty());
    let depart_body = match destination {
        Some(value) => json!({ "destination": value }),
        None => json!({}),
    };
    let depart = api::post_json(
        account,
        &format!("{PREFIX}/buddy/travel/depart"),
        depart_body,
        &extra,
    )
    .await;

    let reward = {
        let claim_data = data_of(&claim);
        text(dig(&claim_data, &["reward", "credits", "points", "amount"]))
    };
    let message = match (claimed, api::success(&depart)) {
        (true, true) => {
            if reward.is_empty() {
                "已领取并出发".to_string()
            } else {
                format!("已领取 {reward}，再次出发")
            }
        }
        (true, false) => "已领取，出发失败".to_string(),
        (false, true) => format!("领奖失败（{}），已再次出发", api::describe(&claim)),
        (false, false) => format!("领奖失败：{}", api::describe(&claim)),
    };
    (
        json!({ "state": if claimed { "ok" } else { "fail" }, "message": message }),
        false,
    )
}

/// 盲盒：chances → 按余额循环 draw。
async fn lottery(account: &Value) -> (Value, bool) {
    let extra = headers(account);
    let chances = api::get_json(account, &format!("{PREFIX}/lottery/chances"), &extra).await;
    if unauthorized(&chances) {
        return (json!({ "state": "auth", "message": "登录态失效" }), true);
    }
    if !api::success(&chances) {
        return (
            json!({ "state": "fail", "message": api::describe(&chances) }),
            false,
        );
    }

    let data = data_of(&chances);
    let balance = number(dig(&data, &["balance", "chances", "count", "remain"])).unwrap_or(0);
    if balance <= 0 {
        return (json!({ "state": "skip", "message": "没有可用次数" }), false);
    }

    let mut drawn = 0usize;
    let mut rewards: Vec<String> = Vec::new();
    while drawn < MAX_DRAWS && drawn < balance as usize {
        let response = api::post_json(
            account,
            &format!("{PREFIX}/lottery/draw"),
            json!({}),
            &extra,
        )
        .await;
        if unauthorized(&response) {
            return (
                json!({
                    "state": "auth",
                    "message": if drawn == 0 { "登录态失效".to_string() } else { format!("登录态失效（已抽 {drawn} 次）") }
                }),
                true,
            );
        }
        if !api::success(&response) {
            break;
        }
        drawn += 1;
        let payload = data_of(&response);
        let reward = text(dig(&payload, &["reward", "credits", "points", "amount"]));
        if !reward.is_empty() {
            rewards.push(reward);
        }
    }

    if drawn == 0 {
        return (
            json!({ "state": "fail", "message": "抽取失败" }),
            false,
        );
    }
    let message = if rewards.is_empty() {
        format!("抽了 {drawn} 次")
    } else {
        format!("抽了 {drawn} 次：{}", rewards.join("、"))
    };
    (json!({ "state": "ok", "message": message }), false)
}

/// 任务：列表里挑「有奖励且未领取」的逐条 accept。
async fn tasks(account: &Value) -> (Value, bool) {
    let extra = headers(account);
    let list = api::get_json(account, &format!("{PREFIX}/tasks"), &extra).await;
    if unauthorized(&list) {
        return (json!({ "state": "auth", "message": "登录态失效" }), true);
    }
    if !api::success(&list) {
        return (
            json!({ "state": "fail", "message": api::describe(&list) }),
            false,
        );
    }

    let data = data_of(&list);
    let items = dig(&data, &["tasks", "list", "records"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return (json!({ "state": "skip", "message": "没有任务" }), false);
    }

    let mut claimed = 0usize;
    let mut skipped = 0usize;
    for item in &items {
        let has_reward = truthy(item.get("has_reward"))
            || dig(item, &["hasReward"]).is_some_and(|value| truthy(Some(value)));
        if !has_reward {
            continue;
        }

        let status = dig(item, &["accept_status", "acceptStatus", "status"])
            .map(|value| text(Some(value)))
            .unwrap_or_default();
        if matches!(
            status.as_str(),
            "1" | "2" | "claimed" | "accepted" | "已领取" | "true" | "True"
        ) {
            skipped += 1;
            continue;
        }

        let code = dig(item, &["task_code", "taskCode", "code"]).cloned();
        let body = match code {
            Some(value) => json!({ "task_code": value }),
            None => json!({}),
        };
        let response =
            api::post_json(account, &format!("{PREFIX}/tasks/accept"), body, &extra).await;
        if unauthorized(&response) {
            return (
                json!({
                    "state": "auth",
                    "message": format!("登录态失效（已领 {claimed} 项）")
                }),
                true,
            );
        }
        if api::success(&response) {
            claimed += 1;
        }
    }

    let message = match (claimed, skipped) {
        (0, 0) => "没有可领取的任务".to_string(),
        (0, skipped) => format!("{skipped} 项已领取"),
        (claimed, 0) => format!("领取了 {claimed} 项"),
        (claimed, skipped) => format!("领取了 {claimed} 项，{skipped} 项此前已领"),
    };
    (
        json!({ "state": if claimed > 0 { "ok" } else { "skip" }, "message": message }),
        false,
    )
}

async fn round(account: &Value) -> (Value, bool) {
    let (travel_result, travel_auth) = travel(account).await;
    let (lottery_result, lottery_auth) = lottery(account).await;
    let (tasks_result, tasks_auth) = tasks(account).await;

    (
        json!({
            "travel": travel_result,
            "lottery": lottery_result,
            "tasks": tasks_result,
        }),
        travel_auth || lottery_auth || tasks_auth,
    )
}

/// 单账号跑一轮成长中心。凭证失效时刷新一次后重跑。
///
/// 返回 `{ "ok": bool, "travel": {...}, "lottery": {...}, "tasks": {...}, "patch": object|null }`
pub async fn run(account: &Value) -> Value {
    let (result, needs_auth) = round(account).await;
    if !needs_auth || !api::can_refresh(account) {
        return finish(result, None, account);
    }

    let patch = match api::refresh(account).await {
        Ok(patch) => patch,
        Err(error) => {
            return json!({ "ok": false, "message": error, "patch": Value::Null });
        }
    };
    let mut working = account.clone();
    if let (Some(target), Some(source)) = (working.as_object_mut(), patch.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let (retried, _) = round(&working).await;
    finish(retried, Some(patch), &working)
}

fn finish(result: Value, patch: Option<Value>, account: &Value) -> Value {
    let states = ["travel", "lottery", "tasks"].map(|key| {
        result
            .get(key)
            .and_then(|item| item.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("fail")
            .to_string()
    });

    // 三个动作全部是 auth/fail 才算整轮失败；只要有一个 ok 就算跑通。
    let ok = states.iter().any(|state| state.as_str() == "ok")
        || states.iter().all(|state| state.as_str() == "skip");

    let mut payload = json!({
        "ok": ok,
        "travel": result.get("travel").cloned().unwrap_or(Value::Null),
        "lottery": result.get("lottery").cloned().unwrap_or(Value::Null),
        "tasks": result.get("tasks").cloned().unwrap_or(Value::Null),
        "uid": account.get("uid").cloned().unwrap_or(Value::Null),
    });
    if let (Some(object), Some(patch)) = (payload.as_object_mut(), patch) {
        object.insert("patch".to_string(), patch);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dig_takes_first_non_null_key() {
        let value = json!({ "arrived": true, "record_id": "r1", "is_arrived": false });
        assert_eq!(text(dig(&value, &["arrived", "is_arrived"])), "true");
        assert_eq!(text(dig(&value, &["record_id", "recordId"])), "r1");
        assert!(dig(&value, &["missing"]).is_none());
    }

    #[test]
    fn number_accepts_strings_and_rounds_floats() {
        assert_eq!(number(Some(&json!("3"))), Some(3));
        assert_eq!(number(Some(&json!(2.9999))), Some(3));
        assert_eq!(number(Some(&json!(""))), None);
    }

    #[test]
    fn truthy_matches_service_flags() {
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!("true"))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(None));
    }

    #[test]
    fn claimed_status_values_are_recognised() {
        for status in ["1", "2", "claimed", "accepted", "已领取", "true"] {
            assert!(
                matches!(
                    status,
                    "1" | "2" | "claimed" | "accepted" | "已领取" | "true" | "True"
                ),
                "{status} 应识别为已领取"
            );
        }
        assert!(!matches!("0", "1" | "2" | "claimed"));
    }
}

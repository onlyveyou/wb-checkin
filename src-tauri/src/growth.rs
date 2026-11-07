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
//! | 任务领奖 | `/v2/activity/growth/tasks/{task_code}/claim` | POST | `{}`（幂等，已领会返回 already_claimed） |
//!
//! 任务的**完成度不由我们驱动**：`progress` 只随客户端里的真实行为（创建画布、读文档、
//! 召唤专家等）增长，服务端没有对外暴露上报进度的入口（`tasks/{code}/complete`、
//! `/finish`、`/report`、`/progress` 四个形态实测全 404）。所以自动化的边界是三件事：
//! ① 接取还没报名的任务（`accept` 要 `{"task_codes": [...]}` 数组，接取后服务端才给
//! 进度槽，`progress` 从 null 变成 0/N）；② 领已完成任务的奖励；③ 把没做完的清单交给
//! 界面，一键跳到客户端里对应的专家/页面。
//!
//! 旧实现调的是 `/tasks/accept` 且传单数 `task_code`，实测该接口要数组，单数体一律
//! 400，因此既没领到奖、也没接取成功。
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

/// 任务状态：`not_accepted` / `accepted` / `in_progress` / `completed` / `claimed`。
fn task_status(item: &Value) -> String {
    dig(item, &["accept_status", "acceptStatus", "status"])
        .map(|value| text(Some(value)))
        .unwrap_or_default()
}

fn task_code(item: &Value) -> Option<String> {
    dig(item, &["task_code", "taskCode", "code"])
        .map(|value| text(Some(value)))
        .filter(|code| !code.is_empty())
}

/// 待体验任务的展示形态：界面上「去体验」要用它拼跳转。
///
/// 已完成、已领取的不算待体验；未接取的也算（接取后才有进度槽）。
fn pending_view(item: &Value) -> Option<Value> {
    let code = task_code(item)?;
    if task_completed(item) || task_status(item) == "claimed" {
        return None;
    }
    let progress = item.get("progress").filter(|value| !value.is_null());
    let current = progress.and_then(|p| number(p.get("current"))).unwrap_or(0);
    let target = progress
        .and_then(|p| number(p.get("target")))
        .filter(|target| *target > 0)
        .unwrap_or(1);
    Some(json!({
        "code": code,
        "title": text(dig(item, &["title", "name"])),
        "description": text(dig(item, &["task_desc", "description"])),
        "jumpUrl": text(dig(item, &["jump_url", "jumpUrl"])),
        "reward": number(item.get("reward_credit")).unwrap_or(0),
        "current": current,
        "target": target,
    }))
}

/// 百分号解码。只处理 `%XX` 与 `+`，够用就行 —— 引一个 crate 只为这两个字符不值当。
fn percent_decode(input: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 从一个 `workbuddy://` 链接的查询串里取 `expertId`。
fn expert_id_in_deeplink(link: &str) -> Option<String> {
    if !link.starts_with("workbuddy://") {
        return None;
    }
    let query = link.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("expertId=") {
            let id = percent_decode(value);
            let id = id.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// 从任务的跳转链接里取专家 id。
///
/// 线上实际有三种形态（2026-11-06 按真实数据核对）：
/// 1. `workbuddy://experts?expertId=ex_xxx` —— 直接是 deeplink；
/// 2. `https://www.workbuddy.cn/work/launch/?deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_xxx&buddy_type=workbuddy`
///    —— 站内中转页，expertId 藏在二次编码的 `deeplink` 参数里，**这是最常见的一种**；
/// 3. `workbuddy://expert`、`workbuddy://library`、`https://…/events/follow-guide/`
///    —— 不带 expertId，界面自动化无从下手。
///
/// 只处理前两种；第三种返回 None，调用方据此退回「只打开链接」。
pub fn expert_id_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("workbuddy://") {
        return expert_id_in_deeplink(trimmed);
    }

    // 站内中转页：把 deeplink 参数解出来再看
    let query = trimmed.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(raw) = pair.strip_prefix("deeplink=") {
            return expert_id_in_deeplink(&percent_decode(raw));
        }
    }
    None
}

/// 任务是否已经「完成」（可以领奖）。
///
/// 服务端用 `progress.current >= progress.target` 表示完成，另有 `completed` 状态；
/// `claimed` 表示奖励已到手，不必再请求。
fn task_completed(item: &Value) -> bool {
    let status = task_status(item);
    if status == "claimed" {
        return false;
    }
    if status == "completed" {
        return true;
    }

    let Some(progress) = item.get("progress").filter(|value| !value.is_null()) else {
        return false;
    };
    match (
        number(progress.get("current")),
        number(progress.get("target")),
    ) {
        (Some(current), Some(target)) => target >= 1 && current >= target,
        _ => false,
    }
}

/// 任务：把「已完成但没领奖」的逐条 claim。
///
/// 之前这里调的是 `/tasks/accept`，且传的是单数 `task_code` —— 实测该接口要的是
/// `{"task_codes": [...]}` 数组，单数体一律 400，所以「任务领奖」从来没生效过。
/// 更关键的是 accept 只是「报名」，不推进完成度：完成度由用户在客户端里真实操作
/// 触发（创建画布、读文档、召唤专家…），因此自动化的落点只能是**领已完成任务的奖励**。
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
    let mut already = 0usize;
    let mut failed = 0usize;
    let mut credit = 0i64;
    let mut titles: Vec<String> = Vec::new();

    // 未接取的任务先批量接取：服务端只有接取之后才给进度槽（`progress` 从 null 变成
    // 0/N），不再「报名」的话，用户在客户端里真的用了也不会被记进度。
    let pending_accept: Vec<Value> = items
        .iter()
        .filter(|item| task_status(item) == "not_accepted")
        .filter_map(task_code)
        .map(Value::String)
        .collect();
    let mut accepted = 0usize;
    if !pending_accept.is_empty() {
        let response = api::post_json(
            account,
            &format!("{PREFIX}/tasks/accept"),
            json!({ "task_codes": pending_accept }),
            &extra,
        )
        .await;
        if unauthorized(&response) {
            return (
                json!({ "state": "auth", "message": "登录态失效" }),
                true,
            );
        }
        if api::success(&response) {
            accepted = pending_accept.len();
        }
    }

    for item in &items {
        if !task_completed(item) {
            continue;
        }
        let Some(code) = dig(item, &["task_code", "taskCode", "code"])
            .map(|value| text(Some(value)))
            .filter(|code| !code.is_empty())
        else {
            continue;
        };

        // 领奖幂等：已领过的会返回 `{"already_claimed": true, "credit": 0}`，不会重复发奖。
        let response = api::post_json(
            account,
            &format!("{PREFIX}/tasks/{code}/claim"),
            json!({}),
            &extra,
        )
        .await;
        if unauthorized(&response) {
            return (
                json!({
                    "state": "auth",
                    "message": format!("登录态失效（已领 {claimed} 项）")
                }),
                true,
            );
        }
        if !api::success(&response) {
            failed += 1;
            continue;
        }

        let payload = data_of(&response);
        if truthy(payload.get("already_claimed")) {
            already += 1;
            continue;
        }
        claimed += 1;
        credit += number(payload.get("credit")).unwrap_or(0);
        let title = text(dig(item, &["title", "name"]));
        if !title.is_empty() {
            titles.push(title);
        }
    }

    // 还没做完的（含刚接取的）汇总给界面，点一下就能跳到客户端里对应的专家/页面。
    let pending: Vec<Value> = items.iter().filter_map(pending_view).collect();

    let mut parts: Vec<String> = Vec::new();
    if accepted > 0 {
        parts.push(format!("接取 {accepted} 项"));
    }
    if claimed > 0 {
        let detail = if titles.is_empty() {
            String::new()
        } else {
            format!("（{}）", titles.join("、"))
        };
        parts.push(format!("领了 {claimed} 项{detail}，共 {credit} 积分"));
    }
    if already > 0 {
        parts.push(format!("{already} 项此前已领"));
    }
    if failed > 0 {
        parts.push(format!("{failed} 项领取失败"));
    }
    if !pending.is_empty() {
        let names: Vec<String> = pending
            .iter()
            .filter_map(|item| item.get("title").and_then(Value::as_str))
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .take(3)
            .collect();
        let suffix = if names.is_empty() {
            String::new()
        } else {
            format!("（{}…）", names.join("、"))
        };
        parts.push(format!("{} 项待体验{suffix}", pending.len()));
    }

    let message = if parts.is_empty() {
        "没有可领的任务".to_string()
    } else {
        parts.join("，")
    };
    (
        json!({
            "state": if claimed > 0 { "ok" } else { "skip" },
            "message": message,
            "claimed": claimed,
            "accepted": accepted,
            "credit": credit,
            "pending": pending,
        }),
        false,
    )
}

/// 只读地拉一次待体验任务（顺带接取未接取的）。
///
/// 供界面的「待体验」清单使用：不刷新凭证、不做旅行与盲盒，失败就把错误交给调用方。
pub async fn pending_only(account: &Value) -> Value {
    let extra = headers(account);
    let list = api::get_json(account, &format!("{PREFIX}/tasks"), &extra).await;
    if unauthorized(&list) {
        return json!({ "ok": false, "message": "登录态失效", "pending": [] });
    }
    if !api::success(&list) {
        return json!({ "ok": false, "message": api::describe(&list), "pending": [] });
    }

    let data = data_of(&list);
    let items = dig(&data, &["tasks", "list", "records"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let pending_accept: Vec<Value> = items
        .iter()
        .filter(|item| task_status(item) == "not_accepted")
        .filter_map(task_code)
        .map(Value::String)
        .collect();
    if !pending_accept.is_empty() {
        let _ = api::post_json(
            account,
            &format!("{PREFIX}/tasks/accept"),
            json!({ "task_codes": pending_accept }),
            &extra,
        )
        .await;
    }

    let pending: Vec<Value> = items.iter().filter_map(pending_view).collect();
    json!({ "ok": true, "message": Value::Null, "pending": pending })
}

/// 单个任务此刻的进度。
///
/// 界面自动化做完之后必须回查这一项：进度是从客户端里的真实行为反推的，
/// 「消息发出去了」不等于「服务端认了」。之前只报「已发送」就宣布成功，
/// 结果任务进度一直是 0/1 —— 那是在骗自己。
///
/// 返回 `{ found, current, target, done, status }`；查不到任务时 `found` 为 false。
pub async fn task_state(account: &Value, code: &str) -> Value {
    let blank = json!({
        "found": false, "current": 0, "target": 0, "done": false, "status": "",
    });
    let code = code.trim();
    if code.is_empty() {
        return blank;
    }

    let extra = headers(account);
    let response = api::get_json(account, &format!("{PREFIX}/tasks"), &extra).await;
    if !api::success(&response) {
        return blank;
    }

    let data = data_of(&response);
    let items = dig(&data, &["tasks", "list", "records"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(item) = items
        .iter()
        .find(|item| task_code(item).as_deref() == Some(code))
    else {
        return blank;
    };

    let progress = item.get("progress").filter(|value| !value.is_null());
    let current = progress.and_then(|p| number(p.get("current"))).unwrap_or(0);
    // target 缺失或为 0 时兜底 1：`current >= target` 不能因为 0 而误判成完成
    let target = progress
        .and_then(|p| number(p.get("target")))
        .filter(|target| *target > 0)
        .unwrap_or(1);
    json!({
        "found": true,
        "current": current,
        "target": target,
        "done": task_completed(item),
        "status": task_status(item),
    })
}

async fn round(account: &Value, with_lottery: bool) -> (Value, bool) {
    let (travel_result, travel_auth) = travel(account).await;
    let (tasks_result, tasks_auth) = tasks(account).await;

    // 自动处理只跑「旅行 + 任务」，盲盒留给用户手动点：抽奖有次数上限，
    // 悄悄替用户抽完不合适。
    if !with_lottery {
        return (
            json!({
                "travel": travel_result,
                "lottery": Value::Null,
                "tasks": tasks_result,
            }),
            travel_auth || tasks_auth,
        );
    }

    let (lottery_result, lottery_auth) = lottery(account).await;
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
    run_with(account, true).await
}

/// 自动签到之后的顺手处理：只做旅行领奖/出发与任务领奖，不抽盲盒。
pub async fn run_auto(account: &Value) -> Value {
    run_with(account, false).await
}

async fn run_with(account: &Value, with_lottery: bool) -> Value {
    let (result, needs_auth) = round(account, with_lottery).await;
    if !needs_auth || !api::can_refresh(account) {
        return finish(result, None, account);
    }

    let patch = match api::refresh_guarded(account).await {
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
    let (retried, _) = round(&working, with_lottery).await;
    finish(retried, Some(patch), &working)
}

fn finish(result: Value, patch: Option<Value>, account: &Value) -> Value {
    // 自动模式没有盲盒，该项是 null，不能参与整体成败判定。
    let states: Vec<String> = ["travel", "lottery", "tasks"]
        .iter()
        .filter_map(|key| result.get(*key).filter(|item| !item.is_null()))
        .map(|item| {
            item.get("state")
                .and_then(Value::as_str)
                .unwrap_or("fail")
                .to_string()
        })
        .collect();

    // 参与的动作里只要有一个 ok 就算跑通；全部 skip（在途/无可领）也算跑通。
    let ok = states.iter().any(|state| state == "ok")
        || (!states.is_empty() && states.iter().all(|state| state == "skip"));

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
    fn task_completed_reads_progress_and_status() {
        // 进度达标 → 可领
        assert!(task_completed(&json!({
            "accept_status": "in_progress",
            "progress": { "current": 3, "target": 3 }
        })));
        // 进度不足 → 不可领
        assert!(!task_completed(&json!({
            "accept_status": "in_progress",
            "progress": { "current": 2, "target": 3 }
        })));
        // 已完成但还没领 → 可领
        assert!(task_completed(&json!({ "accept_status": "completed" })));
        // 已领过 → 不再请求
        assert!(!task_completed(&json!({
            "accept_status": "claimed",
            "progress": { "current": 1, "target": 1 }
        })));
        // 未接取、没有进度 → 不可领
        assert!(!task_completed(&json!({ "accept_status": "not_accepted", "progress": null })));
        // 进度字段缺失或 target 为 0 时不能误判成完成
        assert!(!task_completed(&json!({ "accept_status": "accepted" })));
        assert!(!task_completed(&json!({
            "accept_status": "accepted",
            "progress": { "current": 0, "target": 0 }
        })));
    }

    #[test]
    fn expert_id_is_read_from_deeplink_only() {
        assert_eq!(
            expert_id_from_url("workbuddy://experts?expertId=ex_1"),
            Some("ex_1".to_string())
        );
        // 参数顺序不固定
        assert_eq!(
            expert_id_from_url("workbuddy://experts?from=task&expertId=ex_2"),
            Some("ex_2".to_string())
        );
        // 没有 expertId 的体验任务（关注公众号、授权连接器）自动化帮不上忙
        assert_eq!(expert_id_from_url("workbuddy://tasks"), None);
        assert_eq!(expert_id_from_url("workbuddy://experts?expertId="), None);
        assert_eq!(expert_id_from_url("workbuddy://expert"), None, "线上真有这种不带 id 的");
        assert_eq!(expert_id_from_url("https://www.workbuddy.cn/events/follow-guide/"), None);
        assert_eq!(expert_id_from_url(""), None);
    }

    /// 线上最常见的形态：站内中转页把 deeplink 二次编码放在查询串里。
    #[test]
    fn expert_id_is_read_through_encoded_launch_url() {
        let url = "https://www.workbuddy.cn/work/launch/?deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_GDl31WahyIH0&buddy_type=workbuddy";
        assert_eq!(
            expert_id_from_url(url),
            Some("ex_GDl31WahyIH0".to_string()),
            "中转页里的 expertId 必须能解出来，否则「去体验」只会打开网页"
        );

        // buddy_type 在 deeplink 前面也要认
        let reordered = "https://www.workbuddy.cn/work/launch/?buddy_type=workbuddy&deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_WZY136wV5aMt";
        assert_eq!(
            expert_id_from_url(reordered),
            Some("ex_WZY136wV5aMt".to_string())
        );

        // 中转页但 deeplink 里没有 expertId
        let no_id = "https://www.workbuddy.cn/work/launch/?deeplink=workbuddy%3A%2F%2Flibrary";
        assert_eq!(expert_id_from_url(no_id), None);
    }

    #[test]
    fn percent_decode_handles_escapes_and_literals() {
        assert_eq!(percent_decode("workbuddy%3A%2F%2F"), "workbuddy://");
        assert_eq!(percent_decode("a+b"), "a b");
        // 残缺的转义原样保留，不能吃掉字符
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("纯中文"), "纯中文");
    }

    #[test]
    fn pending_view_keeps_unfinished_and_drops_settled() {
        let done = json!({
            "task_code": "a", "accept_status": "in_progress",
            "progress": { "current": 1, "target": 1 }
        });
        assert!(pending_view(&done).is_none(), "已达标的不该出现在待体验里");

        let claimed = json!({ "task_code": "b", "accept_status": "claimed" });
        assert!(pending_view(&claimed).is_none());

        let todo = json!({
            "task_code": "c", "title": "体验「抖音策略师」", "accept_status": "accepted",
            "progress": { "current": 0, "target": 1 }, "reward_credit": 50,
            "jump_url": "workbuddy://experts?expertId=ex_1"
        });
        let view = pending_view(&todo).expect("未完成的任务要留下");
        assert_eq!(view["code"], json!("c"));
        assert_eq!(view["reward"], json!(50));
        assert_eq!(view["target"], json!(1));
        assert_eq!(view["jumpUrl"], json!("workbuddy://experts?expertId=ex_1"));

        // 还没接取、没有进度槽：同样是待体验，target 兜底 1 防止除零式误判
        let fresh = json!({ "task_code": "d", "accept_status": "not_accepted", "progress": null });
        let view = pending_view(&fresh).expect("未接取的任务也要列出来");
        assert_eq!(view["target"], json!(1));
        assert_eq!(view["current"], json!(0));
    }

    #[test]
    fn auto_scope_ignores_absent_lottery_for_overall_verdict() {
        // 自动模式不跑盲盒，该项是 null：不能因为「null 没有 state」判整轮失败
        let outcome = finish(
            json!({
                "travel": { "state": "skip", "message": "旅行在途" },
                "lottery": Value::Null,
                "tasks": { "state": "ok", "message": "领取了 2 项" },
            }),
            None,
            &json!({ "uid": "u-1" }),
        );
        assert_eq!(outcome["ok"], json!(true));
        assert_eq!(outcome["lottery"], Value::Null);
        assert_eq!(outcome["tasks"]["state"], json!("ok"));
    }

    #[test]
    fn all_skipped_counts_as_success_but_all_failed_does_not() {
        let skipped = finish(
            json!({
                "travel": { "state": "skip" },
                "lottery": Value::Null,
                "tasks": { "state": "skip" },
            }),
            None,
            &json!({}),
        );
        assert_eq!(skipped["ok"], json!(true));

        let failed = finish(
            json!({
                "travel": { "state": "fail" },
                "lottery": Value::Null,
                "tasks": { "state": "auth" },
            }),
            None,
            &json!({}),
        );
        assert_eq!(failed["ok"], json!(false));
    }
}

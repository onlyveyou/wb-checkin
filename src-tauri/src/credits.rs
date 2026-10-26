//! 积分（credits）查询。
//!
//! 官方套餐页把可用量拆成三个接口：summary 给出每个包的周期总量/剩余/已用，
//! paid-packages 与 free-packages 给出可读包名与周期结束时间。实测 summary
//! 一次就足以展示余额，另两个只用于补充名称与到期日，因此三者并发且明细失败
//! 不影响余额。
//!
//! 注意路径前缀：这三个接口挂在 `/billing/meter/...` 下，**不带 `/v2`**，
//! 与签到用的 `/v2/billing/meter/...` 不是同一组路由。实探结果：带 `/v2` 的
//! 旧路径 `/v2/billing/meter/get-user-resource` 虽然存在，但 summary 系列只有
//! 无前缀这条。

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::api;

const SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
const PAID_PATH: &str = "/billing/meter/get-user-resource-paid-packages";
const FREE_PATH: &str = "/billing/meter/get-user-resource-free-packages";

/// 官方套餐配置里的商品码。这两个字段是接口的必需入参：缺省会直接返回
/// `PackageCodes required`，所以不是可选筛选条件。
const PAID_PACKAGE_CODES: &[&str] = &[
    "TCACA_code_002_AkiJS3ZHF5",
    "TCACA_code_023_4xbGhMrE6q",
    "TCACA_code_026_BaESVICNoi",
    "TCACA_code_027_0FCGVA6vSa",
    "TCACA_code_009_0XmEQc2xOf",
    "TCACA_code_038_OhvqZtiPKr",
];
const FREE_PACKAGE_CODES: &[&str] = &[
    "TCACA_code_008_cfWoLwvjU4",
    "TCACA_code_007_nzdH5h4Nl0",
    "TCACA_code_028_NtpWi0jzXs",
    "TCACA_code_029_6wCGEWquYy",
    "TCACA_code_030_BjSt89qTvr",
];

/// 容量字段在响应里是**字符串**（`"127.55000537"`），需要单独解析。
fn number_of(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn text_of(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 用户中心的 Axios 拦截器始终带这些头；缺了 `X-Client-Platform` 网关可能
/// 把请求当成未知客户端。
fn resource_headers(account: &Value) -> Vec<(&'static str, String)> {
    let origin = api::resolve_base(account);
    vec![
        ("X-Client-Platform", "web".to_string()),
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("Origin", origin.clone()),
        ("Referer", format!("{origin}/profile/plans-usage")),
    ]
}

fn paid_body() -> Value {
    json!({
        "PageNumber": 1,
        "PageSize": 200,
        "Status": [0, 3],
        "PackageCodes": PAID_PACKAGE_CODES,
        "NeedRenewInfo": true,
    })
}

/// 免费包按「切片」返回，需要给一个时间窗。用当天即可覆盖在进行中的资源。
fn free_body() -> Value {
    let now = chrono::Local::now();
    json!({
        "PageNumber": 1,
        "PageSize": 200,
        "Status": [0, 3],
        "SlicePeriodStartTime": now.format("%Y-%m-%d 00:00:00").to_string(),
        "SlicePeriodEndTime": now.format("%Y-%m-%d 23:59:59").to_string(),
        "PackageCodes": FREE_PACKAGE_CODES,
    })
}

fn array_at(response: &Value, key: &str) -> Vec<Value> {
    response
        .get("data")
        .and_then(|data| data.get(key))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 从明细接口建立「商品码 → (包名, 到期时间)」索引。
///
/// 同一个码可能在多个资源条目里出现（不同周期/不同子商品），保留第一个非空值即可。
fn meta_index(responses: &[&Value]) -> HashMap<String, (String, String)> {
    let mut index: HashMap<String, (String, String)> = HashMap::new();
    for response in responses {
        for item in array_at(response, "Accounts") {
            let code = text_of(&item, "PackageCode");
            if code.is_empty() {
                continue;
            }
            // 子商品名比大类名更能说明用途，例如「腾讯云代码助手 (IDE) - 赠送包」
            let sub = text_of(&item, "SubProductName");
            let name = if sub.is_empty() {
                text_of(&item, "PackageName")
            } else {
                sub
            };
            let expires = text_of(&item, "CycleEndTime");

            let entry = index.entry(code).or_default();
            if entry.0.is_empty() && !name.is_empty() {
                entry.0 = name;
            }
            if entry.1.is_empty() && !expires.is_empty() {
                entry.1 = expires;
            }
        }
    }
    index
}

fn merge_patch(target: &mut Value, patch: &Value) {
    if let (Some(dst), Some(src)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in src {
            dst.insert(key.clone(), value.clone());
        }
    }
}

/// 查一次积分。不做任何重试，纯请求 + 解析。
async fn fetch_once(account: &Value) -> Value {
    let extra = resource_headers(account);
    let (summary, free, paid) = tokio::join!(
        api::post_json(account, SUMMARY_PATH, json!({}), &extra),
        api::post_json(account, FREE_PATH, free_body(), &extra),
        api::post_json(account, PAID_PATH, paid_body(), &extra),
    );

    if !api::success(&summary) {
        return json!({
            "ok": false,
            "unauthorized": api::is_unauthorized(&summary),
            "message": api::describe(&summary),
        });
    }

    let meta = meta_index(&[&free, &paid]);
    let mut packages = Vec::new();
    let mut total = 0.0f64;
    let mut remain = 0.0f64;
    let mut used = 0.0f64;
    let mut unit = String::new();

    for item in array_at(&summary, "Packages") {
        let code = text_of(&item, "PackageCode");
        let package_total = number_of(item.get("CycleTotalCapacity")).unwrap_or(0.0);
        let package_remain = number_of(item.get("CycleRemainCapacity")).unwrap_or(0.0);
        let package_used = number_of(item.get("CycleUsedCapacity")).unwrap_or(0.0);
        let package_unit = {
            let raw = text_of(&item, "CapacityUnit");
            if raw.is_empty() {
                "credits".to_string()
            } else {
                raw
            }
        };
        if unit.is_empty() {
            unit = package_unit.clone();
        }

        let (name, expires_at) = meta.get(&code).cloned().unwrap_or_default();
        total += package_total;
        remain += package_remain;
        used += package_used;

        packages.push(json!({
            "code": code,
            "name": name,
            "total": package_total,
            "remain": package_remain,
            "used": package_used,
            "unit": package_unit,
            "expiresAt": expires_at,
        }));
    }

    // `CycleEndTime` 是 "YYYY-MM-DD HH:MM:SS"，字典序即时间序，取最小值为最近到期。
    let nearest_expiry = packages
        .iter()
        .filter_map(|package| {
            package
                .get("expiresAt")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .min()
        .map(String::from);

    json!({
        "ok": true,
        "finishedAt": crate::paths::now_ms(),
        "total": total,
        "remain": remain,
        "used": used,
        "unit": if unit.is_empty() { "credits".to_string() } else { unit },
        "nearestExpiry": nearest_expiry,
        "packages": packages,
    })
}

/// 查询账号积分。凭证失效时用 refresh_token 续期一次再重试。
///
/// 返回值中 `patch`（若有）是新 token，调用方需合并回账号记录。
pub async fn fetch(account: &Value) -> Value {
    let first = fetch_once(account).await;
    if first.get("ok").and_then(Value::as_bool) == Some(true) {
        return first;
    }

    let needs_refresh = first.get("unauthorized").and_then(Value::as_bool) == Some(true)
        && api::can_refresh(account);
    if !needs_refresh {
        return first;
    }

    match api::refresh_guarded(account).await {
        Ok(patch) => {
            let mut working = account.clone();
            merge_patch(&mut working, &patch);
            let mut result = fetch_once(&working).await;
            if let Some(object) = result.as_object_mut() {
                object.insert("patch".to_string(), patch);
            }
            result
        }
        Err(error) => json!({ "ok": false, "message": error }),
    }
}

// ---------------------------------------------------------------------------
// 用量趋势
//
// `get-user-request-usage` 返回的是**逐请求**明细（requestTime + credit），没有粒度
// 参数，日汇总只能在本地做。字段口径与社区实测实现一致（MIT，
// smart-open/TraeWorkAssistant）：`data.data[]`，每行 `requestTime` / `credit` /
// `model` / `requestId`。
// ---------------------------------------------------------------------------

const USAGE_PATH: &str = "/billing/meter/get-user-request-usage";
/// 单次最多拉多少行；账号用量远超时只保留最近 30 天窗口内的部分。
const USAGE_PAGE_SIZE: u32 = 3000;
const USAGE_WINDOW_DAYS: i64 = 30;

fn usage_headers(account: &Value) -> Vec<(&'static str, String)> {
    let origin = api::resolve_base(account);
    vec![
        ("X-Client-Platform", "web".to_string()),
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("Origin", origin.clone()),
        ("Referer", format!("{origin}/profile/plans-usage")),
    ]
}

/// 把 `requestTime` 归一成 `YYYY-MM-DD`。上游可能给秒/毫秒时间戳、RFC3339、
/// `YYYY-MM-DD HH:MM:SS` 或纯日期，四种都兼容。
fn row_date(item: &Value) -> Option<String> {
    use chrono::TimeZone;

    let raw = item
        .get("requestTime")
        .or_else(|| item.get("request_time"))
        .or_else(|| item.get("time"))?;

    match raw {
        Value::Number(number) => {
            let value = number.as_i64()?;
            // 13 位是毫秒，10 位是秒
            let (seconds, nanos) = if value > 1_000_000_000_000 {
                (value / 1000, ((value % 1000) * 1_000_000) as u32)
            } else {
                (value, 0)
            };
            chrono::Local
                .timestamp_opt(seconds, nanos)
                .single()
                .map(|time| time.format("%Y-%m-%d").to_string())
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.len() >= 10 && trimmed.is_char_boundary(10) {
                let head = &trimmed[..10];
                if chrono::NaiveDate::parse_from_str(head, "%Y-%m-%d").is_ok() {
                    return Some(head.to_string());
                }
            }
            if let Ok(time) = chrono::DateTime::parse_from_rfc3339(trimmed) {
                return Some(time.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
            }
            None
        }
        _ => None,
    }
}

fn usage_credit(item: &Value) -> Option<f64> {
    let value = item.get("credit")?;
    let parsed = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

/// 查一个账号最近 30 天的逐日积分消耗。
pub async fn fetch_usage(account: &Value) -> Value {
    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(USAGE_WINDOW_DAYS - 1);
    let body = json!({
        "startTime": format!("{start} 00:00:00"),
        "endTime": format!("{today} 23:59:59"),
        "pageNum": 1,
        "pageSize": USAGE_PAGE_SIZE,
    });

    let response = api::post_json(account, USAGE_PATH, body, &usage_headers(account)).await;
    if !api::success(&response) {
        return json!({
            "ok": false,
            "unauthorized": api::is_unauthorized(&response),
            "message": api::describe(&response),
        });
    }

    let rows = response
        .pointer("/data/data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut daily: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    let mut requests = 0usize;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &rows {
        let Some(date) = row_date(row) else { continue };
        let Some(credit) = usage_credit(row) else { continue };
        // 同一 requestId 可能跨页重复，按 id 去重；没有 id 的行直接计入。
        let key = row
            .get("requestId")
            .or_else(|| row.get("request_id"))
            .and_then(Value::as_str)
            .map(String::from);
        if let Some(key) = key {
            if !seen.insert(key) {
                continue;
            }
        }
        requests += 1;
        *daily.entry(date).or_insert(0.0) += credit;
    }

    let mut days: Vec<Value> = Vec::new();
    for offset in (0..USAGE_WINDOW_DAYS).rev() {
        let date = today - chrono::Duration::days(offset);
        let key = date.format("%Y-%m-%d").to_string();
        let credit = daily.get(&key).copied().unwrap_or(0.0);
        days.push(json!({ "date": key, "credit": (credit * 100.0).round() / 100.0 }));
    }

    let sum = |count: i64| -> f64 {
        days.iter()
            .rev()
            .take(count as usize)
            .filter_map(|day| day.get("credit").and_then(Value::as_f64))
            .sum::<f64>()
    };

    json!({
        "ok": true,
        "days": days,
        "today": ((sum(1)) * 100.0).round() / 100.0,
        "week": ((sum(7)) * 100.0).round() / 100.0,
        "month": ((sum(USAGE_WINDOW_DAYS)) * 100.0).round() / 100.0,
        "requests": requests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn row_date_accepts_four_shapes() {
        // 秒 / 毫秒时间戳
        assert!(row_date(&json!({ "requestTime": 1_760_000_000i64 })).is_some());
        assert!(row_date(&json!({ "requestTime": 1_760_000_000_000i64 })).is_some());
        // 纯日期与日期时间串
        assert_eq!(
            row_date(&json!({ "requestTime": "2026-10-20" })),
            Some("2026-10-20".to_string())
        );
        assert_eq!(
            row_date(&json!({ "requestTime": "2026-10-20 13:05:00" })),
            Some("2026-10-20".to_string())
        );
        // 垃圾值不能 panic
        assert_eq!(row_date(&json!({ "requestTime": "不是时间" })), None);
        assert_eq!(row_date(&json!({})), None);
    }

    #[test]
    fn usage_credit_rejects_negative_and_junk() {
        assert_eq!(usage_credit(&json!({ "credit": 1.5 })), Some(1.5));
        assert_eq!(usage_credit(&json!({ "credit": "2" })), Some(2.0));
        assert_eq!(usage_credit(&json!({ "credit": -1 })), None);
        assert_eq!(usage_credit(&json!({ "credit": "abc" })), None);
        assert_eq!(usage_credit(&json!({})), None);
    }

    #[test]
    fn number_of_parses_strings_and_rejects_junk() {
        assert_eq!(number_of(Some(&json!("127.55000537"))), Some(127.55000537));
        assert_eq!(number_of(Some(&json!(4100))), Some(4100.0));
        assert_eq!(number_of(Some(&json!(""))), None);
        assert_eq!(number_of(Some(&json!("abc"))), None);
        assert_eq!(number_of(None), None);
    }

    #[test]
    fn meta_index_prefers_sub_product_name() {
        let free = json!({ "data": { "Accounts": [{
            "PackageCode": "TCACA_code_007_nzdH5h4Nl0",
            "PackageName": "CodeBuddy个人版国内运营裂变包",
            "SubProductName": "腾讯云代码助手 (IDE) - 赠送包",
            "CycleEndTime": "2026-10-16 13:20:45"
        }]}});
        let paid = json!({ "data": { "Accounts": [] }});

        let index = meta_index(&[&free, &paid]);
        let (name, expires) = index.get("TCACA_code_007_nzdH5h4Nl0").expect("应建立索引");
        assert_eq!(name, "腾讯云代码助手 (IDE) - 赠送包");
        assert_eq!(expires, "2026-10-16 13:20:45");
    }

    #[test]
    fn meta_index_tolerates_missing_fields() {
        let free = json!({ "data": { "Accounts": [
            { "PackageCode": "c1", "PackageName": "只有名字" },
            { "PackageName": "没有码，应跳过" }
        ]}});
        let index = meta_index(&[&free]);
        assert_eq!(index.len(), 1);
        assert_eq!(index.get("c1").map(|e| e.0.as_str()), Some("只有名字"));
        assert_eq!(index.get("c1").map(|e| e.1.as_str()), Some(""));
    }

    #[test]
    fn array_at_returns_empty_for_error_envelope() {
        // 网关返回 HTML 错误时没有 data 字段，不能 panic
        let error = json!({ "code": 401, "message": "<html>401</html>" });
        assert!(array_at(&error, "Packages").is_empty());
        assert!(array_at(&json!({}), "Accounts").is_empty());
    }

    #[test]
    fn fetch_body_carries_required_package_codes() {
        // 缺 PackageCodes 服务端会直接 400，因此必须确认入参存在
        let paid = paid_body();
        assert!(paid.get("PackageCodes").and_then(Value::as_array).is_some());
        assert!(paid.get("NeedRenewInfo").is_some());

        let free = free_body();
        let codes = free
            .get("PackageCodes")
            .and_then(Value::as_array)
            .expect("免费包也需要 PackageCodes");
        assert!(!codes.is_empty());
        assert!(free.get("SlicePeriodStartTime").is_some());
        assert!(free.get("SlicePeriodEndTime").is_some());
    }

    #[test]
    fn resource_headers_pin_origin_to_whitelisted_domain() {
        // 账号记录里的域名不可信：白名单外必须回落到默认域名
        let evil = json!({ "domain": "attacker.example" });
        let headers = resource_headers(&evil);
        let origin = headers
            .iter()
            .find(|(key, _)| *key == "Origin")
            .map(|(_, value)| value.clone())
            .expect("应有 Origin");
        assert_eq!(origin, api::DEFAULT_BASE);
    }
}

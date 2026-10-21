//! 签到日志。只存本地，最多保留 500 条。
//!
//! 写入前做一次脱敏：即便服务端在错误信息里回显了凭证，也不会落到日志文件里。

use serde_json::{json, Value};

use crate::paths::{logs_file, now_ms, read_json_array, today_local, write_private};

const MAX_RECORDS: usize = 500;

/// 去掉可能的凭证片段并截断长度。
fn sanitize(message: &str) -> String {
    let mut output = String::with_capacity(message.len().min(240));
    for piece in message.split_whitespace() {
        let looks_like_secret = piece.len() > 40
            && piece
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_=.".contains(c));
        if looks_like_secret {
            output.push_str("[已隐藏]");
        } else {
            output.push_str(piece);
        }
        output.push(' ');
    }
    output.trim().chars().take(240).collect()
}

pub fn append(account_id: &str, label: &str, result: &str, message: &str) {
    let mut records = read_json_array(&logs_file());
    records.insert(
        0,
        json!({
            "ts": now_ms(),
            "date": today_local(),
            "accountId": account_id,
            "account": sanitize(label),
            "result": result,
            "message": sanitize(message),
        }),
    );
    records.truncate(MAX_RECORDS);

    let text = serde_json::to_string_pretty(&Value::Array(records)).unwrap_or_else(|_| "[]".into());
    let _ = write_private(&logs_file(), &text);
}

pub fn list(limit: usize) -> Vec<Value> {
    let mut records = read_json_array(&logs_file());
    records.truncate(limit);
    records
}

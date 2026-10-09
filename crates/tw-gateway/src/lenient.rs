//! 对方答的 JSON 里，**数字有时写成字符串**。
//!
//! OpenAI 的设备码接口 2026-10 起把 `interval` 从 `5` 改成了 `"5"`，按 `u64` 读的整个登录
//! 就报「无法识别」停在那儿。这类「隔几秒」「几秒后过期」的字段两种写法都认；认不出来的
//! 当没给，由调用方用自己的默认值 —— 一个轮询间隔写成了别的样子，不该让登录走不下去。

use serde_json::Value;

/// 秒数：非负整数，或者只由数字组成的字符串（前后空白不算）。别的写法是 `None`
pub fn seconds(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 给 serde 用：字段是数字或数字字符串都读成秒数，认不出来或没给是 0
pub fn seconds_or_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = <Value as serde::Deserialize>::deserialize(d)?;
    Ok(seconds(&v).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_number_and_a_numeric_string_are_both_seconds() {
        assert_eq!(seconds(&json!(5)), Some(5));
        assert_eq!(seconds(&json!("5")), Some(5));
        assert_eq!(seconds(&json!(" 3600 ")), Some(3600));
    }

    #[test]
    fn anything_else_is_not_given() {
        for v in [
            json!(-1),
            json!(1.5),
            json!("5s"),
            json!(""),
            json!(null),
            json!(true),
            json!([5]),
        ] {
            assert_eq!(seconds(&v), None, "{v}");
        }
    }
}

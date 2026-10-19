//! 较旧版本导出的配置包：读不进来的字段按「一键修复」的规矩在内存里修好再导入。
//!
//! 规矩和 `tw_config::repair` 一样，**只修两种、都只是删掉一个键**：取值不在可选范围里的
//! （删掉就回到默认值）、不认识的字段（删掉）。不同的是这里只要求「读得进来」，不要求
//! 「是一份完整、能用的配置」：一个配置包可以只有一个上游，没有网关密钥、没有控制面的钥匙，
//! 拿整份校验去要求它，什么都修不了。修完之后能不能用由导入那一步照常校验。
//!
//! **更新的版本写的不修**：那时不认识的字段是旧版本不懂的新东西，删掉就丢了，要说的是
//! 「请先更新」（[`crate::Manifest::from_newer_core`]）。

use tw_config::repair::{Fix, FixKind};
use tw_yaml::Step;

use crate::model::{self, ModelError};

/// 一份配置最多修几处。删一处冒一处的修不完
const MAX_FIXES: usize = 20;

/// 修好的原文和修了哪几处。读得进来（不用修）、或者修不了，都是 `None`
pub fn repair(text: &str) -> Option<(String, Vec<Fix>)> {
    let mut out = text.to_string();
    let mut fixes: Vec<Fix> = Vec::new();
    for _ in 0..=MAX_FIXES {
        let r = match model::parse(&out) {
            Ok(cfg) => {
                if fixes.is_empty() {
                    return None;
                }
                for f in fixes.iter_mut() {
                    if f.kind == FixKind::UnknownValue {
                        f.now = default_at(&cfg, &f.field);
                    }
                }
                return Some((out, fixes));
            }
            Err(ModelError::Config(r)) => r,
            Err(_) => return None,
        };
        let kind = match r.message.code.as_str() {
            "config.unknown_variant" => FixKind::UnknownValue,
            "config.unknown_field" => FixKind::UnknownField,
            _ => return None,
        };
        let field = r.message.args.get("field")?.clone();
        let path = steps(&field)?;
        // 行号和原值按**修之前的原文**找：前面删过行，`r.line` 已经对不上了
        let before = tw_yaml::find(text, &path).ok();
        let line = before
            .as_ref()
            .map(|f| text[..f.bytes.start].matches('\n').count() + 1)
            .or(r.line);
        let value = before.map(|f| masked(&path, &f.value));
        out = tw_yaml::remove_key(&out, &path).ok()?;
        if fixes.len() == MAX_FIXES {
            return None;
        }
        fixes.push(Fix {
            kind,
            field,
            line,
            value,
            now: None,
        });
    }
    None
}

/// 原值给人看之前脱敏：拼错名字的那个字段完全可能就是一把密钥。连着键名一起判断
fn masked(path: &[Step], v: &str) -> String {
    let name = match path.last() {
        Some(Step::Key(k)) => k.as_str(),
        _ => "value",
    };
    let line = tw_secret::mask_line(&mask_hex_runs(&format!("{name}: {v}")));
    match line.split_once(": ") {
        Some((_, v)) => v.to_string(),
        None => line,
    }
}

/// 正好 64 个十六进制字符的一段（控制面的钥匙那样的）换成打码
fn mask_hex_runs(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i, mut last) = (String::new(), 0, 0);
    while i < b.len() {
        if !b[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start == tw_api::control::KEY_HEX_LEN {
            out.push_str(&s[last..start]);
            out.push_str(tw_api::control::KEY_MASK);
            last = i;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// 修完之后这个字段的值（也就是默认值），写成一个标量。写不成的是 None
fn default_at(cfg: &tw_config::Config, field: &str) -> Option<String> {
    use serde_yaml_ng::to_value;
    let path = steps(field)?;
    let (first, rest) = path.split_first()?;
    let Step::Key(name) = first else { return None };
    // **一节单独序列化**：整份序列化时还是默认值的那几节整节不写，改回默认值的恰恰是它们
    let section = match name.as_str() {
        "clients" => to_value(&cfg.clients),
        "providers" => to_value(&cfg.providers),
        "proxies" => to_value(&cfg.proxies),
        "pricing" => to_value(&cfg.pricing),
        "client_probes" => to_value(&cfg.client_probes),
        "security" => to_value(&cfg.security),
        "failover" => to_value(&cfg.failover),
        "aliases" => to_value(&cfg.aliases),
        "groups" => to_value(&cfg.groups),
        "routes" => to_value(&cfg.routes),
        "plugins" => to_value(&cfg.plugins),
        _ => return None,
    }
    .ok()?;
    let mut cur = &section;
    for s in rest {
        cur = match s {
            Step::Key(k) => cur.get(k.as_str())?,
            Step::Index(i) => cur.get(*i)?,
        };
    }
    match cur {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `providers[0].protocol` → 键、下标、键。认不出（键里带点、方括号）就是 None：那一处
/// 不修 —— 删错一个键比不修糟得多
fn steps(field: &str) -> Option<Vec<Step>> {
    let mut out = Vec::new();
    for part in field.split('.') {
        let (key, rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if key.is_empty() || key.contains(']') {
            return None;
        }
        out.push(Step::key(key));
        let mut rest = rest;
        while !rest.is_empty() {
            let inner = rest.strip_prefix('[')?;
            let (n, after) = inner.split_once(']')?;
            out.push(Step::Index(n.parse().ok()?));
            rest = after;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_pack_with_an_old_field_and_an_old_value_is_repaired_in_memory() {
        let text = "version: 1\nproviders:\n  - name: a\n    base_url: https://a.example\n    balance: retired_kind\n    gone_field: sk-ant-api03-secretsecretsecret\nclient_probes:\n  titling: passthrough\n";
        let (out, fixes) = repair(text).expect("修得了");
        assert!(model::parse(&out).is_ok(), "{out}");
        let got: Vec<(FixKind, &str, Option<usize>)> = fixes
            .iter()
            .map(|f| (f.kind, f.field.as_str(), f.line))
            .collect();
        assert_eq!(
            got,
            vec![
                (FixKind::UnknownValue, "providers[0].balance", Some(5)),
                (FixKind::UnknownField, "providers[0].gone_field", Some(6)),
                (FixKind::UnknownValue, "client_probes.titling", Some(8)),
            ]
        );
        // 默认值不写出来的（余额来源缺省是 auto）说不出来
        assert_eq!(fixes[0].now, None);
        assert_eq!(fixes[2].now.as_deref(), Some("forward"));
        // 原值脱敏
        let v = fixes[1].value.as_deref().unwrap();
        assert!(!v.contains("secretsecret"), "{v}");
        // 别的原样留着
        assert!(out.contains("    base_url: https://a.example\n"), "{out}");
    }

    #[test]
    fn what_reads_or_cannot_be_repaired_is_none() {
        assert!(repair("version: 1\n").is_none());
        // 语法错不修
        assert!(repair("version: 1\nproviders: [\n").is_none());
        // 缺字段（没有默认值可回）不修
        assert!(repair("version: 1\nproviders:\n  - name: a\n").is_none());
    }
}

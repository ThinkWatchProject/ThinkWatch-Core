//! 配置读不进来时的一键修复。
//!
//! **只修两种，而且都只是删掉一个键**：取值不在可选范围里的（删掉就回到默认值），不认识的
//! 字段（删掉）。手改配置最常见的错就是这两种 —— 写了一个已经不存在的取值、拼错一个字段名
//! —— 而它们的修法不需要猜用户想写什么。语法错（要看标点）、缺字段（没有默认值可回）、语义错
//! （整份配置的事）都不碰。
//!
//! **修完整份配置读得进来才算数**：删掉一处之后冒出别的错（比如那个字段没有默认值，删了就
//! 成了缺字段），整个修复就不给。半修好的配置还是读不进来，给了只会让人以为好了。
//!
//! 删是最小改动（`tw_yaml::remove_key`）：注释、排版、别的键原样留着；删空了的父节点一起删。

use crate::{Config, Rejected, try_parse};
use tw_yaml::Step;

/// 一次修复要改的那几处，和改完的原文。
#[derive(Debug, Clone, PartialEq)]
pub struct Repair {
    pub text: String,
    pub fixes: Vec<Fix>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    pub kind: FixKind,
    /// 字段的路径，`client_probes.titling`、`providers[0].protocol`
    pub field: String,
    /// 在**修之前的原文**里是第几行，1 起
    pub line: Option<usize>,
    /// 原来写着的值。不认识的字段是它整个的值（是一个标量的时候）
    pub value: Option<String>,
    /// 修完之后这个字段的值，也就是默认值。不认识的字段、或者默认值写不成一个标量时没有
    pub now: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixKind {
    /// 取值不在可选范围里：删掉这一行，回到默认值
    UnknownValue,
    /// 不认识的字段：删掉
    UnknownField,
}

/// 一份配置最多改几处。**防的是改不完的情况**（删一处冒一处），正常的手改错误一两处就完了
const MAX_FIXES: usize = 20;

/// 算出这份原文的修复。读得进来（不用修）、或者修不了，都是 `None`。
pub fn repair(text: &str) -> Option<Repair> {
    let mut out = text.to_string();
    let mut found = Vec::new();
    for _ in 0..MAX_FIXES {
        let r = match try_parse(&out) {
            Ok(cfg) => {
                if found.is_empty() {
                    return None;
                }
                return Some(Repair {
                    fixes: found.into_iter().map(|f| finish(f, &cfg)).collect(),
                    text: out,
                });
            }
            Err(r) => r,
        };
        let (kind, field) = fixable(&r)?;
        let path = steps(&field)?;
        // 行号和原值都按**修之前的原文**找：前面删过行，`r.line` 已经对不上了
        let before = tw_yaml::find(text, &path).ok();
        let line = before
            .as_ref()
            .map(|f| text[..f.bytes.start].matches('\n').count() + 1)
            .or(r.line);
        let value = before.map(|f| masked(&path, &f.value));
        out = tw_yaml::remove_key(&out, &path).ok()?;
        found.push(Fix {
            kind,
            field,
            line,
            value,
            now: None,
        });
    }
    None
}

/// 原值给人看之前脱敏，和出错那一行的原文同一套（`reload::excerpt_of`）：拼错名字的那个
/// 字段完全可能就是一把密钥。**连着键名一起判断**（`mask_line` 看键名也看值的形状），
/// 判完再把值取回来
fn masked(path: &[Step], v: &str) -> String {
    let name = match path.last() {
        Some(Step::Key(k)) => k.as_str(),
        _ => "value",
    };
    let line = tw_secret::mask_line(&crate::control_key::mask_hex_runs(&format!("{name}: {v}")));
    match line.split_once(": ") {
        Some((_, v)) => v.to_string(),
        None => line,
    }
}

/// 这一处能不能修，能修的话是哪一种、哪个字段
fn fixable(r: &Rejected) -> Option<(FixKind, String)> {
    let kind = match r.message.code.as_str() {
        "config.unknown_variant" => FixKind::UnknownValue,
        "config.unknown_field" => FixKind::UnknownField,
        _ => return None,
    };
    Some((kind, r.message.args.get("field")?.clone()))
}

/// 取值改回默认值的那几处，查出默认值是什么，给人看
fn finish(mut f: Fix, cfg: &Config) -> Fix {
    if f.kind == FixKind::UnknownValue {
        f.now = steps(&f.field).and_then(|p| {
            let (first, rest) = p.split_first()?;
            let Step::Key(name) = first else { return None };
            at(&section(cfg, name)?, rest)
        });
    }
    f
}

/// 顶层的一节，**单独序列化**。整份配置序列化时，还是默认值的那几节（`client_probes`、
/// `security`……）整节不写 —— 改回默认值的恰恰是它们，从整份里查不到
fn section(cfg: &Config, name: &str) -> Option<serde_yaml_ng::Value> {
    use serde_yaml_ng::to_value;
    match name {
        "listen" => to_value(&cfg.listen),
        "clients" => to_value(&cfg.clients),
        "providers" => to_value(&cfg.providers),
        "proxies" => to_value(&cfg.proxies),
        "pricing" => to_value(&cfg.pricing),
        "client_probes" => to_value(&cfg.client_probes),
        "security" => to_value(&cfg.security),
        "retention" => to_value(&cfg.retention),
        "failover" => to_value(&cfg.failover),
        "aliases" => to_value(&cfg.aliases),
        "groups" => to_value(&cfg.groups),
        "routes" => to_value(&cfg.routes),
        "plugins" => to_value(&cfg.plugins),
        _ => return None,
    }
    .ok()
}

fn at(v: &serde_yaml_ng::Value, path: &[Step]) -> Option<String> {
    let mut cur = v;
    for s in path {
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

/// `providers[0].protocol` → 键、下标、键。**认不出就是 `None`**（键里带点、方括号的那种），
/// 那一处就不修 —— 删错一个键比不修糟得多
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

    const GOOD: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\n";

    #[test]
    fn a_value_outside_its_choices_goes_back_to_the_default() {
        let bad =
            format!("{GOOD}client_probes:\n  health_check: intercept\n  titling: passthrough\n");
        let r = repair(&bad).expect("该修得了");
        assert_eq!(
            r.fixes,
            vec![Fix {
                kind: FixKind::UnknownValue,
                field: "client_probes.titling".into(),
                line: Some(10),
                value: Some("passthrough".into()),
                now: Some("forward".into()),
            }]
        );
        // 只删那一行，别的原样留着
        assert_eq!(
            r.text,
            format!("{GOOD}client_probes:\n  health_check: intercept\n")
        );
        assert!(try_parse(&r.text).is_ok());
    }

    #[test]
    fn an_emptied_section_goes_with_its_last_key() {
        // 剩一个空的 `client_probes:` 读回来是 null —— 对一个结构体字段是解析错误
        let bad = format!("{GOOD}client_probes:\n  titling: route\n");
        let r = repair(&bad).unwrap();
        assert_eq!(r.text, GOOD);
    }

    #[test]
    fn several_mistakes_are_fixed_together_with_lines_from_the_original() {
        let bad = format!(
            "{GOOD}client_probes:\n  titling: passthrough\n  suggestion: route\nretention:\n  body_dayz: 3\n"
        );
        let r = repair(&bad).expect("该修得了");
        let got: Vec<_> = r
            .fixes
            .iter()
            .map(|f| (f.kind, f.field.as_str(), f.line))
            .collect();
        assert_eq!(
            got,
            vec![
                (FixKind::UnknownValue, "client_probes.titling", Some(9)),
                (FixKind::UnknownValue, "client_probes.suggestion", Some(10)),
                (FixKind::UnknownField, "retention.body_dayz", Some(12)),
            ]
        );
        assert_eq!(r.fixes[2].value.as_deref(), Some("3"));
        assert_eq!(r.fixes[2].now, None);
        assert!(try_parse(&r.text).is_ok());
    }

    #[test]
    fn a_misspelled_field_in_a_list_item_is_removed() {
        let bad = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\n    colour: red\n";
        let r = repair(bad).unwrap();
        assert_eq!(r.fixes[0].field, "clients[0].colour");
        assert_eq!(r.text, GOOD);
    }

    #[test]
    fn comments_and_layout_are_kept() {
        let bad = format!(
            "# 我的配置\n{GOOD}client_probes:\n  # 标题照常转发\n  titling: passthrough # 旧写法\n  warmup: intercept\n"
        );
        let r = repair(&bad).unwrap();
        assert!(r.text.starts_with("# 我的配置\n"), "{}", r.text);
        assert!(r.text.contains("  warmup: intercept\n"), "{}", r.text);
        assert!(!r.text.contains("passthrough"), "{}", r.text);
    }

    #[test]
    fn nothing_to_fix_or_nothing_fixable_is_none() {
        assert_eq!(repair(GOOD), None);
        // 语法错：要看标点，不猜
        assert_eq!(repair("version: 1\nclients: [\n"), None);
        // 语义错（两个同名上游）：整份配置的事，删哪一处都是猜
        let dup = format!(
            "{GOOD}providers:\n  - name: a\n    base_url: https://x\n  - name: a\n    base_url: https://y\n"
        );
        assert_eq!(repair(&dup), None);
    }

    #[test]
    fn a_half_fixed_config_is_not_offered() {
        // 删掉不认识的字段之后还剩一个语义错（两个同名上游）：修完还是读不进来，就不给
        let bad = format!(
            "{GOOD}providers:\n  - name: a\n    base_url: https://x\n    colour: red\n  - name: a\n    base_url: https://y\n"
        );
        assert_eq!(repair(&bad), None);
        // 只有那一处的话修得好
        let ok =
            format!("{GOOD}providers:\n  - name: a\n    base_url: https://x\n    colour: red\n");
        let r = repair(&ok).unwrap();
        assert_eq!(r.fixes[0].field, "providers[0].colour");
        assert!(try_parse(&r.text).is_ok());
    }

    #[test]
    fn a_secret_under_a_misspelled_name_is_not_shown() {
        let bad = format!(
            "{GOOD}providers:\n  - name: a\n    base_url: https://x\n    api_kye: sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789\n"
        );
        let r = repair(&bad).unwrap();
        let shown = r.fixes[0].value.as_deref().unwrap();
        assert!(!shown.contains("abcdefghijklmnopqrstuvwxyz"), "{shown}");
    }

    #[test]
    fn paths_with_indices_are_understood_and_odd_ones_are_left_alone() {
        assert_eq!(
            steps("providers[0].protocol"),
            Some(vec![
                Step::key("providers"),
                Step::Index(0),
                Step::key("protocol")
            ])
        );
        assert_eq!(steps("a[x].b"), None);
        assert_eq!(steps(""), None);
    }
}

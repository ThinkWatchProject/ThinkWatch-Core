//! TOML 的外科手术，靠 `toml_edit`。
//!
//! 和 [`crate::json`] 同一套规矩：只动点名的那几个键，其余字节原样保留。
//! `toml_edit` 天生就是干这个的 —— 它把注释、空行、缩进都挂在文档树上，
//! 改一个值不会惊动别的地方。
//!
//! 这件事对 Codex 尤其要紧：本机那份 `~/.codex/config.toml` 有四千字节，
//! 里面是几十个 `[projects."..."]` 的信任记录和 marketplace 配置。整体
//! 覆写等于把用户对每个项目的授权全部清零。

use toml_edit::{DocumentMut, Item, Table, Value as TValue};

use crate::json::Val;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TErr {
    #[error("not valid TOML: {0}")]
    Syntax(String),
    #[error("{0} is not a table, so no field can be written into it")]
    NotTable(String),
}

fn parse(text: &str) -> Result<DocumentMut, TErr> {
    text.parse::<DocumentMut>()
        .map_err(|e| TErr::Syntax(e.to_string()))
}

fn to_toml(v: &Val) -> TValue {
    match v {
        Val::Null => TValue::from(""), // TOML 没有 null；我们从不写它
        Val::Bool(b) => TValue::from(*b),
        Val::Num(n) => n
            .parse::<i64>()
            .map(TValue::from)
            .or_else(|_| n.parse::<f64>().map(TValue::from))
            .unwrap_or_else(|_| TValue::from(n.as_str())),
        Val::Str(s) => TValue::from(s.as_str()),
        Val::Arr(es) => {
            let mut a = toml_edit::Array::new();
            for e in es {
                a.push(to_toml(e));
            }
            TValue::Array(a)
        }
        Val::Obj(ms) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in ms {
                t.insert(k, to_toml(v));
            }
            TValue::InlineTable(t)
        }
    }
}

/// 一个对象写成独立表段，嵌套的对象继续往下写成子表 —— `[a.b]`、
/// `[a.b.c]`。
fn to_table(ms: &[(String, Val)]) -> Table {
    let mut t = Table::new();
    for (k, v) in ms {
        match v {
            Val::Obj(inner) if !inner.is_empty() => {
                t.insert(k, Item::Table(to_table(inner)));
            }
            _ => {
                t.insert(k, Item::Value(to_toml(v)));
            }
        }
    }
    t
}

fn from_toml(item: &Item) -> Option<Val> {
    match item {
        Item::Value(v) => Some(match v {
            TValue::String(s) => Val::Str(s.value().clone()),
            TValue::Integer(i) => Val::Num(i.value().to_string()),
            TValue::Float(f) => Val::Num(f.value().to_string()),
            TValue::Boolean(b) => Val::Bool(*b.value()),
            TValue::Datetime(d) => Val::Str(d.value().to_string()),
            TValue::Array(a) => Val::Arr(
                a.iter()
                    .filter_map(|e| from_toml(&Item::Value(e.clone())))
                    .collect(),
            ),
            TValue::InlineTable(t) => Val::Obj(
                t.iter()
                    .filter_map(|(k, v)| {
                        from_toml(&Item::Value(v.clone())).map(|v| (k.to_string(), v))
                    })
                    .collect(),
            ),
        }),
        Item::Table(t) => Some(Val::Obj(
            t.iter()
                .filter_map(|(k, v)| from_toml(v).map(|v| (k.to_string(), v)))
                .collect(),
        )),
        Item::ArrayOfTables(a) => Some(Val::Arr(
            a.iter()
                .map(|t| {
                    Val::Obj(
                        t.iter()
                            .filter_map(|(k, v)| from_toml(v).map(|v| (k.to_string(), v)))
                            .collect(),
                    )
                })
                .collect(),
        )),
        Item::None => None,
    }
}

pub fn get(text: &str, path: &[&str]) -> Result<Option<Val>, TErr> {
    let doc = parse(text)?;
    let mut item: &Item = doc.as_item();
    for k in path {
        let Some(next) = item.get(k) else {
            return Ok(None);
        };
        item = next;
    }
    Ok(from_toml(item))
}

/// 整份文件的语义值，写回校验用。
pub fn value(text: &str) -> Result<Val, TErr> {
    Ok(from_toml(parse(text)?.as_item()).unwrap_or(Val::Obj(vec![])))
}

pub fn set(text: &str, path: &[&str], v: &Val) -> Result<String, TErr> {
    let Some((leaf, parents)) = path.split_last() else {
        return Err(TErr::NotTable(String::new()));
    };
    let mut doc = parse(text)?;
    let mut item: &mut Item = doc.as_item_mut();
    for (i, k) in parents.iter().enumerate() {
        if item.get(k).is_none() {
            let mut t = Table::new();
            // **隐式表**：让它渲染成 `[model_providers.thinkwatch]` 一行，
            // 而不是多出一个空的 `[model_providers]` 头。
            t.set_implicit(true);
            let Some(tbl) = item.as_table_like_mut() else {
                return Err(TErr::NotTable(parents[..i].join(".")));
            };
            tbl.insert(k, Item::Table(t));
        }
        let next = item
            .as_table_like_mut()
            .and_then(|t| t.get_mut(k))
            .expect("the table that was just inserted");
        if next.as_table_like().is_none() {
            return Err(TErr::NotTable(parents[..=i].join(".")));
        }
        item = next;
    }
    let Some(tbl) = item.as_table_like_mut() else {
        return Err(TErr::NotTable(parents.join(".")));
    };
    // 已经有这个键就只换值，键上挂着的注释和空行留在原处 ——
    // **也保住它原来是行内表还是独立表**：把用户写的
    // `x = { a = 1 }` 换成一个 `[x]` 段落，是一次他没要求的重排版
    match tbl.get_mut(leaf) {
        Some(slot) => {
            let keep_table = slot.is_table();
            *slot = match v {
                Val::Obj(ms) if keep_table && !ms.is_empty() => Item::Table(to_table(ms)),
                _ => Item::Value(to_toml(v)),
            };
        }
        None => {
            // **新插入的对象写成独立表段。**Codex 自己的 config.toml 里
            // 每个 MCP server 都是 `[mcp_servers.x]`，塞一个几百字符的
            // 行内表进去，在那个文件里会显得格格不入
            let item = match v {
                Val::Obj(ms) if !ms.is_empty() => Item::Table(to_table(ms)),
                _ => Item::Value(to_toml(v)),
            };
            tbl.insert(leaf, item);
        }
    }
    Ok(doc.to_string())
}

/// 删掉一个键。父表如果因此空了**不会**跟着删 —— 那可能是用户自己建的。
pub fn remove(text: &str, path: &[&str]) -> Result<String, TErr> {
    let Some((leaf, parents)) = path.split_last() else {
        return Ok(text.to_string());
    };
    let mut doc = parse(text)?;
    let mut item: &mut Item = doc.as_item_mut();
    for k in parents {
        // **不能用 `Item::get_mut`** —— 它走的是 `IndexMut`，键不存在时
        // 会当场造一张空表出来。于是「删一个本来就没有的字段」反而给
        // 用户的配置里添了一行 `a = {}`。测试是这么发现的。
        match item.as_table_like_mut().and_then(|t| t.get_mut(k)) {
            Some(next) => item = next,
            None => return Ok(text.to_string()),
        }
    }
    if let Some(tbl) = item.as_table_like_mut() {
        tbl.remove(leaf);
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"

# 我自己加的注释
[projects."/path/to/my-app"]
trust_level = "trusted"

[projects."/path/to/another-app"]
trust_level = "trusted"
"#;

    #[test]
    fn adopting_codex_keeps_every_project_trust_record() {
        // 本机那份 config.toml 有几十条 [projects."..."] 授权。整体覆写
        // 等于把用户对每个项目的信任全部清零。
        let out = set(REAL, &["model_provider"], &Val::s("thinkwatch")).unwrap();
        let out = set(
            &out,
            &["model_providers", "thinkwatch", "base_url"],
            &Val::s("http://127.0.0.1:8080/v1"),
        )
        .unwrap();
        assert!(out.contains(r#"[projects."/path/to/my-app"]"#), "{out}");
        assert!(
            out.contains(r#"[projects."/path/to/another-app"]"#),
            "{out}"
        );
        assert!(out.contains("# 我自己加的注释"), "{out}");
        assert_eq!(get(&out, &["model"]).unwrap(), Some(Val::s("gpt-5.6-sol")));
    }

    #[test]
    fn a_new_provider_renders_as_one_table_header() {
        // 隐式表：不该多出一个空的 [model_providers] 头。
        let out = set(
            "model = \"x\"\n",
            &["model_providers", "tw", "base_url"],
            &Val::s("u"),
        )
        .unwrap();
        assert!(out.contains("[model_providers.tw]"), "{out}");
        assert!(!out.contains("\n[model_providers]\n"), "{out}");
    }

    #[test]
    fn changing_an_existing_value_leaves_its_comment_alone() {
        let src = "# 别动这行\nmodel_provider = \"openai\"  # 尾注释\n";
        let out = set(src, &["model_provider"], &Val::s("thinkwatch")).unwrap();
        assert!(out.contains("# 别动这行"), "{out}");
        assert!(out.contains("thinkwatch"), "{out}");
        assert!(!out.contains("\"openai\""), "{out}");
    }

    #[test]
    fn the_codex_provider_block_matches_what_the_binary_actually_accepts() {
        // 这几个字段名是从 codex 0.139.0 的 ModelProviderInfo 里读出来的，
        // 并且用一个本地嗅探器实跑验证过 —— 不是从文档抄的。
        let mut out = String::from("model = \"gpt-5.6-sol\"\n");
        for (p, v) in [
            (vec!["model_providers", "tw", "name"], Val::s("ThinkWatch")),
            (
                vec!["model_providers", "tw", "base_url"],
                Val::s("http://127.0.0.1:8080/v1"),
            ),
            (
                vec!["model_providers", "tw", "wire_api"],
                Val::s("responses"),
            ),
        ] {
            out = set(&out, &p.to_vec(), &v).unwrap();
        }
        out = set(
            &out,
            &["model_providers", "tw", "http_headers"],
            &Val::Obj(vec![("X-ThinkWatch-Client".into(), Val::s("codex"))]),
        )
        .unwrap();
        assert!(out.contains("wire_api = \"responses\""), "{out}");
        assert!(out.contains("X-ThinkWatch-Client"), "{out}");
        // wire_api = "chat" 已经被上游移除，我们绝不能写它
        assert!(!out.contains("\"chat\""), "{out}");
    }

    #[test]
    fn a_new_nested_object_becomes_a_table_section_like_the_rest_of_the_file() {
        // Codex 自己的 config.toml 里每个 MCP server 都是
        // `[mcp_servers.x]`。塞一个几百字符的行内表进去，在那个文件里
        // 会显得格格不入。
        let out = set(
            "model = \"gpt-5\"\n",
            &["mcp_servers", "fs"],
            &Val::Obj(vec![
                ("command".into(), Val::s("npx")),
                ("args".into(), Val::Arr(vec![Val::s("-y")])),
                ("env".into(), Val::Obj(vec![("K".into(), Val::s("v"))])),
            ]),
        )
        .unwrap();
        assert!(out.contains("[mcp_servers.fs]"), "{out}");
        assert!(out.contains("[mcp_servers.fs.env]"), "{out}");
        assert!(out.contains("command = \"npx\""), "{out}");
        // 写出来的还得是合法 TOML，而且读回来一模一样
        assert_eq!(
            get(&out, &["mcp_servers", "fs", "env", "K"]).unwrap(),
            Some(Val::s("v"))
        );
    }

    #[test]
    fn an_existing_inline_table_stays_inline() {
        // 把用户写的 `x = { a = 1 }` 换成一个 `[x]` 段落，是一次他没
        // 要求的重排版。
        let out = set(
            "x = { a = 1 }\n",
            &["x"],
            &Val::Obj(vec![("a".into(), Val::Num("2".into()))]),
        )
        .unwrap();
        assert!(out.contains("x = {"), "{out}");
        assert!(!out.contains("[x]"), "{out}");
    }

    #[test]
    fn removing_puts_the_file_back() {
        let src = "model = \"x\"\n";
        let with = set(src, &["model_provider"], &Val::s("tw")).unwrap();
        assert_eq!(remove(&with, &["model_provider"]).unwrap(), src);
    }

    #[test]
    fn removing_something_absent_is_not_an_error() {
        assert_eq!(remove(REAL, &["nope"]).unwrap(), REAL);
        assert_eq!(remove(REAL, &["a", "b", "c"]).unwrap(), REAL);
    }

    #[test]
    fn writing_under_a_scalar_is_refused() {
        let src = "model = \"x\"\n";
        assert_eq!(
            set(src, &["model", "sub"], &Val::s("v")),
            Err(TErr::NotTable("model".into()))
        );
    }

    #[test]
    fn a_broken_file_is_refused_rather_than_rewritten() {
        assert!(matches!(
            set("[[[", &["a"], &Val::s("v")),
            Err(TErr::Syntax(_))
        ));
    }

    #[test]
    fn multibyte_keys_and_values_survive() {
        // TOML 的裸键只能是 ASCII，非 ASCII 必须带引号 —— 写出去的时候
        // 也得自动加上，否则我们会生成一份自己都解析不了的文件。
        let src = "\"模型\" = \"通义千问\"\n";
        let out = set(src, &["提供方"], &Val::s("思考手表")).unwrap();
        assert!(out.contains("\"提供方\""), "非 ASCII 的键没加引号：{out}");
        assert_eq!(get(&out, &["模型"]).unwrap(), Some(Val::s("通义千问")));
        assert_eq!(get(&out, &["提供方"]).unwrap(), Some(Val::s("思考手表")));
        // 值里的中文原样保留，不该被转义成别的东西
        assert!(out.contains("通义千问"), "{out}");
    }

    #[test]
    fn removing_an_absent_path_does_not_conjure_a_table() {
        // `Item::get_mut` 是 IndexMut 语义 —— 照着写会在用户的配置里
        // 凭空多出一行 `a = {}`。这个测试盯着的就是那个回归。
        let out = remove(REAL, &["a", "b", "c"]).unwrap();
        assert!(!out.contains("a = {}"), "{out}");
        assert_eq!(out, REAL);
    }

    #[test]
    fn the_semantic_value_sees_nested_tables() {
        let out = set(REAL, &["model_providers", "tw", "base_url"], &Val::s("u")).unwrap();
        let v = value(&out).unwrap();
        let expected = value(REAL)
            .unwrap()
            .with(&["model_providers", "tw", "base_url"], &Val::s("u"));
        assert_eq!(v, expected);
    }
}

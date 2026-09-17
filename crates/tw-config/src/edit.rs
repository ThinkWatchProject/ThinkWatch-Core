//! 按名字编辑配置里的一项：上游、代理、价目表。
//!
//! # 为什么要有这一层
//!
//! 以前界面自己拼一段 YAML 交给补丁接口。拼字符串的那一方不知道 YAML 的
//! 引号规则 —— 一个带 `#` 的代理密码会被读成注释，一个以 `-` 开头的值会
//! 变成列表 —— 而写坏的是用户唯一的那份配置。
//!
//! 现在调用方交过来的是**结构**（serde 的 `Mapping`），渲染由 serde 做，
//! 落盘由 `tw-yaml` 做：
//!
//! - 新建：追加到列表末尾，列表或它的父段不存在就一起补出来；
//! - 修改：**只改变了的字段** —— 没变的字段、以及它们旁边用户写的注释，
//!   一个字节都不动；
//! - 那一项是行内写法（`- { name: a, … }`）时整项换成块式，位置不变。
//!
//! 每次写完都按语义核对一遍：这一项读回来就是交过来的那个结构，其余部分
//! 和写之前完全一样。对不上就不返回。

use serde_yaml_ng::{Mapping, Value};
use tw_yaml::{Put, Step};

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("已存在名为「{name}」的{what}")]
    NameTaken { what: &'static str, name: String },
    #[error("未找到名为「{name}」的{what}")]
    NotFound { what: &'static str, name: String },
    #[error("{0}")]
    Yaml(#[from] tw_yaml::PatchError),
    #[error("配置文件无法解析：{0}")]
    Parse(String),
    /// 渲染不出一个能安全写进去的值。
    #[error("{0}")]
    Unwritable(String),
    #[error("修改后的内容与预期不一致（{0}），未写入文件")]
    SelfCheck(String),
}

/// 配置里的一段列表，以及它在错误信息里叫什么。
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub path: &'static [&'static str],
    pub what: &'static str,
}

pub const PROVIDERS: Section = Section {
    path: &["providers"],
    what: "上游",
};
pub const PROXIES: Section = Section {
    path: &["proxies"],
    what: "代理",
};
pub const PRICE_SHEETS: Section = Section {
    path: &["pricing", "sheets"],
    what: "价目表",
};

impl Section {
    fn steps(&self) -> Vec<Step> {
        self.path.iter().map(|k| Step::key(*k)).collect()
    }

    fn items<'a>(&self, doc: &'a Value) -> &'a [Value] {
        let mut cur = doc;
        for k in self.path {
            match cur.get(*k) {
                Some(v) => cur = v,
                None => return &[],
            }
        }
        cur.as_sequence().map(|s| s.as_slice()).unwrap_or(&[])
    }

    /// 叫这个名字的那一项在第几个。
    pub fn index_of(&self, doc: &Value, name: &str) -> Option<usize> {
        self.items(doc)
            .iter()
            .position(|it| it.get("name").and_then(Value::as_str) == Some(name))
    }
}

/// 解析成语义树。
pub fn parse(text: &str) -> Result<Value, EditError> {
    serde_yaml_ng::from_str(text).map_err(|e| EditError::Parse(e.to_string()))
}

/// 新建一项（`current` 为 `None`），或者把叫 `current` 的那一项改成
/// `item`。`item` 里的 `name` 可以和 `current` 不同 —— 那是改名，引用
/// 它的地方由调用方负责跟着改。
pub fn upsert(
    text: &str,
    section: Section,
    current: Option<&str>,
    item: &Mapping,
) -> Result<String, EditError> {
    let doc = parse(text)?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| EditError::Unwritable(format!("{}缺少名称", section.what)))?
        .to_string();
    let taken = section.index_of(&doc, &name);
    let steps = section.steps();

    let (out, index) = match current {
        None => {
            if taken.is_some() {
                return Err(EditError::NameTaken {
                    what: section.what,
                    name,
                });
            }
            let block = render_block(&Value::Mapping(item.clone()))?;
            let out = tw_yaml::append(text, &steps, &block)?;
            (out, section.items(&doc).len())
        }
        Some(old) => {
            let index = section
                .index_of(&doc, old)
                .ok_or_else(|| EditError::NotFound {
                    what: section.what,
                    name: old.to_string(),
                })?;
            if taken.is_some_and(|i| i != index) {
                return Err(EditError::NameTaken {
                    what: section.what,
                    name,
                });
            }
            let mut path = steps.clone();
            path.push(Step::Index(index));
            let out = if tw_yaml::is_flow_at(text, &path)? {
                // 行内写法里的键删不了、嵌套值塞不进去 —— 整项换成块式
                let block = render_block(&Value::Mapping(item.clone()))?;
                tw_yaml::replace_item(text, &steps, index, &block)?
            } else {
                let old_item = section.items(&doc)[index]
                    .as_mapping()
                    .cloned()
                    .unwrap_or_default();
                sync_fields(text, &path, &old_item, item)?
            };
            (out, index)
        }
    };

    // ── 语义核对 ─────────────────────────────────────────────────────
    let mut expected = doc;
    put_item(&mut expected, section, index, Value::Mapping(item.clone()));
    let got = parse(&out).map_err(|e| EditError::SelfCheck(e.to_string()))?;
    if got != expected {
        return Err(EditError::SelfCheck(format!("{}「{name}」", section.what)));
    }
    Ok(out)
}

/// 删掉叫 `name` 的那一项。引用它的地方由调用方先检查。
///
/// **删掉的是最后一项时，整段列表连同因此变空的父段一起删。**这几段列表
/// 不写和写成空列表是一回事，而默认值不写进文件 —— 留下一个
/// `pricing: { sheets: [] }` 只是噪音。
pub fn remove(text: &str, section: Section, name: &str) -> Result<String, EditError> {
    let doc = parse(text)?;
    let index = section
        .index_of(&doc, name)
        .ok_or_else(|| EditError::NotFound {
            what: section.what,
            name: name.to_string(),
        })?;
    let out = if section.items(&doc).len() == 1 {
        tw_yaml::remove_key(text, &section.steps())?
    } else {
        tw_yaml::remove(text, &section.steps(), index)?
    };
    let got = parse(&out).map_err(|e| EditError::SelfCheck(e.to_string()))?;
    if section.index_of(&got, name).is_some()
        || section.items(&got).len() + 1 != section.items(&doc).len()
    {
        return Err(EditError::SelfCheck(format!("{}「{name}」", section.what)));
    }
    Ok(out)
}

/// 设一个值。`None` 表示删掉这个键、退回默认值 —— **默认值不写进文件**。
pub fn set(text: &str, path: &[Step], value: Option<&Value>) -> Result<String, EditError> {
    let exists = {
        let doc = parse(text)?;
        lookup(&doc, path).is_some()
    };
    match value {
        None if !exists => Ok(text.to_string()),
        None => Ok(tw_yaml::remove_key(text, path)?),
        Some(v) => {
            let rendered = render(v)?;
            Ok(tw_yaml::put(text, path, rendered.as_put())?)
        }
    }
}

/// 一个值渲染成的文本，以及它该按单行还是按块写。
pub struct Rendered {
    text: String,
    block: bool,
}

impl Rendered {
    pub fn as_put(&self) -> Put<'_> {
        if self.block {
            Put::Block(&self.text)
        } else {
            Put::Inline(&self.text)
        }
    }
}

/// 渲染一个值。引号和转义交给 serde —— 它知道哪些字符串不加引号会被
/// 读成别的类型。
pub fn render(v: &Value) -> Result<Rendered, EditError> {
    reject_multiline(v)?;
    let text = serde_yaml_ng::to_string(v).map_err(|e| EditError::Unwritable(e.to_string()))?;
    let text = text.trim_end_matches('\n').to_string();
    let block = match v {
        Value::Mapping(m) => !m.is_empty(),
        Value::Sequence(s) => !s.is_empty(),
        _ => false,
    };
    Ok(Rendered { text, block })
}

fn render_block(v: &Value) -> Result<String, EditError> {
    Ok(render(v)?.text)
}

/// **值里不许有换行。**serde 会把它写成 `|-` 块标量，而块标量里缩进是
/// 内容的一部分 —— 这一层不去冒那个险。配置里本来也没有需要多行的字段。
fn reject_multiline(v: &Value) -> Result<(), EditError> {
    match v {
        Value::String(s) if s.contains('\n') || s.contains('\r') => {
            Err(EditError::Unwritable("值中不能包含换行".to_string()))
        }
        Value::Mapping(m) => m.iter().try_for_each(|(k, v)| {
            reject_multiline(k)?;
            reject_multiline(v)
        }),
        Value::Sequence(s) => s.iter().try_for_each(reject_multiline),
        Value::Tagged(t) => reject_multiline(&t.value),
        _ => Ok(()),
    }
}

/// 按字段把一项改成新的样子：删掉新结构里没有的键，写入新增或变了的键。
fn sync_fields(
    text: &str,
    path: &[Step],
    old: &Mapping,
    new: &Mapping,
) -> Result<String, EditError> {
    let mut out = text.to_string();
    let key_of = |k: &Value| -> Result<String, EditError> {
        k.as_str()
            .map(str::to_string)
            .ok_or_else(|| EditError::Unwritable(format!("键 {k:?} 不是字符串")))
    };
    for (k, _) in old.iter().filter(|(k, _)| !new.contains_key(*k)) {
        let mut p = path.to_vec();
        p.push(Step::Key(key_of(k)?));
        out = tw_yaml::remove_key(&out, &p)?;
    }
    for (k, v) in new.iter().filter(|(k, v)| old.get(*k) != Some(*v)) {
        let mut p = path.to_vec();
        p.push(Step::Key(key_of(k)?));
        let rendered = render(v)?;
        out = tw_yaml::put(&out, &p, rendered.as_put())?;
    }
    Ok(out)
}

fn lookup<'a>(doc: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut cur = doc;
    for st in path {
        cur = match st {
            Step::Key(k) => cur.get(k.as_str())?,
            Step::Index(i) => cur.get(*i)?,
        };
    }
    Some(cur)
}

/// 期望的语义树：把第 `index` 项换成（或追加成）`item`。
fn put_item(doc: &mut Value, section: Section, index: usize, item: Value) {
    let mut cur = doc;
    for k in section.path {
        let m = match cur {
            Value::Mapping(m) => m,
            other => {
                *other = Value::Mapping(Mapping::new());
                let Value::Mapping(m) = other else {
                    unreachable!()
                };
                m
            }
        };
        let key = Value::String((*k).to_string());
        if !matches!(
            m.get(&key),
            Some(Value::Mapping(_)) | Some(Value::Sequence(_))
        ) {
            m.insert(key.clone(), Value::Null);
        }
        cur = m.get_mut(&key).unwrap();
    }
    if !matches!(cur, Value::Sequence(_)) {
        *cur = Value::Sequence(Vec::new());
    }
    let Value::Sequence(seq) = cur else {
        unreachable!()
    };
    if index < seq.len() {
        seq[index] = item;
    } else {
        seq.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(y: &str) -> Mapping {
        serde_yaml_ng::from_str(y).unwrap()
    }

    const CFG: &str = "version: 1
clients:
  - name: default
    key: tw-a
# 两家上游
providers:
  - name: 官方
    base_url: https://api.anthropic.com  # 直连
    key: sk-a
  - { name: relay, base_url: https://relay.example, key: sk-b }
";

    #[test]
    fn a_new_item_is_appended_with_its_values_quoted_by_serde_not_by_hand() {
        // 以前界面拼字符串：这个密码里的 `#` 会被读成注释的开头
        let out = upsert(
            CFG,
            PROXIES,
            None,
            &map("name: corp\ntype: http\naddr: 10.0.0.1:8080\nauth:\n  user: svc\n  pass: 'p@ss #1: x'\n"),
        )
        .unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["proxies"][0]["auth"]["pass"], "p@ss #1: x");
        assert!(out.contains("# 两家上游"), "{out}");
    }

    #[test]
    fn changing_one_field_leaves_every_other_byte_of_the_item() {
        let out = upsert(
            CFG,
            PROVIDERS,
            Some("官方"),
            &map("name: 官方\nbase_url: https://api.anthropic.com\nkey: sk-a\nproxy: corp\n"),
        )
        .unwrap();
        assert!(
            out.contains("base_url: https://api.anthropic.com  # 直连"),
            "{out}"
        );
        assert_eq!(parse(&out).unwrap()["providers"][0]["proxy"], "corp");
    }

    #[test]
    fn a_cleared_optional_field_is_removed_not_written_as_null() {
        let with = upsert(
            CFG,
            PROVIDERS,
            Some("官方"),
            &map("name: 官方\nbase_url: https://api.anthropic.com\nkey: sk-a\nbilling: subscription\n"),
        )
        .unwrap();
        let without = upsert(
            &with,
            PROVIDERS,
            Some("官方"),
            &map("name: 官方\nbase_url: https://api.anthropic.com\nkey: sk-a\n"),
        )
        .unwrap();
        assert!(!without.contains("billing"), "{without}");
    }

    #[test]
    fn a_flow_style_item_is_rewritten_as_a_block_in_the_same_place() {
        let out = upsert(
            CFG,
            PROVIDERS,
            Some("relay"),
            &map("name: relay\nbase_url: https://relay.example\nkey: sk-b\nredact: [api_keys]\n"),
        )
        .unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["providers"][1]["redact"][0], "api_keys");
        assert_eq!(v["providers"][0]["name"], "官方");
        assert!(out.contains("  - name: relay\n"), "{out}");
    }

    #[test]
    fn renaming_onto_another_items_name_is_refused() {
        let e = upsert(
            CFG,
            PROVIDERS,
            Some("relay"),
            &map("name: 官方\nbase_url: https://relay.example\nkey: sk-b\n"),
        )
        .unwrap_err();
        assert!(matches!(e, EditError::NameTaken { .. }), "{e}");
    }

    #[test]
    fn the_first_price_sheet_creates_its_section() {
        let out = upsert(
            CFG,
            PRICE_SHEETS,
            None,
            &map("name: 中转协议价\nmultiplier: 0.8\n"),
        )
        .unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["pricing"]["sheets"][0]["multiplier"], 0.8);
    }

    #[test]
    fn setting_a_value_back_to_its_default_removes_it() {
        let off = set(
            CFG,
            &[Step::key("pricing"), Step::key("auto_update")],
            Some(&Value::Bool(false)),
        )
        .unwrap();
        assert_eq!(parse(&off).unwrap()["pricing"]["auto_update"], false);
        let back = set(
            &off,
            &[Step::key("pricing"), Step::key("auto_update")],
            None,
        )
        .unwrap();
        // 空的 `pricing:` 会被读成 null —— 整段都得没了
        assert!(!back.contains("pricing"), "{back}");
        assert_eq!(back, CFG);
    }

    #[test]
    fn a_value_with_a_newline_is_refused() {
        let e = upsert(
            CFG,
            PROXIES,
            None,
            &map("name: x\ntype: http\naddr: \"a\\nb\"\n"),
        )
        .unwrap_err();
        assert!(matches!(e, EditError::Unwritable(_)), "{e}");
    }

    #[test]
    fn removing_an_item_by_name() {
        let out = remove(CFG, PROVIDERS, "官方").unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["providers"].as_sequence().unwrap().len(), 1);
        assert_eq!(v["providers"][0]["name"], "relay");
    }

    #[test]
    fn removing_the_last_price_sheet_leaves_no_empty_section_behind() {
        let one = upsert(
            CFG,
            PRICE_SHEETS,
            None,
            &map("name: 中转协议价
"),
        )
        .unwrap();
        let out = remove(&one, PRICE_SHEETS, "中转协议价").unwrap();
        assert_eq!(out, CFG);
        // 还有别的设置时，只删 `sheets`
        let off = set(
            &one,
            &[Step::key("pricing"), Step::key("auto_update")],
            Some(&Value::Bool(false)),
        )
        .unwrap();
        let out = remove(&off, PRICE_SHEETS, "中转协议价").unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["pricing"]["auto_update"], false);
        assert!(v["pricing"].get("sheets").is_none(), "{out}");
    }
}

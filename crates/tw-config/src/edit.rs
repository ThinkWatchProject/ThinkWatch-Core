//! 按名字编辑配置里的一项：上游、代理、价目表、路由、策略组。
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
use tw_types::{Msg, msg};
use tw_yaml::{Put, Step, double_quoted, must_escape};

/// 按名字改一项时的失败。
///
/// **英文只写一遍**：`Display` 就是 [`EditError::msg`] 的原句，界面拿码去翻。
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// `what` 是 [`Section::what`] 那个词（`upstream`、`price sheet`），
    /// 界面按它查自己的词表
    #[error("{}", self.msg())]
    NameTaken { what: &'static str, name: String },
    #[error("{}", self.msg())]
    NotFound { what: &'static str, name: String },
    #[error(transparent)]
    Yaml(#[from] tw_yaml::PatchError),
    #[error("{}", self.msg())]
    Parse(String),
    /// 值里有换行。**这一种是用户能填出来的**，所以单独一个码
    #[error("{}", self.msg())]
    Multiline,
    /// 交过来的那一项没有 `name`
    #[error("{}", self.msg())]
    Nameless { what: &'static str },
    /// 渲染不出一个能安全写进去的值。
    #[error("{}", self.msg())]
    Unwritable(String),
    #[error("{}", self.msg())]
    SelfCheck(String),
}

impl EditError {
    /// 给人看的那句话，带码。
    ///
    /// `Parse`、`Unwritable`、`SelfCheck` 的 `detail` 是解析器或序列化器的
    /// 原话 —— 这三种都是「我们自己写坏了」，正常使用碰不到，句子说清是
    /// 哪一步就够了。
    pub fn msg(&self) -> Msg {
        match self {
            EditError::NameTaken { what, name } => msg!(
                "config.edit.name_taken", what = what, name = name =>
                "there is already a {what} named `{name}`"
            ),
            EditError::NotFound { what, name } => msg!(
                "config.edit.not_found", what = what, name = name =>
                "there is no {what} named `{name}`"
            ),
            EditError::Yaml(e) => e.msg(),
            EditError::Parse(d) => msg!(
                "config.edit.parse", detail = d =>
                "the configuration file could not be parsed: {detail}"
            ),
            EditError::Multiline => msg!(
                "config.edit.multiline" =>
                "a value cannot contain a newline"
            ),
            EditError::Nameless { what } => msg!(
                "config.edit.nameless", what = what =>
                "the {what} has no name"
            ),
            EditError::Unwritable(d) => msg!(
                "config.edit.unwritable", detail = d =>
                "the value cannot be written into the configuration: {detail}"
            ),
            EditError::SelfCheck(d) => msg!(
                "config.edit.self_check", detail = d =>
                "the edited content is not what was expected ({detail}), so nothing was written"
            ),
        }
    }
}

/// 配置里的一段列表，以及它在错误信息里叫什么。
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub path: &'static [&'static str],
    pub what: &'static str,
    /// 每一项靠哪个键认：几乎都是 `name`，插件是 `id`
    pub key: &'static str,
    /// 每一项里**可以写多行文字**的那几个键：它们底下的字符串可以带换行（写出去是
    /// 带转义的双引号，见 [`render`]）。**其余的一律单行** —— 名字、地址、密钥、请求头、
    /// 模型和网段写成两行都不是原来那个东西，在这一层就拒绝（[`EditError::Multiline`]）
    pub multiline: &'static [&'static str],
}

pub const PROVIDERS: Section = Section {
    path: &["providers"],
    what: "upstream",
    key: "name",
    multiline: &[],
};
pub const PROXIES: Section = Section {
    path: &["proxies"],
    what: "proxy",
    key: "name",
    multiline: &[],
};
pub const PRICE_SHEETS: Section = Section {
    path: &["pricing", "sheets"],
    what: "price sheet",
    key: "name",
    multiline: &[],
};
pub const ROUTES: Section = Section {
    path: &["routes"],
    what: "route",
    key: "name",
    multiline: &[],
};
pub const GROUPS: Section = Section {
    path: &["groups"],
    what: "group",
    key: "name",
    multiline: &[],
};

/// 插件的设置是插件自己声明的文字，「一行一条」的写法很常见（统一用词的对照表、
/// 打码的正则）。id、文件、哈希、范围照旧单行
pub const PLUGINS: Section = Section {
    path: &["plugins"],
    what: "plugin",
    key: "id",
    multiline: &["settings"],
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
            .position(|it| it.get(self.key).and_then(Value::as_str) == Some(name))
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
        .get(section.key)
        .and_then(Value::as_str)
        .ok_or(EditError::Nameless { what: section.what })?
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
            single_lines(section, item.iter())?;
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
                single_lines(section, item.iter())?;
                let block = render_block(&Value::Mapping(item.clone()))?;
                tw_yaml::replace_item(text, &steps, index, &block)?
            } else {
                let old_item = section.items(&doc)[index]
                    .as_mapping()
                    .cloned()
                    .unwrap_or_default();
                sync_fields(text, &path, &old_item, item, section)?
            };
            (out, index)
        }
    };

    // ── 语义核对 ─────────────────────────────────────────────────────
    let mut expected = doc;
    put_item(&mut expected, section, index, Value::Mapping(item.clone()));
    let got = parse(&out).map_err(|e| EditError::SelfCheck(e.to_string()))?;
    if got != expected {
        return Err(EditError::SelfCheck(format!("{} `{name}`", section.what)));
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
        return Err(EditError::SelfCheck(format!("{} `{name}`", section.what)));
    }
    Ok(out)
}

/// 按 `keys` 的顺序重排一段列表。`keys` 得正好是这一段里的每一项各一次。
///
/// 块式列表整项搬（注释跟着各自那一项走，见 [`tw_yaml::reorder`]）；手写成行内的
/// （`[{…}, {…}]`）整段换掉 —— 行内写法里本来也放不下注释。
pub fn reorder(text: &str, section: Section, keys: &[String]) -> Result<String, EditError> {
    let doc = parse(text)?;
    let items = section.items(&doc);
    let order = keys
        .iter()
        .map(|k| {
            section
                .index_of(&doc, k)
                .ok_or_else(|| EditError::NotFound {
                    what: section.what,
                    name: k.clone(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = doc.clone();
    let reordered: Vec<Value> = order.iter().map(|&i| items[i].clone()).collect();
    let steps = section.steps();
    let out = match tw_yaml::reorder(text, &steps, &order) {
        Ok(out) => out,
        // 行内写法、或者别的块式之外的写法：整段换成重排后的样子。**不再查单行**：
        // 搬的是文件里已有的值
        Err(tw_yaml::PatchError::NotFound(_)) => {
            put_value(text, &steps, &Value::Sequence(reordered.clone()))?
        }
        Err(e) => return Err(e.into()),
    };
    // ── 语义核对 ─────────────────────────────────────────────────────
    let mut cur = &mut expected;
    for k in section.path {
        cur = cur
            .get_mut(*k)
            .ok_or_else(|| EditError::SelfCheck(format!("{} order", section.what)))?;
    }
    *cur = Value::Sequence(reordered);
    let got = parse(&out).map_err(|e| EditError::SelfCheck(e.to_string()))?;
    if got != expected {
        return Err(EditError::SelfCheck(format!("{} order", section.what)));
    }
    Ok(out)
}

/// 设一个值。`None` 表示删掉这个键、退回默认值 —— **默认值不写进文件**。
///
/// 按路径设的值**一律单行**：走这条路的都是名字、地址、开关、网段这一类。
pub fn set(text: &str, path: &[Step], value: Option<&Value>) -> Result<String, EditError> {
    let exists = {
        let doc = parse(text)?;
        lookup(&doc, path).is_some()
    };
    match value {
        None if !exists => Ok(text.to_string()),
        None => Ok(tw_yaml::remove_key(text, path)?),
        Some(v) => {
            reject_multiline(v)?;
            put_value(text, path, v)
        }
    }
}

/// 把一个值写到这个位置上，不查单行（调用方查过，或者搬的是文件里已有的值）
fn put_value(text: &str, path: &[Step], v: &Value) -> Result<String, EditError> {
    let rendered = render(v)?;
    Ok(tw_yaml::put(text, path, rendered.as_put())?)
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
///
/// **带换行、制表符或别的控制字符的字符串除外**：serde 会把多行写成 `|-` 块标量，
/// 而块标量里缩进是内容的一部分，这一层不去冒那个险。这些字符串写成**单行的双引号**，
/// 每个这样的字符都转义（[`tw_yaml::double_quoted`]，按路径改一个值也用它）—— 值里写
/// 什么都动不了文件的结构。做法是先在 serde 渲染的那一份里放一个占位的词，渲染完再换成
/// 双引号的写法：其余的写法（键、嵌套、别的标量的引号）照旧由 serde 决定。
///
/// **这里只管写得对，不管该不该写**：哪些字段只能单行由调用方查（[`Section::multiline`]、
/// [`set`]）。
pub fn render(v: &Value) -> Result<Rendered, EditError> {
    let mut quoted = Vec::new();
    let mark = free_mark(v);
    let swapped = swap_escaped(v, &mark, &mut quoted);
    let text =
        serde_yaml_ng::to_string(&swapped).map_err(|e| EditError::Unwritable(e.to_string()))?;
    let mut text = text.trim_end_matches('\n').to_string();
    for (i, q) in quoted.iter().enumerate() {
        let token = format!("{mark}{i}z");
        // 占位的词得原样、只出现一次：被加了引号、或者撞上了别的字，就不是这个值了
        if text.matches(token.as_str()).count() != 1 {
            return Err(EditError::Unwritable(format!(
                "the placeholder {token} did not come out of the renderer as written"
            )));
        }
        text = text.replacen(token.as_str(), q, 1);
    }
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

/// 占位词的前缀：`twq<n>x`，挑一个**哪个字符串里都没有**的 `n`（连同转义之后的写法）。
/// 占位词是前缀加序号再加 `z` —— 结尾的 `z` 让第 1 个不会是第 10 个的开头
fn free_mark(v: &Value) -> String {
    let mut all = Vec::new();
    strings(v, &mut all);
    let taken = |mark: &str| {
        all.iter().any(|s| {
            s.contains(mark) || (s.chars().any(must_escape) && double_quoted(s).contains(mark))
        })
    };
    (0u64..)
        .map(|n| format!("twq{n}x"))
        .find(|m| !taken(m))
        .unwrap_or_default()
}

fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Mapping(m) => {
            for (k, v) in m {
                strings(k, out);
                strings(v, out);
            }
        }
        Value::Sequence(s) => s.iter().for_each(|v| strings(v, out)),
        Value::Tagged(t) => strings(&t.value, out),
        _ => {}
    }
}

/// 把要转义的字符串换成占位词（键和值都算），转义后的写法按序号收进 `quoted`
fn swap_escaped(v: &Value, mark: &str, quoted: &mut Vec<String>) -> Value {
    match v {
        Value::String(s) if s.chars().any(must_escape) => {
            let token = format!("{mark}{}z", quoted.len());
            quoted.push(double_quoted(s));
            Value::String(token)
        }
        Value::Mapping(m) => Value::Mapping(
            m.iter()
                .map(|(k, v)| (swap_escaped(k, mark, quoted), swap_escaped(v, mark, quoted)))
                .collect(),
        ),
        Value::Sequence(s) => {
            Value::Sequence(s.iter().map(|v| swap_escaped(v, mark, quoted)).collect())
        }
        Value::Tagged(t) => Value::Tagged(Box::new(serde_yaml_ng::value::TaggedValue {
            tag: t.tag.clone(),
            value: swap_escaped(&t.value, mark, quoted),
        })),
        other => other.clone(),
    }
}

/// 有字段可以写多行的那几段（[`Section::multiline`] 不空的）。按路径写值时靠它认位置
const MULTILINE_SECTIONS: &[Section] = &[PLUGINS];

/// 按路径写一个字符串（`PATCH /config`、`twcore config set`）之前：**换行只进得了可以多行
/// 的字段**。和按名字改一项是同一份规矩（各段的 [`Section::multiline`]，现在只有插件的
/// 设置），别处带换行就拒绝（[`EditError::Multiline`]）。别的控制字符、LS、PS 不拦：写出去
/// 是转义过的双引号（[`tw_yaml::double_quoted`]），读回来一字不差。
///
/// `path` 是解析好的路径（列表里的一项是下标）。
pub fn check_line_breaks(path: &[Step], value: &str) -> Result<(), EditError> {
    if !value.contains(['\n', '\r']) || multiline_at(path) {
        return Ok(());
    }
    Err(EditError::Multiline)
}

/// 这个位置在哪一段的哪一项底下、那个键可以多行（`plugins[i].settings…`）
fn multiline_at(path: &[Step]) -> bool {
    MULTILINE_SECTIONS.iter().any(|s| {
        let n = s.path.len();
        path.len() > n + 1
            && s.path
                .iter()
                .zip(path)
                .all(|(k, st)| matches!(st, Step::Key(x) if x == k))
            && matches!(path[n], Step::Index(_))
            && matches!(&path[n + 1], Step::Key(k) if s.multiline.contains(&k.as_str()))
    })
}

/// **单行的字段里不许有换行**：名字、地址、密钥写成两行就不是原来那个东西了。
/// 哪些字段可以多行由那一段自己说（[`Section::multiline`]）
fn reject_multiline(v: &Value) -> Result<(), EditError> {
    match v {
        Value::String(s) if s.contains('\n') || s.contains('\r') => Err(EditError::Multiline),
        Value::Mapping(m) => m.iter().try_for_each(|(k, v)| {
            reject_multiline(k)?;
            reject_multiline(v)
        }),
        Value::Sequence(s) => s.iter().try_for_each(reject_multiline),
        Value::Tagged(t) => reject_multiline(&t.value),
        _ => Ok(()),
    }
}

/// 一项里要写的这些字段，除了这一段允许多行的，都得是单行
fn single_lines<'a>(
    section: Section,
    fields: impl Iterator<Item = (&'a Value, &'a Value)>,
) -> Result<(), EditError> {
    for (k, v) in fields {
        reject_multiline(k)?;
        let free = k.as_str().is_some_and(|k| section.multiline.contains(&k));
        if !free {
            reject_multiline(v)?;
        }
    }
    Ok(())
}

/// 按字段把一项改成新的样子：删掉新结构里没有的键，写入新增或变了的键。**只查要写的
/// 那几个字段**：没变的字段原样留着，不管它是怎么写进文件的
fn sync_fields(
    text: &str,
    path: &[Step],
    old: &Mapping,
    new: &Mapping,
    section: Section,
) -> Result<String, EditError> {
    let mut out = text.to_string();
    let key_of = |k: &Value| -> Result<String, EditError> {
        k.as_str()
            .map(str::to_string)
            .ok_or_else(|| EditError::Unwritable(format!("the key {k:?} is not a string")))
    };
    let changed: Vec<(&Value, &Value)> = new
        .iter()
        .filter(|(k, v)| old.get(*k) != Some(*v))
        .collect();
    single_lines(section, changed.iter().copied())?;
    for (k, _) in old.iter().filter(|(k, _)| !new.contains_key(*k)) {
        let mut p = path.to_vec();
        p.push(Step::Key(key_of(k)?));
        out = tw_yaml::remove_key(&out, &p)?;
    }
    for (k, v) in changed {
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
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
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
            &map("name: 官方\nbase_url: https://api.anthropic.com\nkey: sk-a\nbilling: free\n"),
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

    /// 单行的字段里有换行：拒绝。新加的、改的、按路径设的都一样
    #[test]
    fn a_newline_in_a_single_line_field_is_refused() {
        let e = upsert(
            CFG,
            PROXIES,
            None,
            &map("name: x\ntype: http\naddr: \"a\\nb\"\n"),
        )
        .unwrap_err();
        assert!(matches!(e, EditError::Multiline), "{e}");
        let e = upsert(
            CFG,
            PROVIDERS,
            Some("官方"),
            &map("name: 官方\nbase_url: https://api.anthropic.com\nkey: \"sk-a\\r\"\n"),
        )
        .unwrap_err();
        assert!(matches!(e, EditError::Multiline), "{e}");
        let e = set(
            CFG,
            &[Step::key("default_route")],
            Some(&Value::String("a\nb".into())),
        )
        .unwrap_err();
        assert!(matches!(e, EditError::Multiline), "{e}");
    }

    const PLUGIN: &str = "  - id: p\n    file: plugins/p.js\n    sha256: 6f1c000000000000000000000000000000000000000000000000000000000abc\n";

    fn plugin_item(settings: &str) -> Mapping {
        map(&format!(
            "id: p\nfile: plugins/p.js\nsha256: 6f1c000000000000000000000000000000000000000000000000000000000abc\nsettings:\n{settings}"
        ))
    }

    /// 插件的设置可以多行：写成一行双引号，换行转义，读回来一字不差 —— 新加的和改的都是
    #[test]
    fn a_plugin_setting_may_span_lines_and_is_written_on_one_line() {
        let terms = "登陆=登录\n帐号=账号\n";
        let out = upsert(
            CFG,
            PLUGINS,
            None,
            &plugin_item("  terms: \"登陆=登录\\n帐号=账号\\n\"\n"),
        )
        .unwrap();
        assert!(
            out.contains("\n      terms: \"登陆=登录\\n帐号=账号\\n\"\n"),
            "{out}"
        );
        assert_eq!(
            parse(&out).unwrap()["plugins"][0]["settings"]["terms"],
            terms
        );
        assert!(out.contains("# 两家上游"), "{out}");

        let patterns = "\\bsk-[a-z]+\\b\n\"quoted\"\t#1: x\r\n---\n...";
        let mut item = plugin_item("  terms: x\n");
        item["settings"]["terms"] = Value::String(patterns.into());
        let again = upsert(&out, PLUGINS, Some("p"), &item).unwrap();
        assert_eq!(
            parse(&again).unwrap()["plugins"][0]["settings"]["terms"],
            patterns
        );
        // 只有那一行变了
        let changed: Vec<_> = again
            .lines()
            .filter(|l| !out.lines().any(|o| o == *l))
            .collect();
        assert_eq!(changed.len(), 1, "{again}");
        assert!(changed[0].starts_with("      terms: \""), "{again}");
    }

    /// 按路径写（`PATCH /config`）：换行只进得了插件的设置，和按名字改一项同一份规矩；
    /// 别的控制字符、LS、PS 不拦
    #[test]
    fn a_line_break_written_by_path_goes_only_into_plugin_settings() {
        use tw_yaml::path;
        for s in ["a\nb", "a\rb", "\r\n"] {
            for p in [
                &path!["clients", 0, "name"][..],
                &path!["providers", 0, "key"],
                &path!["plugins", 0, "id"],
                &path!["plugins", 0, "scope", "models", 0],
                &path!["listen", "gateway", "bind"],
            ] {
                let e = check_line_breaks(p, s).unwrap_err();
                assert!(matches!(e, EditError::Multiline), "{p:?} {s:?}");
                assert_eq!(e.msg().code, "config.edit.multiline");
            }
            check_line_breaks(&path!["plugins", 1, "settings", "terms"], s).unwrap();
        }
        for s in [
            "a\u{2028}b",
            "a\u{2029}b",
            "a\u{85}b",
            "a\tb",
            "a\u{0}b",
            "plain",
        ] {
            check_line_breaks(&path!["clients", 0, "name"], s).unwrap();
        }
    }

    /// 有字段能多行的段都在 [`MULTILINE_SECTIONS`] 里：按路径写的时候认得出它们
    #[test]
    fn every_section_with_multiline_fields_is_known_to_path_writes() {
        for s in [PROVIDERS, PROXIES, PRICE_SHEETS, ROUTES, GROUPS, PLUGINS] {
            if !s.multiline.is_empty() {
                assert!(
                    MULTILINE_SECTIONS.iter().any(|m| m.path == s.path),
                    "{}",
                    s.what
                );
            }
        }
    }

    /// 插件那一项里只有设置能多行：范围里的模式、id 照旧单行
    #[test]
    fn only_the_settings_of_a_plugin_may_span_lines() {
        let mut item = plugin_item("  note: ok\n");
        item.insert("scope".into(), map("models: [\"a\\nb\"]\n").into());
        let e = upsert(CFG, PLUGINS, None, &item).unwrap_err();
        assert!(matches!(e, EditError::Multiline), "{e}");
    }

    /// 控制字符、制表符、YAML 1.1 当换行的那几个字符：单行字段里也能写，转义成双引号，
    /// 读回来一字不差
    #[test]
    fn control_characters_and_line_separators_are_escaped_everywhere() {
        for s in [
            "a\tb",
            "a\u{0}b",
            "a\u{7}b\u{1b}",
            "a\u{7f}b",
            "a\u{85}b",
            "a\u{9f}b",
            "a\u{2028}b",
            "a\u{2029}b",
            "\u{feff}a",
            "a\u{fffe}\u{ffff}",
            "\t",
        ] {
            let mut item = map("name: 官方\nbase_url: https://api.anthropic.com\nkey: sk-a\n");
            item["key"] = Value::String(s.into());
            let out = upsert(CFG, PROVIDERS, Some("官方"), &item)
                .unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(parse(&out).unwrap()["providers"][0]["key"], s, "{out}");
            assert!(out.contains("\n    key: \""), "{s:?}: {out}");
            assert!(
                !out.chars().any(|c| c != '\n' && must_escape(c)),
                "{s:?} was written raw: {out:?}"
            );
        }
    }

    /// 文件里本来就有一个多行的值（手写的块标量），这次没改它：只改的那个字段要查
    #[test]
    fn an_untouched_multiline_value_written_by_hand_does_not_block_an_edit() {
        let text = CFG.replace(
            "    key: sk-a\n",
            "    key: sk-a\n    notes: |\n      第一行\n      第二行\n",
        );
        let mut item = parse(&text).unwrap()["providers"][0]
            .as_mapping()
            .cloned()
            .unwrap();
        item.insert("proxy".into(), "corp".into());
        let out = upsert(&text, PROVIDERS, Some("官方"), &item).unwrap();
        assert!(
            out.contains("    notes: |\n      第一行\n      第二行\n"),
            "{out}"
        );
        assert_eq!(parse(&out).unwrap()["providers"][0]["proxy"], "corp");
    }

    /// 行内写法的插件列表重排：整段重写，多行的设置照样搬过去
    #[test]
    fn reordering_a_flow_list_carries_multiline_settings_along() {
        let text = format!(
            "{CFG}plugins: [{{id: a, file: plugins/a.js, sha256: x, settings: {{t: \"1\\n2\"}}}}, {{id: b, file: plugins/b.js, sha256: y}}]\n"
        );
        let out = reorder(&text, PLUGINS, &["b".into(), "a".into()]).unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v["plugins"][0]["id"], "b");
        assert_eq!(v["plugins"][1]["settings"]["t"], "1\n2");
    }

    /// 占位词撞上了值里本来就有的字：换一个
    #[test]
    fn the_placeholder_never_matches_text_that_is_already_there() {
        let v: Value =
            serde_yaml_ng::from_str("a: \"twq0x0z\\n\"\nb: twq0x0z\nc: twq1x\nd: \"x\\ty\"\n")
                .unwrap();
        let r = render(&v).unwrap();
        let back: Value = serde_yaml_ng::from_str(&r.text).unwrap();
        assert_eq!(back, v, "{}", r.text);
        assert!(r.block);
    }

    #[test]
    fn a_plugin_entry_appended_to_a_config_without_plugins_starts_the_section() {
        let out = upsert(CFG, PLUGINS, None, &plugin_item("  t: \"a\\nb\"\n")).unwrap();
        assert!(
            out.ends_with(&format!(
                "plugins:\n{PLUGIN}    settings:\n      t: \"a\\nb\"\n"
            )),
            "{out}"
        );
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

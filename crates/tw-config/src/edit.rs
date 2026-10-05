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
}

pub const PROVIDERS: Section = Section {
    path: &["providers"],
    what: "upstream",
    key: "name",
};
pub const PROXIES: Section = Section {
    path: &["proxies"],
    what: "proxy",
    key: "name",
};
pub const PRICE_SHEETS: Section = Section {
    path: &["pricing", "sheets"],
    what: "price sheet",
    key: "name",
};
pub const ROUTES: Section = Section {
    path: &["routes"],
    what: "route",
    key: "name",
};
pub const GROUPS: Section = Section {
    path: &["groups"],
    what: "group",
    key: "name",
};

pub const PLUGINS: Section = Section {
    path: &["plugins"],
    what: "plugin",
    key: "id",
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
            single_lines(item.iter())?;
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
                single_lines(item.iter())?;
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
/// **这里只管写得对，不管该不该写**：字段只能单行由调用方查（[`upsert`]、[`set`]、
/// [`check_line_breaks`]）。
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

/// 按路径写一个字符串（`PATCH /config`、`twcore config set`）之前：**配置里的字段一律
/// 单行**，带换行（`\n`、`\r`）就拒绝（[`EditError::Multiline`]）—— 和按名字改一项是同一份
/// 规矩。别的控制字符、LS、PS 不拦：写出去是转义过的双引号（[`tw_yaml::double_quoted`]），
/// 读回来一字不差。
pub fn check_line_breaks(value: &str) -> Result<(), EditError> {
    if value.contains(['\n', '\r']) {
        return Err(EditError::Multiline);
    }
    Ok(())
}

/// **单行的字段里不许有换行**：名字、地址、密钥写成两行就不是原来那个东西了
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

/// 一项里要写的这些字段都得是单行
fn single_lines<'a>(fields: impl Iterator<Item = (&'a Value, &'a Value)>) -> Result<(), EditError> {
    for (k, v) in fields {
        reject_multiline(k)?;
        reject_multiline(v)?;
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
    single_lines(changed.iter().copied())?;
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

// ─────────────────────────────────────────────────────────── 别名表

/// 别名表在错误信息里叫什么。
pub const ALIAS: &str = "alias";

/// 别名表的键。**它是一张映射**（名字 → 模型），不是按 `name` 认的列表，所以不走
/// [`Section`]：新加的接在表的末尾，改名在原位换键（[`tw_yaml::rename_key`]），表的
/// 顺序是用户定的。
const ALIASES: &str = "aliases";

/// 新建一个别名（`current` 为 `None`），或者把叫 `current` 的那一个改成 `alias`
/// —— 名字可以不同，那是改名，**位置不变**；引用它的地方由调用方跟着改
/// （[`crate::refs::rename_alias`]）。
///
/// 一个模型写成字符串，几个写成列表。模型没变就不碰它的值（原来写成 `[a]` 的照旧）。
/// 表是块式的时候只动这一项；表写成了行内（`aliases: {a: b}`）、或者空着，整张表换成
/// 块式。
pub fn upsert_alias(
    text: &str,
    current: Option<&str>,
    alias: &crate::Alias,
) -> Result<String, EditError> {
    reject_multiline(&Value::String(alias.name.clone()))?;
    alias
        .models
        .iter()
        .try_for_each(|m| reject_multiline(&Value::String(m.clone())))?;
    let doc = parse(text)?;
    let mut table = alias_table(&doc);
    let taken = table.iter().position(|(k, _)| *k == alias.name);
    let value = alias_value(&alias.models);
    let (index, changed) = match current {
        None => {
            if taken.is_some() {
                return Err(EditError::NameTaken {
                    what: ALIAS,
                    name: alias.name.clone(),
                });
            }
            table.push((alias.name.clone(), value.clone()));
            (table.len() - 1, true)
        }
        Some(old) => {
            let index =
                table
                    .iter()
                    .position(|(k, _)| k == old)
                    .ok_or_else(|| EditError::NotFound {
                        what: ALIAS,
                        name: old.to_string(),
                    })?;
            if taken.is_some_and(|i| i != index) {
                return Err(EditError::NameTaken {
                    what: ALIAS,
                    name: alias.name.clone(),
                });
            }
            let changed = models_of(&table[index].1).as_deref() != Some(alias.models.as_slice());
            table[index].0 = alias.name.clone();
            if changed {
                table[index].1 = value.clone();
            }
            (index, changed)
        }
    };
    let out = if block_table(text, &doc)? {
        let mut out = text.to_string();
        if let Some(old) = current
            && old != alias.name
        {
            out = tw_yaml::rename_key(&out, &[Step::key(ALIASES), Step::key(old)], &alias.name)?;
        }
        if changed {
            out = put_value(
                &out,
                &[Step::key(ALIASES), Step::key(&table[index].0)],
                &value,
            )?;
        }
        out
    } else if doc.get(ALIASES).is_none() {
        // 第一个别名：表和这一项一起补出来，缩进和按项加的一样
        put_value(text, &[Step::key(ALIASES), Step::key(&alias.name)], &value)?
    } else {
        put_value(text, &[Step::key(ALIASES)], &table_value(&table))?
    };
    check_table(&doc, &out, &table, &alias.name)?;
    Ok(out)
}

/// 删掉叫 `name` 的别名。**删掉的是最后一个时，整张表一起删** —— 不写和空表是一回事，
/// 默认值不写进文件。引用它的地方由调用方决定要不要管。
pub fn remove_alias(text: &str, name: &str) -> Result<String, EditError> {
    let doc = parse(text)?;
    let mut table = alias_table(&doc);
    let index = table
        .iter()
        .position(|(k, _)| k == name)
        .ok_or_else(|| EditError::NotFound {
            what: ALIAS,
            name: name.to_string(),
        })?;
    table.remove(index);
    let out = if table.is_empty() {
        tw_yaml::remove_key(text, &[Step::key(ALIASES)])?
    } else if block_table(text, &doc)? {
        tw_yaml::remove_key(text, &[Step::key(ALIASES), Step::key(name)])?
    } else {
        put_value(text, &[Step::key(ALIASES)], &table_value(&table))?
    };
    check_table(&doc, &out, &table, name)?;
    Ok(out)
}

/// 文件里的别名表，按书写顺序。键一律当字符串（配置读进来时就是这么读的）
fn alias_table(doc: &Value) -> Vec<(String, Value)> {
    let Some(Value::Mapping(m)) = doc.get(ALIASES) else {
        return Vec::new();
    };
    m.iter()
        .filter_map(|(k, v)| Some((key_string(k)?, v.clone())))
        .collect()
}

fn key_string(k: &Value) -> Option<String> {
    match k {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 一个模型写成字符串，几个写成列表 —— 和 `Aliases` 写回去的样子一样
fn alias_value(models: &[String]) -> Value {
    match models {
        [one] => Value::String(one.clone()),
        many => Value::Sequence(many.iter().cloned().map(Value::String).collect()),
    }
}

/// 文件里一个别名的值读成模型列表。读不成（写成了别的类型）就是 `None`，当作变了
fn models_of(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Sequence(xs) => xs.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        Value::Null => Some(Vec::new()),
        _ => None,
    }
}

fn table_value(table: &[(String, Value)]) -> Value {
    Value::Mapping(
        table
            .iter()
            .map(|(k, v)| (Value::String(k.clone()), v.clone()))
            .collect(),
    )
}

/// 别名表是不是一张有内容的块式映射：是的话按项改，不是（没有、空着、行内）就整张换
fn block_table(text: &str, doc: &Value) -> Result<bool, EditError> {
    let non_empty = matches!(doc.get(ALIASES), Some(Value::Mapping(m)) if !m.is_empty());
    Ok(non_empty && !tw_yaml::is_flow_at(text, &[Step::key(ALIASES)])?)
}

/// 语义核对：别名表读回来正是 `table`（**顺序也对**），其余部分和写之前一样。
fn check_table(
    before: &Value,
    out: &str,
    table: &[(String, Value)],
    name: &str,
) -> Result<(), EditError> {
    let mut expected = before.clone();
    if let Value::Mapping(m) = &mut expected {
        if table.is_empty() {
            m.remove(ALIASES);
        } else {
            m.insert(Value::String(ALIASES.into()), table_value(table));
        }
    }
    let got = parse(out).map_err(|e| EditError::SelfCheck(e.to_string()))?;
    let order = |t: &[(String, Value)]| t.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    // `Mapping` 比相等不看顺序，顺序单独比
    if got != expected || order(&alias_table(&got)) != order(table) {
        return Err(EditError::SelfCheck(format!("{ALIAS} `{name}`")));
    }
    Ok(())
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

    fn plugin_item() -> Mapping {
        map(
            "id: p\nfile: plugins/p.js\nsha256: 6f1c000000000000000000000000000000000000000000000000000000000abc\n",
        )
    }

    /// 按路径写（`PATCH /config`）：配置里的字段一律单行，换行在哪儿都拒绝；别的控制字符、
    /// LS、PS 不拦
    #[test]
    fn a_line_break_written_by_path_is_refused() {
        for s in ["a\nb", "a\rb", "\r\n"] {
            let e = check_line_breaks(s).unwrap_err();
            assert!(matches!(e, EditError::Multiline), "{s:?}");
            assert_eq!(e.msg().code, "config.edit.multiline");
        }
        for s in [
            "a\u{2028}b",
            "a\u{2029}b",
            "a\u{85}b",
            "a\tb",
            "a\u{0}b",
            "plain",
        ] {
            check_line_breaks(s).unwrap();
        }
    }

    /// 插件那一项和别的一样，一律单行
    #[test]
    fn a_plugin_entry_is_single_line_throughout() {
        let mut item = plugin_item();
        item.insert("id".into(), "a\nb".into());
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

    /// 行内写法的插件列表重排：整段重写，文件里已有的值（连同 0.58 留下的、带换行的设置）
    /// 照样搬过去，不查单行
    #[test]
    fn reordering_a_flow_list_carries_every_value_along() {
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
        let out = upsert(CFG, PLUGINS, None, &plugin_item()).unwrap();
        assert!(out.ends_with(&format!("plugins:\n{PLUGIN}")), "{out}");
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

    fn alias(name: &str, models: &[&str]) -> crate::Alias {
        crate::Alias {
            name: name.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    const ALIASES_CFG: &str = "version: 1
# 别名，顺序是我排的
aliases:
  deepseek: DeepSeek-v4  # 只有一家
  sonnet:
    - claude-sonnet-5
    # Bedrock 上叫这个
    - us.anthropic.claude-sonnet-5-v1:0
  opus: [claude-opus-5]
routes: []
";

    fn aliases_of(text: &str) -> Vec<crate::Alias> {
        crate::try_parse(&format!("{text}listen:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: default\n    key: tw-a\n"))
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
            .aliases
            .0
    }

    /// 第一个别名：表和它一起补出来；一个模型写成字符串，几个写成缩进的列表
    #[test]
    fn the_first_alias_starts_the_table() {
        let out = upsert_alias(CFG, None, &alias("x", &["a"])).unwrap();
        assert!(out.ends_with("aliases:\n  x: a\n"), "{out}");
        let out = upsert_alias(CFG, None, &alias("z", &["a", "b"])).unwrap();
        assert!(out.ends_with("aliases:\n  z:\n    - a\n    - b\n"), "{out}");
        assert!(out.contains("# 两家上游"), "{out}");
    }

    /// 新的接在末尾；改一个只改它的值，别的项和注释原样；模型没变就不碰值
    #[test]
    fn an_alias_is_appended_or_changed_in_place() {
        let out = upsert_alias(ALIASES_CFG, None, &alias("haiku", &["claude-haiku-5"])).unwrap();
        assert!(
            out.contains("  opus: [claude-opus-5]\n  haiku: claude-haiku-5\n"),
            "{out}"
        );
        let names: Vec<_> = aliases_of(&out).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["deepseek", "sonnet", "opus", "haiku"]);

        let out = upsert_alias(
            ALIASES_CFG,
            Some("deepseek"),
            &alias("deepseek", &["DeepSeek-v4", "deepseek-v4"]),
        )
        .unwrap();
        assert!(out.contains("# Bedrock 上叫这个"), "{out}");
        assert_eq!(
            aliases_of(&out)[0],
            alias("deepseek", &["DeepSeek-v4", "deepseek-v4"])
        );
        // 没变：一个字节都不动
        let same = upsert_alias(
            ALIASES_CFG,
            Some("opus"),
            &alias("opus", &["claude-opus-5"]),
        )
        .unwrap();
        assert_eq!(same, ALIASES_CFG);
    }

    /// 改名在原位：位置、值、注释都不动
    #[test]
    fn renaming_an_alias_keeps_its_place() {
        let out = upsert_alias(
            ALIASES_CFG,
            Some("sonnet"),
            &alias(
                "claude-sonnet-5",
                &["claude-sonnet-5", "us.anthropic.claude-sonnet-5-v1:0"],
            ),
        )
        .unwrap();
        assert_eq!(
            out,
            ALIASES_CFG.replace("  sonnet:\n", "  claude-sonnet-5:\n")
        );
        // 改名同时改模型
        let out = upsert_alias(ALIASES_CFG, Some("deepseek"), &alias("ds", &["a", "b"])).unwrap();
        let all = aliases_of(&out);
        assert_eq!(all[0], alias("ds", &["a", "b"]));
        assert_eq!(all[1].name, "sonnet");
        assert!(out.contains("# 别名，顺序是我排的"), "{out}");
    }

    #[test]
    fn an_alias_name_that_is_taken_or_missing_is_refused() {
        let e = upsert_alias(ALIASES_CFG, None, &alias("opus", &["x"])).unwrap_err();
        assert!(
            matches!(e, EditError::NameTaken { what: "alias", .. }),
            "{e}"
        );
        let e = upsert_alias(ALIASES_CFG, Some("deepseek"), &alias("opus", &["x"])).unwrap_err();
        assert!(matches!(e, EditError::NameTaken { .. }), "{e}");
        let e = upsert_alias(ALIASES_CFG, Some("nope"), &alias("nope", &["x"])).unwrap_err();
        assert!(
            matches!(e, EditError::NotFound { what: "alias", .. }),
            "{e}"
        );
        let e = remove_alias(ALIASES_CFG, "nope").unwrap_err();
        assert!(matches!(e, EditError::NotFound { .. }), "{e}");
        let e = upsert_alias(ALIASES_CFG, None, &alias("a\nb", &["x"])).unwrap_err();
        assert!(matches!(e, EditError::Multiline), "{e}");
    }

    /// 删到最后一个，整张表一起没了
    #[test]
    fn removing_aliases_down_to_none_removes_the_table() {
        let out = remove_alias(ALIASES_CFG, "sonnet").unwrap();
        assert!(
            out.contains("  deepseek: DeepSeek-v4  # 只有一家\n  opus:"),
            "{out}"
        );
        let out = remove_alias(&out, "deepseek").unwrap();
        let out = remove_alias(&out, "opus").unwrap();
        assert!(!out.contains("aliases"), "{out}");
        assert!(out.ends_with("routes: []\n"), "{out}");
    }

    /// 写成行内的表、空着的表：整张换成块式，顺序照旧
    #[test]
    fn an_inline_or_empty_table_is_rewritten_as_a_block() {
        let inline = "version: 1\naliases: {b: x, a: [y, z]}\n";
        let out = upsert_alias(inline, Some("b"), &alias("c", &["x"])).unwrap();
        let names: Vec<_> = aliases_of(&out).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["c", "a"], "{out}");
        let out = remove_alias(inline, "b").unwrap();
        assert_eq!(aliases_of(&out), [alias("a", &["y", "z"])], "{out}");
        let out = remove_alias(&out, "a").unwrap();
        assert_eq!(out, "version: 1\n");
        let empty = "version: 1\naliases: {}\n";
        let out = upsert_alias(empty, None, &alias("x", &["y"])).unwrap();
        assert_eq!(aliases_of(&out), [alias("x", &["y"])], "{out}");
    }

    /// 要加引号的名字加上引号，读回来还是它
    #[test]
    fn an_alias_name_that_needs_quotes_reads_back_as_written() {
        for name in ["yes", "1.5", "a: b", "x #y"] {
            let out = upsert_alias(ALIASES_CFG, None, &alias(name, &["m"])).unwrap();
            assert_eq!(aliases_of(&out)[3], alias(name, &["m"]), "{out}");
            let out = upsert_alias(ALIASES_CFG, Some("opus"), &alias(name, &["m"])).unwrap();
            assert_eq!(aliases_of(&out)[2], alias(name, &["m"]), "{out}");
        }
    }
}

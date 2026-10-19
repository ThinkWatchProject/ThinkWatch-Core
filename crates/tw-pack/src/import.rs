//! 导入：把配置包里的项目逐项加进本机的配置。
//!
//! # 规则
//!
//! - 本机没有的直接导入；同名且内容不同的是冲突，由人选覆盖、改名导入或跳过。全局设置
//!   的每一节、通用偏好是一项：本机还是默认值（没有这一项）时直接采用，否则不同就是冲突，
//!   只能覆盖或跳过
//! - 本机有、包里没有的不动。整份替换不是导入，是恢复
//! - 导入进来的插件停用着；包里不带密钥的上游停用着（等填上密钥），网关密钥重新生成；
//!   ChatGPT 账号上游停用着（等登录）—— 本机同名的那一家已经登录着、选了覆盖的除外，登录
//!   留着本机的
//! - 改名导入：包里引用它的路由、别名、分组跟着改用新名字（[`crate::model::rename`]）
//! - 不能导入（错误）：引用了本机和包里都没有的东西、本机没有它要的 AWS profile、插件文件
//!   和配置里批准的哈希对不上、网关密钥的值和本机另一把相同、或者导进来配置就过不了校验

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use tw_types::{Msg, msg};

use crate::model::{
    self, Item, ItemId, ItemKind, ModelError, PluginCodec, Snapshot, free_name, global, rename,
    unit,
};
use crate::{Pack, export};

/// 包里的一项和本机比。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    New,
    Same,
    Conflict,
    Error,
}

/// 导入时要知道的一件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Flag {
    /// 包里不带密钥
    MissingKey,
    /// ChatGPT 账号上游，登录不随包带
    NeedsLogin,
    /// 插件导入为停用
    PluginDisabled,
}

/// 预览里的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub item: ItemId,
    pub status: Status,
    /// 全局设置、通用偏好：冲突只能覆盖或跳过
    pub global: bool,
    pub flags: Vec<Flag>,
    /// 不同的合并单位
    pub differs: Vec<String>,
    pub suggested_name: Option<String>,
    pub error: Option<Msg>,
    /// 它引用的、包里也有的项目
    pub refs: Vec<ItemId>,
}

/// 对一项怎么处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Import,
    Overwrite,
    Rename(String),
    Skip,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub item: ItemId,
    pub action: Action,
}

/// 本机的情况：外面才知道的几件事。
pub struct Context<'a> {
    /// 这台机器的 AWS 凭证文件里有没有这个 profile
    pub has_aws_profile: &'a dyn Fn(&str) -> bool,
    /// 新的网关密钥（包里不带密钥时重新生成）
    pub new_key: &'a dyn Fn() -> String,
    /// 通用偏好要不要导入（经远程连接导入时偏好由 Lite 自己应用，core 不写）
    pub with_prefs: bool,
}

/// 导入没成。**英文只写一遍**：`Display` 就是 [`ImportError::msg`] 的原句。
#[derive(Debug, Clone, thiserror::Error)]
pub enum ImportError {
    #[error("{}", self.msg())]
    Model(ModelError),
    /// 这一项导入不了（预览里的错误那一行）
    #[error("{}", self.msg())]
    Item(ItemId, Box<Msg>),
    /// 本机已经有同名的：导入要选覆盖或改名
    #[error("{}", self.msg())]
    Exists(ItemId),
    #[error("{}", self.msg())]
    NameTaken(ItemId, String),
    #[error("{}", self.msg())]
    NotInPack(ItemId),
    /// 导进来之后配置过不了校验
    #[error("{}", self.msg())]
    Rejected(Box<tw_config::Rejected>),
}

impl From<ModelError> for ImportError {
    fn from(e: ModelError) -> Self {
        ImportError::Model(e)
    }
}

impl ImportError {
    pub fn msg(&self) -> Msg {
        match self {
            ImportError::Model(e) => e.msg(),
            ImportError::Item(_, m) => (**m).clone(),
            ImportError::Exists(id) => msg!(
                "import.exists", kind = id.kind.slug(), name = &id.name =>
                "There is already a {kind} named `{name}` here; choose to overwrite it, import it \
                 under another name, or skip it."
            ),
            ImportError::NameTaken(id, name) => msg!(
                "import.name_taken", kind = id.kind.slug(), name = name =>
                "There is already a {kind} named `{name}`."
            ),
            ImportError::NotInPack(id) => msg!(
                "import.not_in_pack", kind = id.kind.slug(), name = &id.name =>
                "The configuration pack has no {kind} named `{name}`."
            ),
            ImportError::Rejected(r) => r.msg(),
        }
    }
}

fn missing_ref(id: &ItemId, target: &ItemId) -> Msg {
    msg!(
        "import.missing_reference", kind = id.kind.slug(), name = &id.name,
        target_kind = target.kind.slug(), target = &target.name =>
        "{kind} `{name}` refers to {target_kind} `{target}`, which is neither here nor in the \
         configuration pack."
    )
}

fn skipped_ref(id: &ItemId, target: &ItemId) -> Msg {
    msg!(
        "import.skipped_reference", kind = id.kind.slug(), name = &id.name,
        target_kind = target.kind.slug(), target = &target.name =>
        "{kind} `{name}` refers to {target_kind} `{target}`, which is skipped."
    )
}

/// 全局设置和通用偏好：冲突只能覆盖或跳过
fn is_global(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Global | ItemKind::ClientPrefs)
}

/// 预览。**什么都不写**
pub fn preview(local: &Pack, incoming: &Pack, ctx: &Context) -> Result<Vec<Row>, ImportError> {
    preview_with(local, incoming, ctx, model::codec())
}

pub fn preview_with(
    local: &Pack,
    incoming: &Pack,
    ctx: &Context,
    codec: &dyn PluginCodec,
) -> Result<Vec<Row>, ImportError> {
    let l = model::snapshot_with(local, codec)?;
    let i = model::snapshot_with(incoming, codec)?;
    let both = union(&l, &i);
    let mut rows = Vec::new();
    for (id, item) in &i.items {
        if id.kind.is_order() || (id.kind == ItemKind::ClientPrefs && !ctx.with_prefs) {
            continue;
        }
        let global = is_global(id.kind);
        let mut row = Row {
            item: id.clone(),
            status: Status::New,
            global,
            flags: flags(id, item, incoming, &l),
            differs: Vec::new(),
            suggested_name: None,
            error: None,
            refs: both.items[id]
                .refs
                .iter()
                .filter(|r| i.items.contains_key(*r))
                .cloned()
                .collect(),
        };
        if let Some(mine) = l.items.get(id) {
            if mine.units == item.units {
                row.status = Status::Same;
            } else {
                row.status = Status::Conflict;
                row.differs = differing(&mine.units, &item.units);
                if !global {
                    row.suggested_name = Some(free_name(&both, id.kind, &suggest_base(id)));
                }
            }
        }
        if row.status != Status::Same {
            row.error = problem(id, item, local, incoming, &l, &i, &both, ctx, codec);
            if row.error.is_some() {
                row.status = Status::Error;
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// 改名导入时缺省名字的底：自定义安全规则是完整的项目名（`content/规则`）
fn suggest_base(id: &ItemId) -> String {
    id.name.clone()
}

fn differing(a: &BTreeMap<String, Json>, b: &BTreeMap<String, Json>) -> Vec<String> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect()
}

fn union(l: &Snapshot, i: &Snapshot) -> Snapshot {
    let mut both = l.clone();
    for (id, item) in &i.items {
        both.items.insert(id.clone(), item.clone());
    }
    both.relink();
    both
}

fn flags(id: &ItemId, item: &Item, incoming: &Pack, l: &Snapshot) -> Vec<Flag> {
    let mut out = Vec::new();
    if !incoming.manifest.secrets && export::missing_secret(id, item) {
        out.push(Flag::MissingKey);
    }
    if id.kind == ItemKind::Provider && provider_is_chatgpt(id, item) {
        // 本机同名的那一家也是 ChatGPT 账号：覆盖时登录留着本机的
        let here = l
            .items
            .get(id)
            .is_some_and(|mine| provider_is_chatgpt(id, mine));
        if !here {
            out.push(Flag::NeedsLogin);
        }
    }
    if id.kind == ItemKind::Plugin {
        out.push(Flag::PluginDisabled);
    }
    out
}

fn provider_is_chatgpt(id: &ItemId, item: &Item) -> bool {
    model::provider_of(id, item).is_ok_and(|p| model::is_chatgpt(&p))
}

/// 这一项导入不了的原因。没有是 None
#[allow(clippy::too_many_arguments)]
fn problem(
    id: &ItemId,
    item: &Item,
    local: &Pack,
    incoming: &Pack,
    l: &Snapshot,
    i: &Snapshot,
    both: &Snapshot,
    ctx: &Context,
    codec: &dyn PluginCodec,
) -> Option<Msg> {
    // 引用了本机和包里都没有的
    for r in &both.items[id].refs {
        if !both.items.contains_key(r) && !builtin(r) {
            return Some(missing_ref(id, r));
        }
    }
    match id.kind {
        ItemKind::Provider => {
            if let Some(profile) = item
                .units
                .get(unit::CONN)
                .and_then(|c| c.get("aws"))
                .and_then(|a| a.get("profile"))
                .and_then(Json::as_str)
                && !(ctx.has_aws_profile)(profile)
            {
                return Some(msg!(
                    "import.no_aws_profile", name = &id.name, profile = profile =>
                    "Upstream `{name}` uses the AWS profile `{profile}`, which this machine does \
                     not have."
                ));
            }
        }
        ItemKind::Plugin => {
            let entry = model::parse(&incoming.config_yaml)
                .ok()
                .and_then(|c| c.plugins.into_iter().find(|p| p.id == id.name));
            match (entry, incoming.plugins.get(&id.name)) {
                (_, None) => {
                    return Some(msg!(
                        "import.plugin_missing", plugin = &id.name =>
                        "The configuration pack has no script for plugin `{plugin}`."
                    ));
                }
                (Some(e), Some(f)) => {
                    let ok = model::sha256_hex(&f.script) == e.sha256
                        || f.approved
                            .as_ref()
                            .is_some_and(|a| model::sha256_hex(a) == e.sha256);
                    if !ok {
                        return Some(msg!(
                            "import.plugin_mismatch", plugin = &id.name =>
                            "The script of plugin `{plugin}` in the configuration pack is not \
                             the one that was approved."
                        ));
                    }
                }
                (None, Some(_)) => {}
            }
        }
        ItemKind::Key => {
            if let Some(v) = key_value(item)
                && !v.is_empty()
                && let Some((other, _)) = l
                    .of_kind(ItemKind::Key)
                    .find(|(o, it)| o.name != id.name && key_value(it).as_deref() == Some(&v))
            {
                return Some(msg!(
                    "import.key_value_taken", key = &id.name, other = &other.name =>
                    "Gateway key `{key}` has the same value as gateway key `{other}` here."
                ));
            }
        }
        _ => {}
    }
    // 连同它引用的、本机没有的那些一起导进来，配置过得了校验吗
    let mut target = l.clone();
    let mut todo = vec![id.clone()];
    let mut seen = BTreeSet::new();
    while let Some(next) = todo.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        if next != *id && l.items.contains_key(&next) {
            continue;
        }
        if let Some(it) = i.items.get(&next) {
            target
                .items
                .insert(next.clone(), prepared(&next, it, incoming, l, ctx));
            todo.extend(both.items[&next].refs.iter().cloned());
        }
    }
    target.relink();
    match land(local, &target, codec) {
        Ok(_) => None,
        Err(e) => Some(e.msg()),
    }
}

/// 引擎自己有的东西：叫「默认」的那条路由（没写出来时由引擎合成）
fn builtin(r: &ItemId) -> bool {
    r.kind == ItemKind::Route && r.name == tw_engine::DEFAULT_ROUTE
}

fn key_value(item: &Item) -> Option<String> {
    item.units
        .get(unit::ALL)
        .and_then(|a| a.get("key"))
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// 包里的一项按导入的规矩调整：插件停用，缺密钥、没登录的上游停用，空的网关密钥重新生成
fn prepared(id: &ItemId, item: &Item, incoming: &Pack, l: &Snapshot, ctx: &Context) -> Item {
    let mut item = item.clone();
    match id.kind {
        ItemKind::Plugin => {
            item.units
                .insert(unit::ENABLED.to_string(), Json::Bool(false));
        }
        ItemKind::Provider => {
            let f = flags(id, &item, incoming, l);
            if (f.contains(&Flag::MissingKey) || f.contains(&Flag::NeedsLogin))
                && let Some(Json::Object(other)) = item.units.get_mut(unit::OTHER)
            {
                other.insert("disabled".into(), Json::Bool(true));
            }
        }
        ItemKind::Key => {
            if key_value(&item).as_deref() == Some("")
                && let Some(Json::Object(all)) = item.units.get_mut(unit::ALL)
            {
                all.insert("key".into(), Json::String((ctx.new_key)()));
            }
        }
        _ => {}
    }
    item
}

/// 落下去并校验。校验和换入配置时是同一套（`tw_config::try_parse`）
fn land(local: &Pack, target: &Snapshot, codec: &dyn PluginCodec) -> Result<Pack, ImportError> {
    let pack = model::materialize_with(local, target, codec)?;
    tw_config::try_parse(&pack.config_yaml).map_err(|r| ImportError::Rejected(Box::new(r)))?;
    Ok(pack)
}

/// 导入的结果。
#[derive(Debug, Clone)]
pub struct Applied {
    pub pack: Pack,
    /// 值变了的网关密钥（覆盖进来的），名字是本机原来的
    pub changed_keys: Vec<String>,
}

/// 照逐项的处理导入。没有写进 `decisions` 的项目不导入
pub fn apply(
    local: &Pack,
    incoming: &Pack,
    decisions: &[Decision],
    ctx: &Context,
) -> Result<Applied, ImportError> {
    apply_with(local, incoming, decisions, ctx, model::codec())
}

pub fn apply_with(
    local: &Pack,
    incoming: &Pack,
    decisions: &[Decision],
    ctx: &Context,
    codec: &dyn PluginCodec,
) -> Result<Applied, ImportError> {
    let l = model::snapshot_with(local, codec)?;
    let mut i = model::snapshot_with(incoming, codec)?;
    let rows = preview_with(local, incoming, ctx, codec)?;
    let errors: BTreeMap<&ItemId, &Msg> = rows
        .iter()
        .filter_map(|r| r.error.as_ref().map(|e| (&r.item, e)))
        .collect();

    // 先改名（包里引用它的跟着改），再逐项落
    let mut names: BTreeMap<ItemId, ItemId> = BTreeMap::new();
    for d in decisions {
        if let Action::Rename(new) = &d.action {
            if !d.item.kind.is_named() {
                return Err(ImportError::NotInPack(d.item.clone()));
            }
            let new_id = match d.item.kind {
                ItemKind::SecurityRule if !new.contains('/') => {
                    let guard = d.item.name.split('/').next().unwrap_or_default();
                    ItemId::new(ItemKind::SecurityRule, format!("{guard}/{new}"))
                }
                k => ItemId::new(k, new.clone()),
            };
            if new.trim().is_empty() || new.trim() != new {
                return Err(ImportError::NameTaken(d.item.clone(), new.clone()));
            }
            if l.items.contains_key(&new_id) || i.items.contains_key(&new_id) {
                return Err(ImportError::NameTaken(d.item.clone(), new.clone()));
            }
            rename(&mut i, &d.item, &new_id.name);
            names.insert(d.item.clone(), new_id);
        }
    }

    let mut target = l.clone();
    let mut taken: Vec<ItemId> = Vec::new();
    for d in decisions {
        if d.item.kind.is_order() || (d.item.kind == ItemKind::ClientPrefs && !ctx.with_prefs) {
            continue;
        }
        let id = names
            .get(&d.item)
            .cloned()
            .unwrap_or_else(|| d.item.clone());
        if d.action != Action::Skip
            && let Some(e) = errors.get(&d.item)
        {
            return Err(ImportError::Item(d.item.clone(), Box::new((*e).clone())));
        }
        let Some(item) = i.items.get(&id) else {
            if d.action == Action::Skip {
                continue;
            }
            return Err(ImportError::NotInPack(d.item.clone()));
        };
        match &d.action {
            Action::Skip => continue,
            Action::Import if l.items.contains_key(&id) && !is_global(id.kind) => {
                // 内容一样的「导入」什么都不做；不一样的要选
                if l.items[&id].units != item.units {
                    return Err(ImportError::Exists(id));
                }
                continue;
            }
            Action::Import | Action::Overwrite | Action::Rename(_) => {}
        }
        let item = prepared(&id, item, incoming, &l, ctx);
        target.items.insert(id.clone(), item);
        taken.push(id);
    }

    // 新加的上游、路由、插件接在后面，按包里的先后
    for kind in [ItemKind::Provider, ItemKind::Route, ItemKind::Plugin] {
        let order_id = ItemId::new(kind.order().expect("has an order"), "");
        let pack_order: Vec<String> = i
            .get(&order_id)
            .map(|o| names_of(&o.units))
            .unwrap_or_default();
        let mut list: Vec<String> = target
            .get(&order_id)
            .map(|o| names_of(&o.units))
            .unwrap_or_default();
        let new: Vec<&ItemId> = taken
            .iter()
            .filter(|id| id.kind == kind && !list.contains(&id.name))
            .collect();
        if new.is_empty() {
            continue;
        }
        let mut added: Vec<String> = new.iter().map(|id| id.name.clone()).collect();
        added.sort_by_key(|n| pack_order.iter().position(|p| p == n).unwrap_or(usize::MAX));
        list.extend(added);
        target.items.insert(
            order_id,
            Item {
                units: BTreeMap::from([(
                    unit::ALL.to_string(),
                    Json::Array(list.into_iter().map(Json::String).collect()),
                )]),
                ..Default::default()
            },
        );
    }
    target.relink();

    // 导进来的项目引用的东西都得在：被跳过的那些不在
    for id in &taken {
        for r in &target.items[id].refs {
            if !target.items.contains_key(r) && !builtin(r) {
                return Err(ImportError::Item(id.clone(), Box::new(skipped_ref(id, r))));
            }
        }
    }

    let pack = land(local, &target, codec)?;
    Ok(Applied {
        changed_keys: changed_keys(local, &pack),
        pack,
    })
}

fn names_of(units: &BTreeMap<String, Json>) -> Vec<String> {
    match units.get(unit::ALL) {
        Some(Json::Array(a)) => a
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// 值变了或者没了的网关密钥：名字是 `before` 里的
pub fn changed_keys(before: &Pack, after: &Pack) -> Vec<String> {
    let keys = |p: &Pack| -> BTreeMap<String, String> {
        model::parse(&p.config_yaml)
            .map(|c| c.clients.into_iter().map(|c| (c.name, c.key)).collect())
            .unwrap_or_default()
    };
    let (b, a) = (keys(before), keys(after));
    b.into_iter()
        .filter(|(name, v)| a.get(name) != Some(v))
        .map(|(name, _)| name)
        .collect()
}

/// 全局设置的一节在导入时叫什么（给界面列在「全局设置」下）
pub fn is_global_section(id: &ItemId) -> bool {
    id.kind == ItemKind::Global && global::ALL.contains(&id.name.as_str())
}

#[cfg(test)]
mod tests;

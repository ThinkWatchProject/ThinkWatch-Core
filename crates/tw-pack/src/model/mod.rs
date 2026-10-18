//! 一份配置拆成「项目」与「合并单位」，以及反过来把目标状态落回配置文本。
//!
//! 导入（两方比较）、同步（三方合并）、恢复（按项目挑）都只在这一层上比较：比的是
//! [`Snapshot`]，不是 YAML 原文。原文里的注释、排版、默认值写没写都不算差别。
//!
//! # 项目
//!
//! 一个项目是配置里有名字的一样东西（[`ItemId`]）：上游、出站代理、网关密钥、别名、策略组、
//! 路由、价目表、自定义安全规则、插件，加上几样没有名字的：上游、路由、插件各自的先后顺序
//! （顺序有意义：没写路由时按上游的顺序故障转移，插件按顺序运行），全局设置的每一节，
//! Lite 的通用偏好。
//!
//! # 合并单位
//!
//! 一个项目里能各自合并的部分（[`Item::units`]）。值是**规范化**之后的 JSON：按配置的类型
//! 读进来再写出去，默认值不写，同样的内容总是同样的值。
//!
//! - 上游：`conn`（地址、协议、密钥、请求头、出站代理、AWS、认证方式）、`models`、
//!   `billing`、`other`（并发、转发客户端身份、余额来源、停用，以及将来新加的字段）
//! - 插件：`script`（数据换成缺省之后的源码）、`settings`（出错时怎么办、范围、设置的值）、
//!   `enabled`
//! - 全局设置、通用偏好：逐个字段，单位名是字段的路径（`failover.pause_secs`）。**等于
//!   默认值的字段不在里面**，一节全是默认值时这一项就不在快照里
//! - 其余：一个单位 `all`
//!
//! # 本机设置
//!
//! 不进快照，[`materialize`] 时保留本机的值：`listen`、`retention`、`sync` 三节，ChatGPT
//! 上游的登录（`oauth`）和登录所得的 `ChatGPT-Account-Id` 请求头，Core 自带的默认插件的
//! 脚本（它的设置和开关照常比较）。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use tw_types::{Msg, msg};

use crate::{Pack, PluginFiles};

mod codec;
mod items;
mod json;
mod rename;
mod write;

pub use codec::{PluginCodec, WholeFile};
pub use rename::{free_name, rename};
pub use write::{materialize, materialize_with};

/// 配置里属于这台设备、不随导出和同步带走的几节
pub const LOCAL_SECTIONS: &[&str] = &["listen", "retention", "sync"];

/// 合并单位的名字。
pub mod unit {
    /// 整个项目是一个单位
    pub const ALL: &str = "all";
    /// 上游：服务类型、地址、协议、认证方式、密钥、请求头、出站代理、AWS
    pub const CONN: &str = "conn";
    /// 上游：模型列表、只用这些模型、手动规格
    pub const MODELS: &str = "models";
    /// 上游：计费方式、价目表
    pub const BILLING: &str = "billing";
    /// 上游：并发上限、转发客户端身份、余额来源、停用
    pub const OTHER: &str = "other";
    /// 插件：数据换成缺省之后的源码
    pub const SCRIPT: &str = "script";
    /// 插件：出错时怎么办、范围、设置的值
    pub const SETTINGS: &str = "settings";
    /// 插件：开着没有
    pub const ENABLED: &str = "enabled";
}

/// 全局设置各节的名字（[`ItemKind::Global`] 的 `name`）。
pub mod global {
    pub const FAILOVER: &str = "failover";
    pub const SECURITY: &str = "security";
    pub const DEFAULT_ROUTE: &str = "default_route";
    pub const DEFAULT_KEY: &str = "default_key";
    pub const CLIENT_PROBES: &str = "client_probes";
    pub const PRICING: &str = "pricing";
    /// 全部，按这个顺序
    pub const ALL: &[&str] = &[
        FAILOVER,
        SECURITY,
        DEFAULT_ROUTE,
        DEFAULT_KEY,
        CLIENT_PROBES,
        PRICING,
    ];
}

/// 自定义安全规则分在哪一项防护下：`security.<分项>.custom`
pub const GUARDS: &[&str] = &["redact", "inspect_tools", "content"];

/// 项目的种类。**先后就是排序**：列表、预览里按这个顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Provider,
    Proxy,
    /// 网关密钥（配置里的 `clients`）
    Key,
    Alias,
    Group,
    Route,
    PriceSheet,
    /// `name` 是 `<分项>/<规则名>`，如 `content/项目代号`
    SecurityRule,
    Plugin,
    /// 上游的先后。`name` 是空的
    ProviderOrder,
    /// 路由的先后。`name` 是空的
    RouteOrder,
    /// 插件的先后（运行的顺序）。`name` 是空的
    PluginOrder,
    /// `name` 是节名，见 [`global`]
    Global,
    /// Lite 的通用偏好。`name` 是空的
    ClientPrefs,
}

impl ItemKind {
    pub const ALL: &'static [ItemKind] = &[
        ItemKind::Provider,
        ItemKind::Proxy,
        ItemKind::Key,
        ItemKind::Alias,
        ItemKind::Group,
        ItemKind::Route,
        ItemKind::PriceSheet,
        ItemKind::SecurityRule,
        ItemKind::Plugin,
        ItemKind::ProviderOrder,
        ItemKind::RouteOrder,
        ItemKind::PluginOrder,
        ItemKind::Global,
        ItemKind::ClientPrefs,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            ItemKind::Provider => "provider",
            ItemKind::Proxy => "proxy",
            ItemKind::Key => "key",
            ItemKind::Alias => "alias",
            ItemKind::Group => "group",
            ItemKind::Route => "route",
            ItemKind::PriceSheet => "price_sheet",
            ItemKind::SecurityRule => "security_rule",
            ItemKind::Plugin => "plugin",
            ItemKind::ProviderOrder => "provider_order",
            ItemKind::RouteOrder => "route_order",
            ItemKind::PluginOrder => "plugin_order",
            ItemKind::Global => "global",
            ItemKind::ClientPrefs => "client_prefs",
        }
    }

    pub fn from_slug(s: &str) -> Option<ItemKind> {
        ItemKind::ALL.iter().copied().find(|k| k.slug() == s)
    }

    /// 一种先后顺序。它们不是用户挑的项目：导入时新加的项目接在后面，恢复、同步时整个比
    pub fn is_order(&self) -> bool {
        matches!(
            self,
            ItemKind::ProviderOrder | ItemKind::RouteOrder | ItemKind::PluginOrder
        )
    }

    /// 有名字、可以改名的那几种
    pub fn is_named(&self) -> bool {
        !self.is_order() && !matches!(self, ItemKind::Global | ItemKind::ClientPrefs)
    }

    /// 这种项目的先后由哪一种顺序管
    pub fn order(&self) -> Option<ItemKind> {
        match self {
            ItemKind::Provider => Some(ItemKind::ProviderOrder),
            ItemKind::Route => Some(ItemKind::RouteOrder),
            ItemKind::Plugin => Some(ItemKind::PluginOrder),
            _ => None,
        }
    }
}

/// 一个项目是谁：种类加名字。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemId {
    pub kind: ItemKind,
    pub name: String,
}

impl ItemId {
    pub fn new(kind: ItemKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.name.is_empty() {
            f.write_str(self.kind.slug())
        } else {
            write!(f, "{} `{}`", self.kind.slug(), self.name)
        }
    }
}

/// 一个项目：它的合并单位，和它引用的项目。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Item {
    /// 单位名 → 规范化的值
    pub units: BTreeMap<String, Json>,
    /// 它引用的项目（路由 → 上游、上游 → 出站代理……）。**按单位算出来的**，单位改了要
    /// [`Snapshot::relink`]。被引用的那一项不一定在快照里：引用了不存在的东西，导入时
    /// 按这个说出缺的是什么
    pub refs: BTreeSet<ItemId>,
    /// 插件的原文件，**不参与比较**：落盘时优先用它，字节原样；和单位对不上、或者没有时
    /// 按单位重新拼出源码（manifest 那一段按固定的样子写）
    #[serde(skip)]
    pub files: Option<PluginFiles>,
}

/// 比较只看单位和引用：同样的内容从两份文件读出来，原文件的字节可以不同
impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.units == other.units && self.refs == other.refs
    }
}

/// 一份配置拆开之后的样子。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub items: BTreeMap<ItemId, Item>,
}

/// 写成 JSON 时是一个列表：`ItemId` 当不了 JSON 对象的键
impl Serialize for Snapshot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Entry<'a> {
            kind: ItemKind,
            name: &'a str,
            units: &'a BTreeMap<String, Json>,
            #[serde(skip_serializing_if = "BTreeSet::is_empty")]
            refs: &'a BTreeSet<ItemId>,
        }
        s.collect_seq(self.items.iter().map(|(id, it)| Entry {
            kind: id.kind,
            name: &id.name,
            units: &it.units,
            refs: &it.refs,
        }))
    }
}

impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Entry {
            kind: ItemKind,
            name: String,
            #[serde(default)]
            units: BTreeMap<String, Json>,
            #[serde(default)]
            refs: BTreeSet<ItemId>,
        }
        let entries = Vec::<Entry>::deserialize(d)?;
        Ok(Snapshot {
            items: entries
                .into_iter()
                .map(|e| {
                    (
                        ItemId::new(e.kind, e.name),
                        Item {
                            units: e.units,
                            refs: e.refs,
                            files: None,
                        },
                    )
                })
                .collect(),
        })
    }
}

impl Snapshot {
    pub fn get(&self, id: &ItemId) -> Option<&Item> {
        self.items.get(id)
    }

    /// 某一种的全部项目，按名字
    pub fn of_kind(&self, kind: ItemKind) -> impl Iterator<Item = (&ItemId, &Item)> {
        self.items.iter().filter(move |(id, _)| id.kind == kind)
    }

    /// 这一项连同它（直接或间接）引用的、快照里有的项目。导出所选项目时用：路由带上
    /// 它的上游，上游带上它的出站代理
    pub fn closure(&self, roots: &[ItemId]) -> BTreeSet<ItemId> {
        let mut out = BTreeSet::new();
        let mut todo: Vec<ItemId> = roots.to_vec();
        while let Some(id) = todo.pop() {
            let Some(item) = self.items.get(&id) else {
                continue;
            };
            if !out.insert(id) {
                continue;
            }
            todo.extend(item.refs.iter().cloned());
        }
        out
    }

    /// 按单位重算每一项的引用。改了单位之后（合并、改名）调它
    pub fn relink(&mut self) {
        items::relink(&mut self.items);
    }

    /// 只留下这几项，**顺序项目跟着裁**：只剩留下来的那些名字
    pub fn retain(&mut self, keep: &BTreeSet<ItemId>) {
        let names = |kind: ItemKind| -> BTreeSet<String> {
            keep.iter()
                .filter(|id| id.kind == kind)
                .map(|id| id.name.clone())
                .collect()
        };
        let mut orders = BTreeMap::new();
        for kind in [ItemKind::Provider, ItemKind::Route, ItemKind::Plugin] {
            orders.insert(kind.order().unwrap_or(kind), names(kind));
        }
        self.items.retain(|id, item| {
            if let Some(allowed) = orders.get(&id.kind) {
                if let Some(Json::Array(list)) = item.units.get_mut(unit::ALL) {
                    list.retain(|n| n.as_str().is_some_and(|n| allowed.contains(n)));
                    return !list.is_empty();
                }
                return false;
            }
            keep.contains(id)
        });
    }
}

/// 拆不开、或者落不回去。**英文只写一遍**：`Display` 就是 [`ModelError::msg`] 的原句。
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    /// 配置读不进来（语法错、字段不认识）。里面是和换入配置时同一句话
    #[error("{}", self.msg())]
    Config(Box<tw_config::Rejected>),
    /// 配置的格式版本比这一份 Core 新
    #[error("{}", self.msg())]
    TooNew { schema: u32 },
    /// 一项的单位拼不回配置里的样子（同步合并出来的东西不成形）
    #[error("{}", self.msg())]
    Shape { item: String, detail: String },
    /// 按名字改配置时没成
    #[error("{}", self.msg())]
    Edit(String, Box<Msg>),
    /// 写完读回来和要的不一样。是我们自己的错
    #[error("{}", self.msg())]
    Verify { detail: String },
}

impl ModelError {
    pub fn msg(&self) -> Msg {
        match self {
            ModelError::Config(r) => r.msg(),
            ModelError::TooNew { schema } => msg!(
                "pack.schema_too_new", schema = schema, supported = tw_config::SCHEMA_VERSION =>
                "The configuration is schema version {schema}, and this version supports up to \
                 {supported}. Update ThinkWatch Lite first."
            ),
            ModelError::Shape { item, detail } => msg!(
                "pack.item_shape", item = item, detail = detail =>
                "{item} cannot be written into the configuration: {detail}"
            ),
            ModelError::Edit(_, m) => (**m).clone(),
            ModelError::Verify { detail } => msg!(
                "pack.verify_failed", detail = detail =>
                "The configuration that was put together does not read back as intended \
                 ({detail}), so nothing was written."
            ),
        }
    }

    /// 读不进来是因为它由更新的版本写的（不认识的字段、取值，或者格式版本更大）
    pub fn is_too_new(&self) -> bool {
        match self {
            ModelError::TooNew { .. } => true,
            ModelError::Config(r) => matches!(
                r.message.code.as_str(),
                "config.unknown_field" | "config.unknown_variant"
            ),
            _ => false,
        }
    }
}

impl From<tw_config::edit::EditError> for ModelError {
    fn from(e: tw_config::edit::EditError) -> Self {
        ModelError::Edit(e.to_string(), Box::new(e.msg()))
    }
}

impl From<tw_yaml::PatchError> for ModelError {
    fn from(e: tw_yaml::PatchError) -> Self {
        ModelError::Edit(e.to_string(), Box::new(e.msg()))
    }
}

// ─────────────────────────────────────────────────────────── 插件的拆法

static CODEC: OnceLock<Box<dyn PluginCodec>> = OnceLock::new();

/// 装上插件源码的拆法（[`PluginCodec`]）。**进程里装一次**：Core 启动时装上真的那一套
/// （它要编插件的那几个 crate）；没装时按 [`WholeFile`] 整份比较。装过了返回 false
pub fn install_codec(codec: Box<dyn PluginCodec>) -> bool {
    CODEC.set(codec).is_ok()
}

/// 此刻用的拆法
pub fn codec() -> &'static dyn PluginCodec {
    match CODEC.get() {
        Some(c) => c.as_ref(),
        None => &WholeFile,
    }
}

// ─────────────────────────────────────────────────────────── 拆

/// 把一个包拆成项目。本机设置不在里面。用进程里装的插件拆法（[`install_codec`]）
pub fn snapshot(pack: &Pack) -> Result<Snapshot, ModelError> {
    snapshot_with(pack, codec())
}

/// 同 [`snapshot`]，插件拆法由调用方给
pub fn snapshot_with(pack: &Pack, codec: &dyn PluginCodec) -> Result<Snapshot, ModelError> {
    let cfg = parse(&pack.config_yaml)?;
    let mut items = items::of_config(&cfg, pack, codec)?;
    items::relink(&mut items);
    Ok(Snapshot { items })
}

/// 读一份配置原文，**不做整份校验**：一个只有一个上游的部分包没有网关密钥，也没有
/// 控制面的钥匙。本机那几节先去掉再读，同步那一节的写法这里不必认得
pub(crate) fn parse(text: &str) -> Result<tw_config::Config, ModelError> {
    let reject = || -> ModelError {
        match tw_config::try_parse(text) {
            Err(r) if r.stage != tw_config::Stage::Semantics => ModelError::Config(Box::new(r)),
            _ => ModelError::Verify {
                detail: "the configuration could not be read".into(),
            },
        }
    };
    let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|_| reject())?;
    if doc.is_null() {
        doc = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if let serde_yaml_ng::Value::Mapping(m) = &mut doc {
        for k in LOCAL_SECTIONS {
            m.remove(*k);
        }
    }
    let cfg: tw_config::Config = serde_yaml_ng::from_value(doc).map_err(|e| {
        // 本机那几节以外读不进来：原文整份读一遍，借它的行号和那句带码的话
        match tw_config::try_parse(text) {
            Err(r) if r.stage != tw_config::Stage::Semantics => ModelError::Config(Box::new(r)),
            _ => ModelError::Config(Box::new(tw_config::Rejected {
                stage: tw_config::Stage::Schema,
                message: Box::new(msg!(tw_config::reload::UNPARSABLE, detail = e => "{detail}")),
                line: None,
                column: None,
                excerpt: None,
            })),
        }
    })?;
    if cfg.version > tw_config::SCHEMA_VERSION {
        return Err(ModelError::TooNew {
            schema: cfg.version,
        });
    }
    Ok(cfg)
}

/// [`Pack::content_id`]
pub(crate) fn content_id(pack: &Pack) -> String {
    let hash = match snapshot(pack) {
        Ok(s) => blake3::hash(
            serde_json::to_string(&normalized(&s))
                .unwrap_or_default()
                .as_bytes(),
        ),
        Err(_) => {
            let mut h = blake3::Hasher::new();
            h.update(pack.config_yaml.as_bytes());
            for (id, f) in &pack.plugins {
                h.update(id.as_bytes());
                h.update(&f.script);
            }
            if let Some(p) = &pack.client_prefs {
                h.update(p.to_string().as_bytes());
            }
            h.finalize()
        }
    };
    hash.to_hex()[..12].to_string()
}

/// 只留单位（引用是算出来的），全局设置里等于默认值的单位去掉、空了的项目去掉 ——
/// 同步合并出来的快照可能把默认值写明了，落回配置之后它们读出来就是没写
pub fn normalized(s: &Snapshot) -> Snapshot {
    let mut out = s.clone();
    for (id, item) in out.items.iter_mut() {
        item.refs.clear();
        item.files = None;
        if id.kind == ItemKind::Global {
            item.units
                .retain(|path, v| !items::is_global_default(path, v));
        }
    }
    out.items.retain(|id, item| {
        !(matches!(id.kind, ItemKind::Global | ItemKind::ClientPrefs) && item.units.is_empty())
    });
    out
}

#[cfg(test)]
mod tests;

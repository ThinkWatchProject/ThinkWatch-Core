//! 一轮同步：看同步位置上有什么新版本，和本机合并，该应用的应用、该上传的上传。
//!
//! # 一轮里做什么
//!
//! 1. 看 keyring 定这一轮加不加密（[`Engine::security`]）。**加密不可降级**：本机设置为加密、
//!    或者同步位置上有开着的 keyring 时，不加密的版本和设备记录一律不认；keyring 不见了、被换成
//!    旧的不当作「关掉了加密」，要人处理（[`RoundError::KeyringMissing`]）。只有用口令解得开、
//!    写着已关闭的 keyring 才算关了加密。
//! 2. 列出 `versions/`，读出每个版本（加密的用版本密钥解开）。读不出来的分两种：**坏了**的（不是
//!    配置包、不是这把密钥加密的、加密的位置上没加密）马上跳过；**可能没传完**的当作尚未到达，
//!    最新那一层有没到的就等下一轮，等了 30 分钟还读不出来也跳过。跳过的计数报给界面。
//! 3. 找出所有的**头**（没有别的版本依据它的版本）。不止一个时（两台设备同时上传、云盘晚到
//!    的版本）先以它们共同的上一版本为基准两两合并，得到「同步位置的最新」。
//! 4. 以本机基准（上次同步的那一版）为基准，和本机当前配置三方合并（[`crate::merge`]）。基准
//!    **得是同步位置最新的祖先**（版本记着自己的祖先，见 [`Meta::history`]）；不是的话（同步位置
//!    被别的设备重新上传过、keyring 换过）按两方比较。冲突照常往下走：本机留本机的，上传的留
//!    同步位置的，记进冲突记录。
//! 5. 本机的结果和现在不一样就应用（经控制面的写入流程，来源记为同步）；上传的结果和同步
//!    位置上的不一样（或者合并了几个头、冲突记录变了）就上传一个新版本，然后删掉更早的。
//!
//! # 冲突记录随版本走
//!
//! 一处冲突在被处理之前，**两边各留各的**：参与的每台设备用自己的内容，同步版本里是先上传
//! 的那一边（[`Conflict::shared`]）。任一设备处理之后，冲突从记录里去掉、结果上传，别的设备
//! 看到记录没了，就照同步版本改成处理之后的样子。
//!
//! # 等登录的上游
//!
//! ChatGPT 账号的登录、OAuth 的令牌属于各自的设备，不随同步走。一台设备上登录着的上游到了
//! 另一台没登录的设备上，**在那台停用着**（不然过不了校验）。这个停用是那台设备自己的事
//! （[`State::held_login`]），和插件的停用一样：合并之前换回同步的样子，落地时再停用，上传的
//! 里面永远是同步的样子 —— 不会因为一台没登录的设备把别的设备上用着的上游停掉。那台登录之后
//! 照同步的样子启用。

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tw_pack::model::{self, Item, ItemId, ItemKind, Snapshot, unit};
use tw_pack::{Pack, PackError, PackKind, PackOrigin, Seal, Unseal};
use tw_types::{Msg, msg};

use crate::device::DeviceRecord;
use crate::keyring::{self, Keyring, KeyringError};
use crate::layout::{self, DEVICES, KEYRING, Listed, VERSIONS, VersionName};
use crate::local::{Local, State, UrlChange};
use crate::merge::{self, Objects};
use crate::order;
use crate::record::{self, Conflict, HISTORY, Meta, Reason, Side};
use crate::store::{Store, StoreError};

/// 设备记录多久没写就写一次（最后同步时间），即便这一轮什么都没变
const DEVICE_RECORD_EVERY_MS: u64 = 6 * 3600 * 1000;

/// 一个可能没传完的版本（或者 keyring）等多久：之后跳过，不再让它挡着
pub const SKIP_AFTER_MS: u64 = 30 * 60 * 1000;

/// 同步位置看上去空了：连着这么多轮、过了这么久、自己的设备记录也不在了，才算真的清空了
const EMPTY_ROUNDS: u32 = 3;
const EMPTY_AFTER_MS: u64 = 10 * 60 * 1000;

/// 本机这一边：配置从哪儿读、往哪儿写。控制面实现它（`tw-control` 的 `sync`），测试里换成
/// 两个目录里的两份配置
pub trait Host: Send + Sync {
    /// 本机此刻的完整配置包（含本机设置、插件、通用偏好）
    fn current(&self) -> Result<Current, Msg>;
    /// 照这个包写本机配置：经现有的写入流程（校验、热更新、记一个本机版本），来源记为同步。
    /// `base_version` 是 [`Current::version`]：读完之后配置被别人改过就是 [`ApplyError::Stale`]
    fn apply(
        &self,
        pack: Pack,
        base_version: &str,
        from: &str,
    ) -> impl Future<Output = Result<(), ApplyError>> + Send;
    /// 本机已接管的客户端正在用的网关密钥（名字）
    fn pinned_keys(&self) -> BTreeSet<String>;
    /// 本机备份里内容标识是这个的那一版
    fn backup(&self, content_id: &str) -> Option<Pack>;
}

/// 本机此刻的配置
pub struct Current {
    pub pack: Pack,
    /// 配置文件的版本号（`tw_config::version_of`），写的时候拿它核对
    pub version: String,
    /// 配置最后改动的时间（冲突里「本机改的时间」）
    pub changed_ms: u64,
}

#[derive(Debug)]
pub enum ApplyError {
    /// 读完之后配置被改过了：下一轮再来
    Stale,
    Rejected(Msg),
}

/// 这台设备在同步里的设置
#[derive(Debug, Clone)]
pub struct Settings {
    pub device_id: String,
    pub device_name: String,
    pub encrypt: bool,
    pub passphrase: Option<String>,
    /// 不同步的上游
    pub exclude: BTreeSet<String>,
    /// 同步位置的指纹（[`crate::local::place`]）。本机的同步状态是对着它的：换了位置就作废
    pub place: String,
    /// 这台设备的 core 开着远程控制端口：设备记录里写上（界面标「远程 core」）
    pub remote_core: bool,
}

impl Settings {
    pub fn of(s: &tw_config::sync::Sync) -> Self {
        Self {
            device_id: s.device_id.clone(),
            device_name: s.device_name.clone(),
            encrypt: s.encrypt,
            passphrase: s.passphrase.clone(),
            exclude: s.exclude.iter().cloned().collect(),
            place: crate::local::place(&s.target),
            remote_core: false,
        }
    }
}

/// 一轮没成
#[derive(Debug)]
pub enum RoundError {
    /// 同步位置连不上、拒绝访问……
    Store(StoreError),
    /// 同步口令不对（别的设备改了口令），或者同步位置开了加密而这台设备还没有口令
    Locked(Msg),
    /// keyring 不见了、被换成了更早的那一份、一直读不出来：**要人处理**（放回这台设备记着的那
    /// 一份，或者停止同步）。不会因此关掉加密，也不删本机的口令
    KeyringMissing(Msg),
    /// 同步位置的最新版本由较新的 Core 写，本机读不了：暂停
    TooNew { core_version: String },
    /// 最新的版本还没传完（云盘还在下载）：下一轮再来，不算失败
    NotArrived,
    /// 同步位置看上去空了（网络盘没挂上时挂载点是个空目录）：再看几轮才下结论
    LooksEmpty,
    /// 同步位置确实空了（别的设备停止同步并删除了同步数据，或者换成了别的位置）：**要人处理**
    /// （重新上传本机配置，或者停止同步）。本机的同步设置不动
    Emptied,
    /// 本机的配置读不了、写不进去
    Local(Msg),
    /// 这一轮里本机配置被改了：马上再来一轮
    Stale,
}

impl From<StoreError> for RoundError {
    fn from(e: StoreError) -> Self {
        RoundError::Store(e)
    }
}

impl RoundError {
    pub fn msg(&self) -> Msg {
        match self {
            RoundError::Store(e) => e.msg(),
            RoundError::Locked(m) | RoundError::Local(m) | RoundError::KeyringMissing(m) => {
                m.clone()
            }
            RoundError::TooNew { core_version } if core_version.is_empty() => msg!(
                "sync.too_new_unknown" =>
                "another device uses a newer version. Update ThinkWatch Lite to keep syncing"
            ),
            RoundError::TooNew { core_version } => msg!(
                "sync.too_new", version = core_version =>
                "another device uses a newer version (Core {version}). Update ThinkWatch Lite to \
                 keep syncing"
            ),
            RoundError::NotArrived => msg!(
                "sync.not_arrived" =>
                "the latest version has not finished arriving at the sync location"
            ),
            RoundError::LooksEmpty => msg!(
                "sync.looks_empty" =>
                "the sync location looks empty. It is checked a few more times before anything \
                 is done"
            ),
            RoundError::Emptied => msg!(
                "sync.location_empty" =>
                "the sync location is empty: its sync data was deleted, or it is not the \
                 location it was. Upload this device's configuration to it again, or stop syncing"
            ),
            RoundError::Stale => msg!(
                "sync.stale" => "the configuration changed during the sync; it will run again"
            ),
        }
    }
}

fn locked_new() -> RoundError {
    RoundError::Locked(msg!(
        "sync.locked_new" =>
        "the sync location is now encrypted. Enter the sync passphrase to keep syncing"
    ))
}

fn keyring_missing() -> RoundError {
    RoundError::KeyringMissing(msg!(
        "sync.keyring_missing" =>
        "the keyring is missing from the sync location, so nothing was synced. Put back the \
         copy this device keeps, or stop syncing"
    ))
}

/// 一轮做了什么
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// 上传的新版本
    pub uploaded: Option<String>,
    /// 应用了别的设备的改动：那些设备的名字
    pub applied_from: Vec<String>,
    /// 还没处理的冲突
    pub conflicts: Vec<Conflict>,
    pub devices: Vec<DeviceRecord>,
    /// 同步位置此刻加没加密。和设置不一样时（别的设备开了、或者经口令认证关了加密）调用方
    /// 改设置
    pub encrypted: bool,
    /// 本机应用之后值变了、或者被删掉的网关密钥
    pub keys_changed: Vec<String>,
    /// 同步位置上跳过了的文件：坏了的、不是这把密钥加密的、加密的位置上没加密的、等了太久
    /// 还没传完的
    pub skipped: u32,
}

/// 读出来的一个版本
#[derive(Clone)]
pub struct Version {
    pub name: VersionName,
    pub pack: Pack,
    pub meta: Meta,
    pub snap: Snapshot,
    pub content: String,
}

/// 读过的版本（缓存）。可能没传完的不缓存，下一轮再读
#[derive(Clone)]
enum Fetched {
    Ok(Arc<Version>),
    TooNew(String),
    /// 读不出来、也不会自己好
    Broken,
}

enum Got {
    Ok(Arc<Version>),
    TooNew(String),
    Broken,
    /// 可能还没传完
    Pending,
    /// 列出来之后又没了
    Gone,
}

/// 同步位置上的版本，读过一遍之后
#[derive(Default)]
struct Scan {
    /// 参与这一轮的：读出来的、版本过新的、还在等的
    names: Vec<VersionName>,
    ok: Vec<Arc<Version>>,
    too_new: Vec<(VersionName, String)>,
    /// 可能没传完、还在等的
    pending: Vec<VersionName>,
    /// 跳过了的
    skipped: Vec<VersionName>,
    /// 跳过了的里面这台设备自己写的（keyring 换过之后解不开了）：重新上传之后删掉
    own_broken: Vec<VersionName>,
    /// 同步位置上全部的版本名
    all: Vec<VersionName>,
}

/// 同步位置的最新（几个头合并之后）
struct Remote {
    snap: Snapshot,
    conflicts: Vec<Conflict>,
    heads: Vec<Arc<Version>>,
    /// 写最新那个头的设备
    device: String,
    device_name: String,
    at_ms: u64,
}

/// 同步位置是不是空的、加没加密、有哪些设备（开启同步时的「测试连接」）
#[derive(Debug, Clone)]
pub struct Probe {
    pub writable: Result<(), StoreError>,
    pub existing: Option<Existing>,
}

#[derive(Debug, Clone)]
pub struct Existing {
    pub encrypted: bool,
    /// 加密时口令对不对（没给口令是 `false`）
    pub unlocked: bool,
    pub devices: Vec<DeviceRecord>,
}

/// 上一轮结束时同步位置上参与的版本、本机的内容、加没加密：都没变就什么都不用做
#[derive(Debug, Clone, PartialEq, Eq)]
struct Quiet {
    listing: Vec<String>,
    content: String,
    sealed: bool,
}

pub struct Engine<S: Store, H: Host> {
    pub store: S,
    pub host: H,
    local: Local,
    settings: Settings,
    /// 解开了的版本密钥（开着或关着的）
    keyring: Option<Keyring>,
    /// 这一份 keyring 文件原样的字节：同步成功之后存进本机（[`Local::save_keyring`]）
    keyring_raw: Option<Vec<u8>>,
    /// 同步位置开着加密：只认加密的版本和设备记录
    sealed: bool,
    /// 上面这几项是照哪一份 keyring 定的（文件的哈希，没有 keyring 是 `Some(None)`）。没定过是
    /// `None`
    mode_of: Option<Option<[u8; 32]>>,
    cache: BTreeMap<VersionName, Fetched>,
    quiet: Option<Quiet>,
    last_report: Report,
    /// 这台设备以前留下的临时文件清过了
    swept: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn local_err(e: impl std::fmt::Display) -> RoundError {
    RoundError::Local(msg!(
        "sync.local", detail = e =>
        "the local configuration could not be used for sync: {detail}"
    ))
}

/// 内容的 SHA-256：配置原文、插件、通用偏好（不加密的版本带着它，发现传输损坏）
fn content_sha256(p: &Pack) -> String {
    let mut h = Sha256::new();
    h.update(p.config_yaml.as_bytes());
    for (id, f) in &p.plugins {
        h.update(id.as_bytes());
        h.update([0]);
        h.update(&f.script);
        h.update([0]);
        if let Some(a) = &f.approved {
            h.update(a);
        }
        h.update([0]);
    }
    if let Some(c) = &p.client_prefs {
        h.update(c.to_string().as_bytes());
    }
    let out: [u8; 32] = h.finalize().into();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn objects(s: &Snapshot) -> Objects<ItemId> {
    s.items
        .iter()
        .map(|(k, v)| (k.clone(), v.units.clone()))
        .collect()
}

/// 合并出来的单位拼回项目：插件的原文件从脚本一样的那一边拿
fn rebuild(objs: &Objects<ItemId>, sides: &[&Snapshot]) -> Snapshot {
    let mut items = BTreeMap::new();
    for (id, units) in objs {
        let files = sides.iter().find_map(|s| {
            let it = s.items.get(id)?;
            (it.units.get(unit::SCRIPT) == units.get(unit::SCRIPT))
                .then(|| it.files.clone())
                .flatten()
        });
        items.insert(
            id.clone(),
            Item {
                units: units.clone(),
                refs: Default::default(),
                files,
            },
        );
    }
    let mut s = Snapshot { items };
    s.relink();
    s
}

/// 一种顺序项目的名字列表
fn order_list(s: Option<&Snapshot>, kind: ItemKind) -> Option<Vec<String>> {
    let it = s?.items.get(&ItemId::new(kind, ""))?;
    match it.units.get(unit::ALL) {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

fn names_of(s: &Snapshot, kind: ItemKind) -> BTreeSet<String> {
    s.of_kind(kind).map(|(id, _)| id.name.clone()).collect()
}

fn set_order(s: &mut Snapshot, kind: ItemKind, list: Vec<String>) {
    let id = ItemId::new(kind, "");
    if list.is_empty() {
        s.items.remove(&id);
        return;
    }
    let item = s.items.entry(id).or_default();
    item.units.insert(
        unit::ALL.to_string(),
        Value::Array(list.into_iter().map(Value::String).collect()),
    );
}

const ORDERS: &[(ItemKind, ItemKind)] = &[
    (ItemKind::ProviderOrder, ItemKind::Provider),
    (ItemKind::RouteOrder, ItemKind::Route),
    (ItemKind::PluginOrder, ItemKind::Plugin),
];

/// 对象里冲突的那几个单位此刻的值；对象不在是 `None`
fn side_units(s: &Snapshot, c: &Conflict) -> Option<BTreeMap<String, Value>> {
    s.items.get(&c.item).map(|it| c.pick(&it.units))
}

/// 合并新发现的一处冲突写成记录：`shared` 是同步版本那一边（快照和它的设备），`other` 是另一边
fn found(
    f: &merge::Found<ItemId>,
    shared: (&Snapshot, Side),
    other: (&Snapshot, Side),
) -> Conflict {
    // 两边都改了：记冲突的那几个单位；一边删了：记整个对象
    let placeholder = (f.kind == merge::Kind::Modified).then(BTreeMap::new);
    let mut c = Conflict {
        id: record::new_id(),
        item: f.key.clone(),
        units: f.units.iter().cloned().collect(),
        shared: Side {
            units: placeholder.clone(),
            ..shared.1
        },
        other: Side {
            units: placeholder,
            ..other.1
        },
        reason: None,
    };
    c.shared.units = side_units(shared.0, &c);
    c.other.units = side_units(other.0, &c);
    c
}

/// 把冲突的那一部分改成 `src` 里的样子
fn force(dst: &mut Snapshot, src: &Snapshot, c: &Conflict) {
    match src.items.get(&c.item) {
        None => {
            dst.items.remove(&c.item);
        }
        Some(it) if c.whole_item() => {
            dst.items.insert(c.item.clone(), it.clone());
        }
        Some(it) => {
            let entry = dst
                .items
                .entry(c.item.clone())
                .or_insert_with(|| it.clone());
            for u in &c.units {
                match it.units.get(u) {
                    Some(v) => {
                        entry.units.insert(u.clone(), v.clone());
                    }
                    None => {
                        entry.units.remove(u);
                    }
                }
            }
            if it.units.get(unit::SCRIPT) == entry.units.get(unit::SCRIPT) {
                entry.files = it.files.clone();
            }
        }
    }
}

/// 把冲突那一部分写成一边记录下来的值
fn set_side(dst: &mut Snapshot, c: &Conflict, side: &Side, from: &Snapshot) {
    match &side.units {
        None => {
            dst.items.remove(&c.item);
        }
        Some(units) => {
            let entry = dst
                .items
                .entry(c.item.clone())
                .or_insert_with(|| from.items.get(&c.item).cloned().unwrap_or_default());
            for (u, v) in units {
                entry.units.insert(u.clone(), v.clone());
            }
            for u in &c.units {
                if !units.contains_key(u) {
                    entry.units.remove(u);
                }
            }
        }
    }
}

/// 处理一处冲突：采用哪一边
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// 本机的
    Local,
    /// 另一边的
    Remote,
    /// 两者都保留：本机的留原名称，另一边的以「名称 (设备名)」新增
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: String,
    pub take: Take,
    /// 两者都保留时另一份的名字。不给就是「名称 (设备名)」，被占了往后加序号
    pub new_name: Option<String>,
}

/// 一处冲突能怎么处理：本机的总能选；另一边的、两者都保留看冲突（[`Conflict::remote_allowed`]、
/// [`Conflict::both_allowed`]）
pub fn options(c: &Conflict) -> Vec<Take> {
    let mut out = vec![Take::Local];
    if c.remote_allowed() {
        out.push(Take::Remote);
    }
    if c.both_allowed() {
        out.push(Take::Both);
    }
    out
}

/// 「两者都保留」那一份的名字。自定义安全规则的名字带着分项（`content/项目代号`），只换规则名
fn both_name(id: &ItemId, given: Option<&str>, device_name: &str) -> String {
    let (prefix, plain) = match (id.kind, id.name.split_once('/')) {
        (ItemKind::SecurityRule, Some((guard, rule))) => (format!("{guard}/"), rule),
        _ => (String::new(), id.name.as_str()),
    };
    match given.map(str::trim).filter(|g| !g.is_empty()) {
        Some(g) if g.contains('/') && id.kind == ItemKind::SecurityRule => g.to_string(),
        Some(g) => format!("{prefix}{g}"),
        None => format!("{prefix}{plain} ({device_name})"),
    }
}

/// 不同步的上游，连同直接或间接引用它们的项目（路由、别名、分组）
pub fn excluded(names: &BTreeSet<String>, snaps: &[&Snapshot]) -> BTreeSet<ItemId> {
    let mut out: BTreeSet<ItemId> = names
        .iter()
        .map(|n| ItemId::new(ItemKind::Provider, n.clone()))
        .collect();
    if out.is_empty() {
        return out;
    }
    loop {
        let before = out.len();
        for s in snaps {
            for (id, it) in &s.items {
                if !id.kind.is_order() && it.refs.iter().any(|r| out.contains(r)) {
                    out.insert(id.clone());
                }
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// 逐字段合并的那几种：全局设置的一节、Lite 的通用偏好（全是默认值时整个不在）
fn fieldwise(id: &ItemId) -> bool {
    matches!(id.kind, ItemKind::Global | ItemKind::ClientPrefs)
}

/// 一个上游的地址（`conn.base_url`）
fn base_url(it: &Item) -> Option<&str> {
    it.units.get(unit::CONN)?.get("base_url")?.as_str()
}

/// 一把网关密钥的值
fn key_value(it: &Item) -> Option<&Value> {
    it.units.get(unit::ALL)?.get("key")
}

// ─────────────────────────────────────────────── 等登录的上游

/// 本机配置里的上游（带着这台设备自己的登录）
fn local_providers(p: &Pack) -> BTreeMap<String, tw_config::Provider> {
    tw_config::try_parse(&p.config_yaml)
        .map(|c| {
            c.providers
                .into_iter()
                .map(|p| (p.name.clone(), p))
                .collect()
        })
        .unwrap_or_default()
}

/// 这个上游落到这台设备上没有登录可用：ChatGPT 账号这台没登录，OAuth 这台没有同一个令牌端点
/// 的刷新令牌（落地时令牌取本机的，见 `tw_pack::model`）
fn needs_login(id: &ItemId, it: &Item, here: &BTreeMap<String, tw_config::Provider>) -> bool {
    if id.kind != ItemKind::Provider {
        return false;
    }
    let Ok(p) = model::provider_of(id, it) else {
        return false;
    };
    let mine = here.get(&id.name);
    if model::is_chatgpt(&p) {
        return !mine.is_some_and(|m| model::is_chatgpt(m) && m.oauth.is_some());
    }
    match &p.oauth {
        Some(o) => !mine
            .and_then(|m| m.oauth.as_ref())
            .is_some_and(|m| m.endpoint == o.endpoint && !m.refresh.trim().is_empty()),
        None => false,
    }
}

fn disabled(it: &Item) -> bool {
    it.units
        .get(unit::OTHER)
        .and_then(|o| o.get("disabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 停用一个上游
fn force_disabled(it: &mut Item) {
    let other = it
        .units
        .entry(unit::OTHER.to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(o) = other {
        o.insert("disabled".into(), Value::Bool(true));
    }
}

/// 一个上游的停用照 `from` 的样子
fn copy_disabled(it: &mut Item, from: &Item) {
    let want = from
        .units
        .get(unit::OTHER)
        .and_then(|o| o.get("disabled"))
        .cloned();
    let other = it
        .units
        .entry(unit::OTHER.to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(o) = other {
        match want {
            Some(v) => o.insert("disabled".into(), v),
            None => o.remove("disabled"),
        };
    }
}

/// 落地之前：没登录、在这台停用着的上游停用掉，记下哪些是这台自己停用的（同步的样子是启用的）
fn hold_logins(
    target: &mut Snapshot,
    here: &BTreeMap<String, tw_config::Provider>,
) -> BTreeSet<String> {
    let mut held = BTreeSet::new();
    let ids: Vec<ItemId> = target
        .of_kind(ItemKind::Provider)
        .filter(|(id, it)| needs_login(id, it, here) && !disabled(it))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(it) = target.items.get_mut(&id) {
            force_disabled(it);
        }
        held.insert(id.name);
    }
    held
}

// ─────────────────────────────────────────────── 版本的谱系

/// 版本标识 → 它记着的上一版本和更早的祖先
type Index = BTreeMap<String, (Vec<String>, Vec<String>)>;

fn index_of<'a>(vs: impl Iterator<Item = &'a Version>) -> Index {
    vs.map(|v| {
        (
            v.name.id(),
            (v.meta.parents.clone(), v.meta.history.clone()),
        )
    })
    .collect()
}

/// `start` 自己和它的全部祖先（按谱系能找到的）
fn ancestors(index: &Index, start: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut todo = vec![start.to_string()];
    while let Some(id) = todo.pop() {
        if !out.insert(id.clone()) {
            continue;
        }
        if let Some((ps, hs)) = index.get(&id) {
            todo.extend(ps.iter().cloned());
            todo.extend(hs.iter().cloned());
        }
    }
    out
}

/// 依据 `parents` 的新版本要记的更早的祖先：父版本们的祖先，最近的 [`HISTORY`] 个
fn history_of(parents: &[Arc<Version>]) -> Vec<String> {
    let direct: BTreeSet<String> = parents.iter().map(|p| p.name.id()).collect();
    let mut all: BTreeSet<VersionName> = parents
        .iter()
        .flat_map(|p| p.meta.history.iter().chain(&p.meta.parents))
        .filter(|id| !direct.contains(*id))
        .filter_map(|id| VersionName::parse(id))
        .collect();
    while all.len() > HISTORY {
        all.pop_first();
    }
    all.into_iter().map(|n| n.id()).collect()
}

impl<S: Store, H: Host> Engine<S, H> {
    /// `data_dir` 是配置文件所在的目录：本机的同步状态放在它底下的 `sync/`
    pub fn new(store: S, host: H, data_dir: &Path, settings: Settings) -> Self {
        Self {
            store,
            host,
            local: Local::new(data_dir),
            settings,
            keyring: None,
            keyring_raw: None,
            sealed: false,
            mode_of: None,
            cache: BTreeMap::new(),
            quiet: None,
            last_report: Report::default(),
            swept: false,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn set_settings(&mut self, s: Settings) {
        if s.passphrase != self.settings.passphrase || s.encrypt != self.settings.encrypt {
            self.mode_of = None;
            self.keyring = None;
            self.cache.clear();
        }
        self.quiet = None;
        self.settings = s;
    }

    pub fn local(&self) -> &Local {
        &self.local
    }

    pub fn state(&self) -> State {
        self.local.state()
    }

    /// 上一轮的结果（冲突、设备列表）
    pub fn last_report(&self) -> &Report {
        &self.last_report
    }

    /// 下一轮不走捷径：哪怕同步位置和本机都没变，也从头看一遍
    pub fn invalidate(&mut self) {
        self.quiet = None;
    }

    // ─────────────────────────────────────────────── 加不加密

    /// 照 keyring 定下来：开着、关着、没有。`of` 是 keyring 文件的哈希
    fn set_mode(
        &mut self,
        of: Option<[u8; 32]>,
        keyring: Option<Keyring>,
        raw: Option<Vec<u8>>,
    ) -> bool {
        if self.mode_of != Some(of) {
            self.cache.clear();
            self.quiet = None;
        }
        self.mode_of = Some(of);
        self.sealed = keyring.as_ref().is_some_and(|k| !k.off());
        self.keyring = keyring;
        self.keyring_raw = raw;
        self.sealed
    }

    /// 这一轮加不加密：keyring 在、开着是 `true`（之后只认加密的版本和设备记录）。
    ///
    /// - 没有 keyring：本机设置为加密、或者以前见过 keyring 时是 [`RoundError::KeyringMissing`]
    ///   —— **不当作关掉了加密**；
    /// - keyring 解得开：开着就加密；写着已关闭（有口令的人关的）就不加密，代号比见过的小
    ///   （有人放回了一份旧的）是 [`RoundError::KeyringMissing`]；
    /// - 本来就不加密、没有口令的设备认明文的「已关闭」标记（见 [`crate::keyring`]）
    async fn security(&mut self, st: &mut State) -> Result<bool, RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            st.unreadable.remove(KEYRING);
            if self.settings.encrypt || st.keyring_gen > 0 {
                return Err(keyring_missing());
            }
            return Ok(self.set_mode(None, None, None));
        };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if self.mode_of == Some(Some(hash)) {
            return Ok(self.sealed);
        }
        let trust_mark = keyring::marked_off(&bytes) && !self.settings.encrypt;
        let Some(pass) = self.settings.passphrase.clone().filter(|p| !p.is_empty()) else {
            if trust_mark {
                return Ok(self.set_mode(Some(hash), None, None));
            }
            return Err(locked_new());
        };
        let opened = {
            let b = bytes.clone();
            tokio::task::spawn_blocking(move || Keyring::open(&b, &pass))
                .await
                .map_err(local_err)?
        };
        match opened {
            Ok(k) => {
                st.unreadable.remove(KEYRING);
                if k.off() && k.generation() < st.keyring_gen {
                    return Err(RoundError::KeyringMissing(msg!(
                        "sync.keyring_rolled_back" =>
                        "the keyring at the sync location is older than the one this device \
                         saw last, so nothing was synced. Put back the copy this device keeps, \
                         or stop syncing"
                    )));
                }
                st.keyring_gen = st.keyring_gen.max(k.generation());
                Ok(self.set_mode(Some(hash), Some(k), Some(bytes)))
            }
            Err(_) if trust_mark => Ok(self.set_mode(Some(hash), None, None)),
            Err(KeyringError::WrongPassphrase) => Err(RoundError::Locked(msg!(
                "sync.passphrase_changed" =>
                "the sync passphrase was changed on another device. Enter the new passphrase"
            ))),
            Err(KeyringError::TooNew) => Err(RoundError::TooNew {
                core_version: String::new(),
            }),
            Err(KeyringError::Corrupt) => {
                let since = *st
                    .unreadable
                    .entry(KEYRING.to_string())
                    .or_insert_with(now_ms);
                if now_ms().saturating_sub(since) >= SKIP_AFTER_MS {
                    Err(RoundError::KeyringMissing(KeyringError::Corrupt.msg()))
                } else {
                    Err(RoundError::NotArrived)
                }
            }
        }
    }

    /// 解开了的 keyring 写设备记录、版本时用不用：加密开着才用
    fn sealing(&self) -> Option<&Keyring> {
        self.keyring.as_ref().filter(|_| self.sealed)
    }

    // ─────────────────────────────────────────────── 读

    /// 读一个版本
    async fn fetch(&mut self, name: &VersionName) -> Result<Got, RoundError> {
        if let Some(f) = self.cache.get(name) {
            return Ok(match f.clone() {
                Fetched::Ok(v) => Got::Ok(v),
                Fetched::TooNew(c) => Got::TooNew(c),
                Fetched::Broken => Got::Broken,
            });
        }
        let Some(bytes) = self.store.get(&name.path()).await? else {
            return Ok(Got::Gone);
        };
        // 空的：云盘的占位文件、正在写
        if bytes.is_empty() {
            return Ok(Got::Pending);
        }
        // 认得出是配置包（ZIP）或者加密过的：读不出来多半是没传完
        let shaped = keyring::is_age(&bytes) || bytes.starts_with(b"PK\x03\x04");
        let identity = self.keyring.as_ref().map(|k| k.identity().clone());
        let strict = self.sealed;
        let decoded = tokio::task::spawn_blocking(move || {
            let unseal = match &identity {
                Some(id) => Unseal::Identity(id),
                None => Unseal::None,
            };
            let pack = if strict {
                tw_pack::read_strict(&bytes, unseal)?
            } else {
                tw_pack::read(&bytes, unseal)?
            };
            let snap = model::snapshot(&pack);
            Ok::<_, PackError>((pack, snap))
        })
        .await
        .map_err(local_err)?;
        let fetched = match decoded {
            Err(PackError::TooNew { core_version, .. }) => Fetched::TooNew(core_version),
            // 加密的版本，而 keyring 还没到
            Err(PackError::NeedKey) => return Ok(Got::Pending),
            Err(PackError::Corrupt { .. }) if shaped => return Ok(Got::Pending),
            Err(_) => Fetched::Broken,
            Ok((pack, Err(e))) if e.is_too_new() => Fetched::TooNew(pack.manifest.core_version),
            Ok((_, Err(_))) => Fetched::Broken,
            Ok((pack, Ok(snap))) => {
                let meta = pack
                    .manifest
                    .sync
                    .clone()
                    .and_then(|v| serde_json::from_value::<Meta>(v).ok());
                match meta {
                    // 名字和内容对不上（被人挪过、改过名），或者不加密的版本校验和对不上：坏了
                    Some(meta)
                        if meta.seq == name.seq
                            && layout::device_tag(&meta.device) == name.device
                            && (pack.manifest.encrypted
                                || meta.sha256.as_deref()
                                    == Some(content_sha256(&pack).as_str())) =>
                    {
                        let content = pack.content_id();
                        Fetched::Ok(Arc::new(Version {
                            name: name.clone(),
                            pack,
                            meta,
                            snap,
                            content,
                        }))
                    }
                    _ => Fetched::Broken,
                }
            }
        };
        self.cache.insert(name.clone(), fetched.clone());
        Ok(match fetched {
            Fetched::Ok(v) => Got::Ok(v),
            Fetched::TooNew(c) => Got::TooNew(c),
            Fetched::Broken => Got::Broken,
        })
    }

    /// 这一份基准（不加密的配置包）读成版本
    fn version_of(bytes: &[u8], id: &str) -> Option<Version> {
        let pack = tw_pack::read(bytes, Unseal::None).ok()?;
        let meta: Meta = serde_json::from_value(pack.manifest.sync.clone()?).ok()?;
        let snap = model::snapshot(&pack).ok()?;
        let name = VersionName::parse(id)?;
        let content = pack.content_id();
        Some(Version {
            name,
            pack,
            meta,
            snap,
            content,
        })
    }

    fn base(&self) -> Option<Version> {
        Self::version_of(&self.local.base()?, &self.local.state().base?)
    }

    /// 同步位置上的版本名
    async fn listing(&self) -> Result<Vec<VersionName>, RoundError> {
        let mut names: Vec<VersionName> = self
            .store
            .list(VERSIONS)
            .await?
            .into_iter()
            .filter_map(|e| VersionName::parse(&e.name))
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// 读一遍同步位置上的版本，分出读得出来的、过新的、还在等的、跳过的
    async fn scan(&mut self, all: Vec<VersionName>, st: &mut State) -> Result<Scan, RoundError> {
        self.cache.retain(|n, _| all.contains(n));
        let me = layout::device_tag(&self.settings.device_id);
        let now = now_ms();
        let mut sc = Scan::default();
        for n in &all {
            match self.fetch(n).await? {
                Got::Ok(v) => {
                    st.unreadable.remove(&n.id());
                    sc.names.push(n.clone());
                    sc.ok.push(v);
                }
                Got::TooNew(cv) => {
                    st.unreadable.remove(&n.id());
                    sc.names.push(n.clone());
                    sc.too_new.push((n.clone(), cv));
                }
                Got::Broken => {
                    st.unreadable.remove(&n.id());
                    sc.skipped.push(n.clone());
                    if n.device == me {
                        sc.own_broken.push(n.clone());
                    }
                }
                Got::Pending => {
                    let since = *st.unreadable.entry(n.id()).or_insert(now);
                    if now.saturating_sub(since) >= SKIP_AFTER_MS {
                        sc.skipped.push(n.clone());
                    } else {
                        sc.names.push(n.clone());
                        sc.pending.push(n.clone());
                    }
                }
                Got::Gone => {}
            }
        }
        st.unreadable
            .retain(|k, _| k == KEYRING || all.iter().any(|n| &n.id() == k));
        sc.all = all;
        Ok(sc)
    }

    /// 头：没有别的版本依据它的版本。最新的是较新的 Core 写的是 [`RoundError::TooNew`]，最新那
    /// 一层还没到齐是 [`RoundError::NotArrived`]
    fn heads(sc: &Scan) -> Result<Vec<Arc<Version>>, RoundError> {
        let referenced: BTreeSet<String> = sc
            .ok
            .iter()
            .flat_map(|v| v.meta.parents.iter().cloned())
            .collect();
        if let Some((_, cv)) = sc
            .too_new
            .iter()
            .find(|(n, _)| !referenced.contains(&n.id()))
        {
            return Err(RoundError::TooNew {
                core_version: cv.clone(),
            });
        }
        let top_ok = sc.ok.iter().map(|v| v.name.seq).max();
        let top_all = sc.names.iter().map(|n| n.seq).max();
        if top_ok.is_none() || top_ok < top_all {
            return Err(RoundError::NotArrived);
        }
        Ok(sc
            .ok
            .iter()
            .filter(|v| !referenced.contains(&v.name.id()))
            .cloned()
            .collect())
    }

    /// 同步位置上的设备记录。半年没更新的顺手删掉。加密着时不加密的记录不认
    async fn devices(&mut self) -> Result<Vec<DeviceRecord>, RoundError> {
        let kr = self.keyring.as_ref();
        let now = now_ms();
        let mut out = Vec::new();
        for e in self.store.list(DEVICES).await? {
            if !layout::is_device_file(&e.name) {
                continue;
            }
            let path = format!("{DEVICES}/{}", e.name);
            let Some(bytes) = self.store.get(&path).await? else {
                continue;
            };
            let Some(rec) = DeviceRecord::open(&bytes, kr, self.sealed) else {
                continue;
            };
            if rec.is_stale(now) && rec.id != self.settings.device_id {
                let _ = self.store.delete(&path).await;
                continue;
            }
            out.push(rec);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn write_device(&mut self, st: &mut State, paused: bool) -> Result<(), RoundError> {
        let rec = DeviceRecord {
            id: self.settings.device_id.clone(),
            name: self.settings.device_name.clone(),
            core_version: tw_pack::CORE_VERSION.to_string(),
            last_seen_ms: now_ms(),
            paused_update: paused,
            remote_core: self.settings.remote_core,
        };
        let bytes = rec.seal(self.sealing());
        self.store
            .put(&layout::device_path(&self.settings.device_id), &bytes)
            .await?;
        st.device_record_ms = rec.last_seen_ms;
        st.device_record_paused = paused;
        Ok(())
    }

    /// 存下本机的同步状态，连同这一轮解开的 keyring 的原样（留着放回去用）
    fn commit(&self, st: &State) -> Result<(), RoundError> {
        self.local.save_state(st).map_err(RoundError::Local)?;
        if let Some(raw) = &self.keyring_raw {
            self.local.save_keyring(raw).map_err(RoundError::Local)?;
        }
        Ok(())
    }

    // ─────────────────────────────────────────────── 合并几个头

    /// 同步位置上早删了的版本：本机之前的基准里找，本机备份里按内容找
    fn elsewhere(&self, id: &str, content: Option<&String>) -> Option<Version> {
        if let Some(v) = self
            .local
            .base_by_id(id)
            .and_then(|b| Self::version_of(&b, id))
        {
            return Some(v);
        }
        let content = content?;
        let pack = self.host.backup(content)?;
        let snap = model::snapshot(&pack).ok()?;
        Some(Version {
            name: VersionName::parse(id)?,
            meta: Meta::default(),
            content: content.clone(),
            snap,
            pack,
        })
    }

    /// `a`、`b` 共同的上一版本
    fn common_ancestor(
        &self,
        a: &[Arc<Version>],
        b: &Version,
        all: &[Arc<Version>],
        base: Option<&Version>,
    ) -> Option<Version> {
        let index = index_of(all.iter().map(|v| v.as_ref()).chain(base));
        let mut content_of: BTreeMap<String, String> = BTreeMap::new();
        for v in all.iter().map(|v| v.as_ref()).chain(base) {
            for (p, c) in v.meta.parents.iter().zip(&v.meta.parent_content) {
                content_of.insert(p.clone(), c.clone());
            }
        }
        let left: BTreeSet<String> = a
            .iter()
            .flat_map(|v| ancestors(&index, &v.name.id()))
            .collect();
        let right = ancestors(&index, &b.name.id());
        let mut common: Vec<VersionName> = left
            .intersection(&right)
            .filter_map(|id| VersionName::parse(id))
            .collect();
        common.sort();
        let id = common.pop()?.id();
        if let Some(v) = all.iter().find(|v| v.name.id() == id) {
            return Some((**v).clone());
        }
        if let Some(b) = base.filter(|b| b.name.id() == id) {
            return Some(b.clone());
        }
        self.elsewhere(&id, content_of.get(&id))
    }

    /// 几个头合并成一份
    fn fold(&self, heads: &[Arc<Version>], all: &[Arc<Version>], base: Option<&Version>) -> Remote {
        let first = &heads[0];
        let mut acc = Remote {
            snap: first.snap.clone(),
            conflicts: first.meta.conflicts.clone(),
            heads: vec![first.clone()],
            device: first.meta.device.clone(),
            device_name: first.meta.device_name.clone(),
            at_ms: first.pack.manifest.created_at_ms,
        };
        for h in &heads[1..] {
            let anc = self.common_ancestor(&acc.heads, h, all, base);
            let anc_conf: BTreeSet<&str> = anc
                .as_ref()
                .map(|a| a.meta.conflicts.iter().map(|c| c.id.as_str()).collect())
                .unwrap_or_default();
            // 两边的冲突记录合起来，减去在共同的上一版本里有、而某一边已经处理掉了的
            let ids_a: BTreeSet<&str> = acc.conflicts.iter().map(|c| c.id.as_str()).collect();
            let ids_b: BTreeSet<&str> = h.meta.conflicts.iter().map(|c| c.id.as_str()).collect();
            let mut open: Vec<Conflict> = Vec::new();
            for c in acc.conflicts.iter().chain(&h.meta.conflicts) {
                let resolved = anc_conf.contains(c.id.as_str())
                    && !(ids_a.contains(c.id.as_str()) && ids_b.contains(c.id.as_str()));
                if !resolved && !open.iter().any(|o| o.id == c.id) {
                    open.push(c.clone());
                }
            }
            let (mut hold_items, hold_units) = record::holds(&open);
            for (o, _) in ORDERS {
                hold_items.insert(ItemId::new(*o, ""));
            }
            let base_objs = anc.as_ref().map(|a| objects(&a.snap));
            let (lo, ro) = (objects(&acc.snap), objects(&h.snap));
            let out = merge::merge(merge::Input {
                base: base_objs.as_ref(),
                local: &lo,
                remote: &ro,
                hold_items: &hold_items,
                hold_units: &hold_units,
                fieldwise: &fieldwise,
            });
            let mut snap = rebuild(&out.shared, &[&h.snap, &acc.snap]);
            for (o, k) in ORDERS {
                let m = order::merge(
                    order_list(anc.as_ref().map(|a| &a.snap), *o).as_deref(),
                    &order_list(Some(&acc.snap), *o).unwrap_or_default(),
                    &order_list(Some(&h.snap), *o).unwrap_or_default(),
                    &names_of(&snap, *k),
                    &names_of(&snap, *k),
                    open.iter().any(|c| c.item.kind == *o),
                );
                set_order(&mut snap, *o, m.shared);
                if m.conflict {
                    let id = ItemId::new(*o, "");
                    open.push(Conflict {
                        id: record::new_id(),
                        units: vec![unit::ALL.to_string()],
                        shared: Side {
                            device: h.meta.device.clone(),
                            device_name: h.meta.device_name.clone(),
                            at_ms: h.pack.manifest.created_at_ms,
                            units: h.snap.items.get(&id).map(|i| i.units.clone()),
                        },
                        other: Side {
                            device: acc.device.clone(),
                            device_name: acc.device_name.clone(),
                            at_ms: acc.at_ms,
                            units: acc.snap.items.get(&id).map(|i| i.units.clone()),
                        },
                        item: id,
                        reason: None,
                    });
                }
            }
            for f in &out.conflicts {
                open.push(found(
                    f,
                    (
                        &h.snap,
                        Side {
                            device: h.meta.device.clone(),
                            device_name: h.meta.device_name.clone(),
                            at_ms: h.pack.manifest.created_at_ms,
                            units: None,
                        },
                    ),
                    (
                        &acc.snap,
                        Side {
                            device: acc.device.clone(),
                            device_name: acc.device_name.clone(),
                            at_ms: acc.at_ms,
                            units: None,
                        },
                    ),
                ));
            }
            // 同步版本里冲突的那一部分是「同步版本那一边」记下的值
            for c in &open {
                set_side(&mut snap, c, &c.shared, &h.snap);
            }
            snap.relink();
            let later = h.pack.manifest.created_at_ms >= acc.at_ms;
            acc = Remote {
                snap,
                conflicts: open,
                heads: acc.heads.iter().cloned().chain([h.clone()]).collect(),
                device: if later {
                    h.meta.device.clone()
                } else {
                    acc.device
                },
                device_name: if later {
                    h.meta.device_name.clone()
                } else {
                    acc.device_name
                },
                at_ms: acc.at_ms.max(h.pack.manifest.created_at_ms),
            };
        }
        acc
    }

    // ─────────────────────────────────────────────── 写

    /// 新版本的序号：比参与的版本都大，和同步位置上已有的名字不重
    fn next_seq(&self, sc_names: &[VersionName], all: &[VersionName]) -> u64 {
        let mut seq = sc_names.iter().map(|n| n.seq).max().unwrap_or(0) + 1;
        let tag = layout::device_tag(&self.settings.device_id);
        while all.iter().any(|n| n.seq == seq && n.device == tag) {
            seq += 1;
        }
        seq
    }

    /// 上传一个新版本：内容是 `shared`，依据 `parents`
    async fn publish(
        &mut self,
        shared: &Snapshot,
        seq: u64,
        parents: &[Arc<Version>],
        conflicts: Vec<Conflict>,
    ) -> Result<Version, RoundError> {
        let name = VersionName::new(seq, &self.settings.device_id);
        let empty = Pack {
            manifest: tw_pack::Manifest::new(PackKind::Full, PackOrigin::Sync),
            config_yaml: format!("version: {}\n", tw_config::SCHEMA_VERSION),
            plugins: Default::default(),
            client_prefs: None,
        };
        let mut pack =
            model::materialize(&empty, shared).map_err(|e| RoundError::Local(e.msg()))?;
        pack.manifest = tw_pack::Manifest::new(PackKind::Full, PackOrigin::Sync);
        let mut meta = Meta {
            seq,
            device: self.settings.device_id.clone(),
            device_name: self.settings.device_name.clone(),
            parents: parents.iter().map(|p| p.name.id()).collect(),
            parent_content: parents.iter().map(|p| p.content.clone()).collect(),
            history: history_of(parents),
            sha256: None,
            conflicts,
        };
        let encrypted = self.sealed;
        if !encrypted {
            meta.sha256 = Some(content_sha256(&pack));
        }
        pack.manifest.sync = Some(serde_json::to_value(&meta).expect("plain data serializes"));
        let bytes = {
            let recipient = self.sealing().map(|k| k.recipient());
            let seal = match &recipient {
                Some(r) => Seal::Recipient(r),
                None => Seal::None,
            };
            tw_pack::write(&pack, seal).map_err(|e| RoundError::Local(e.msg()))?
        };
        match self.store.put_new(&name.path(), &bytes).await {
            Ok(()) => {}
            Err(StoreError::Exists { .. }) => return Err(RoundError::Stale),
            Err(e) => return Err(e.into()),
        }
        let plain = tw_pack::write(&pack, Seal::None).map_err(|e| RoundError::Local(e.msg()))?;
        let pack = tw_pack::read(&plain, Unseal::None).map_err(|e| RoundError::Local(e.msg()))?;
        let snap = model::snapshot(&pack).map_err(|e| RoundError::Local(e.msg()))?;
        let content = pack.content_id();
        let v = Version {
            name: name.clone(),
            pack,
            meta,
            snap,
            content,
        };
        self.cache.insert(name, Fetched::Ok(Arc::new(v.clone())));
        Ok(v)
    }

    fn save_base(&self, v: &Version, st: &mut State) -> Result<(), RoundError> {
        let bytes = tw_pack::write(&v.pack, Seal::None).map_err(|e| RoundError::Local(e.msg()))?;
        self.local
            .save_base(&v.name.id(), &bytes)
            .map_err(RoundError::Local)?;
        st.base = Some(v.name.id());
        Ok(())
    }

    /// 上传之后删掉更早的版本（[`layout::to_delete`]），连同比留下的都早的、跳过了的文件。删了
    /// 哪些
    async fn prune(
        &mut self,
        names: &[VersionName],
        ok: &[Arc<Version>],
        skipped: &[VersionName],
    ) -> BTreeSet<VersionName> {
        let ids = |v: &[String]| -> Vec<VersionName> {
            v.iter().filter_map(|p| VersionName::parse(p)).collect()
        };
        let listed: Vec<Listed> = names
            .iter()
            .map(|n| {
                let meta = ok.iter().find(|v| &v.name == n).map(|v| &v.meta);
                Listed {
                    name: n.clone(),
                    parents: meta.map(|m| ids(&m.parents)).unwrap_or_default(),
                    history: meta.map(|m| ids(&m.history)).unwrap_or_default(),
                }
            })
            .collect();
        let del: BTreeSet<VersionName> = layout::to_delete(&listed).into_iter().collect();
        let oldest_kept = names
            .iter()
            .filter(|n| !del.contains(*n))
            .map(|n| n.seq)
            .min();
        let mut gone = BTreeSet::new();
        let below = skipped
            .iter()
            .filter(|n| oldest_kept.is_some_and(|min| n.seq < min));
        for n in del.iter().chain(below) {
            if self.store.delete(&n.path()).await.is_ok() {
                self.cache.remove(n);
                gone.insert(n.clone());
            }
        }
        gone
    }

    // ─────────────────────────────────────────────── 一轮

    /// 同步一轮。冲突不算失败：在 [`Report::conflicts`] 里
    pub async fn round(&mut self) -> Result<Report, RoundError> {
        self.round_with(&[]).await
    }

    /// 同步一轮，顺带处理几处冲突。处理的结果作为新版本上传，别的设备随之清除这几处冲突。
    /// 已经被别的设备处理掉了的那几处不管
    pub async fn round_with(&mut self, decisions: &[Decision]) -> Result<Report, RoundError> {
        if !decisions.is_empty() {
            self.quiet = None;
        }
        let mut st = self.local.state();
        if st.place != self.settings.place && !st.place.is_empty() {
            // 换了同步位置：原来的基准是那个位置上的，作废
            self.local.clear();
            st = State::default();
            self.cache.clear();
            self.mode_of = None;
        }
        st.place.clone_from(&self.settings.place);
        if !self.swept {
            self.store
                .sweep(&layout::device_tag(&self.settings.device_id))
                .await;
            self.swept = true;
        }
        let r = self.round_inner(decisions, &mut st).await;
        // 成没成都存：读不出来的文件从什么时候起、见过的 keyring 代号、看上去空了几轮
        self.commit(&st)?;
        if let Ok(r) = &r {
            self.last_report = r.clone();
        }
        r
    }

    /// 同步位置上没有可用的版本
    async fn on_empty(&mut self, st: &mut State, has_base: bool) -> Result<Report, RoundError> {
        if !has_base {
            // 开启同步时上传第一个版本没成（中途断了）：这一轮补上
            self.security(st).await?;
            let all = self.listing().await?;
            let seq = self.next_seq(&[], &all);
            self.publish_first(seq, st).await?;
            st.empty_rounds = 0;
            st.empty_since_ms = None;
            return Ok(Report {
                encrypted: self.sealed,
                devices: self.devices().await?,
                ..Default::default()
            });
        }
        let now = now_ms();
        st.empty_rounds = st.empty_rounds.saturating_add(1);
        let since = *st.empty_since_ms.get_or_insert(now);
        let mine = self
            .store
            .get(&layout::device_path(&self.settings.device_id))
            .await?;
        if st.empty_rounds >= EMPTY_ROUNDS
            && now.saturating_sub(since) >= EMPTY_AFTER_MS
            && mine.is_none()
        {
            return Err(RoundError::Emptied);
        }
        Err(RoundError::LooksEmpty)
    }

    async fn round_inner(
        &mut self,
        decisions: &[Decision],
        st: &mut State,
    ) -> Result<Report, RoundError> {
        let base = self.base();
        let listed = self.listing().await?;
        if listed.is_empty() {
            return self.on_empty(st, base.is_some()).await;
        }
        let kr = self.security(st).await?;
        let sc = self.scan(listed, st).await?;
        if sc.names.is_empty() {
            return self.on_empty(st, base.is_some()).await;
        }
        st.empty_rounds = 0;
        st.empty_since_ms = None;
        let skipped = sc.skipped.len() as u32;
        let cur = self.host.current().map_err(RoundError::Local)?;
        // 本机的内容按原文算（含这台设备自己的登录：登录了要放开等登录的上游）
        let quiet = Quiet {
            listing: sc.names.iter().map(|n| n.id()).collect(),
            content: content_sha256(&cur.pack),
            sealed: kr,
        };
        if sc.pending.is_empty() && sc.own_broken.is_empty() && self.quiet.as_ref() == Some(&quiet)
        {
            // 什么都没变：冲突还是上一轮的。照样记一次同步成功，设备记录隔一阵写一次，设备
            // 列表重读（别的设备的记录会变）
            st.last_synced_ms = Some(now_ms());
            st.failing_since_ms = None;
            st.error = None;
            if now_ms().saturating_sub(st.device_record_ms) > DEVICE_RECORD_EVERY_MS {
                self.write_device(st, false).await?;
            }
            return Ok(Report {
                encrypted: kr,
                conflicts: self.last_report.conflicts.clone(),
                devices: self.devices().await?,
                skipped,
                ..Default::default()
            });
        }
        let heads = match Self::heads(&sc) {
            Ok(h) => h,
            Err(e @ RoundError::TooNew { .. }) => {
                if !st.device_record_paused {
                    self.write_device(st, true).await?;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        // 基准得是同步位置最新的祖先：不是的话（同步位置被重新上传过、keyring 换过）按两方比较
        let index = index_of(sc.ok.iter().map(|v| v.as_ref()).chain(base.as_ref()));
        let base = base.filter(|b| {
            heads
                .iter()
                .any(|h| ancestors(&index, &h.name.id()).contains(&b.name.id()))
        });
        let remote = self.fold(&heads, &sc.ok, base.as_ref());
        let local_snap = model::snapshot(&cur.pack).map_err(|e| RoundError::Local(e.msg()))?;
        let here = local_providers(&cur.pack);

        let me = self.settings.device_id.clone();
        let mut local = local_snap.clone();
        // 等登录的上游：这台的停用不算本机的改动，换回同步的样子再合并（登录了的这一轮也换回
        // 去，落地时就照同步的样子启用）
        let neutral: BTreeSet<String> = local
            .of_kind(ItemKind::Provider)
            .filter(|(id, it)| needs_login(id, it, &here))
            .map(|(id, _)| id.name.clone())
            .chain(st.held_login.iter().cloned())
            .collect();
        for name in &neutral {
            let id = ItemId::new(ItemKind::Provider, name.clone());
            let synced = base
                .as_ref()
                .and_then(|b| b.snap.items.get(&id))
                .or(remote.snap.items.get(&id))
                .cloned();
            if let (Some(it), Some(s)) = (local.items.get_mut(&id), synced) {
                copy_disabled(it, &s);
            }
        }
        // 别的设备处理掉了的冲突：照同步版本改
        let open_ids: BTreeSet<&str> = remote.conflicts.iter().map(|c| c.id.as_str()).collect();
        if let Some(b) = &base {
            for c in &b.meta.conflicts {
                if !open_ids.contains(c.id.as_str()) {
                    force(&mut local, &remote.snap, c);
                }
            }
        }
        // 自己参与的冲突：本机又改了就更新记录；改得和另一边一样了，冲突就过去了
        let mut open: Vec<Conflict> = Vec::new();
        let mut i_moved: Vec<Conflict> = Vec::new();
        for c in &remote.conflicts {
            let now = side_units(&local, c);
            let mut c = c.clone();
            if c.other.device == me {
                if now == c.shared.units {
                    continue;
                }
                if now != c.other.units {
                    c.other.units = now;
                    c.other.at_ms = cur.changed_ms;
                }
            } else if c.shared.device == me {
                if now == c.other.units {
                    continue;
                }
                if now != c.shared.units {
                    c.shared.units = now;
                    c.shared.at_ms = cur.changed_ms;
                    i_moved.push(c.clone());
                }
            }
            open.push(c);
        }

        // 这一轮要处理的冲突：从记录里拿掉（不再各留各的），本机按选的那一边改
        for d in decisions {
            let Some(pos) = open.iter().position(|c| c.id == d.id) else {
                continue;
            };
            if d.take == Take::Remote && !open[pos].remote_allowed() {
                return Err(RoundError::Local(msg!(
                    "sync.take_not_allowed" =>
                    "using the other side is not possible for this conflict: the result would \
                     not pass validation"
                )));
            }
            let c = open.remove(pos);
            let local_units = local.items.get(&c.item).map(|it| c.pick(&it.units));
            let (_, theirs) = c.mine(&me, local_units.as_ref());
            let theirs = theirs.clone();
            match d.take {
                Take::Local => {}
                Take::Remote => set_side(&mut local, &c, &theirs, &remote.snap),
                Take::Both => {
                    if !c.both_allowed() {
                        return Err(RoundError::Local(msg!(
                            "sync.both_not_allowed" =>
                            "keeping both is not possible for this conflict: one side deleted \
                             it, or it has no name"
                        )));
                    }
                    let name = both_name(&c.item, d.new_name.as_deref(), &theirs.device_name);
                    let id = ItemId::new(c.item.kind, name);
                    let taken =
                        local.items.contains_key(&id) || remote.snap.items.contains_key(&id);
                    let id = match (taken, d.new_name.is_some()) {
                        (false, _) => id,
                        (true, true) => {
                            return Err(RoundError::Local(msg!(
                                "sync.both_name_taken", name = id.name =>
                                "the name {name} is already taken"
                            )));
                        }
                        (true, false) => {
                            let mut all = local.clone();
                            all.items.extend(remote.snap.items.clone());
                            ItemId::new(c.item.kind, model::free_name(&all, c.item.kind, &id.name))
                        }
                    };
                    let mut copy = local
                        .items
                        .get(&c.item)
                        .or(remote.snap.items.get(&c.item))
                        .cloned()
                        .unwrap_or_default();
                    for (u, v) in theirs.units.iter().flatten() {
                        copy.units.insert(u.clone(), v.clone());
                    }
                    // 网关密钥的另一份是一把新的密钥：同一个值出现两次，客户端用的是哪一把
                    // 就说不清了。它也不是为哪个客户端生成的
                    if c.item.kind == ItemKind::Key
                        && let Some(Value::Object(all)) = copy.units.get_mut(unit::ALL)
                    {
                        all.insert("key".into(), Value::String(tw_config::generate_key()));
                        all.remove("client");
                    }
                    local.items.insert(id, copy);
                    local.relink();
                }
            }
        }

        let (mut hold_items, mut hold_units) = record::holds(&open);
        let ex = excluded(
            &self.settings.exclude,
            &[&local, &remote.snap]
                .into_iter()
                .chain(base.as_ref().map(|b| &b.snap))
                .collect::<Vec<_>>(),
        );
        hold_items.extend(ex.iter().cloned());
        for (o, _) in ORDERS {
            hold_items.insert(ItemId::new(*o, ""));
        }
        // 不加密时经同步停用的插件：本机启用了就放开
        st.held_plugins.retain(|id, _| {
            let item = local.items.get(&ItemId::new(ItemKind::Plugin, id.clone()));
            item.is_some_and(|it| it.units.get(unit::ENABLED) != Some(&Value::Bool(true)))
        });
        for id in st.held_plugins.keys() {
            hold_units.insert((
                ItemId::new(ItemKind::Plugin, id.clone()),
                unit::ENABLED.to_string(),
            ));
        }

        let base_objs = base.as_ref().map(|b| objects(&b.snap));
        let (lo, ro) = (objects(&local), objects(&remote.snap));
        let out = merge::merge(merge::Input {
            base: base_objs.as_ref(),
            local: &lo,
            remote: &ro,
            hold_items: &hold_items,
            hold_units: &hold_units,
            fieldwise: &fieldwise,
        });
        let mut target = rebuild(&out.local, &[&local, &remote.snap]);
        let mut shared = rebuild(&out.shared, &[&remote.snap, &local]);
        // 不同步的上游、随之不同步的项目：上传的里面是同步位置原有的，不是本机的
        for id in &ex {
            match remote.snap.items.get(id) {
                Some(it) => shared.items.insert(id.clone(), it.clone()),
                None => shared.items.remove(id),
            };
        }
        // 本机改了自己是「同步版本那一边」的冲突：同步版本跟着改
        for c in &i_moved {
            set_side(&mut shared, c, &c.shared, &local);
        }

        let side_me = |units: Option<BTreeMap<String, Value>>| Side {
            device: me.clone(),
            device_name: self.settings.device_name.clone(),
            at_ms: cur.changed_ms,
            units,
        };
        let side_remote = |units: Option<BTreeMap<String, Value>>| Side {
            device: remote.device.clone(),
            device_name: remote.device_name.clone(),
            at_ms: remote.at_ms,
            units,
        };
        let mut fresh: Vec<Conflict> = Vec::new();
        for f in &out.conflicts {
            fresh.push(found(
                f,
                (&remote.snap, side_remote(None)),
                (&local, side_me(None)),
            ));
        }
        // 已接管的客户端正在用的网关密钥：另一边删了，这边不删，作为冲突列出
        let pinned = self.host.pinned_keys();
        for (id, it) in local.of_kind(ItemKind::Key) {
            if pinned.contains(&id.name)
                && !target.items.contains_key(id)
                && !hold_items.contains(id)
            {
                target.items.insert(id.clone(), it.clone());
                fresh.push(Conflict {
                    id: record::new_id(),
                    item: id.clone(),
                    units: it.units.keys().cloned().collect(),
                    shared: side_remote(None),
                    other: side_me(Some(it.units.clone())),
                    reason: Some(Reason::KeyInUse),
                });
            }
        }
        // 顺序
        for (o, k) in ORDERS {
            let held = open.iter().chain(&fresh).any(|c| c.item.kind == *o);
            let m = order::merge(
                order_list(base.as_ref().map(|b| &b.snap), *o).as_deref(),
                &order_list(Some(&local), *o).unwrap_or_default(),
                &order_list(Some(&remote.snap), *o).unwrap_or_default(),
                &names_of(&target, *k),
                &names_of(&shared, *k),
                held,
            );
            set_order(&mut target, *o, m.local);
            set_order(&mut shared, *o, m.shared);
            if m.conflict {
                let id = ItemId::new(*o, "");
                fresh.push(Conflict {
                    id: record::new_id(),
                    units: vec![unit::ALL.to_string()],
                    shared: side_remote(remote.snap.items.get(&id).map(|i| i.units.clone())),
                    other: side_me(local.items.get(&id).map(|i| i.units.clone())),
                    item: id,
                    reason: None,
                });
            }
        }
        target.relink();
        shared.relink();
        // 没登录的上游在这台停用着
        st.held_login = hold_logins(&mut target, &here);
        let mut start = local.clone();
        hold_logins(&mut start, &here);

        // 合并的结果要过配置校验：过不了的那几项留本机的，作为冲突列出
        self.check_target(
            &cur.pack,
            &start,
            &local,
            &mut target,
            &mut shared,
            &remote,
            &mut fresh,
            &side_me,
            &side_remote,
        )?;

        // 不加密：经同步新增或脚本变了的插件在本机停用；上游的地址变了记一笔
        if !kr {
            for (id, it) in target
                .of_kind(ItemKind::Plugin)
                .map(|(a, b)| (a.clone(), b.clone()))
                .collect::<Vec<_>>()
            {
                let before = local.items.get(&id);
                let script = it.units.get(unit::SCRIPT);
                let changed = before.is_none_or(|b| b.units.get(unit::SCRIPT) != script);
                let from_remote = remote
                    .snap
                    .items
                    .get(&id)
                    .is_some_and(|r| r.units.get(unit::SCRIPT) == script);
                if changed && from_remote && it.units.get(unit::ENABLED) == Some(&Value::Bool(true))
                {
                    if let Some(t) = target.items.get_mut(&id) {
                        t.units
                            .insert(unit::ENABLED.to_string(), Value::Bool(false));
                    }
                    st.held_plugins
                        .insert(id.name.clone(), script.cloned().unwrap_or(Value::Null));
                }
            }
            for (id, it) in target.of_kind(ItemKind::Provider) {
                if let (Some(before), Some(to)) =
                    (local.items.get(id).and_then(base_url), base_url(it))
                    && before != to
                {
                    st.url_changed.insert(
                        id.name.clone(),
                        UrlChange {
                            from: before.to_string(),
                            to: to.to_string(),
                            device: Some(remote.device_name.clone()),
                            at_ms: remote.at_ms,
                        },
                    );
                }
            }
        }
        // 地址又被本机改过、上游没了：那条提示过去了
        st.url_changed.retain(|name, ch| {
            target
                .items
                .get(&ItemId::new(ItemKind::Provider, name.clone()))
                .and_then(base_url)
                == Some(ch.to.as_str())
        });

        let mut conflicts = open.clone();
        conflicts.extend(fresh);

        let mut report = Report {
            encrypted: kr,
            skipped,
            ..Default::default()
        };
        let mut content_after = quiet.content.clone();
        // 应用
        if model::normalized(&target) != model::normalized(&local_snap) {
            report.keys_changed = local_snap
                .of_kind(ItemKind::Key)
                .filter(|(id, it)| {
                    target.items.get(*id).map(|t| key_value(t)) != Some(key_value(it))
                })
                .map(|(id, _)| id.name.clone())
                .collect();
            let pack =
                model::materialize(&cur.pack, &target).map_err(|e| RoundError::Local(e.msg()))?;
            content_after = content_sha256(&pack);
            let from: Vec<String> = {
                let mut v: Vec<String> = remote
                    .heads
                    .iter()
                    .filter(|h| h.meta.device != me)
                    .map(|h| h.meta.device_name.clone())
                    .collect();
                v.dedup();
                v
            };
            let label = from.join(", ");
            match self.host.apply(pack, &cur.version, &label).await {
                Ok(()) => {}
                Err(ApplyError::Stale) => return Err(RoundError::Stale),
                Err(ApplyError::Rejected(m)) => return Err(RoundError::Local(m)),
            }
            report.applied_from = from;
        }
        // 上传
        let need_upload = heads.len() > 1
            || !sc.own_broken.is_empty()
            || model::normalized(&shared) != model::normalized(&remote.snap)
            || conflicts != remote.conflicts;
        let mut listing: BTreeSet<VersionName> = sc.names.iter().cloned().collect();
        let next_base = if need_upload {
            let seq = self.next_seq(&sc.names, &sc.all);
            let v = self
                .publish(&shared, seq, &heads, conflicts.clone())
                .await?;
            report.uploaded = Some(v.name.id());
            listing.insert(v.name.clone());
            let all: Vec<VersionName> = listing.iter().cloned().collect();
            let mut ok_all = sc.ok.clone();
            ok_all.push(Arc::new(v.clone()));
            let gone = self.prune(&all, &ok_all, &sc.skipped).await;
            // 这台设备自己写的、换了 keyring 之后解不开的：内容已经在新版本里了
            for n in &sc.own_broken {
                if !gone.contains(n) && self.store.delete(&n.path()).await.is_ok() {
                    self.cache.remove(n);
                }
            }
            listing.retain(|n| !gone.contains(n));
            v
        } else {
            (*heads[0]).clone()
        };
        self.save_base(&next_base, st)?;
        st.last_synced_ms = Some(now_ms());
        st.failing_since_ms = None;
        st.error = None;
        report.conflicts = conflicts;

        let changed = report.uploaded.is_some() || !report.applied_from.is_empty();
        if changed
            || st.device_record_paused
            || now_ms().saturating_sub(st.device_record_ms) > DEVICE_RECORD_EVERY_MS
        {
            self.write_device(st, false).await?;
        }
        report.devices = self.devices().await?;
        // 下一轮的捷径照这一轮**开始时**看到的算，加上这一轮自己做的：这一轮中途别的设备传上
        // 来的、本机被改的，下一轮都看得出来
        self.quiet = sc.pending.is_empty().then(|| Quiet {
            listing: listing.iter().map(|n| n.id()).collect(),
            content: content_after,
            sealed: kr,
        });
        Ok(report)
    }

    /// 合并出来的本机结果过一遍配置校验。过不了时一项一项试：从本机现在的样子（`start`）出发，
    /// 把变了的项目一个个换过去，换了就过不了的那一项留本机的，记成冲突（`invalid`），上传的
    /// 里面也留本机的（`mine`：这台停用着的上游是同步的样子）—— 不然别的设备收到的也是一份
    /// 过不了校验的配置。**先试别的、后试上游**：一个新上游用着另一边新加的出站代理，出站代理
    /// 先换过去，上游才过得了
    #[allow(clippy::too_many_arguments)]
    fn check_target(
        &self,
        local_pack: &Pack,
        start: &Snapshot,
        mine: &Snapshot,
        target: &mut Snapshot,
        shared: &mut Snapshot,
        remote: &Remote,
        fresh: &mut Vec<Conflict>,
        side_me: &dyn Fn(Option<BTreeMap<String, Value>>) -> Side,
        side_remote: &dyn Fn(Option<BTreeMap<String, Value>>) -> Side,
    ) -> Result<(), RoundError> {
        let valid = |s: &Snapshot| -> bool {
            model::materialize(local_pack, s)
                .ok()
                .is_some_and(|p| tw_config::try_parse(&p.config_yaml).is_ok())
        };
        if valid(target) {
            return Ok(());
        }
        let mut changed: Vec<ItemId> = start
            .items
            .keys()
            .chain(target.items.keys())
            .filter(|id| !id.kind.is_order() && start.items.get(*id) != target.items.get(*id))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // 新增和修改按引用的先后（被引用的先到位），删除倒过来（引用它的先走）。一遍下来还有
        // 过不了的，再来一遍：前面换过去的可能正是它缺的
        let rank = |id: &ItemId| -> u8 {
            let r = match id.kind {
                ItemKind::Provider => 1,
                ItemKind::Alias | ItemKind::Group => 2,
                ItemKind::Route => 3,
                ItemKind::Key | ItemKind::Global => 4,
                _ => 0,
            };
            if target.items.contains_key(id) {
                r
            } else {
                10 - r
            }
        };
        changed.sort_by_key(|id| rank(id));
        let mut t = start.clone();
        let fix_orders = |s: &mut Snapshot, from: &Snapshot| {
            for (o, k) in ORDERS {
                let names = names_of(s, *k);
                let mut list: Vec<String> = order_list(Some(from), *o)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|n| names.contains(n))
                    .collect();
                for n in &names {
                    if !list.contains(n) {
                        list.push(n.clone());
                    }
                }
                set_order(s, *o, list);
            }
            s.relink();
        };
        let mut left = changed;
        loop {
            let before = left.len();
            let mut still = Vec::new();
            for id in left {
                let mut next = t.clone();
                match target.items.get(&id) {
                    Some(it) => next.items.insert(id.clone(), it.clone()),
                    None => next.items.remove(&id),
                };
                fix_orders(&mut next, target);
                if valid(&next) {
                    t = next;
                } else {
                    still.push(id);
                }
            }
            left = still;
            if left.is_empty() || left.len() == before {
                break;
            }
        }
        for id in left {
            // 这一项留本机的
            match mine.items.get(&id) {
                Some(it) => shared.items.insert(id.clone(), it.clone()),
                None => shared.items.remove(&id),
            };
            let units: Vec<String> = mine
                .items
                .get(&id)
                .or(target.items.get(&id))
                .map(|it| it.units.keys().cloned().collect())
                .unwrap_or_default();
            fresh.retain(|c| c.item != id);
            fresh.push(Conflict {
                id: record::new_id(),
                item: id.clone(),
                units,
                shared: side_remote(remote.snap.items.get(&id).map(|i| i.units.clone())),
                other: side_me(mine.items.get(&id).map(|i| i.units.clone())),
                reason: Some(Reason::Invalid),
            });
        }
        fix_orders(&mut t, target);
        let before = shared.clone();
        fix_orders(shared, &before);
        *target = t;
        if !valid(target) {
            return Err(RoundError::Local(msg!(
                "sync.merge_invalid" =>
                "the merged configuration does not pass validation, so nothing was applied"
            )));
        }
        Ok(())
    }

    // ─────────────────────────────────────────────── 开启、加入

    /// 看一眼同步位置：写得进去吗，空的还是已经有数据，加没加密，有哪些设备（加密时给了
    /// 对的口令才列得出）
    pub async fn probe(store: &S, passphrase: Option<&str>) -> Probe {
        let r: u64 = rand::random();
        let path = format!("probe-{r:016x}");
        let writable = match store.put_new(&path, b"ThinkWatch sync probe").await {
            Ok(()) => store.delete(&path).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &writable {
            return Probe {
                writable: Err(e.clone()),
                existing: None,
            };
        }
        let versions = store.list(VERSIONS).await.unwrap_or_default();
        let keyring = store.get(KEYRING).await.ok().flatten();
        if versions.is_empty() && keyring.is_none() {
            return Probe {
                writable,
                existing: None,
            };
        }
        let opened = match (&keyring, passphrase) {
            (Some(b), Some(p)) => {
                let (b, p) = (b.clone(), p.to_string());
                tokio::task::spawn_blocking(move || Keyring::open(&b, &p).ok())
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        let encrypted = match (&keyring, &opened) {
            (None, _) => false,
            (Some(_), Some(k)) => !k.off(),
            (Some(b), None) => !keyring::marked_off(b),
        };
        let unlocked = !encrypted || opened.is_some();
        let mut devices = Vec::new();
        if unlocked {
            for e in store.list(DEVICES).await.unwrap_or_default() {
                if !layout::is_device_file(&e.name) {
                    continue;
                }
                if let Ok(Some(b)) = store.get(&format!("{DEVICES}/{}", e.name)).await
                    && let Some(rec) = DeviceRecord::open(&b, opened.as_ref(), encrypted)
                {
                    devices.push(rec);
                }
            }
            devices.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Probe {
            writable,
            existing: Some(Existing {
                encrypted,
                unlocked,
                devices,
            }),
        }
    }

    /// 同步位置空着时开启：加密的先建 keyring（**仅当不存在时**），然后以本机配置上传第一个
    /// 版本。已经有了版本（两台设备同时开启、后写的那一台）是 `Ok(false)`，转去加入。
    ///
    /// 只有 keyring、没有版本（之前开启加密时第一个版本没传上去）时，口令解得开就接着用它
    pub async fn create(&mut self) -> Result<bool, RoundError> {
        self.local.clear();
        let mut st = State {
            place: self.settings.place.clone(),
            ..Default::default()
        };
        let r = self.create_inner(&mut st).await;
        if matches!(r, Ok(true)) {
            self.commit(&st)?;
        }
        r
    }

    async fn create_inner(&mut self, st: &mut State) -> Result<bool, RoundError> {
        if self.settings.encrypt {
            let pass = self
                .settings
                .passphrase
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    RoundError::Local(tw_config::sync::SyncError::PassphraseMissing.msg())
                })?;
            let (sealed, k) = tokio::task::spawn_blocking({
                let pass = pass.clone();
                move || {
                    let k = Keyring::generate(0);
                    (k.seal(&pass), k)
                }
            })
            .await
            .map_err(local_err)?;
            if self.store.create_exclusive(KEYRING, &sealed).await? {
                st.keyring_gen = k.generation();
                let hash: [u8; 32] = Sha256::digest(&sealed).into();
                self.set_mode(Some(hash), Some(k), Some(sealed));
            } else {
                // 已经有一份：别的设备的（转去加入），或者上次开启到一半留下的
                self.security(st).await?;
                if !self.sealed {
                    return Ok(false);
                }
            }
        } else if self.store.get(KEYRING).await?.is_some() {
            return Ok(false);
        } else {
            self.set_mode(None, None, None);
        }
        if !self.listing().await?.is_empty() {
            return Ok(false);
        }
        self.publish_first(1, st).await?;
        self.quiet = None;
        Ok(true)
    }

    /// 同步位置的最新（几个头合并之后），写成一个配置包：加入时给导入预览比对。不含本机设置。
    /// 同步位置上还没有版本（只有 keyring）时是一份空的配置
    pub async fn latest(&mut self) -> Result<Pack, RoundError> {
        let mut st = self.local.state();
        match self.latest_remote(&mut st).await? {
            Some((remote, _)) => self.pack_of(&remote.snap),
            None => self.pack_of(&Snapshot::default()),
        }
    }

    fn pack_of(&self, snap: &Snapshot) -> Result<Pack, RoundError> {
        let empty = Pack {
            manifest: tw_pack::Manifest::new(PackKind::Full, PackOrigin::Sync),
            config_yaml: format!("version: {}\n", tw_config::SCHEMA_VERSION),
            plugins: Default::default(),
            client_prefs: None,
        };
        model::materialize(&empty, snap).map_err(|e| RoundError::Local(e.msg()))
    }

    /// 同步位置的最新。没有可用的版本是 `None`
    async fn latest_remote(
        &mut self,
        st: &mut State,
    ) -> Result<Option<(Remote, Scan)>, RoundError> {
        self.security(st).await?;
        let listed = self.listing().await?;
        let sc = self.scan(listed, st).await?;
        if sc.names.is_empty() {
            return Ok(None);
        }
        let heads = Self::heads(&sc)?;
        let base = self.base();
        let remote = self.fold(&heads, &sc.ok, base.as_ref());
        Ok(Some((remote, sc)))
    }

    /// 加入已有的同步。`merged` 是本机按导入的规则合并之后的完整配置包（「合并」）；`None` 是
    /// 用同步数据替换本机配置（新安装、或者选了「使用同步数据」）—— 那时本机已接管的客户端
    /// 正在用的网关密钥并入同步数据，不同步的上游留在本机。
    ///
    /// 本机先照结果改，再上传一个依据同步位置最新版本的新版本，作为这台设备的基准。两种方式
    /// 下，别的设备上登录着、这台没登录的上游都在这台停用着，**上传的是同步的样子**（启用着）。
    /// 同步位置上只有 keyring、还没有版本时，以本机配置上传第一个版本
    pub async fn join(&mut self, merged: Option<Pack>) -> Result<Report, RoundError> {
        self.local.clear();
        let mut st = State {
            place: self.settings.place.clone(),
            ..Default::default()
        };
        let r = self.join_inner(merged, &mut st).await;
        if r.is_ok() {
            self.commit(&st)?;
        }
        r
    }

    async fn join_inner(
        &mut self,
        merged: Option<Pack>,
        st: &mut State,
    ) -> Result<Report, RoundError> {
        let Some((remote, sc)) = self.latest_remote(st).await? else {
            let all = self.listing().await?;
            let seq = self.next_seq(&[], &all);
            self.publish_first(seq, st).await?;
            let report = Report {
                encrypted: self.sealed,
                devices: self.devices().await?,
                ..Default::default()
            };
            self.quiet = None;
            self.last_report = report.clone();
            return Ok(report);
        };
        let cur = self.host.current().map_err(RoundError::Local)?;
        let local = model::snapshot(&cur.pack).map_err(|e| RoundError::Local(e.msg()))?;
        let here = local_providers(&cur.pack);
        let ex = excluded(&self.settings.exclude, &[&local, &remote.snap]);
        let mut target = match &merged {
            Some(p) => model::snapshot(p).map_err(|e| RoundError::Local(e.msg()))?,
            None => {
                let mut t = remote.snap.clone();
                let pinned = self.host.pinned_keys();
                for (id, it) in local.of_kind(ItemKind::Key) {
                    if pinned.contains(&id.name) {
                        t.items.insert(id.clone(), it.clone());
                    }
                }
                t
            }
        };
        for id in &ex {
            match local.items.get(id) {
                Some(it) => target.items.insert(id.clone(), it.clone()),
                None => target.items.remove(id),
            };
        }
        let mut shared = target.clone();
        for id in &ex {
            match remote.snap.items.get(id) {
                Some(it) => shared.items.insert(id.clone(), it.clone()),
                None => shared.items.remove(id),
            };
        }
        // 没登录的上游：导入的规矩在本机停用了它，上传的照同步数据里的样子
        let waiting: Vec<ItemId> = shared
            .of_kind(ItemKind::Provider)
            .filter(|(id, it)| !ex.contains(*id) && needs_login(id, it, &here))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &waiting {
            if let (Some(it), Some(r)) = (shared.items.get_mut(id), remote.snap.items.get(id)) {
                copy_disabled(it, r);
            }
        }
        for (o, k) in ORDERS {
            let m = order::merge(
                order_list(Some(&remote.snap), *o).as_deref(),
                &order_list(Some(&target), *o)
                    .or_else(|| order_list(Some(&local), *o))
                    .unwrap_or_default(),
                &order_list(Some(&remote.snap), *o).unwrap_or_default(),
                &names_of(&target, *k),
                &names_of(&shared, *k),
                false,
            );
            set_order(&mut target, *o, m.local);
            set_order(&mut shared, *o, m.shared);
        }
        target.relink();
        shared.relink();
        // 本机这一份：没登录的停用着；同步的样子是启用着的，记成等登录
        hold_logins(&mut target, &here);
        st.held_login = waiting
            .iter()
            .filter(|id| shared.items.get(*id).is_some_and(|it| !disabled(it)))
            .map(|id| id.name.clone())
            .collect();
        // 不加密：经同步新来、或者脚本变了的插件在本机停用着，要在本机启用（同步一轮里是同一条）
        if !self.sealed {
            let ids: Vec<ItemId> = target
                .of_kind(ItemKind::Plugin)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                let script = target.items[&id].units.get(unit::SCRIPT).cloned();
                let changed = local
                    .items
                    .get(&id)
                    .is_none_or(|b| b.units.get(unit::SCRIPT) != script.as_ref());
                let from_remote = remote
                    .snap
                    .items
                    .get(&id)
                    .is_some_and(|r| r.units.get(unit::SCRIPT) == script.as_ref());
                let on = target.items[&id].units.get(unit::ENABLED) == Some(&Value::Bool(true));
                if changed && from_remote && on {
                    if let Some(t) = target.items.get_mut(&id) {
                        t.units
                            .insert(unit::ENABLED.to_string(), Value::Bool(false));
                    }
                    st.held_plugins
                        .insert(id.name.clone(), script.unwrap_or(Value::Null));
                }
            }
        }
        let mut report = Report {
            encrypted: self.sealed,
            skipped: sc.skipped.len() as u32,
            ..Default::default()
        };
        if model::normalized(&target) != model::normalized(&local) {
            report.keys_changed = local
                .of_kind(ItemKind::Key)
                .filter(|(id, it)| {
                    target.items.get(*id).map(|t| key_value(t)) != Some(key_value(it))
                })
                .map(|(id, _)| id.name.clone())
                .collect();
            let pack =
                model::materialize(&cur.pack, &target).map_err(|e| RoundError::Local(e.msg()))?;
            if tw_config::try_parse(&pack.config_yaml).is_err() {
                return Err(RoundError::Local(msg!(
                    "sync.merge_invalid" =>
                    "the merged configuration does not pass validation, so nothing was applied"
                )));
            }
            match self
                .host
                .apply(pack, &cur.version, &remote.device_name)
                .await
            {
                Ok(()) => {}
                Err(ApplyError::Stale) => return Err(RoundError::Stale),
                Err(ApplyError::Rejected(m)) => return Err(RoundError::Local(m)),
            }
            report.applied_from = vec![remote.device_name.clone()];
        }
        let seq = self.next_seq(&sc.names, &sc.all);
        let v = self
            .publish(&shared, seq, &remote.heads, remote.conflicts.clone())
            .await?;
        report.uploaded = Some(v.name.id());
        self.save_base(&v, st)?;
        st.last_synced_ms = Some(now_ms());
        st.failing_since_ms = None;
        st.error = None;
        self.write_device(st, false).await?;
        report.devices = self.devices().await?;
        report.conflicts = remote.conflicts;
        self.quiet = None;
        self.last_report = report.clone();
        Ok(report)
    }

    // ─────────────────────────────────────────────── 加密

    /// 之后开启加密：建 keyring（同步位置上有一份已关闭的就换掉它），以它重新上传最新版本，
    /// 删掉未加密的版本和设备记录。其他设备下次同步时要输入口令
    pub async fn enable_encryption(&mut self, passphrase: &str) -> Result<(), RoundError> {
        let mut st = self.local.state();
        let r = self.enable_inner(passphrase, &mut st).await;
        self.commit(&st)?;
        r
    }

    async fn enable_inner(&mut self, passphrase: &str, st: &mut State) -> Result<(), RoundError> {
        let existing = self.store.get(KEYRING).await?;
        let mut after = st.keyring_gen;
        if let Some(b) = &existing {
            // 给的口令、记着的口令，哪个解得开（第一个是给的那个）
            let tries: Vec<String> = [
                Some(passphrase.to_string()),
                self.settings.passphrase.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            let b2 = b.clone();
            let opened = tokio::task::spawn_blocking(move || {
                tries
                    .iter()
                    .enumerate()
                    .find_map(|(i, p)| Keyring::open(&b2, p).ok().map(|k| (i == 0, k)))
            })
            .await
            .map_err(local_err)?;
            match opened {
                // 已经开着：同一个口令就当作开好了
                Some((true, k)) if !k.off() => {
                    self.settings.encrypt = true;
                    self.settings.passphrase = Some(passphrase.to_string());
                    self.mode_of = None;
                    return Ok(());
                }
                Some((_, k)) if !k.off() => return Err(locked_new()),
                Some((_, k)) => after = after.max(k.generation()),
                None if keyring::marked_off(b) => {}
                None => return Err(locked_new()),
            }
        }
        let remote = self.latest_remote(st).await?;
        let all = self.listing().await?;
        let k = Keyring::generate(after);
        let sealed = k.seal(passphrase);
        if existing.is_some() {
            self.store.put(KEYRING, &sealed).await?;
        } else if !self.store.create_exclusive(KEYRING, &sealed).await? {
            return Err(locked_new());
        }
        st.keyring_gen = k.generation();
        let hash: [u8; 32] = Sha256::digest(&sealed).into();
        self.settings.encrypt = true;
        self.settings.passphrase = Some(passphrase.to_string());
        self.set_mode(Some(hash), Some(k), Some(sealed));
        self.reupload(remote, &all, st).await?;
        // 不加密的设备记录：加密的位置上不认它们，各设备下次同步时加密着重写
        for e in self.store.list(DEVICES).await? {
            if !layout::is_device_file(&e.name) {
                continue;
            }
            let path = format!("{DEVICES}/{}", e.name);
            if let Some(b) = self.store.get(&path).await?
                && !keyring::is_age(&b)
            {
                let _ = self.store.delete(&path).await;
            }
        }
        Ok(())
    }

    /// 关闭加密：口令要对。keyring 改写成「已关闭」（**不删**：别的设备靠它认出这是有口令的人
    /// 关的），重新上传不加密的最新版本，删掉加密过的版本
    pub async fn disable_encryption(&mut self, passphrase: &str) -> Result<(), RoundError> {
        let mut st = self.local.state();
        let r = self.disable_inner(passphrase, &mut st).await;
        self.commit(&st)?;
        r
    }

    async fn disable_inner(&mut self, passphrase: &str, st: &mut State) -> Result<(), RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            return Err(keyring_missing());
        };
        let pass = passphrase.to_string();
        let opened = tokio::task::spawn_blocking(move || Keyring::open(&bytes, &pass))
            .await
            .map_err(local_err)?
            .map_err(|e| RoundError::Locked(e.msg()))?;
        if opened.off() {
            self.settings.encrypt = false;
            self.mode_of = None;
            return Ok(());
        }
        self.settings.passphrase = Some(passphrase.to_string());
        self.settings.encrypt = true;
        self.mode_of = None;
        let remote = self.latest_remote(st).await?;
        let all = self.listing().await?;
        let off = opened.next(true);
        let sealed = off.seal(passphrase);
        self.store.put(KEYRING, &sealed).await?;
        st.keyring_gen = st.keyring_gen.max(off.generation());
        let hash: [u8; 32] = Sha256::digest(&sealed).into();
        self.settings.encrypt = false;
        self.set_mode(Some(hash), Some(off), Some(sealed));
        self.reupload(remote, &all, st).await
    }

    /// 以现在的加密设置重新上传最新版本，删掉别的版本（它们是另一种加密设置下写的）
    async fn reupload(
        &mut self,
        remote: Option<(Remote, Scan)>,
        names: &[VersionName],
        st: &mut State,
    ) -> Result<(), RoundError> {
        let v = match remote {
            Some((remote, sc)) => {
                let seq = self.next_seq(&sc.names, names);
                self.publish(&remote.snap, seq, &remote.heads, remote.conflicts.clone())
                    .await?
            }
            None => {
                let seq = self.next_seq(&[], names);
                return self.publish_first(seq, st).await.map(|_| ());
            }
        };
        for n in names {
            let _ = self.store.delete(&n.path()).await;
            self.cache.remove(n);
        }
        self.save_base(&v, st)?;
        st.last_synced_ms = Some(now_ms());
        self.write_device(st, false).await?;
        self.quiet = None;
        Ok(())
    }

    /// 更改同步口令：只重新加密 keyring，已有的版本不变。其他设备下次同步时要输入新口令
    pub async fn change_passphrase(&mut self, old: &str, new: &str) -> Result<(), RoundError> {
        let mut st = self.local.state();
        let Some(bytes) = self.store.get(KEYRING).await? else {
            return Err(RoundError::Local(msg!(
                "sync.not_encrypted" => "the sync location is not encrypted"
            )));
        };
        let (old, new2) = (old.to_string(), new.to_string());
        let (sealed, k) = tokio::task::spawn_blocking(move || {
            Keyring::open(&bytes, &old).map(|k| {
                let off = k.off();
                let k = k.next(off);
                (k.seal(&new2), k)
            })
        })
        .await
        .map_err(local_err)?
        .map_err(|e| RoundError::Locked(e.msg()))?;
        self.store.put(KEYRING, &sealed).await?;
        st.keyring_gen = st.keyring_gen.max(k.generation());
        let hash: [u8; 32] = Sha256::digest(&sealed).into();
        self.settings.passphrase = Some(new.to_string());
        self.set_mode(Some(hash), Some(k), Some(sealed));
        self.commit(&st)
    }

    /// 口令对不对得上同步位置的 keyring（别的设备改了口令、开了加密之后重新输入）。加密开着是
    /// `Ok(true)`；没有 keyring、或者是已关闭的是 `Ok(false)`
    pub async fn unlock(&mut self, passphrase: &str) -> Result<bool, RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            return Ok(false);
        };
        let pass = passphrase.to_string();
        let k = tokio::task::spawn_blocking(move || Keyring::open(&bytes, &pass))
            .await
            .map_err(local_err)?;
        match k {
            Ok(k) => {
                self.settings.passphrase = Some(passphrase.to_string());
                self.settings.encrypt = !k.off();
                self.mode_of = None;
                self.keyring = None;
                self.quiet = None;
                Ok(!k.off())
            }
            Err(e) => Err(RoundError::Locked(e.msg())),
        }
    }

    // ─────────────────────────────────────────────── 要人处理的状态

    /// 同步位置清空了（[`RoundError::Emptied`]）、keyring 不见了或者被换成旧的
    /// （[`RoundError::KeyringMissing`]）之后，由人选的「重新上传本机配置」：
    ///
    /// - 加密着时 keyring 放回去：这台设备记着的那一份（口令解得开的话），没有就新建一把 ——
    ///   之前的版本这时谁也解不开了，各设备跳过它们；
    /// - 同步位置上没有读得出来的版本时，以本机配置上传一个新版本；
    /// - 然后照常同步一轮
    pub async fn republish(&mut self) -> Result<Report, RoundError> {
        let mut st = self.local.state();
        let r = self.republish_inner(&mut st).await;
        self.commit(&st)?;
        r?;
        self.quiet = None;
        self.round().await
    }

    async fn republish_inner(&mut self, st: &mut State) -> Result<(), RoundError> {
        let pass = self.settings.passphrase.clone().filter(|p| !p.is_empty());
        let open = |b: Vec<u8>| {
            let pass = pass.clone();
            async move {
                let Some(p) = pass else {
                    return Err(KeyringError::WrongPassphrase);
                };
                tokio::task::spawn_blocking(move || Keyring::open(&b, &p))
                    .await
                    .unwrap_or(Err(KeyringError::Corrupt))
            }
        };
        let seen = st.keyring_gen;
        let usable = |k: &Keyring| !(k.off() && k.generation() < seen);
        let here = match self.store.get(KEYRING).await? {
            Some(b) => Some(open(b).await),
            None => None,
        };
        let fine = matches!(&here, Some(Ok(k)) if usable(k));
        if !fine && (self.settings.encrypt || st.keyring_gen > 0) {
            match &here {
                // 口令不对的那一份不是丢了：别的设备改了口令，要输入新口令
                Some(Err(KeyringError::WrongPassphrase)) => {
                    return Err(RoundError::Locked(KeyringError::WrongPassphrase.msg()));
                }
                Some(Err(KeyringError::TooNew)) => {
                    return Err(RoundError::TooNew {
                        core_version: String::new(),
                    });
                }
                _ => {}
            }
            let copy = match self.local.keyring() {
                Some(b) => match open(b.clone()).await {
                    Ok(k) if usable(&k) => Some(b),
                    _ => None,
                },
                None => None,
            };
            match (copy, &pass) {
                (Some(b), _) => self.store.put(KEYRING, &b).await?,
                (None, Some(p)) if self.settings.encrypt => {
                    let p = p.clone();
                    let k = Keyring::generate(seen);
                    st.keyring_gen = k.generation();
                    let sealed = tokio::task::spawn_blocking(move || k.seal(&p))
                        .await
                        .map_err(local_err)?;
                    self.store.put(KEYRING, &sealed).await?;
                }
                // 不加密、也没有记着的 keyring：当作从没加密过
                _ => st.keyring_gen = 0,
            }
        }
        self.mode_of = None;
        self.security(st).await?;
        st.empty_rounds = 0;
        st.empty_since_ms = None;
        let all = self.listing().await?;
        let sc = self.scan(all.clone(), st).await?;
        if sc.ok.is_empty() {
            let seq = self.next_seq(&[], &all);
            self.publish_first(seq, st).await?;
        }
        Ok(())
    }

    // ─────────────────────────────────────────────── 停止

    /// 停止同步：从同步位置移除本设备的设备记录；`purge` 时连全部版本、设备记录和 keyring 一起
    /// 删掉（其他设备随后发现同步位置空了，由人决定怎么办）。**只删认得出的同步文件**：用户
    /// 自己放进 `versions/`、`devices/` 的不动。本机的同步状态目录删掉
    pub async fn stop(&mut self, purge: bool) -> Result<(), RoundError> {
        let mine = self
            .store
            .delete(&layout::device_path(&self.settings.device_id))
            .await;
        if purge {
            for e in self.store.list(VERSIONS).await? {
                if VersionName::parse(&e.name).is_some() {
                    self.store.delete(&format!("{VERSIONS}/{}", e.name)).await?;
                }
            }
            for e in self.store.list(DEVICES).await? {
                if layout::is_device_file(&e.name) {
                    self.store.delete(&format!("{DEVICES}/{}", e.name)).await?;
                }
            }
            self.store.delete(KEYRING).await?;
        } else {
            mine?;
        }
        self.local.clear();
        self.cache.clear();
        self.quiet = None;
        self.mode_of = None;
        Ok(())
    }

    // ─────────────────────────────────────────────── 同步位置上的备份

    /// 同步位置上读得出来的版本（最新与上一个）：给备份列表
    pub async fn remote_versions(&mut self) -> Result<Vec<Arc<Version>>, RoundError> {
        let mut st = self.local.state();
        self.security(&mut st).await?;
        let listed = self.listing().await?;
        Ok(self.scan(listed, &mut st).await?.ok)
    }

    /// 同步位置上内容标识是这个的版本
    pub async fn remote_version(&mut self, content_id: &str) -> Result<Option<Pack>, RoundError> {
        Ok(self
            .remote_versions()
            .await?
            .into_iter()
            .find(|v| v.content == content_id)
            .map(|v| v.pack.clone()))
    }

    /// 以本机配置上传一个不依据别的版本的版本（同步位置空着时）
    async fn publish_first(&mut self, seq: u64, st: &mut State) -> Result<Version, RoundError> {
        let cur = self.host.current().map_err(RoundError::Local)?;
        let snap = model::snapshot(&cur.pack).map_err(|e| RoundError::Local(e.msg()))?;
        let mut shared = snap.clone();
        let ex = excluded(&self.settings.exclude, &[&snap]);
        shared.items.retain(|id, _| !ex.contains(id));
        for (o, k) in ORDERS {
            let names = names_of(&shared, *k);
            let list: Vec<String> = order_list(Some(&snap), *o)
                .unwrap_or_default()
                .into_iter()
                .filter(|n| names.contains(n))
                .collect();
            set_order(&mut shared, *o, list);
        }
        shared.relink();
        let v = self.publish(&shared, seq, &[], Vec::new()).await?;
        st.place.clone_from(&self.settings.place);
        self.save_base(&v, st)?;
        st.last_synced_ms = Some(now_ms());
        st.failing_since_ms = None;
        st.error = None;
        self.write_device(st, false).await?;
        Ok(v)
    }
}

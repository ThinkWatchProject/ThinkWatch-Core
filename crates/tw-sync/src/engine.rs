//! 一轮同步：看同步位置上有什么新版本，和本机合并，该应用的应用、该上传的上传。
//!
//! # 一轮里做什么
//!
//! 1. 列出 `versions/`，读出每个版本（加密的用版本密钥解开）。读不出来的当作尚未到达：最新
//!    那一层有没到的就等下一轮，不拿一半去合并。
//! 2. 找出所有的**头**（没有别的版本依据它的版本）。不止一个时（两台设备同时上传、云盘晚到
//!    的版本）先以它们共同的上一版本为基准两两合并，得到「同步位置的最新」。
//! 3. 以本机基准（上次同步的那一版）为基准，和本机当前配置三方合并（[`crate::merge`]）。
//!    冲突照常往下走：本机留本机的，上传的留同步位置的，记进冲突记录。
//! 4. 本机的结果和现在不一样就应用（经控制面的写入流程，来源记为同步）；上传的结果和同步
//!    位置上的不一样（或者合并了几个头、冲突记录变了）就上传一个新版本，然后删掉更早的。
//!
//! # 冲突记录随版本走
//!
//! 一处冲突在被处理之前，**两边各留各的**：参与的每台设备用自己的内容，同步版本里是先上传
//! 的那一边（[`Conflict::shared`]）。任一设备处理之后，冲突从记录里去掉、结果上传，别的设备
//! 看到记录没了，就照同步版本改成处理之后的样子。

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
use crate::keyring::{Keyring, KeyringError};
use crate::layout::{self, DEVICES, KEYRING, Listed, VERSIONS, VersionName};
use crate::local::{Local, State, UrlChange};
use crate::merge::{self, Objects};
use crate::order;
use crate::record::{self, Conflict, Meta, Reason, Side};
use crate::store::{Store, StoreError};

/// 设备记录多久没写就写一次（最后同步时间），即便这一轮什么都没变
const DEVICE_RECORD_EVERY_MS: u64 = 6 * 3600 * 1000;

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
    /// 同步位置的最新版本由较新的 Core 写，本机读不了：暂停
    TooNew { core_version: String },
    /// 最新的版本还没传完（云盘还在下载）：下一轮再来，不算失败
    NotArrived,
    /// 同步位置被清空了（别的设备停止同步并删除了同步数据）
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
            RoundError::Locked(m) | RoundError::Local(m) => m.clone(),
            RoundError::TooNew { core_version } => msg!(
                "sync.too_new", version = core_version =>
                "another device uses a newer version (Core {version}). Update ThinkWatch Lite to \
                 keep syncing"
            ),
            RoundError::NotArrived => msg!(
                "sync.not_arrived" =>
                "the latest version has not finished arriving at the sync location"
            ),
            RoundError::Emptied => msg!(
                "sync.emptied" =>
                "the sync location is empty: another device stopped syncing and deleted the \
                 sync data"
            ),
            RoundError::Stale => msg!(
                "sync.stale" => "the configuration changed during the sync; it will run again"
            ),
        }
    }
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
    /// 同步位置此刻加没加密。和设置不一样时（别的设备关了加密）调用方改设置
    pub encrypted: bool,
    /// 本机应用之后值变了、或者被删掉的网关密钥
    pub keys_changed: Vec<String>,
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

#[derive(Clone)]
enum Fetched {
    Ok(Arc<Version>),
    TooNew(String),
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

pub struct Engine<S: Store, H: Host> {
    pub store: S,
    pub host: H,
    local: Local,
    settings: Settings,
    /// 解开了的版本密钥，和 keyring 文件的哈希（文件变了要重新解）
    keyring: Option<(Keyring, [u8; 32])>,
    cache: BTreeMap<VersionName, Fetched>,
    /// 上一轮结束时同步位置上的名字和本机的内容：都没变就什么都不用做
    quiet: Option<(Vec<String>, String)>,
    last_report: Report,
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

/// 一个上游的地址（`conn.base_url`）
fn base_url(it: &Item) -> Option<&str> {
    it.units.get(unit::CONN)?.get("base_url")?.as_str()
}

/// 一把网关密钥的值
fn key_value(it: &Item) -> Option<&Value> {
    it.units.get(unit::ALL)?.get("key")
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
            cache: BTreeMap::new(),
            quiet: None,
            last_report: Report::default(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn set_settings(&mut self, s: Settings) {
        if s.passphrase != self.settings.passphrase || s.encrypt != self.settings.encrypt {
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

    // ─────────────────────────────────────────────── 读

    /// 拿到版本密钥。没加密是 `Ok(None)`
    async fn keyring(&mut self) -> Result<Option<&Keyring>, RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            self.keyring = None;
            return Ok(None);
        };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if self.keyring.as_ref().is_some_and(|(_, h)| *h == hash) {
            return Ok(self.keyring.as_ref().map(|(k, _)| k));
        }
        let Some(pass) = self.settings.passphrase.clone().filter(|p| !p.is_empty()) else {
            return Err(RoundError::Locked(msg!(
                "sync.locked_new" =>
                "the sync location is now encrypted. Enter the sync passphrase to keep syncing"
            )));
        };
        let k = tokio::task::spawn_blocking(move || Keyring::open(&bytes, &pass))
            .await
            .map_err(local_err)?;
        match k {
            Ok(k) => {
                self.cache.clear();
                self.keyring = Some((k, hash));
                Ok(self.keyring.as_ref().map(|(k, _)| k))
            }
            Err(KeyringError::WrongPassphrase) => Err(RoundError::Locked(msg!(
                "sync.passphrase_changed" =>
                "the sync passphrase was changed on another device. Enter the new passphrase"
            ))),
            Err(KeyringError::TooNew) => Err(RoundError::TooNew {
                core_version: String::new(),
            }),
            Err(KeyringError::Corrupt) => Err(RoundError::NotArrived),
        }
    }

    /// 读一个版本。读不出来（没传完、坏了、不是这把密钥加密的）是 `None`
    async fn fetch(&mut self, name: &VersionName) -> Result<Option<Fetched>, RoundError> {
        if let Some(f) = self.cache.get(name) {
            return Ok(Some(f.clone()));
        }
        let Some(bytes) = self.store.get(&name.path()).await? else {
            return Ok(None);
        };
        let identity = self.keyring.as_ref().map(|(k, _)| k.identity().clone());
        let decoded = tokio::task::spawn_blocking(move || {
            let unseal = match &identity {
                Some(id) => Unseal::Identity(id),
                None => Unseal::None,
            };
            let pack = tw_pack::read(&bytes, unseal)?;
            let snap = model::snapshot(&pack);
            Ok::<_, PackError>((pack, snap))
        })
        .await
        .map_err(local_err)?;
        let fetched = match decoded {
            Err(PackError::TooNew { core_version, .. }) => Fetched::TooNew(core_version),
            Err(_) => return Ok(None),
            Ok((pack, Err(e))) if e.is_too_new() => Fetched::TooNew(pack.manifest.core_version),
            Ok((_, Err(_))) => return Ok(None),
            Ok((pack, Ok(snap))) => {
                let Some(meta) = pack
                    .manifest
                    .sync
                    .clone()
                    .and_then(|v| serde_json::from_value::<Meta>(v).ok())
                else {
                    return Ok(None);
                };
                // 名字和内容对不上（被人挪过、改过名），或者不加密的版本校验和对不上：当作坏了
                if meta.seq != name.seq || layout::device_tag(&meta.device) != name.device {
                    return Ok(None);
                }
                if !pack.manifest.encrypted
                    && meta.sha256.as_deref() != Some(content_sha256(&pack).as_str())
                {
                    return Ok(None);
                }
                let content = pack.content_id();
                Fetched::Ok(Arc::new(Version {
                    name: name.clone(),
                    pack,
                    meta,
                    snap,
                    content,
                }))
            }
        };
        self.cache.insert(name.clone(), fetched.clone());
        Ok(Some(fetched))
    }

    fn base(&self) -> Option<Version> {
        let bytes = self.local.base()?;
        let pack = tw_pack::read(&bytes, Unseal::None).ok()?;
        let meta: Meta = serde_json::from_value(pack.manifest.sync.clone()?).ok()?;
        let snap = model::snapshot(&pack).ok()?;
        let name = VersionName::parse(&self.local.state().base?)?;
        let content = pack.content_id();
        Some(Version {
            name,
            pack,
            meta,
            snap,
            content,
        })
    }

    /// 同步位置上的版本：读得出来的、读不出来的名字
    async fn versions(
        &mut self,
    ) -> Result<
        (
            Vec<VersionName>,
            Vec<Arc<Version>>,
            Vec<(VersionName, String)>,
        ),
        RoundError,
    > {
        let mut names: Vec<VersionName> = self
            .store
            .list(VERSIONS)
            .await?
            .into_iter()
            .filter_map(|e| VersionName::parse(&e.name))
            .collect();
        names.sort();
        self.cache.retain(|n, _| names.contains(n));
        let mut ok = Vec::new();
        let mut too_new = Vec::new();
        for n in &names {
            match self.fetch(n).await? {
                Some(Fetched::Ok(v)) => ok.push(v),
                Some(Fetched::TooNew(cv)) => too_new.push((n.clone(), cv)),
                None => {}
            }
        }
        Ok((names, ok, too_new))
    }

    /// 同步位置上的设备记录。半年没更新的顺手删掉
    async fn devices(&mut self) -> Result<Vec<DeviceRecord>, RoundError> {
        let kr = self.keyring.as_ref().map(|(k, _)| k);
        let now = now_ms();
        let mut out = Vec::new();
        for e in self.store.list(DEVICES).await? {
            let path = format!("{DEVICES}/{}", e.name);
            let Some(bytes) = self.store.get(&path).await? else {
                continue;
            };
            let Some(rec) = DeviceRecord::open(&bytes, kr) else {
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

    async fn write_device(&mut self, paused: bool) -> Result<(), RoundError> {
        let rec = DeviceRecord {
            id: self.settings.device_id.clone(),
            name: self.settings.device_name.clone(),
            core_version: tw_pack::CORE_VERSION.to_string(),
            last_seen_ms: now_ms(),
            paused_update: paused,
            remote_core: self.settings.remote_core,
        };
        let kr = self.keyring.as_ref().map(|(k, _)| k);
        self.store
            .put(
                &layout::device_path(&self.settings.device_id),
                &rec.seal(kr),
            )
            .await?;
        let mut st = self.local.state();
        st.device_record_ms = rec.last_seen_ms;
        st.device_record_paused = paused;
        self.local.save_state(&st).map_err(RoundError::Local)?;
        Ok(())
    }

    // ─────────────────────────────────────────────── 合并几个头

    /// `a`、`b` 共同的上一版本
    fn common_ancestor(
        &self,
        a: &[Arc<Version>],
        b: &Version,
        all: &[Arc<Version>],
        base: Option<&Version>,
    ) -> Option<Version> {
        // 版本标识 → (上一版本, 它们的内容标识)
        let mut parents: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        for v in all.iter().map(|v| v.as_ref()).chain(base) {
            parents.insert(
                v.name.id(),
                (v.meta.parents.clone(), v.meta.parent_content.clone()),
            );
        }
        let mut content_of: BTreeMap<String, String> = BTreeMap::new();
        for (ps, cs) in parents.values() {
            for (p, c) in ps.iter().zip(cs) {
                content_of.insert(p.clone(), c.clone());
            }
        }
        let ancestors = |start: &str| -> BTreeSet<String> {
            let mut out = BTreeSet::new();
            let mut todo = vec![start.to_string()];
            while let Some(id) = todo.pop() {
                if !out.insert(id.clone()) {
                    continue;
                }
                if let Some((ps, _)) = parents.get(&id) {
                    todo.extend(ps.iter().cloned());
                }
            }
            out
        };
        let left: BTreeSet<String> = a.iter().flat_map(|v| ancestors(&v.name.id())).collect();
        let right = ancestors(&b.name.id());
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
        let content = content_of.get(&id)?;
        let pack = self.host.backup(content)?;
        let snap = model::snapshot(&pack).ok()?;
        Some(Version {
            name: VersionName::parse(&id)?,
            meta: Meta::default(),
            content: content.clone(),
            snap,
            pack,
        })
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
            sha256: None,
            conflicts,
        };
        let encrypted = self.keyring.is_some();
        if !encrypted {
            meta.sha256 = Some(content_sha256(&pack));
        }
        pack.manifest.sync = Some(serde_json::to_value(&meta).expect("plain data serializes"));
        let bytes = {
            let recipient = self.keyring.as_ref().map(|(k, _)| k.recipient());
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
        self.local.save_base(&bytes).map_err(RoundError::Local)?;
        st.base = Some(v.name.id());
        Ok(())
    }

    async fn prune(&mut self, names: &[VersionName], ok: &[Arc<Version>]) {
        let listed: Vec<Listed> = names
            .iter()
            .map(|n| Listed {
                name: n.clone(),
                parents: ok
                    .iter()
                    .find(|v| &v.name == n)
                    .map(|v| {
                        v.meta
                            .parents
                            .iter()
                            .filter_map(|p| VersionName::parse(p))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        for n in layout::to_delete(&listed) {
            let _ = self.store.delete(&n.path()).await;
            self.cache.remove(&n);
        }
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
        match self.round_inner(decisions).await {
            Ok(r) => {
                self.last_report = r.clone();
                Ok(r)
            }
            Err(e) => Err(e),
        }
    }

    async fn round_inner(&mut self, decisions: &[Decision]) -> Result<Report, RoundError> {
        let mut st = self.local.state();
        if st.place != self.settings.place && !st.place.is_empty() {
            // 换了同步位置：原来的基准是那个位置上的，作废
            self.local.clear();
            st = State {
                place: self.settings.place.clone(),
                ..Default::default()
            };
            self.local.save_state(&st).map_err(RoundError::Local)?;
            self.cache.clear();
        }
        st.place.clone_from(&self.settings.place);
        let base = self.base();
        let kr = self.keyring().await?.is_some();
        let (names, ok, too_new) = self.versions().await?;
        if names.is_empty() {
            if base.is_some() {
                return Err(RoundError::Emptied);
            }
            // 开启同步时上传第一个版本没成（中途断了）：这一轮补上
            self.publish_first().await?;
            return self.finish_quiet(Vec::new(), kr).await;
        }
        let cur = self.host.current().map_err(RoundError::Local)?;
        let listing: Vec<String> = names.iter().map(|n| n.id()).collect();
        let local_content = cur.pack.content_id();
        if self.quiet.as_ref() == Some(&(listing.clone(), local_content.clone())) {
            // 什么都没变：冲突还是上一轮的。照样记一次同步成功，设备记录隔一阵写一次，设备
            // 列表重读（别的设备的记录会变）
            st.last_synced_ms = Some(now_ms());
            st.failing_since_ms = None;
            st.error = None;
            self.local.save_state(&st).map_err(RoundError::Local)?;
            if now_ms().saturating_sub(st.device_record_ms) > DEVICE_RECORD_EVERY_MS {
                self.write_device(false).await?;
            }
            let r = Report {
                encrypted: kr,
                conflicts: self.last_report.conflicts.clone(),
                devices: self.devices().await?,
                ..Default::default()
            };
            return Ok(r);
        }
        // 头：没有别的版本依据它的版本
        let referenced: BTreeSet<String> = ok
            .iter()
            .flat_map(|v| v.meta.parents.iter().cloned())
            .collect();
        if let Some((_, cv)) = too_new.iter().find(|(n, _)| !referenced.contains(&n.id())) {
            if !st.device_record_paused {
                self.write_device(true).await?;
            }
            return Err(RoundError::TooNew {
                core_version: cv.clone(),
            });
        }
        let top_ok = ok.iter().map(|v| v.name.seq).max();
        let top_all = names.iter().map(|n| n.seq).max();
        if top_ok.is_none() || top_ok < top_all {
            return Err(RoundError::NotArrived);
        }
        let heads: Vec<Arc<Version>> = ok
            .iter()
            .filter(|v| !referenced.contains(&v.name.id()))
            .cloned()
            .collect();
        // 同步位置没有 keyring、版本却是加密的：keyring 还没传到
        if !kr && heads.iter().any(|h| h.pack.manifest.encrypted) {
            return Err(RoundError::NotArrived);
        }
        let remote = self.fold(&heads, &ok, base.as_ref());
        let local_snap = model::snapshot(&cur.pack).map_err(|e| RoundError::Local(e.msg()))?;

        let me = self.settings.device_id.clone();
        let mut local = local_snap.clone();
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

        // 合并的结果要过配置校验：过不了的那几项留本机的，作为冲突列出
        self.check_target(
            &cur.pack,
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
            ..Default::default()
        };
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
            || model::normalized(&shared) != model::normalized(&remote.snap)
            || conflicts != remote.conflicts;
        let next_base = if need_upload {
            let seq = names.iter().map(|n| n.seq).max().unwrap_or(0) + 1;
            let v = self
                .publish(&shared, seq, &heads, conflicts.clone())
                .await?;
            report.uploaded = Some(v.name.id());
            let mut all: Vec<VersionName> = names.clone();
            all.push(v.name.clone());
            let mut ok_all = ok.clone();
            ok_all.push(Arc::new(v.clone()));
            self.prune(&all, &ok_all).await;
            v
        } else {
            (*heads[0]).clone()
        };
        self.save_base(&next_base, &mut st)?;
        st.last_synced_ms = Some(now_ms());
        st.failing_since_ms = None;
        st.error = None;
        self.local.save_state(&st).map_err(RoundError::Local)?;
        report.conflicts = conflicts;

        let changed = report.uploaded.is_some() || !report.applied_from.is_empty();
        if changed
            || st.device_record_paused
            || now_ms().saturating_sub(st.device_record_ms) > DEVICE_RECORD_EVERY_MS
        {
            self.write_device(false).await?;
        }
        report.devices = self.devices().await?;
        // 这一轮之后同步位置上的样子
        let after: Vec<String> = self
            .store
            .list(VERSIONS)
            .await?
            .into_iter()
            .filter_map(|e| VersionName::parse(&e.name).map(|n| n.id()))
            .collect();
        let local_after = self
            .host
            .current()
            .map(|c| c.pack.content_id())
            .unwrap_or_default();
        self.quiet = Some((after, local_after));
        Ok(report)
    }

    async fn finish_quiet(
        &mut self,
        conflicts: Vec<Conflict>,
        encrypted: bool,
    ) -> Result<Report, RoundError> {
        let devices = self.devices().await?;
        let r = Report {
            conflicts,
            devices,
            encrypted,
            ..Default::default()
        };
        Ok(r)
    }

    /// 合并出来的本机结果过一遍配置校验。过不了时一项一项试：从本机现在的样子出发，把变了的
    /// 项目一个个换过去，换了就过不了的那一项留本机的，记成冲突（`invalid`），上传的里面也留
    /// 本机的 —— 不然别的设备收到的也是一份过不了校验的配置
    #[allow(clippy::too_many_arguments)]
    fn check_target(
        &self,
        local_pack: &Pack,
        local: &Snapshot,
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
        let changed: Vec<ItemId> = local
            .items
            .keys()
            .chain(target.items.keys())
            .filter(|id| !id.kind.is_order() && local.items.get(*id) != target.items.get(*id))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut t = local.clone();
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
        for id in changed {
            let mut next = t.clone();
            match target.items.get(&id) {
                Some(it) => next.items.insert(id.clone(), it.clone()),
                None => next.items.remove(&id),
            };
            fix_orders(&mut next, target);
            if valid(&next) {
                t = next;
                continue;
            }
            // 这一项留本机的
            match local.items.get(&id) {
                Some(it) => shared.items.insert(id.clone(), it.clone()),
                None => shared.items.remove(&id),
            };
            let units: Vec<String> = local
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
                other: side_me(local.items.get(&id).map(|i| i.units.clone())),
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
        let encrypted = keyring.is_some();
        let unlocked = !encrypted || opened.is_some();
        let mut devices = Vec::new();
        if unlocked {
            for e in store.list(DEVICES).await.unwrap_or_default() {
                if let Ok(Some(b)) = store.get(&format!("{DEVICES}/{}", e.name)).await
                    && let Some(rec) = DeviceRecord::open(&b, opened.as_ref())
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

    /// 同步位置空着时开启：加密的先建 keyring（**仅当不存在时**：两台设备同时开启时后写的
    /// 那一台拿到 `Ok(false)`，转去加入），然后以本机配置上传第一个版本
    pub async fn create(&mut self) -> Result<bool, RoundError> {
        if self.settings.encrypt {
            let pass = self
                .settings
                .passphrase
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    RoundError::Local(tw_config::sync::SyncError::PassphraseMissing.msg())
                })?;
            let k = Keyring::generate();
            let sealed = tokio::task::spawn_blocking({
                let pass = pass.clone();
                move || (k.seal(&pass), k)
            })
            .await
            .map_err(local_err)?;
            let (sealed, k) = sealed;
            if !self.store.create_exclusive(KEYRING, &sealed).await? {
                return Ok(false);
            }
            let hash: [u8; 32] = Sha256::digest(&sealed).into();
            self.keyring = Some((k, hash));
        } else if self.store.get(KEYRING).await?.is_some() {
            return Ok(false);
        }
        if !self.store.list(VERSIONS).await?.is_empty() {
            return Ok(false);
        }
        self.local.clear();
        let st = State {
            place: self.settings.place.clone(),
            ..Default::default()
        };
        self.local.save_state(&st).map_err(RoundError::Local)?;
        self.publish_first().await?;
        self.quiet = None;
        Ok(true)
    }

    /// 同步位置的最新（几个头合并之后），写成一个配置包：加入时给导入预览比对。不含本机设置
    pub async fn latest(&mut self) -> Result<Pack, RoundError> {
        let remote = self.latest_remote().await?.0;
        self.pack_of(&remote.snap)
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

    async fn latest_remote(
        &mut self,
    ) -> Result<(Remote, Vec<VersionName>, Vec<Arc<Version>>), RoundError> {
        self.keyring().await?;
        let (names, ok, too_new) = self.versions().await?;
        let referenced: BTreeSet<String> = ok
            .iter()
            .flat_map(|v| v.meta.parents.iter().cloned())
            .collect();
        if let Some((_, cv)) = too_new.iter().find(|(n, _)| !referenced.contains(&n.id())) {
            return Err(RoundError::TooNew {
                core_version: cv.clone(),
            });
        }
        let heads: Vec<Arc<Version>> = ok
            .iter()
            .filter(|v| !referenced.contains(&v.name.id()))
            .cloned()
            .collect();
        let top_ok = ok.iter().map(|v| v.name.seq).max();
        if heads.is_empty() || top_ok < names.iter().map(|n| n.seq).max() {
            return Err(RoundError::NotArrived);
        }
        let base = self.base();
        Ok((self.fold(&heads, &ok, base.as_ref()), names, ok))
    }

    /// 加入已有的同步。`merged` 是本机按导入的规则合并之后的完整配置包（「合并」）；`None` 是
    /// 用同步数据替换本机配置（新安装、或者选了「使用同步数据」）—— 那时本机已接管的客户端
    /// 正在用的网关密钥并入同步数据，不同步的上游留在本机。
    ///
    /// 本机先照结果改，再上传一个依据同步位置最新版本的新版本，作为这台设备的基准
    pub async fn join(&mut self, merged: Option<Pack>) -> Result<Report, RoundError> {
        let (remote, names, _) = self.latest_remote().await?;
        let cur = self.host.current().map_err(RoundError::Local)?;
        let local = model::snapshot(&cur.pack).map_err(|e| RoundError::Local(e.msg()))?;
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
        // 不加密：经同步新来、或者脚本变了的插件在本机停用着，要在本机启用（同步一轮里是同一条）
        let mut held = BTreeMap::new();
        if self.keyring.is_none() {
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
                    held.insert(id.name.clone(), script.unwrap_or(Value::Null));
                }
            }
        }
        let mut report = Report {
            encrypted: self.keyring.is_some(),
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
        let seq = names.iter().map(|n| n.seq).max().unwrap_or(0) + 1;
        let v = self
            .publish(&shared, seq, &remote.heads, remote.conflicts.clone())
            .await?;
        report.uploaded = Some(v.name.id());
        let mut st = State {
            place: self.settings.place.clone(),
            held_plugins: held,
            ..Default::default()
        };
        self.save_base(&v, &mut st)?;
        st.last_synced_ms = Some(now_ms());
        st.failing_since_ms = None;
        st.error = None;
        self.local.save_state(&st).map_err(RoundError::Local)?;
        self.write_device(false).await?;
        report.devices = self.devices().await?;
        report.conflicts = remote.conflicts;
        self.quiet = None;
        self.last_report = report.clone();
        Ok(report)
    }

    // ─────────────────────────────────────────────── 加密

    /// 之后开启加密：建 keyring，以它重新上传最新版本，删掉未加密的版本。其他设备下次同步时
    /// 要输入口令
    pub async fn enable_encryption(&mut self, passphrase: &str) -> Result<(), RoundError> {
        let (remote, names, _) = self.latest_remote().await?;
        if self.keyring.is_some() {
            return Ok(());
        }
        let k = Keyring::generate();
        let sealed = k.seal(passphrase);
        if !self.store.create_exclusive(KEYRING, &sealed).await? {
            return Err(RoundError::Locked(msg!(
                "sync.locked_new" =>
                "the sync location is now encrypted. Enter the sync passphrase to keep syncing"
            )));
        }
        let hash: [u8; 32] = Sha256::digest(&sealed).into();
        self.keyring = Some((k, hash));
        self.settings.encrypt = true;
        self.settings.passphrase = Some(passphrase.to_string());
        self.reupload(remote, &names).await
    }

    /// 关闭加密：口令要对。重新上传不加密的最新版本，删掉 keyring 和加密过的版本
    pub async fn disable_encryption(&mut self, passphrase: &str) -> Result<(), RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            self.keyring = None;
            return Ok(());
        };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let pass = passphrase.to_string();
        let opened = tokio::task::spawn_blocking(move || Keyring::open(&bytes, &pass))
            .await
            .map_err(local_err)?
            .map_err(|e| RoundError::Locked(e.msg()))?;
        self.settings.passphrase = Some(passphrase.to_string());
        self.keyring = Some((opened, hash));
        let (remote, names, _) = self.latest_remote().await?;
        // 之后写的一律不加密
        self.keyring = None;
        self.settings.encrypt = false;
        self.reupload(remote, &names).await?;
        self.store.delete(KEYRING).await?;
        Ok(())
    }

    /// 以现在的加密设置重新上传最新版本，删掉别的版本（它们是另一种加密设置下写的）
    async fn reupload(&mut self, remote: Remote, names: &[VersionName]) -> Result<(), RoundError> {
        let seq = names.iter().map(|n| n.seq).max().unwrap_or(0) + 1;
        let v = self
            .publish(&remote.snap, seq, &remote.heads, remote.conflicts.clone())
            .await?;
        for n in names {
            let _ = self.store.delete(&n.path()).await;
            self.cache.remove(n);
        }
        let mut st = self.local.state();
        self.save_base(&v, &mut st)?;
        st.last_synced_ms = Some(now_ms());
        self.local.save_state(&st).map_err(RoundError::Local)?;
        self.write_device(false).await?;
        self.quiet = None;
        Ok(())
    }

    /// 更改同步口令：只重新加密 keyring，已有的版本不变。其他设备下次同步时要输入新口令
    pub async fn change_passphrase(&mut self, old: &str, new: &str) -> Result<(), RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            return Err(RoundError::Local(msg!(
                "sync.not_encrypted" => "the sync location is not encrypted"
            )));
        };
        let (old, new2) = (old.to_string(), new.to_string());
        let sealed = tokio::task::spawn_blocking(move || {
            Keyring::open(&bytes, &old).map(|k| (k.seal(&new2), k))
        })
        .await
        .map_err(local_err)?
        .map_err(|e| RoundError::Locked(e.msg()))?;
        let (sealed, k) = sealed;
        self.store.put(KEYRING, &sealed).await?;
        let hash: [u8; 32] = Sha256::digest(&sealed).into();
        self.keyring = Some((k, hash));
        self.settings.passphrase = Some(new.to_string());
        Ok(())
    }

    /// 口令对不对得上同步位置的 keyring（别的设备改了口令之后重新输入）。没加密是 `Ok(false)`
    pub async fn unlock(&mut self, passphrase: &str) -> Result<bool, RoundError> {
        let Some(bytes) = self.store.get(KEYRING).await? else {
            return Ok(false);
        };
        let pass = passphrase.to_string();
        let k = tokio::task::spawn_blocking(move || Keyring::open(&bytes, &pass))
            .await
            .map_err(local_err)?;
        match k {
            Ok(_) => {
                self.settings.passphrase = Some(passphrase.to_string());
                self.settings.encrypt = true;
                self.keyring = None;
                self.quiet = None;
                Ok(true)
            }
            Err(e) => Err(RoundError::Locked(e.msg())),
        }
    }

    // ─────────────────────────────────────────────── 停止

    /// 停止同步：从同步位置移除本设备的设备记录；`purge` 时连全部版本、设备记录和 keyring 一起
    /// 删掉（其他设备随后发现同步位置空了，停止同步）。本机的同步状态目录删掉
    pub async fn stop(&mut self, purge: bool) -> Result<(), RoundError> {
        let mine = self
            .store
            .delete(&layout::device_path(&self.settings.device_id))
            .await;
        if purge {
            for e in self.store.list(VERSIONS).await? {
                self.store.delete(&format!("{VERSIONS}/{}", e.name)).await?;
            }
            for e in self.store.list(DEVICES).await? {
                self.store.delete(&format!("{DEVICES}/{}", e.name)).await?;
            }
            self.store.delete(KEYRING).await?;
        } else {
            mine?;
        }
        self.local.clear();
        self.cache.clear();
        self.quiet = None;
        Ok(())
    }

    // ─────────────────────────────────────────────── 同步位置上的备份

    /// 同步位置上读得出来的版本（最新与上一个）：给备份列表
    pub async fn remote_versions(&mut self) -> Result<Vec<Arc<Version>>, RoundError> {
        self.keyring().await?;
        let (_, ok, _) = self.versions().await?;
        Ok(ok)
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

    /// 同步位置空着：以本机配置上传第一个版本（加密时先建 keyring）
    async fn publish_first(&mut self) -> Result<Version, RoundError> {
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
        let v = self.publish(&shared, 1, &[], Vec::new()).await?;
        let mut st = self.local.state();
        st.place.clone_from(&self.settings.place);
        self.save_base(&v, &mut st)?;
        st.last_synced_ms = Some(now_ms());
        self.local.save_state(&st).map_err(RoundError::Local)?;
        self.write_device(false).await?;
        Ok(v)
    }
}

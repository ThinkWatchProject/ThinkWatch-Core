//! 配置包、导入导出、本机备份与恢复、Lite 的通用偏好、固定密钥。
//!
//! 格式、项目的划分、导入和恢复的规矩都在 `tw-pack`；这里把它们接到控制面上：读磁盘上此刻
//! 的样子、写插件文件和配置（同一条写入路径：核版本、先校验再写、留备份版本、换入）、发事件。
//!
//! # 写的顺序
//!
//! 和插件页一样**先写文件、再写配置**，攥着 `Plugins::edits`：插件文件和通用偏好先落盘，配置
//! 一换入网关就照它重读插件；配置没写成（版本对不上、校验没过），刚写的文件按写之前的样子
//! 还原。
//!
//! # 远程连接
//!
//! Lite 的通用偏好经远程连接时在 Lite 那边：导出用请求里带来的那一份，导入、恢复不写 core
//! 这边的偏好（导入预览把包里的偏好交给 Lite，由它在本机应用）。
//!
//! # 安全模式
//!
//! 备份的几个端点在安全模式下照样可用：配置读不进来时，恢复把那一版整份写回去，只有控制面
//! 的钥匙换成现在这一把（[`tw_pack::restore::whole`]）。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine;
use tw_api::ep;
use tw_config::history::Origin;
use tw_pack::model::{self, ItemId, ItemKind};
use tw_pack::{Pack, PackError, PackOrigin, Seal, Unseal};
use tw_types::{Msg, msg};

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail, unreadable_config};

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::ConfigItems, items)
        .at(ep::ExportConfig, export)
        .at(ep::ImportPreview, import_preview)
        .at(ep::ImportConfig, import)
        .at(ep::Backups, backups)
        .at(ep::BackupFile, backup_file)
        .at(ep::RestorePreview, restore_preview)
        .at(ep::Restore, restore)
        .at(ep::GetClientPrefs, get_prefs)
        .at(ep::PutClientPrefs, put_prefs)
        .at(ep::PinKeys, pin_keys)
}

// ─────────────────────────────────────────────────────────── 同步位置上的版本

/// 一个装箱的 future。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 同步位置上的一个版本。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteVersionInfo {
    /// [`Pack::content_id`]：和本机同一份内容的版本靠它合成一行
    pub content_id: String,
    pub at_ms: u64,
    /// 哪台设备上传的
    pub device_name: String,
    pub core_version: String,
    pub counts: tw_pack::Counts,
}

/// 同步位置上的备份版本。**同步那一路实现并装上**（[`crate::ConfigManager::set_remote_backups`]），
/// 没开同步时没有。
pub trait RemoteBackups: Send + Sync {
    /// 同步位置上留着的版本。连不上就是空的：列表照样列本机的
    fn list(&self) -> BoxFuture<'_, Vec<RemoteVersionInfo>>;
    /// 取一个版本（已经解开）
    fn fetch<'a>(&'a self, content_id: &'a str) -> BoxFuture<'a, Result<Pack, Msg>>;
    /// 恢复之后会同步到几台别的设备
    fn other_devices(&self) -> BoxFuture<'_, u32> {
        Box::pin(async { 0 })
    }
}

/// 同步位置上的版本在 `BackupEntry.id` 里写成这样：`sync-<content_id>`
const SYNC_ID: &str = "sync-";

// ─────────────────────────────────────────────────────────── 插件的拆法

/// 插件源码的拆法：代码和数据分开（`tw_gateway::plugin::source`）。**进程里装一次**，
/// `ConfigManager::new` 和命令行都会调，装过了什么都不做
pub fn install_codec() {
    tw_pack::model::install_codec(Box::new(Codec));
}

struct Codec;

impl tw_pack::model::PluginCodec for Codec {
    fn split(&self, source: &str) -> Option<(String, serde_json::Value)> {
        tw_gateway::plugin::source::split_data(source)
    }
    fn join(&self, source: &str, data: &serde_json::Value) -> Option<String> {
        tw_gateway::plugin::source::join_data(source, data)
    }
    fn shipped(&self, id: &str) -> Option<String> {
        tw_gateway::plugin::defaults::ALL
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s.to_string())
    }
    fn needs_approval(&self, source: &str) -> bool {
        tw_gateway::plugin::source::steers_declared(source.as_bytes())
    }
}

// ─────────────────────────────────────────────────────────── 换成线上的样子

pub(crate) fn kind_to_api(k: ItemKind) -> tw_api::ItemKind {
    use tw_api::ItemKind as A;
    match k {
        ItemKind::Provider => A::Provider,
        ItemKind::Proxy => A::Proxy,
        ItemKind::Key => A::Key,
        ItemKind::Alias => A::Alias,
        ItemKind::Group => A::Group,
        ItemKind::Route => A::Route,
        ItemKind::PriceSheet => A::PriceSheet,
        ItemKind::SecurityRule => A::SecurityRule,
        ItemKind::Plugin => A::Plugin,
        ItemKind::ProviderOrder => A::ProviderOrder,
        ItemKind::RouteOrder => A::RouteOrder,
        ItemKind::PluginOrder => A::PluginOrder,
        ItemKind::Global => A::Global,
        ItemKind::ClientPrefs => A::ClientPrefs,
    }
}

pub(crate) fn kind_from_api(k: tw_api::ItemKind) -> ItemKind {
    // 两边的词一样
    ItemKind::from_slug(k.slug()).expect("the two kinds of item share their words")
}

pub(crate) fn item_ref(id: &ItemId) -> tw_api::ItemRef {
    tw_api::ItemRef {
        kind: kind_to_api(id.kind),
        name: id.name.clone(),
    }
}

pub(crate) fn item_id(r: &tw_api::ItemRef) -> ItemId {
    ItemId::new(kind_from_api(r.kind), r.name.clone())
}

fn origin_to_api(o: PackOrigin) -> tw_api::PackOrigin {
    tw_api::PackOrigin::from_slug(o.slug()).unwrap_or(tw_api::PackOrigin::Other)
}

fn counts(c: tw_pack::Counts) -> tw_api::PackCounts {
    tw_api::PackCounts {
        providers: c.providers,
        proxies: c.proxies,
        clients: c.clients,
        aliases: c.aliases,
        groups: c.groups,
        routes: c.routes,
        price_sheets: c.price_sheets,
        security_rules: c.security_rules,
        plugins: c.plugins,
    }
}

fn pack_info(m: &tw_pack::Manifest) -> tw_api::PackInfo {
    tw_api::PackInfo {
        kind: match m.kind {
            tw_pack::PackKind::Full => tw_api::PackKind::Full,
            tw_pack::PackKind::Partial => tw_api::PackKind::Partial,
        },
        created_at_ms: m.created_at_ms,
        core_version: m.core_version.clone(),
        secrets: m.secrets,
        encrypted: m.encrypted,
        origin: origin_to_api(m.origin),
        counts: counts(m.counts),
    }
}

// ─────────────────────────────────────────────────────────── 失败

fn bad_pack(e: PackError) -> Fail {
    fail(StatusCode::BAD_REQUEST, e.msg())
}

/// 配置（本机的、包里的）拆不开。**由更新的版本写的**说成「太新」
fn model_fail(e: model::ModelError, pack: Option<&Pack>) -> Fail {
    if let Some(p) = pack
        && e.is_too_new()
    {
        return bad_pack(PackError::TooNew {
            format: p.manifest.format,
            core_version: p.manifest.core_version.clone(),
        });
    }
    match e {
        model::ModelError::Config(r) => apply_fail(ApplyError::Rejected(*r)),
        other => fail(StatusCode::UNPROCESSABLE_ENTITY, other.msg()),
    }
}

fn io_fail(path: &std::path::Path, e: std::io::Error) -> Fail {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        msg!(
            "backup.write_failed", path = path.display(), detail = e =>
            "{path} could not be written: {detail}"
        ),
    )
}

fn backup_fail(e: tw_pack::backup::BackupError) -> Fail {
    let code = match &e {
        tw_pack::backup::BackupError::NotFound { .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    fail(code, e.msg())
}

fn decode(b64: &str) -> Result<Vec<u8>, Fail> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| {
            fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "pack.not_base64", detail = e =>
                    "The file content is not valid base64: {detail}"
                ),
            )
        })
}

/// 口令：至少 8 个字符
fn passphrase(p: Option<&str>) -> Result<Option<&str>, Fail> {
    match p {
        None => Ok(None),
        Some(p) if p.chars().count() >= tw_pack::MIN_PASSPHRASE => Ok(Some(p)),
        Some(_) => Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "pack.passphrase_short", min = tw_pack::MIN_PASSPHRASE =>
                "The passphrase needs at least {min} characters."
            ),
        )),
    }
}

fn open_pack(data_b64: &str, pass: Option<&str>) -> Result<Pack, Fail> {
    let bytes = decode(data_b64)?;
    let unseal = match pass {
        Some(p) => Unseal::Passphrase(p),
        None => Unseal::None,
    };
    tw_pack::read(&bytes, unseal).map_err(bad_pack)
}

fn file_of(pack: &Pack, pass: Option<&str>) -> Result<(String, String), Fail> {
    let seal = match pass {
        Some(p) => Seal::Passphrase(p),
        None => Seal::None,
    };
    let bytes = tw_pack::write(pack, seal).map_err(bad_pack)?;
    Ok((
        tw_pack::export::file_name(pack.manifest.created_at_ms),
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

// ─────────────────────────────────────────────────────────── 此刻的样子

/// 磁盘上此刻的配置（原文和版本）连同它的插件、通用偏好
fn current(s: &ControlState, origin: PackOrigin) -> Result<(tw_config::Loaded, Pack), Fail> {
    let cur = s.cfg.current().map_err(unreadable_config)?;
    let pack = tw_pack::disk::current(s.config_path(), &cur.text, origin);
    Ok((cur, pack))
}

/// 通用偏好由 core 存着吗：经本机连接是，经远程连接不是（在 Lite 那边）
fn core_keeps_prefs() -> bool {
    !crate::remote::is_remote()
}

/// 写下去：插件文件、通用偏好、配置，**按这个顺序**，配置没写成就把前两样还原。之后报
/// 网关密钥换了、偏好变了
async fn land(
    s: &ControlState,
    before: &Pack,
    after: &Pack,
    base_version: &str,
    origin: Origin,
    changed_keys: Vec<String>,
) -> Result<String, Fail> {
    let path = s.config_path().to_path_buf();
    let _edit = s.gateway.plugins.edits.lock().await;
    let undo =
        tw_pack::disk::write_plugins(&path, &before.plugins, &after.plugins).map_err(|e| {
            io_fail(
                &tw_config::plugins::dir_in(&tw_pack::disk::dir_of(&path)),
                e,
            )
        })?;
    let prefs_changed = core_keeps_prefs() && after.client_prefs != before.client_prefs;
    if prefs_changed && let Err(e) = tw_pack::disk::write_prefs(&path, after.client_prefs.as_ref())
    {
        undo.restore();
        return Err(io_fail(&tw_pack::disk::prefs_path(&path), e));
    }
    let written = s
        .cfg
        .write(&after.config_yaml, Some(base_version), origin)
        .await;
    let version = match written {
        Ok(v) => v,
        Err(e) => {
            undo.restore();
            if prefs_changed {
                let _ = tw_pack::disk::write_prefs(&path, before.client_prefs.as_ref());
            }
            return Err(apply_fail(e));
        }
    };
    if prefs_changed {
        s.bus().emit(tw_api::Event::ClientPrefsChanged {
            id: s.bus().next_id(),
            at_ms: crate::config::now_ms(),
        });
    }
    if !changed_keys.is_empty() {
        s.bus().emit(tw_api::Event::GatewayKeysChanged {
            id: s.bus().next_id(),
            names: changed_keys,
            at_ms: crate::config::now_ms(),
        });
    }
    Ok(version)
}

// ─────────────────────────────────────────────────────────── 项目

async fn items(State(s): State<ControlState>) -> Result<Json<Vec<tw_api::ItemNode>>, Fail> {
    let (_, pack) = current(&s, PackOrigin::Export)?;
    let snap = model::snapshot(&pack).map_err(|e| model_fail(e, None))?;
    Ok(Json(
        snap.items
            .iter()
            .filter(|(id, _)| !id.kind.is_order())
            .filter(|(id, _)| core_keeps_prefs() || id.kind != ItemKind::ClientPrefs)
            .map(|(id, item)| tw_api::ItemNode {
                item: item_ref(id),
                refs: item
                    .refs
                    .iter()
                    .filter(|r| !r.kind.is_order())
                    .map(item_ref)
                    .collect(),
            })
            .collect(),
    ))
}

// ─────────────────────────────────────────────────────────── 导出

async fn export(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ExportRequest>,
) -> Result<Json<tw_api::ExportedFile>, Fail> {
    let pass = passphrase(req.passphrase.as_deref())?;
    let (_, mut local) = current(&s, PackOrigin::Export)?;
    if !core_keeps_prefs() {
        local.client_prefs = req.client_prefs.clone();
    }
    let scope = match &req.scope {
        tw_api::ExportScope::All => None,
        tw_api::ExportScope::Items(items) => {
            let snap = model::snapshot(&local).map_err(|e| model_fail(e, None))?;
            let ids: Vec<ItemId> = items.iter().map(item_id).collect();
            if let Some(missing) = ids.iter().find(|id| !snap.items.contains_key(*id)) {
                return Err(fail(
                    StatusCode::NOT_FOUND,
                    msg!(
                        "pack.no_such_item", kind = missing.kind.slug(), name = &missing.name =>
                        "There is no {kind} named `{name}`."
                    ),
                ));
            }
            Some(ids)
        }
    };
    let out = tw_pack::export::export(
        &local,
        tw_pack::export::Options {
            scope,
            secrets: req.secrets,
            client_prefs: None,
            origin: PackOrigin::Export,
        },
    )
    .map_err(|e| model_fail(e, None))?;
    let (file_name, data_b64) = file_of(&out.pack, pass)?;
    Ok(Json(tw_api::ExportedFile {
        file_name,
        data_b64,
        included: out.included.iter().map(item_ref).collect(),
        omitted_logins: out.omitted_logins,
    }))
}

// ─────────────────────────────────────────────────────────── 导入

fn import_ctx() -> tw_pack::import::Context<'static> {
    tw_pack::import::Context {
        has_aws_profile: &|p| tw_config::aws_profile::credentials(p).is_ok(),
        new_key: &tw_config::generate_key,
        with_prefs: core_keeps_prefs(),
    }
}

fn import_fail(e: tw_pack::import::ImportError, pack: &Pack) -> Fail {
    use tw_pack::import::ImportError as E;
    match e {
        E::Model(m) => model_fail(m, Some(pack)),
        E::Rejected(r) => apply_fail(ApplyError::Rejected(*r)),
        E::Exists(_) | E::NameTaken(..) => fail(StatusCode::CONFLICT, e.msg()),
        E::NotInPack(_) => fail(StatusCode::NOT_FOUND, e.msg()),
        E::Item(..) => fail(StatusCode::UNPROCESSABLE_ENTITY, e.msg()),
    }
}

fn row_view(r: tw_pack::import::Row) -> tw_api::ImportRow {
    use tw_pack::import::{Flag, Status};
    tw_api::ImportRow {
        item: item_ref(&r.item),
        status: match r.status {
            Status::New => tw_api::ImportStatus::New,
            Status::Same => tw_api::ImportStatus::Same,
            Status::Conflict => tw_api::ImportStatus::Conflict,
            Status::Error => tw_api::ImportStatus::Error,
        },
        global: r.global,
        flags: r
            .flags
            .into_iter()
            .map(|f| match f {
                Flag::MissingKey => tw_api::ImportFlag::MissingKey,
                Flag::NeedsLogin => tw_api::ImportFlag::NeedsLogin,
                Flag::PluginDisabled => tw_api::ImportFlag::PluginDisabled,
            })
            .collect(),
        differs: r.differs,
        suggested_name: r.suggested_name,
        error: r.error,
        refs: r.refs.iter().map(item_ref).collect(),
    }
}

async fn import_preview(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ImportSource>,
) -> Result<Json<tw_api::ImportPreview>, Fail> {
    let incoming = open_pack(&req.data_b64, req.passphrase.as_deref())?;
    let (cur, local) = current(&s, PackOrigin::Import)?;
    let rows = tw_pack::import::preview(&local, &incoming, &import_ctx())
        .map_err(|e| import_fail(e, &incoming))?;
    Ok(Json(tw_api::ImportPreview {
        manifest: pack_info(&incoming.manifest),
        base_version: cur.version(),
        rows: rows.into_iter().map(row_view).collect(),
        client_prefs: incoming.client_prefs.clone(),
    }))
}

async fn import(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ImportRequest>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let incoming = open_pack(&req.data_b64, req.passphrase.as_deref())?;
    let (cur, local) = current(&s, PackOrigin::Import)?;
    if cur.version() != req.base_version {
        return Err(apply_fail(ApplyError::Stale {
            base: req.base_version,
            current: cur.version(),
        }));
    }
    let decisions: Vec<tw_pack::import::Decision> = req
        .decisions
        .iter()
        .map(|d| tw_pack::import::Decision {
            item: item_id(&d.item),
            action: match d.action {
                tw_api::ImportAction::Import => tw_pack::import::Action::Import,
                tw_api::ImportAction::Overwrite => tw_pack::import::Action::Overwrite,
                tw_api::ImportAction::Rename => {
                    tw_pack::import::Action::Rename(d.new_name.clone().unwrap_or_default())
                }
                tw_api::ImportAction::Skip => tw_pack::import::Action::Skip,
            },
        })
        .collect();
    let applied = tw_pack::import::apply(&local, &incoming, &decisions, &import_ctx())
        .map_err(|e| import_fail(e, &incoming))?;
    // 直接写原文的那几条路一样：装上、打开、批准一个改工具调用的插件要点头。导入落下去时这种
    // 插件停在待批准，这一关照样过一遍
    crate::plugins::guard_raw_write(&s, &cur.text, &applied.pack.config_yaml).await?;
    let version = land(
        &s,
        &local,
        &applied.pack,
        &cur.version(),
        Origin::Import,
        applied.changed_keys,
    )
    .await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ─────────────────────────────────────────────────────────── 备份

async fn backups(State(s): State<ControlState>) -> Result<Json<Vec<tw_api::BackupEntry>>, Fail> {
    let path = s.config_path().to_path_buf();
    let all = tw_pack::backup::list(&path).map_err(backup_fail)?;
    // 现在的内容：配置读不进来（安全模式）时没有哪一版算「当前」
    let now = s
        .cfg
        .current()
        .ok()
        .map(|cur| tw_pack::disk::current(&path, &cur.text, PackOrigin::Ui))
        .filter(|p| model::snapshot(p).is_ok())
        .map(|p| p.content_id());
    let mut rows: Vec<(tw_api::BackupEntry, String)> = Vec::new();
    for b in all.iter().rev() {
        let Ok(pack) = tw_pack::backup::open(b) else {
            continue;
        };
        let content = pack.content_id();
        rows.push((
            tw_api::BackupEntry {
                id: b.id.clone(),
                at_ms: b.at_ms,
                origin: b.origin.into(),
                device: None,
                local: true,
                sync: false,
                counts: tw_api::BackupCounts {
                    providers: pack.manifest.counts.providers,
                    plugins: pack.manifest.counts.plugins,
                },
                core_version: pack.manifest.core_version.clone(),
                current: false,
            },
            content,
        ));
    }
    if let Some(remote) = s.cfg.remote_backups() {
        for v in remote.list().await {
            // 同一份内容只并进本机最新的那一行
            match rows.iter_mut().find(|(e, c)| *c == v.content_id && !e.sync) {
                Some((e, _)) => {
                    e.sync = true;
                    e.device = Some(v.device_name.clone());
                }
                None => rows.push((
                    tw_api::BackupEntry {
                        id: format!("{SYNC_ID}{}", v.content_id),
                        at_ms: v.at_ms,
                        origin: tw_api::ConfigOrigin::Sync,
                        device: Some(v.device_name.clone()),
                        local: false,
                        sync: true,
                        counts: tw_api::BackupCounts {
                            providers: v.counts.providers,
                            plugins: v.counts.plugins,
                        },
                        core_version: v.core_version.clone(),
                        current: false,
                    },
                    v.content_id.clone(),
                )),
            }
        }
    }
    // 「当前」只标一行：内容和现在一样的版本里最新的那个
    if let Some(now) = &now
        && let Some((e, _)) = rows
            .iter_mut()
            .filter(|(_, c)| c == now)
            .max_by_key(|(e, _)| e.at_ms)
    {
        e.current = true;
    }
    let mut out: Vec<tw_api::BackupEntry> = rows.into_iter().map(|(e, _)| e).collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
    Ok(Json(out))
}

/// 按标识取一个版本：本机的读文件，同步位置上的去取
async fn backup_pack(s: &ControlState, id: &str) -> Result<Pack, Fail> {
    if let Some(content) = id.strip_prefix(SYNC_ID) {
        let Some(remote) = s.cfg.remote_backups() else {
            return Err(backup_fail(tw_pack::backup::BackupError::NotFound {
                id: id.to_string(),
            }));
        };
        return remote
            .fetch(content)
            .await
            .map_err(|m| fail(StatusCode::BAD_GATEWAY, m));
    }
    let b = tw_pack::backup::find(s.config_path(), id).map_err(backup_fail)?;
    tw_pack::backup::open(&b).map_err(backup_fail)
}

async fn backup_file(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::BackupFileRequest>,
) -> Result<Json<tw_api::ExportedFile>, Fail> {
    let pass = passphrase(req.passphrase.as_deref())?;
    let pack = backup_pack(&s, &req.id).await?;
    // 另存出去的和导出的一样：本机设置、ChatGPT 的登录不跟着走
    let out = tw_pack::export::export(
        &pack,
        tw_pack::export::Options {
            scope: None,
            secrets: true,
            client_prefs: None,
            origin: PackOrigin::Export,
        },
    )
    .map_err(|e| model_fail(e, Some(&pack)))?;
    let mut exported = out.pack;
    exported.manifest.created_at_ms = pack.manifest.created_at_ms;
    let (file_name, data_b64) = file_of(&exported, pass)?;
    Ok(Json(tw_api::ExportedFile {
        file_name,
        data_b64,
        included: out.included.iter().map(item_ref).collect(),
        omitted_logins: out.omitted_logins,
    }))
}

fn restore_row(r: tw_pack::restore::Row) -> tw_api::RestoreRow {
    use tw_pack::restore::{Change, Kept};
    tw_api::RestoreRow {
        item: item_ref(&r.item),
        change: match r.change {
            Change::Add => tw_api::RestoreChange::Add,
            Change::Remove => tw_api::RestoreChange::Remove,
            Change::Modify => tw_api::RestoreChange::Modify,
        },
        kept: r.kept.map(|k| match k {
            Kept::KeyInUse => tw_api::KeptReason::KeyInUse,
        }),
    }
}

async fn restore_preview(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::RestorePreviewRequest>,
) -> Result<Json<tw_api::RestorePreview>, Fail> {
    let backup = backup_pack(&s, &req.id).await?;
    let (cur, local) = current(&s, PackOrigin::Restore)?;
    let sync_devices = match s.cfg.remote_backups() {
        Some(r) => r.other_devices().await,
        None => 0,
    };
    // 配置读不进来：整份换回那一版，每一项都是「新增」
    let rows = match tw_pack::restore::preview(
        &local,
        &backup,
        &s.cfg.pinned_keys(),
        core_keeps_prefs(),
    ) {
        Ok(rows) => rows.into_iter().map(restore_row).collect(),
        Err(_) if model::snapshot(&local).is_err() => model::snapshot(&backup)
            .map_err(|e| model_fail(e, Some(&backup)))?
            .items
            .keys()
            .map(|id| tw_api::RestoreRow {
                item: item_ref(id),
                change: tw_api::RestoreChange::Add,
                kept: None,
            })
            .collect(),
        Err(e) => return Err(model_fail(e, Some(&backup))),
    };
    Ok(Json(tw_api::RestorePreview {
        base_version: cur.version(),
        rows,
        sync_devices,
    }))
}

async fn restore(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::RestoreRequest>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let backup = backup_pack(&s, &req.id).await?;
    let (cur, local) = current(&s, PackOrigin::Restore)?;
    if cur.version() != req.base_version {
        return Err(apply_fail(ApplyError::Stale {
            base: req.base_version,
            current: cur.version(),
        }));
    }
    let picked: Option<Vec<ItemId>> = req
        .items
        .as_ref()
        .map(|xs| xs.iter().map(item_id).collect());
    let (after, changed) = if model::snapshot(&local).is_err() {
        // 安全模式：现在这份读不进来，整份换回去
        let mut whole = tw_pack::restore::whole(&backup, &cur.text);
        if !core_keeps_prefs() {
            whole.client_prefs = local.client_prefs.clone();
        }
        (whole, Vec::new())
    } else {
        let applied = tw_pack::restore::apply(
            &local,
            &backup,
            picked.as_deref(),
            &s.cfg.pinned_keys(),
            core_keeps_prefs(),
        )
        .map_err(|e| model_fail(e, Some(&backup)))?;
        (applied.pack, applied.changed_keys)
    };
    crate::plugins::guard_raw_write(&s, &cur.text, &after.config_yaml).await?;
    let version = land(&s, &local, &after, &cur.version(), Origin::Restore, changed).await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ─────────────────────────────────────────────────────────── 通用偏好、固定密钥

async fn get_prefs(State(s): State<ControlState>) -> Json<tw_api::ClientPrefs> {
    Json(tw_api::ClientPrefs {
        prefs: tw_pack::disk::read_prefs(s.config_path()),
    })
}

async fn put_prefs(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ClientPrefs>,
) -> Result<Json<()>, Fail> {
    if let Some(p) = &req.prefs
        && p.to_string().len() > tw_api::CLIENT_PREFS_MAX
    {
        return Err(fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            msg!(
                "backup.prefs_too_large", limit = tw_api::CLIENT_PREFS_MAX / 1024 =>
                "The client preferences are larger than {limit} KiB."
            ),
        ));
    }
    let path = s.config_path().to_path_buf();
    if tw_pack::disk::read_prefs(&path) == req.prefs {
        return Ok(Json(()));
    }
    tw_pack::disk::write_prefs(&path, req.prefs.as_ref())
        .map_err(|e| io_fail(&tw_pack::disk::prefs_path(&path), e))?;
    // 偏好也是备份的一部分：只改了它也留一版
    if let Ok(cur) = s.cfg.current()
        && let Err(e) = tw_pack::backup::record(&path, &cur.text, Origin::Ui)
    {
        tracing::warn!("a backup version could not be kept: {e}");
    }
    s.bus().emit(tw_api::Event::ClientPrefsChanged {
        id: s.bus().next_id(),
        at_ms: crate::config::now_ms(),
    });
    Ok(Json(()))
}

async fn pin_keys(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PinnedKeys>,
) -> Result<Json<()>, Fail> {
    let names: BTreeSet<String> = req.names.into_iter().collect();
    s.cfg
        .set_pinned_keys(names)
        .map_err(|e| io_fail(&tw_pack::disk::pinned_path(s.config_path()), e))?;
    Ok(Json(()))
}

/// 给同步那一路用：固定密钥
pub fn pinned_keys(s: &ControlState) -> BTreeSet<String> {
    s.cfg.pinned_keys()
}

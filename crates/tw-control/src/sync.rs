//! 多设备同步的控制面一半：什么时候同步、同步设置怎么写、端点、状态事件。
//!
//! 合并规则、同步位置上的文件、加密都在 `tw-sync`；这里把它接到配置的写入流程上：
//!
//! | 时机 | 行为 |
//! |---|---|
//! | 本机配置变更 | 合并最近 10 秒内的变更后上传一个版本。退出前立即上传还没上传的 |
//! | 定时 | WebDAV、S3 每 30 分钟看一次；文件夹位置盯着文件变化，新版本一到就合并 |
//! | 事件 | 启动、从睡眠中唤醒、网络恢复（失败之后按 1、2、5、10 分钟重试）、`POST /sync/now` |
//!
//! 应用别的设备的版本走现有的写入流程（校验、热更新、记一个本机版本），来源记为 `sync`；
//! 这种写入不会再触发上传。**安全模式下不自动同步**（`paused`）。
//!
//! 连不上同步位置的头 2 小时里状态照旧（已同步 / 冲突），过了 2 小时才是 `unreachable`：
//! 笔记本合上盖子、换个网络都会失败几次，那不该是一条要人处理的提示。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::sync::Notify;
use tw_api::ep;
use tw_config::history::Origin;
use tw_pack::model::{self, ItemId, ItemKind, Snapshot};
use tw_pack::{Pack, PackOrigin};
use tw_sync::engine::{self, Current, Decision, Engine, Host, Report, RoundError, Take};
use tw_sync::{AnyStore, StoreError};
use tw_types::{Msg, msg};

use crate::backups::{BoxFuture, RemoteBackups, RemoteVersionInfo};
use crate::contract::RouterExt;
use crate::{ApplyError, ConfigManager, ControlState, Fail, apply_fail, fail};

/// 本机配置改了之后等多久再上传：这期间的改动合成一个版本
pub const SETTLE: Duration = Duration::from_secs(10);
/// WebDAV、S3 多久看一次
pub const POLL: Duration = Duration::from_secs(30 * 60);
/// 连续失败多久才算「同步位置不可用」
pub const UNREACHABLE_AFTER_MS: u64 = 2 * 3600 * 1000;
/// 失败之后隔多久重试：网络恢复后几分钟内就接上
const RETRY: &[u64] = &[60, 120, 300, 600];
/// 退出前上传最多等多久
const FLUSH_WITHIN: Duration = Duration::from_secs(8);

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::SyncStatus, status)
        .at(ep::SyncTest, test)
        .at(ep::SyncJoinPreview, join_preview)
        .at(ep::SyncEnable, enable)
        .at(ep::SyncSave, save)
        .at(ep::SyncNow, now)
        .at(ep::SyncConflicts, conflicts)
        .at(ep::SyncResolve, resolve)
        .at(ep::SyncEncryption, encryption)
        .at(ep::SyncUnlock, unlock)
        .at(ep::SyncDisable, disable)
        .at(ep::SyncRepublish, republish)
}

fn now_ms() -> u64 {
    crate::config::now_ms()
}

// ─────────────────────────────────────────────────────────── 本机这一边

/// 本机的配置：读成配置包、照配置包写回去
#[derive(Clone)]
struct CoreHost {
    mgr: Weak<ConfigManager>,
}

impl CoreHost {
    fn mgr(&self) -> Result<Arc<ConfigManager>, Msg> {
        self.mgr
            .upgrade()
            .ok_or_else(|| msg!("sync.shutting_down" => "the core is shutting down"))
    }
}

/// 本机此刻的完整配置包：配置原文、插件文件、Lite 的通用偏好（和备份、导出读的是同一份，
/// `tw_pack::disk::current`）
pub fn current_pack(mgr: &ConfigManager) -> Result<Current, Msg> {
    let loaded = mgr.current().map_err(|e| e.msg())?;
    let pack = tw_pack::disk::current(mgr.path(), &loaded.text, PackOrigin::Ui);
    let changed_ms = loaded
        .fingerprint
        .mtime
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(now_ms);
    Ok(Current {
        version: loaded.version(),
        pack,
        changed_ms,
    })
}

/// 照一个配置包写本机，和导入、恢复同一个顺序：攥着插件的编辑锁，先写插件文件和通用偏好，再
/// 经写入流程写配置（来源记为同步，留一个本机版本，记下是哪台设备传来的）；配置没写成就把
/// 文件还原
async fn apply_pack(
    mgr: &ConfigManager,
    after: Pack,
    base_version: &str,
    from: &str,
) -> Result<(), engine::ApplyError> {
    let path = mgr.path().to_path_buf();
    let _edit = mgr.gateway().plugins.edits.lock().await;
    let cur = mgr
        .current()
        .map_err(|e| engine::ApplyError::Rejected(e.msg()))?;
    if cur.version() != base_version {
        return Err(engine::ApplyError::Stale);
    }
    let before = tw_pack::disk::current(&path, &cur.text, PackOrigin::Sync);
    let io = |p: &Path, e: std::io::Error| {
        engine::ApplyError::Rejected(msg!(
            "sync.local.io", path = p.display(), detail = e =>
            "{path} could not be used: {detail}"
        ))
    };
    let undo =
        tw_pack::disk::write_plugins(&path, &before.plugins, &after.plugins).map_err(|e| {
            io(
                &tw_config::plugins::dir_in(&tw_pack::disk::dir_of(&path)),
                e,
            )
        })?;
    // 远程连接下偏好在 Lite 那边；同步这一路跑在 core 自己的任务里，不是远程连接
    let prefs_changed = after.client_prefs.is_some() && after.client_prefs != before.client_prefs;
    if prefs_changed && let Err(e) = tw_pack::disk::write_prefs(&path, after.client_prefs.as_ref())
    {
        undo.restore();
        return Err(io(&tw_pack::disk::prefs_path(&path), e));
    }
    let started = now_ms();
    if let Err(e) = mgr
        .write(&after.config_yaml, Some(base_version), Origin::Sync)
        .await
    {
        undo.restore();
        if prefs_changed {
            let _ = tw_pack::disk::write_prefs(&path, before.client_prefs.as_ref());
        }
        return Err(match e {
            ApplyError::Stale { .. }
            | ApplyError::Store(tw_config::StoreError::Conflict { .. }) => {
                engine::ApplyError::Stale
            }
            other => engine::ApplyError::Rejected(other.msg()),
        });
    }
    // 本机这一版是从哪台设备来的：备份列表里写「同步自 MacBook Air」
    if !from.is_empty()
        && let Ok(all) = tw_pack::backup::list(&path)
        && let Some(b) = all
            .iter()
            .rev()
            .find(|b| b.origin == Origin::Sync && b.at_ms + 1000 >= started)
        && let Err(e) = tw_pack::backup::note_device(b, from)
    {
        tracing::debug!("the backup version could not be labelled with its device: {e}");
    }
    if prefs_changed {
        mgr.bus().emit(tw_api::Event::ClientPrefsChanged {
            id: mgr.bus().next_id(),
            at_ms: now_ms(),
        });
    }
    Ok(())
}

impl Host for CoreHost {
    fn current(&self) -> Result<Current, Msg> {
        current_pack(&*self.mgr()?)
    }
    async fn apply(
        &self,
        pack: Pack,
        base_version: &str,
        from: &str,
    ) -> Result<(), engine::ApplyError> {
        let mgr = self.mgr().map_err(engine::ApplyError::Rejected)?;
        apply_pack(&mgr, pack, base_version, from).await
    }
    fn pinned_keys(&self) -> BTreeSet<String> {
        self.mgr
            .upgrade()
            .map(|m| m.pinned_keys())
            .unwrap_or_default()
    }
    /// 本机备份里内容一样的那一版（找合并基准）。新的在前
    fn backup(&self, content_id: &str) -> Option<Pack> {
        let mgr = self.mgr.upgrade()?;
        let all = tw_pack::backup::list(mgr.path()).ok()?;
        all.iter()
            .rev()
            .filter_map(|b| tw_pack::backup::open(b).ok())
            .find(|p| p.content_id() == content_id)
    }
}

// ─────────────────────────────────────────────────────────── 同步位置

/// 发 WebDAV、S3 请求用的客户端：经同步设置里选的出站代理，**不跟重定向**（跟到别的主机，
/// 同步位置的密码就跟着去了），证书照常校验
fn http(cfg: &tw_config::Config, proxy: &str) -> Result<reqwest::Client, Msg> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // 一轮里的请求是接连发的；两轮之间的空闲连接不留：很多服务端几秒就关掉空闲连接，
        // 留着的下一轮一发就断（发请求那一处另有一次重发）
        .pool_idle_timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(tw_gateway::user_agent());
    match proxy {
        tw_config::DIRECT => b = b.no_proxy(),
        tw_config::SYSTEM => {}
        name => {
            let p = cfg.proxies.iter().find(|x| x.name == name).ok_or_else(|| {
                tw_config::sync::SyncError::UnknownProxy {
                    proxy: name.to_string(),
                }
                .msg()
            })?;
            let url = p.url().map_err(|e| e.msg())?;
            let px = reqwest::Proxy::all(&url).map_err(|e| {
                msg!(
                    "sync.proxy_unusable", proxy = name, detail = e =>
                    "proxy `{proxy}` cannot be used for the sync location: {detail}"
                )
            })?;
            b = b.proxy(px);
        }
    }
    b.build().map_err(|e| {
        msg!(
            "sync.http_client", detail = e =>
            "the connection to the sync location could not be set up: {detail}"
        )
    })
}

fn store_for(cfg: &tw_config::Config, t: &tw_config::sync::Target) -> Result<AnyStore, Msg> {
    t.check().map_err(|e| e.msg())?;
    tw_config::sync::check_proxy(t, &cfg.proxies).map_err(|e| e.msg())?;
    AnyStore::new(t, http(cfg, t.proxy())?).map_err(|e| e.msg())
}

pub fn target_to_config(t: &tw_api::SyncTarget) -> tw_config::sync::Target {
    use tw_api::SyncTarget as A;
    use tw_config::sync::Target as C;
    let proxy = |p: &str| {
        if p.trim().is_empty() {
            tw_config::DIRECT.to_string()
        } else {
            p.trim().to_string()
        }
    };
    match t {
        A::Folder { path, proxy: p } => C::Folder {
            path: path.trim().to_string(),
            proxy: proxy(p),
        },
        A::Webdav {
            url,
            user,
            pass,
            proxy: p,
        } => C::Webdav {
            url: url.trim().to_string(),
            user: user.clone(),
            pass: pass.clone(),
            proxy: proxy(p),
        },
        A::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            proxy: p,
        } => C::S3 {
            endpoint: endpoint.trim().to_string(),
            region: region.trim().to_string(),
            bucket: bucket.trim().to_string(),
            prefix: prefix.trim().trim_matches('/').to_string(),
            access_key_id: access_key_id.trim().to_string(),
            secret_access_key: secret_access_key.trim().to_string(),
            proxy: proxy(p),
        },
    }
}

pub fn target_to_api(t: &tw_config::sync::Target) -> tw_api::SyncTarget {
    use tw_api::SyncTarget as A;
    use tw_config::sync::Target as C;
    match t.clone() {
        C::Folder { path, proxy } => A::Folder { path, proxy },
        C::Webdav {
            url,
            user,
            pass,
            proxy,
        } => A::Webdav {
            url,
            user,
            pass,
            proxy,
        },
        C::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            proxy,
        } => A::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            proxy,
        },
    }
}

/// 写 `sync` 一节（`None` 是删掉）。这是本机设置：同步一轮里比较的快照不含它，写它不会
/// 触发上传
async fn write_section(
    mgr: &ConfigManager,
    sync: Option<&tw_config::sync::Sync>,
    origin: Origin,
) -> Result<String, ApplyError> {
    let value = match sync {
        Some(s) => Some(serde_yaml_ng::to_value(s).map_err(|e| {
            ApplyError::Invalid(msg!(
                "sync.write_section", detail = e =>
                "the sync settings could not be written: {detail}"
            ))
        })?),
        None => None,
    };
    mgr.transform(None, origin, |text, _| {
        Ok(tw_config::edit::set(
            text,
            &[tw_yaml::Step::key("sync")],
            value.as_ref(),
        )?)
    })
    .await
}

/// 引擎的设置：`sync` 一节，加上这台设备是不是远程 core（开着远程控制端口）
fn settings_of(cfg: &tw_config::Config, sync: &tw_config::sync::Sync) -> engine::Settings {
    let mut s = engine::Settings::of(sync);
    s.remote_core = cfg
        .listen
        .control
        .remote
        .as_ref()
        .is_some_and(|r| r.enabled);
    s
}

// ─────────────────────────────────────────────────────────── 服务

type Eng = Engine<AnyStore, CoreHost>;

struct Running {
    engine: Eng,
    /// 建它时的 `sync` 一节和出站代理：变了就重建
    config: tw_config::sync::Sync,
    proxies: String,
    /// 文件夹位置的监听，留着
    _watch: Option<tw_watch::Watch>,
}

/// 同步服务。每份配置一个（[`service`]）
pub struct Sync {
    /// 自己（装进备份列表的那扇门拿着它）
    me: Weak<Sync>,
    mgr: Weak<ConfigManager>,
    run: tokio::sync::Mutex<Option<Running>>,
    view: std::sync::Mutex<Option<tw_api::SyncView>>,
    /// 经同步改了地址的上游（不加密时）。上游视图读它
    url_changed: std::sync::Mutex<BTreeMap<String, tw_api::SyncUrlChange>>,
    kick: Arc<Notify>,
    /// 本机改了配置、还没上传：这个时刻之后上传
    dirty: std::sync::Mutex<Option<tokio::time::Instant>>,
    paused: AtomicBool,
    started: AtomicBool,
    /// 连续失败了几次（重试的间隔）
    failures: AtomicU32,
}

/// 这份配置的同步服务。第一次用的时候建（端点、启动、测试都走这里）
pub fn service(s: &ControlState) -> Arc<Sync> {
    of(&s.cfg)
}

pub fn of(mgr: &Arc<ConfigManager>) -> Arc<Sync> {
    mgr.sync_slot()
        .get_or_init(|| {
            let dir = crate::plugins::dir_of(mgr.path());
            let url_changed = tw_sync::local::Local::new(&dir)
                .state()
                .url_changed
                .into_iter()
                .map(|(k, v)| (k, url_change(&v)))
                .collect();
            Arc::new_cyclic(|me| Sync {
                me: me.clone(),
                mgr: Arc::downgrade(mgr),
                run: tokio::sync::Mutex::new(None),
                view: std::sync::Mutex::new(None),
                url_changed: std::sync::Mutex::new(url_changed),
                kick: Arc::new(Notify::new()),
                dirty: std::sync::Mutex::new(None),
                paused: AtomicBool::new(false),
                started: AtomicBool::new(false),
                failures: AtomicU32::new(0),
            })
        })
        .clone()
}

fn url_change(v: &tw_sync::local::UrlChange) -> tw_api::SyncUrlChange {
    tw_api::SyncUrlChange {
        from: v.from.clone(),
        device: v.device.clone(),
        at_ms: v.at_ms,
    }
}

/// 一轮或者一次设置操作没成，作为控制面的失败
fn round_fail(e: &RoundError) -> Fail {
    let code = match e {
        RoundError::Store(StoreError::AuthFailed(_)) => StatusCode::FORBIDDEN,
        RoundError::Store(_) => StatusCode::BAD_GATEWAY,
        RoundError::Locked(_) => StatusCode::BAD_REQUEST,
        RoundError::TooNew { .. }
        | RoundError::Stale
        | RoundError::Emptied
        | RoundError::KeyringMissing(_) => StatusCode::CONFLICT,
        RoundError::NotArrived | RoundError::LooksEmpty => StatusCode::SERVICE_UNAVAILABLE,
        RoundError::Local(_) => StatusCode::BAD_REQUEST,
    };
    fail(code, e.msg())
}

fn sync_off() -> Fail {
    fail(
        StatusCode::CONFLICT,
        msg!("sync.off" => "sync is not turned on"),
    )
}

/// 口令够长吗（设口令的那一刻才查）
fn check_passphrase(p: Option<&str>) -> Result<&str, Fail> {
    match p {
        Some(p) if p.chars().count() >= tw_config::sync::PASSPHRASE_MIN => Ok(p),
        _ => Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "sync.passphrase_short", min = tw_config::sync::PASSPHRASE_MIN =>
                "the sync passphrase has to be at least {min} characters"
            ),
        )),
    }
}

use crate::backups::item_ref;

impl Sync {
    fn mgr(&self) -> Option<Arc<ConfigManager>> {
        self.mgr.upgrade()
    }

    fn data_dir(&self) -> Option<PathBuf> {
        self.mgr().map(|m| crate::plugins::dir_of(m.path()))
    }

    /// 某个上游的地址最近一次经同步改过：改之前的地址、哪台设备、什么时候
    pub fn url_changed(&self, provider: &str) -> Option<tw_api::SyncUrlChange> {
        self.url_changed
            .lock()
            .ok()
            .and_then(|m| m.get(provider).cloned())
    }

    /// 本机配置变了：`SETTLE` 之后上传
    pub fn touch(&self) {
        if let Ok(mut d) = self.dirty.lock()
            && d.is_none()
        {
            *d = Some(tokio::time::Instant::now() + SETTLE);
            self.kick.notify_one();
        }
    }

    /// 马上来一轮（不等定时）
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// 配置里的 `sync` 一节和此刻在跑的对得上；对不上就照配置重建（同步位置、口令、不同步
    /// 的上游、代理变了）。没开同步是 `None`
    fn ensure<'a>(&self, run: &'a mut Option<Running>) -> Result<Option<&'a mut Running>, Msg> {
        let Some(mgr) = self.mgr() else {
            *run = None;
            return Ok(None);
        };
        let cfg = mgr.gateway().config();
        let Some(sync) = cfg.sync.clone() else {
            *run = None;
            // 安全模式下配置里没有这一节是因为在服务的是临时配置：门留着（见 `start`）
            if !self.paused.load(Ordering::Relaxed) {
                self.register_backups(false);
            }
            return Ok(None);
        };
        self.register_backups(true);
        let proxies = format!("{:?}", cfg.proxies);
        let fresh = match run.as_ref() {
            Some(r) => r.config == sync && r.proxies == proxies,
            None => false,
        };
        if !fresh {
            let store = store_for(&cfg, &sync.target)?;
            let watch = match store.folder_root() {
                Some(root) => match tw_sync::store::folder::watch(root) {
                    Ok((w, mut rx)) => {
                        let kick = self.kick.clone();
                        tokio::spawn(async move {
                            while rx.recv().await.is_some() {
                                kick.notify_one();
                            }
                        });
                        Some(w)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "the sync folder cannot be watched, so it is checked every 30 minutes: {e}"
                        );
                        None
                    }
                },
                None => None,
            };
            let dir = crate::plugins::dir_of(mgr.path());
            let mut engine = Engine::new(
                store,
                CoreHost {
                    mgr: Arc::downgrade(&mgr),
                },
                &dir,
                settings_of(&cfg, &sync),
            );
            // 同一个位置、同一个口令：缓存和捷径留着也没错，但重建了就从头看一遍
            engine.invalidate();
            *run = Some(Running {
                engine,
                config: sync,
                proxies,
                _watch: watch,
            });
        }
        Ok(run.as_mut())
    }

    /// 此刻给界面看的样子
    pub fn view(&self) -> tw_api::SyncView {
        let on = self
            .mgr()
            .map(|m| m.gateway().config().sync.is_some())
            .unwrap_or(false);
        if let Ok(v) = self.view.lock()
            && let Some(v) = v.as_ref()
            // 配置里开着、记下的却是关着（刚开启），或者反过来（手改文件关掉了）：重新拼
            && on == (v.state != tw_api::SyncState::Off)
        {
            return v.clone();
        }
        self.compute_view(None, None)
    }

    /// 照配置、本机的同步状态和上一轮的结果拼出视图
    fn compute_view(
        &self,
        report: Option<&Report>,
        state: Option<tw_api::SyncState>,
    ) -> tw_api::SyncView {
        let off = tw_api::SyncView {
            state: tw_api::SyncState::Off,
            target: None,
            encrypted: false,
            device: None,
            last_synced_ms: None,
            failing_since_ms: None,
            error: None,
            devices: Vec::new(),
            conflicts: 0,
            excluded: Vec::new(),
            excluded_cascade: Vec::new(),
            skipped: 0,
        };
        let Some(mgr) = self.mgr() else { return off };
        let cfg = mgr.gateway().config();
        let paused = self.paused.load(Ordering::Relaxed);
        // 安全模式下在服务的是一份临时配置：同步设置在磁盘上那份读不进来的原文里
        let raw = (paused && cfg.sync.is_none())
            .then(|| mgr.current().ok())
            .flatten()
            .and_then(|l| tw_config::sync::from_raw(&l.text))
            .map(|(s, _)| s);
        let Some(sync) = cfg.sync.as_ref().or(raw.as_ref()) else {
            return off;
        };
        let local = tw_sync::local::Local::new(&crate::plugins::dir_of(mgr.path()));
        let st = local.state();
        let report = report.cloned();
        let prev = self.view.lock().ok().and_then(|v| v.clone());
        let conflicts = report
            .as_ref()
            .map(|r| r.conflicts.len() as u32)
            .or(prev.as_ref().map(|p| p.conflicts))
            .unwrap_or(0);
        let skipped = report
            .as_ref()
            .map(|r| r.skipped)
            .or(prev.as_ref().map(|p| p.skipped))
            .unwrap_or(0);
        let devices = match &report {
            Some(r) => r
                .devices
                .iter()
                .map(|d| tw_api::SyncDevice {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    core_version: d.core_version.clone(),
                    last_seen_ms: d.last_seen_ms,
                    this: d.id == sync.device_id,
                    paused_update: d.paused_update,
                    remote_core: d.remote_core,
                })
                .collect(),
            None => prev.as_ref().map(|p| p.devices.clone()).unwrap_or_default(),
        };
        let state = if paused {
            tw_api::SyncState::Paused
        } else {
            state.unwrap_or(if conflicts > 0 {
                tw_api::SyncState::Conflict
            } else {
                tw_api::SyncState::Idle
            })
        };
        let cascade = current_pack(&mgr)
            .ok()
            .and_then(|c| model::snapshot(&c.pack).ok())
            .map(|snap| {
                let names: BTreeSet<String> = sync.exclude.iter().cloned().collect();
                engine::excluded(&names, &[&snap])
                    .into_iter()
                    .filter(|id| !(id.kind == ItemKind::Provider && names.contains(&id.name)))
                    .filter(|id| snap.items.contains_key(id))
                    .map(|id| item_ref(&id))
                    .collect()
            })
            .unwrap_or_default();
        tw_api::SyncView {
            state,
            target: Some(target_to_api(&sync.target)),
            encrypted: report.as_ref().map(|r| r.encrypted).unwrap_or(sync.encrypt),
            device: Some(tw_api::SyncDeviceRef {
                id: sync.device_id.clone(),
                name: sync.device_name.clone(),
            }),
            last_synced_ms: st.last_synced_ms,
            failing_since_ms: st.failing_since_ms,
            error: st.error.clone(),
            devices,
            conflicts,
            excluded: sync.exclude.clone(),
            excluded_cascade: cascade,
            skipped,
        }
    }

    fn publish(&self, v: tw_api::SyncView) {
        let changed = self
            .view
            .lock()
            .map(|mut g| {
                let changed = g.as_ref() != Some(&v);
                *g = Some(v.clone());
                changed
            })
            .unwrap_or(false);
        if changed && let Some(mgr) = self.mgr() {
            mgr.bus().emit(tw_api::Event::SyncStatus {
                id: mgr.bus().next_id(),
                view: v,
                at_ms: now_ms(),
            });
        }
    }

    /// 记一次失败：从什么时候开始一直失败、原因
    fn record_failure(&self, r: &Running, e: &RoundError) {
        let local = r.engine.local();
        let mut st = local.state();
        st.failing_since_ms.get_or_insert_with(now_ms);
        st.error = Some(e.msg());
        let _ = local.save_state(&st);
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// 同步一轮（顺带处理几处冲突），更新状态、发事件
    pub async fn round(&self, decisions: &[Decision]) -> Result<tw_api::SyncView, Fail> {
        let mut guard = self.run.lock().await;
        let r = match self.ensure(&mut guard) {
            Ok(Some(r)) => r,
            Ok(None) => {
                let v = self.compute_view(None, None);
                self.publish(v.clone());
                return Ok(v);
            }
            Err(m) => {
                // 照设置建不起来（出站代理没了、地址不合规矩）：改设置，不是拒绝访问
                let v = tw_api::SyncView {
                    error: Some(m.clone()),
                    ..self.compute_view(None, Some(tw_api::SyncState::Misconfigured))
                };
                self.publish(v.clone());
                return Err(fail(StatusCode::BAD_REQUEST, m));
            }
        };
        if self.paused.load(Ordering::Relaxed) {
            let v = self.compute_view(None, None);
            self.publish(v.clone());
            return Ok(v);
        }
        if let Ok(mut d) = self.dirty.lock() {
            *d = None;
        }
        if let Ok(mut g) = self.view.lock()
            && let Some(v) = g.as_mut()
        {
            v.state = tw_api::SyncState::Syncing;
        }
        let mut result = r.engine.round_with(decisions).await;
        for _ in 0..2 {
            if matches!(result, Err(RoundError::Stale)) {
                result = r.engine.round_with(decisions).await;
            }
        }
        let outcome = match result {
            Ok(report) => {
                self.failures.store(0, Ordering::Relaxed);
                let encrypted = report.encrypted;
                let st = r.engine.local().state();
                if let Ok(mut m) = self.url_changed.lock() {
                    *m = st
                        .url_changed
                        .iter()
                        .map(|(k, v)| (k.clone(), url_change(v)))
                        .collect();
                }
                if !report.keys_changed.is_empty()
                    && let Some(mgr) = self.mgr()
                {
                    mgr.bus().emit(tw_api::Event::GatewayKeysChanged {
                        id: mgr.bus().next_id(),
                        names: report.keys_changed.clone(),
                        at_ms: now_ms(),
                    });
                }
                // 别的设备关掉了加密（keyring 写着已关闭，口令解得开）、或者又开了（口令还是
                // 这台记着的那个）：这台跟着改设置。**口令留着**：靠它认出以后的 keyring
                let follow = (encrypted != r.config.encrypt
                    && (!encrypted || r.config.passphrase.is_some()))
                .then(|| {
                    let mut s = r.config.clone();
                    s.encrypt = encrypted;
                    s
                });
                Ok((report, follow))
            }
            Err(e) => Err(e),
        };
        let v = match outcome {
            Ok((report, follow)) => {
                let v = self.compute_view(Some(&report), None);
                drop(guard);
                if let (Some(s), Some(mgr)) = (follow, self.mgr()) {
                    let _ = write_section(&mgr, Some(&s), Origin::Sync).await;
                }
                v
            }
            Err(RoundError::NotArrived) => {
                drop(guard);
                self.view()
            }
            Err(e) => {
                // 同步位置空了、keyring 不见了也一样：记下来、等人处理，**同步设置不动**
                self.record_failure(r, &e);
                let st = r.engine.local().state();
                let state = match &e {
                    RoundError::Store(StoreError::AuthFailed(_)) => tw_api::SyncState::AuthFailed,
                    RoundError::Locked(_) => tw_api::SyncState::Locked,
                    RoundError::TooNew { .. } => tw_api::SyncState::TooNew,
                    RoundError::Emptied => tw_api::SyncState::Emptied,
                    RoundError::KeyringMissing(_) => tw_api::SyncState::KeyringMissing,
                    _ => {
                        let since = st.failing_since_ms.unwrap_or_else(now_ms);
                        if now_ms().saturating_sub(since) >= UNREACHABLE_AFTER_MS {
                            tw_api::SyncState::Unreachable
                        } else {
                            let prev = self.view.lock().ok().and_then(|v| v.clone());
                            match prev.map(|p| p.conflicts) {
                                Some(n) if n > 0 => tw_api::SyncState::Conflict,
                                _ => tw_api::SyncState::Idle,
                            }
                        }
                    }
                };
                drop(guard);
                self.compute_view(None, Some(state))
            }
        };
        self.publish(v.clone());
        Ok(v)
    }

    /// 退出前：还有没上传的改动就马上传
    pub async fn flush(&self) {
        let pending = self.dirty.lock().map(|d| d.is_some()).unwrap_or(false);
        if !pending || self.paused.load(Ordering::Relaxed) {
            return;
        }
        let _ = tokio::time::timeout(FLUSH_WITHIN, self.round(&[])).await;
    }

    /// 下一次定时检查隔多久：失败着就按重试的间隔
    fn next_poll(&self) -> Duration {
        let n = self.failures.load(Ordering::Relaxed) as usize;
        if n == 0 {
            return POLL;
        }
        Duration::from_secs(RETRY[(n - 1).min(RETRY.len() - 1)])
    }

    /// 冲突列表
    pub async fn conflicts(&self) -> Vec<tw_api::SyncConflict> {
        let guard = self.run.lock().await;
        let Some(r) = guard.as_ref() else {
            return Vec::new();
        };
        let me = r.config.device_id.clone();
        let Some(mgr) = self.mgr() else {
            return Vec::new();
        };
        let local = current_pack(&mgr)
            .ok()
            .and_then(|c| model::snapshot(&c.pack).ok())
            .unwrap_or_default();
        r.engine
            .last_report()
            .conflicts
            .iter()
            .map(|c| {
                let units = local.items.get(&c.item).map(|it| c.pick(&it.units));
                let (mine, theirs) = c.mine(&me, units.as_ref());
                tw_api::SyncConflict {
                    id: c.id.clone(),
                    item: item_ref(&c.item),
                    units: c.units.clone(),
                    local: tw_api::SyncConflictSide {
                        device: mine.device_name.clone(),
                        at_ms: mine.at_ms,
                        deleted: mine.units.is_none(),
                    },
                    remote: tw_api::SyncConflictSide {
                        device: theirs.device_name.clone(),
                        at_ms: theirs.at_ms,
                        deleted: theirs.units.is_none(),
                    },
                    diff: c
                        .units
                        .iter()
                        .map(|u| tw_api::SyncConflictDiff {
                            unit: u.clone(),
                            local: mine.units.as_ref().and_then(|m| m.get(u).cloned()),
                            remote: theirs.units.as_ref().and_then(|m| m.get(u).cloned()),
                        })
                        .collect(),
                    both_allowed: c.both_allowed(),
                    reason: c.reason.map(|r| match r {
                        tw_sync::record::Reason::KeyInUse => tw_api::SyncConflictReason::KeyInUse,
                        tw_sync::record::Reason::Invalid => tw_api::SyncConflictReason::Invalid,
                    }),
                    options: engine::options(c)
                        .into_iter()
                        .map(|t| match t {
                            Take::Local => tw_api::SyncTake::Local,
                            Take::Remote => tw_api::SyncTake::Remote,
                            Take::Both => tw_api::SyncTake::Both,
                        })
                        .collect(),
                }
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────── 同步位置上的备份

impl Sync {
    /// 读同步位置用的引擎：开着同步就是在跑的那一个。**安全模式下**在服务的是一份临时配置，
    /// 没有 `sync` 一节：照磁盘上那份读不进来的原文里的 `sync` 一节临时建一个
    async fn with_engine<T>(
        &self,
        f: impl AsyncFnOnce(&mut Eng) -> Result<T, RoundError>,
    ) -> Option<Result<T, RoundError>> {
        let mut guard = self.run.lock().await;
        if let Ok(Some(r)) = self.ensure(&mut guard) {
            return Some(f(&mut r.engine).await);
        }
        drop(guard);
        let mgr = self.mgr()?;
        let text = mgr.current().ok()?.text;
        let (sync, proxies) = tw_config::sync::from_raw(&text)?;
        let cfg = tw_config::Config {
            proxies,
            ..Default::default()
        };
        let store = store_for(&cfg, &sync.target).ok()?;
        let mut e = Engine::new(
            store,
            CoreHost {
                mgr: Arc::downgrade(&mgr),
            },
            &crate::plugins::dir_of(mgr.path()),
            settings_of(&cfg, &sync),
        );
        Some(f(&mut e).await)
    }

    /// 同步位置上读得出来的版本。没开同步、连不上是空的
    pub async fn remote_versions(&self) -> Vec<RemoteVersionInfo> {
        let got = self
            .with_engine(async |e: &mut Eng| e.remote_versions().await)
            .await;
        match got {
            Some(Ok(vs)) => vs
                .iter()
                .map(|v| RemoteVersionInfo {
                    content_id: v.content.clone(),
                    at_ms: v.pack.manifest.created_at_ms,
                    device_name: v.meta.device_name.clone(),
                    core_version: v.pack.manifest.core_version.clone(),
                    counts: v.pack.manifest.counts,
                })
                .collect(),
            Some(Err(e)) => {
                tracing::debug!(
                    "the sync location could not be listed for backups: {}",
                    e.msg()
                );
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// 同步位置上内容标识是这个的版本（不加密的配置包，不含本机设置）
    pub async fn remote_pack(&self, content_id: &str) -> Result<Option<Pack>, Msg> {
        let id = content_id.to_string();
        match self
            .with_engine(async move |e: &mut Eng| e.remote_version(&id).await)
            .await
        {
            Some(Ok(p)) => Ok(p),
            Some(Err(e)) => Err(e.msg()),
            None => Ok(None),
        }
    }
}

/// 备份列表看同步位置的那一扇门（`crate::backups::RemoteBackups`）。开着同步时装在配置管理上
/// （[`ConfigManager::set_remote_backups`]），关掉时拿掉
struct Backups(Weak<Sync>);

impl RemoteBackups for Backups {
    fn list(&self) -> BoxFuture<'_, Vec<RemoteVersionInfo>> {
        Box::pin(async move {
            match self.0.upgrade() {
                Some(s) => s.remote_versions().await,
                None => Vec::new(),
            }
        })
    }

    fn fetch<'a>(&'a self, content_id: &'a str) -> BoxFuture<'a, Result<Pack, Msg>> {
        Box::pin(async move {
            let Some(s) = self.0.upgrade() else {
                return Err(msg!("sync.shutting_down" => "the core is shutting down"));
            };
            s.remote_pack(content_id).await?.ok_or_else(|| {
                msg!(
                    "sync.version_gone", id = content_id =>
                    "the sync location no longer has version {id}"
                )
            })
        })
    }

    fn other_devices(&self) -> BoxFuture<'_, u32> {
        Box::pin(async move {
            self.0
                .upgrade()
                .map(|s| s.view().devices.iter().filter(|d| !d.this).count() as u32)
                .unwrap_or(0)
        })
    }
}

impl Sync {
    /// 开着同步（配置里有 `sync` 一节，安全模式下原文里有）就把这扇门装上，没开就拿掉
    fn register_backups(&self, on: bool) {
        let Some(mgr) = self.mgr() else { return };
        let have = mgr.remote_backups().is_some();
        if on && !have {
            mgr.set_remote_backups(Some(Arc::new(Backups(self.me.clone()))));
        } else if !on && have {
            mgr.set_remote_backups(None);
        }
    }
}

// ─────────────────────────────────────────────────────────── 调度

/// 起同步的后台任务：启动时一轮，之后按上面那张表。`safe` 是安全模式：不自动同步，状态是
/// `paused`，备份列表照样能读同步位置
pub fn start(s: &ControlState, safe: bool) -> Arc<Sync> {
    let sync = service(s);
    if safe {
        sync.paused.store(true, Ordering::Relaxed);
        // 出错页的「从备份恢复」要列同步位置上的版本：原文里有同步设置就把门装上
        let on = s.config().sync.is_some()
            || s.cfg
                .current()
                .ok()
                .and_then(|l| tw_config::sync::from_raw(&l.text))
                .is_some();
        sync.register_backups(on);
        sync.publish(sync.compute_view(None, None));
        return sync;
    }
    if sync.started.swap(true, Ordering::Relaxed) {
        return sync;
    }
    let bus = s.bus().clone();
    let me = sync.clone();
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        // 从睡眠中醒来：墙上的钟比单调钟多走了一大截
        let mut wall = std::time::SystemTime::now();
        let mut mono = tokio::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next = tokio::time::Instant::now();
        loop {
            let dirty = me.dirty.lock().ok().and_then(|d| *d);
            let due = match dirty {
                Some(d) if d < next => d,
                _ => next,
            };
            tokio::select! {
                _ = me.kick.notified() => {
                    // 本机刚改了配置：等它稳下来
                    if let Some(d) = me.dirty.lock().ok().and_then(|d| *d)
                        && d > tokio::time::Instant::now()
                    {
                        continue;
                    }
                }
                _ = tokio::time::sleep_until(due) => {}
                ev = rx.recv() => {
                    match ev {
                        Ok(tw_api::Event::ConfigReloaded { origin, .. })
                            if origin != tw_api::ConfigOrigin::Sync => me.touch(),
                        Ok(tw_api::Event::ClientPrefsChanged { .. }) => me.touch(),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        _ => {}
                    }
                    continue;
                }
                _ = tick.tick() => {
                    let wall_gap = std::time::SystemTime::now()
                        .duration_since(wall)
                        .unwrap_or_default();
                    let mono_gap = mono.elapsed();
                    wall = std::time::SystemTime::now();
                    mono = tokio::time::Instant::now();
                    if wall_gap < mono_gap + Duration::from_secs(120) {
                        continue;
                    }
                    tracing::debug!("woke from sleep; checking the sync location");
                }
            }
            if me.mgr().is_none() {
                return;
            }
            let _ = me.round(&[]).await;
            next = tokio::time::Instant::now() + me.next_poll();
        }
    });
    sync.kick();
    sync
}

// ─────────────────────────────────────────────────────────── 加入

/// 本机配置还是初始配置（新安装后没改过）：只有一把网关密钥，没有上游、路由这些，全局设置
/// 都是默认值。随 core 发的默认插件（停用着）不算
fn is_initial(snap: &Snapshot) -> bool {
    let defaults: BTreeSet<&str> = tw_gateway::plugin::defaults::ALL
        .iter()
        .map(|(id, _)| *id)
        .collect();
    let mut keys = 0;
    for (id, it) in &snap.items {
        match id.kind {
            ItemKind::Key => keys += 1,
            ItemKind::ClientPrefs | ItemKind::PluginOrder => {}
            ItemKind::Plugin
                if defaults.contains(id.name.as_str())
                    && it.units.get(model::unit::ENABLED)
                        != Some(&serde_json::Value::Bool(true)) => {}
            _ => return false,
        }
    }
    keys <= 1
}

/// 加入时「合并」：和导入同一套规矩（`tw_pack::import`），本机打底，同步数据逐项并进来。
/// 新增的默认导入，冲突的必须选（覆盖、改名导入、跳过），没选是 `sync.join_undecided`。
///
/// **三处和导入不一样**：导入进来的插件按导入的规矩是停用的，加入同步时照同步数据里的开关
/// （各设备一致，见方案 6.2；不加密时同步一轮里另有停用的规矩）；导入预览里只有本机有的
/// 项目不出现，这里也一样 —— 它们会随这次加入上传；两边的默认网关密钥同名、值不同时不用选，
/// 采用同步数据的值（[`engine::adopted_default_key`]，给了别的选择也不看）
fn merge_join(
    local: &Pack,
    remote: &Pack,
    decisions: &[tw_api::ImportDecision],
) -> Result<Pack, Fail> {
    let ctx = crate::backups::import_ctx();
    let rows = tw_pack::import::preview(local, remote, &ctx).map_err(|e| import_fail(&e))?;
    let decided: BTreeMap<ItemId, &tw_api::ImportDecision> = decisions
        .iter()
        .map(|d| (crate::backups::item_id(&d.item), d))
        .collect();
    let adopt = model::snapshot(local)
        .ok()
        .and_then(|l| adopted(&l, remote));
    let mut out = Vec::new();
    for r in &rows {
        if adopt.as_ref() == Some(&r.item) {
            out.push(tw_pack::import::Decision {
                item: r.item.clone(),
                action: tw_pack::import::Action::Overwrite,
            });
            continue;
        }
        let action = match decided.get(&r.item) {
            Some(d) => match d.action {
                tw_api::ImportAction::Import => tw_pack::import::Action::Import,
                tw_api::ImportAction::Overwrite => tw_pack::import::Action::Overwrite,
                tw_api::ImportAction::Rename => {
                    tw_pack::import::Action::Rename(d.new_name.clone().unwrap_or_default())
                }
                tw_api::ImportAction::Skip => tw_pack::import::Action::Skip,
            },
            None => match r.status {
                tw_pack::import::Status::New => tw_pack::import::Action::Import,
                tw_pack::import::Status::Same | tw_pack::import::Status::Error => {
                    tw_pack::import::Action::Skip
                }
                tw_pack::import::Status::Conflict => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        msg!(
                            "sync.join_undecided", item = &r.item =>
                            "{item} differs between this device and the sync data; choose to \
                             overwrite, import under a new name, or skip"
                        ),
                    ));
                }
            },
        };
        out.push(tw_pack::import::Decision {
            item: r.item.clone(),
            action,
        });
    }
    let applied = tw_pack::import::apply(local, remote, &out, &ctx).map_err(|e| import_fail(&e))?;
    // 插件照同步数据里的开关，不按导入的规矩停用
    let mut snap =
        model::snapshot(&applied.pack).map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
    let theirs = model::snapshot(remote).map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
    let mut changed = false;
    for d in &out {
        if d.item.kind != ItemKind::Plugin || matches!(d.action, tw_pack::import::Action::Skip) {
            continue;
        }
        let name = match &d.action {
            tw_pack::import::Action::Rename(n) => n.clone(),
            _ => d.item.name.clone(),
        };
        let id = ItemId::new(ItemKind::Plugin, name);
        let want = theirs
            .items
            .get(&d.item)
            .and_then(|it| it.units.get(model::unit::ENABLED))
            .cloned();
        if let (Some(it), Some(want)) = (snap.items.get_mut(&id), want)
            && it.units.get(model::unit::ENABLED) != Some(&want)
        {
            it.units.insert(model::unit::ENABLED.to_string(), want);
            changed = true;
        }
    }
    if !changed {
        return Ok(applied.pack);
    }
    snap.relink();
    model::materialize(local, &snap).map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))
}

fn import_fail(e: &tw_pack::import::ImportError) -> Fail {
    use tw_pack::import::ImportError as E;
    let code = match e {
        E::Exists(_) | E::NameTaken(..) => StatusCode::CONFLICT,
        E::NotInPack(_) => StatusCode::NOT_FOUND,
        E::Item(..) => StatusCode::UNPROCESSABLE_ENTITY,
        E::Model(_) | E::Rejected(_) | E::TooNew(_) => StatusCode::BAD_REQUEST,
    };
    fail(code, e.msg())
}

// ─────────────────────────────────────────────────────────── 端点

async fn status(State(s): State<ControlState>) -> Json<tw_api::SyncView> {
    Json(service(&s).view())
}

fn config_target(
    s: &ControlState,
    t: &tw_api::SyncTarget,
) -> Result<(tw_config::sync::Target, AnyStore), Fail> {
    let target = target_to_config(t);
    let store = store_for(&s.config(), &target).map_err(|m| fail(StatusCode::BAD_REQUEST, m))?;
    Ok((target, store))
}

fn device_view(d: &tw_sync::device::DeviceRecord, me: Option<&str>) -> tw_api::SyncDevice {
    tw_api::SyncDevice {
        id: d.id.clone(),
        name: d.name.clone(),
        core_version: d.core_version.clone(),
        last_seen_ms: d.last_seen_ms,
        this: Some(d.id.as_str()) == me,
        paused_update: d.paused_update,
        remote_core: d.remote_core,
    }
}

async fn test(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncTestRequest>,
) -> Result<Json<tw_api::SyncProbe>, Fail> {
    let (_, store) = config_target(&s, &req.target)?;
    let p = Eng::probe(&store, req.passphrase.as_deref()).await;
    let me = s.config().sync.as_ref().map(|y| y.device_id.clone());
    Ok(Json(tw_api::SyncProbe {
        writable: p.writable.is_ok(),
        error: p.writable.err().map(|e| e.msg()),
        existing: p.existing.map(|e| tw_api::SyncExisting {
            encrypted: e.encrypted,
            unlocked: e.unlocked,
            devices: e
                .devices
                .iter()
                .map(|d| device_view(d, me.as_deref()))
                .collect(),
        }),
    }))
}

/// 一台还没开同步的设备对着某个同步位置的引擎（加入预览、开启）
fn temp_engine(
    s: &ControlState,
    target: &tw_config::sync::Target,
    store: AnyStore,
    sync: &tw_config::sync::Sync,
) -> Eng {
    let _ = target;
    Engine::new(
        store,
        CoreHost {
            mgr: Arc::downgrade(&s.cfg),
        },
        &crate::plugins::dir_of(s.config_path()),
        settings_of(&s.config(), sync),
    )
}

fn draft(
    target: tw_config::sync::Target,
    encrypt: bool,
    passphrase: Option<String>,
    device_name: &str,
) -> tw_config::sync::Sync {
    tw_config::sync::Sync {
        device_id: tw_config::sync::generate_device_id(),
        device_name: device_name.trim().to_string(),
        encrypt,
        passphrase,
        exclude: Vec::new(),
        target,
    }
}

async fn join_preview(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncTestRequest>,
) -> Result<Json<tw_api::SyncJoinPreview>, Fail> {
    let (target, store) = config_target(&s, &req.target)?;
    let cur = current_pack(&s.cfg).map_err(|m| fail(StatusCode::INTERNAL_SERVER_ERROR, m))?;
    let local = model::snapshot(&cur.pack).map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
    let initial = is_initial(&local);
    let probe = Eng::probe(&store, req.passphrase.as_deref()).await;
    if probe.existing.is_none() {
        return Ok(Json(tw_api::SyncJoinPreview {
            initial,
            rows: Vec::new(),
            auto: Vec::new(),
        }));
    }
    let encrypted = probe.existing.as_ref().is_some_and(|e| e.encrypted);
    let mut e = temp_engine(
        &s,
        &target,
        store,
        &draft(target.clone(), encrypted, req.passphrase.clone(), "preview"),
    );
    let remote = match e.latest().await {
        Ok(r) => r,
        // 初始配置不出现合并这一步：读不到同步数据（口令还没给）也照样答
        Err(_) if initial => {
            return Ok(Json(tw_api::SyncJoinPreview {
                initial,
                rows: Vec::new(),
                auto: Vec::new(),
            }));
        }
        Err(e) => return Err(round_fail(&e)),
    };
    let auto = adopted(&local, &remote)
        .map(|id| tw_api::SyncJoinAuto {
            item: item_ref(&id),
            action: tw_api::ImportAction::Overwrite,
            reason: tw_api::SyncJoinAutoReason::DefaultKey,
        })
        .into_iter()
        .collect();
    if initial {
        return Ok(Json(tw_api::SyncJoinPreview {
            initial,
            rows: Vec::new(),
            auto,
        }));
    }
    // 和导入预览同一套规矩
    let rows = tw_pack::import::preview(&cur.pack, &remote, &crate::backups::import_ctx())
        .map_err(|e| import_fail(&e))?;
    Ok(Json(tw_api::SyncJoinPreview {
        initial,
        rows: rows.into_iter().map(crate::backups::row_view).collect(),
        auto,
    }))
}

/// 加入时不用选、采用同步数据的那一把默认网关密钥（[`engine::adopted_default_key`]）
fn adopted(local: &Snapshot, remote: &Pack) -> Option<ItemId> {
    let remote = model::snapshot(remote).ok()?;
    engine::adopted_default_key(local, &remote)
}

async fn enable(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncEnable>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let sync = service(&s);
    let mut guard = sync.run.lock().await;
    if s.config().sync.is_some() {
        return Err(fail(
            StatusCode::CONFLICT,
            msg!("sync.already_on" => "sync is already on. Stop it before using another location"),
        ));
    }
    let (target, store) = config_target(&s, &req.target)?;
    let probe = Eng::probe(&store, req.passphrase.as_deref()).await;
    if let Err(e) = &probe.writable {
        return Err(round_fail(&RoundError::Store(e.clone())));
    }
    let mut settings = match &probe.existing {
        None => {
            let pass = if req.encrypt {
                Some(check_passphrase(req.passphrase.as_deref())?.to_string())
            } else {
                None
            };
            draft(target.clone(), req.encrypt, pass, &req.device_name)
        }
        Some(e) => {
            if e.encrypted && !e.unlocked {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    tw_sync::keyring::KeyringError::WrongPassphrase.msg(),
                ));
            }
            draft(
                target.clone(),
                e.encrypted,
                e.encrypted.then(|| req.passphrase.clone()).flatten(),
                &req.device_name,
            )
        }
    };
    settings
        .check()
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
    let mut engine = temp_engine(&s, &target, store, &settings);
    match &probe.existing {
        None => {
            if !engine.create().await.map_err(|e| round_fail(&e))? {
                return Err(fail(
                    StatusCode::CONFLICT,
                    msg!(
                        "sync.location_taken" =>
                        "another device has just started syncing at this location. Check the \
                         location again and join it"
                    ),
                ));
            }
        }
        Some(_) => {
            let cur =
                current_pack(&s.cfg).map_err(|m| fail(StatusCode::INTERNAL_SERVER_ERROR, m))?;
            let local =
                model::snapshot(&cur.pack).map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
            let merged = if is_initial(&local) || req.join == tw_api::SyncJoin::UseRemote {
                None
            } else {
                let remote_pack = engine.latest().await.map_err(|e| round_fail(&e))?;
                Some(merge_join(&cur.pack, &remote_pack, &req.decisions)?)
            };
            let report = engine.join(merged).await.map_err(|e| round_fail(&e))?;
            if !report.keys_changed.is_empty() {
                s.bus().emit(tw_api::Event::GatewayKeysChanged {
                    id: s.bus().next_id(),
                    names: report.keys_changed.clone(),
                    at_ms: now_ms(),
                });
            }
            settings.encrypt = report.encrypted;
        }
    }
    let version = write_section(&s.cfg, Some(&settings), Origin::Ui)
        .await
        .map_err(apply_fail)?;
    let cfg = s.config();
    *guard = Some(Running {
        engine,
        config: settings,
        proxies: format!("{:?}", cfg.proxies),
        _watch: None,
    });
    // 文件夹的监听在下一轮由 ensure 补上：重建一次
    if let Some(r) = guard.as_mut() {
        r.proxies.clear();
    }
    drop(guard);
    sync.failures.store(0, Ordering::Relaxed);
    sync.register_backups(true);
    sync.publish(sync.compute_view(None, None));
    sync.kick();
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn save(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncSettingsSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let sync = service(&s);
    let _guard = sync.run.lock().await;
    let Some(mut cur) = s.config().sync.clone() else {
        return Err(sync_off());
    };
    if let Some(t) = &req.target {
        let t = target_to_config(t);
        if !t.same_place(&cur.target) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "sync.place_changed" =>
                    "this is a different sync location. Stop syncing, then turn sync on with the \
                     new location"
                ),
            ));
        }
        cur.target = t;
    }
    if let Some(n) = &req.device_name {
        cur.device_name = n.trim().to_string();
    }
    if let Some(ex) = &req.excluded {
        let mut v: Vec<String> = ex.iter().map(|e| e.trim().to_string()).collect();
        v.dedup();
        cur.exclude = v;
    }
    cur.check()
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))?;
    let version = write_section(&s.cfg, Some(&cur), Origin::Ui)
        .await
        .map_err(apply_fail)?;
    drop(_guard);
    sync.kick();
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn now(State(s): State<ControlState>) -> Result<Json<tw_api::SyncView>, Fail> {
    let sync = service(&s);
    if s.config().sync.is_none() {
        return Ok(Json(sync.view()));
    }
    sync.round(&[]).await.map(Json)
}

async fn conflicts(State(s): State<ControlState>) -> Json<Vec<tw_api::SyncConflict>> {
    Json(service(&s).conflicts().await)
}

async fn resolve(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncResolve>,
) -> Result<Json<tw_api::SyncView>, Fail> {
    if s.config().sync.is_none() {
        return Err(sync_off());
    }
    let decisions: Vec<Decision> = req
        .decisions
        .iter()
        .map(|d| Decision {
            id: d.id.clone(),
            take: match d.take {
                tw_api::SyncTake::Local => Take::Local,
                tw_api::SyncTake::Remote => Take::Remote,
                tw_api::SyncTake::Both => Take::Both,
            },
            new_name: d.new_name.clone(),
        })
        .collect();
    let sync = service(&s);
    let v = sync.round(&decisions).await?;
    if let Some(e) = v
        .error
        .as_ref()
        .filter(|_| v.state != tw_api::SyncState::Conflict && v.state != tw_api::SyncState::Idle)
    {
        return Err(fail(StatusCode::BAD_GATEWAY, e.clone()));
    }
    Ok(Json(v))
}

async fn encryption(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncEncryption>,
) -> Result<Json<tw_api::SyncView>, Fail> {
    let sync = service(&s);
    let mut guard = sync.run.lock().await;
    let r = match sync.ensure(&mut guard) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(sync_off()),
        Err(m) => return Err(fail(StatusCode::BAD_REQUEST, m)),
    };
    let mut next = r.config.clone();
    let res = match req.action {
        tw_api::SyncEncryptionAction::Enable => {
            let p = check_passphrase(req.passphrase.as_deref())?.to_string();
            next.encrypt = true;
            next.passphrase = Some(p.clone());
            r.engine.enable_encryption(&p).await
        }
        tw_api::SyncEncryptionAction::Disable => {
            // 口令留着：同步位置上的 keyring 写着「已关闭」，靠它认出来
            let p = req.passphrase.clone().unwrap_or_default();
            next.encrypt = false;
            next.passphrase = Some(p.clone()).filter(|p| !p.is_empty());
            r.engine.disable_encryption(&p).await
        }
        tw_api::SyncEncryptionAction::Change => {
            let new = check_passphrase(req.new_passphrase.as_deref())?.to_string();
            let old = req.passphrase.clone().unwrap_or_default();
            next.passphrase = Some(new.clone());
            r.engine.change_passphrase(&old, &new).await
        }
    };
    res.map_err(|e| round_fail(&e))?;
    r.config = next.clone();
    r.engine.set_settings(settings_of(&s.config(), &next));
    drop(guard);
    write_section(&s.cfg, Some(&next), Origin::Ui)
        .await
        .map_err(apply_fail)?;
    sync.round(&[]).await.map(Json)
}

async fn unlock(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncUnlock>,
) -> Result<Json<tw_api::SyncView>, Fail> {
    let sync = service(&s);
    let mut guard = sync.run.lock().await;
    let r = match sync.ensure(&mut guard) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(sync_off()),
        Err(m) => return Err(fail(StatusCode::BAD_REQUEST, m)),
    };
    let encrypted = r
        .engine
        .unlock(&req.passphrase)
        .await
        .map_err(|e| round_fail(&e))?;
    let mut next = r.config.clone();
    next.encrypt = encrypted;
    next.passphrase = Some(req.passphrase.clone());
    r.config = next.clone();
    r.engine.set_settings(settings_of(&s.config(), &next));
    drop(guard);
    write_section(&s.cfg, Some(&next), Origin::Ui)
        .await
        .map_err(apply_fail)?;
    sync.round(&[]).await.map(Json)
}

/// 同步位置空了、keyring 不见了之后，由人选的「重新上传本机配置」
async fn republish(State(s): State<ControlState>) -> Result<Json<tw_api::SyncView>, Fail> {
    let sync = service(&s);
    let mut guard = sync.run.lock().await;
    let r = match sync.ensure(&mut guard) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(sync_off()),
        Err(m) => return Err(fail(StatusCode::BAD_REQUEST, m)),
    };
    let res = r.engine.republish().await;
    drop(guard);
    if let Err(e) = res {
        return Err(round_fail(&e));
    }
    sync.failures.store(0, Ordering::Relaxed);
    sync.round(&[]).await.map(Json)
}

async fn disable(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::SyncDisable>,
) -> Result<Json<()>, Fail> {
    let sync = service(&s);
    let mut guard = sync.run.lock().await;
    if let Ok(Some(r)) = sync.ensure(&mut guard) {
        match r.engine.stop(req.purge).await {
            Ok(()) => {}
            // 只是移除自己的设备记录时连不上也照样停：半年之后它自己会过期
            Err(e) if !req.purge => {
                tracing::warn!(
                    "the device record could not be removed from the sync location: {}",
                    e.msg()
                );
                r.engine.local().clear();
            }
            Err(e) => return Err(round_fail(&e)),
        }
    } else if let Some(dir) = sync.data_dir() {
        tw_sync::local::Local::new(&dir).clear();
    }
    *guard = None;
    drop(guard);
    if s.config().sync.is_some() {
        write_section(&s.cfg, None, Origin::Ui)
            .await
            .map_err(apply_fail)?;
    }
    if let Ok(mut m) = sync.url_changed.lock() {
        m.clear();
    }
    sync.register_backups(false);
    sync.publish(sync.compute_view(None, None));
    Ok(Json(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_convert_both_ways_and_blank_proxies_are_direct() {
        let api = tw_api::SyncTarget::S3 {
            endpoint: " https://abc.r2.cloudflarestorage.com ".into(),
            region: "auto".into(),
            bucket: "tw".into(),
            prefix: "/me/".into(),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            proxy: String::new(),
        };
        let c = target_to_config(&api);
        assert_eq!(c.proxy(), "direct");
        match target_to_api(&c) {
            tw_api::SyncTarget::S3 {
                endpoint, prefix, ..
            } => {
                assert_eq!(endpoint, "https://abc.r2.cloudflarestorage.com");
                assert_eq!(prefix, "me");
            }
            other => panic!("{other:?}"),
        }
    }
}

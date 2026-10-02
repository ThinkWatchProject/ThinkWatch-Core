//! 脚本插件：装、改、换源码、批准、排顺序、删、试跑、日志。
//!
//! # 文件和配置是一件事的两半
//!
//! 插件文件（`plugins/<id>.js`）和它的底稿（`plugins/.approved/<id>.js`）由这里写，
//! 批准的哈希在配置里。**先写文件、再写配置**：配置一落盘，网关就照它重读文件、比
//! 哈希（不变式 I9）。配置没写成（版本对不上、校验没过），刚写的文件按写之前的样子
//! 还原 —— 不留下一个和配置对不上的插件文件。整个过程攥着 `Plugins::edits`，目录
//! 监听不会落在两半之间。
//!
//! # 四个端点网页调不了
//!
//! 装（`CreatePlugin`）、换源码（`ReplacePluginSource`）、批准改过的文件
//! （`ApprovePluginFile`）**不在桌面端网页的 `call` 白名单里**（不变式 I12）：这三件事
//! 要在系统的确认框里点头，那一步在桌面端的 Rust 里，它自己再编一遍源码，把名字、
//! 权限和哈希摆给人看。所以这里不假设调用方看过什么：源码在这里再编一遍，批准时
//! 磁盘上的文件得正好是调用方看过的那一份（哈希核对）。
//!
//! 第四个是**确认过的改动**（`UpdatePluginConfirmed`）。改得了回答里工具调用的插件
//! （`reply_tool_calls`）决定客户端执行什么：网页里注入的脚本要是能打开它、改它的设置
//! 或范围，就能借它改客户端要跑的命令。所以 `UpdatePlugin`（网页调得到）对这种插件只做
//! 停用、改出错时怎么办，打开、改设置、改范围要走确认过的那一条。**读不出权限的插件按
//! 改得了算**：它此刻跑不了，可一旦又跑得了（运行时恢复了），网页替它打开的开关就生效了。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde_yaml_ng::{Mapping, Value};
use tw_api::{SettingValue, ep};
use tw_config::edit::{self, EditError};
use tw_config::history::Origin;
use tw_gateway::plugin::load::{read_capped, sha256_hex};
use tw_gateway::plugin::{Active, Broken, LoadError, Manifest};
use tw_types::{Msg, msg};

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail, internal};

pub mod defaults;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Plugins, list)
        .at(ep::PluginInspect, inspect)
        .at(ep::CreatePlugin, create)
        .at(ep::ReorderPlugins, reorder)
        .at(ep::UpdatePlugin, update)
        .at(ep::UpdatePluginConfirmed, update_confirmed)
        .at(ep::DeletePlugin, delete)
        .at(ep::ReplacePluginSource, replace_source)
        .at(ep::PluginSourceDiff, source_diff)
        .at(ep::ApprovePluginFile, approve)
        .at(ep::TrialPlugin, trial)
        .at(ep::PluginLogs, logs)
}

/// 配置文件所在的目录：插件文件的路径相对它。**远程 core 也一样** —— 文件在 core
/// 那台机器上，由 core 写
pub fn dir_of(config: &Path) -> PathBuf {
    match config.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn config_dir(s: &ControlState) -> PathBuf {
    dir_of(s.config_path())
}

fn not_found(id: &str) -> Fail {
    fail(
        StatusCode::NOT_FOUND,
        msg!(
            "control.plugin.not_found", plugin = id =>
            "There is no plugin `{plugin}`."
        ),
    )
}

/// 配置里没有这个插件（在 `transform` 里，配置是磁盘上那一份）
fn missing(id: &str) -> ApplyError {
    ApplyError::Edit(EditError::NotFound {
        what: "plugin",
        name: id.to_string(),
    })
}

// ---------------------------------------------------------------- 读

async fn list(State(s): State<ControlState>) -> Json<Vec<tw_api::PluginView>> {
    let rt = s.gateway.runtime();
    Json(
        rt.plugins
            .all()
            .iter()
            .filter_map(|a| {
                let entry = rt.config.plugins.iter().find(|p| p.id == a.id)?;
                Some(view(a, entry))
            })
            .collect(),
    )
}

fn view(a: &Active, entry: &tw_config::Plugin) -> tw_api::PluginView {
    let m = a.manifest.as_ref();
    // 交给插件的那一份（默认值补齐了）；插件跑不了、没算出来时就照配置里写的说
    let settings = if a.settings.is_empty() {
        entry
            .settings
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), from_yaml(v)?)))
            .collect()
    } else {
        a.settings
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), from_json(v)?)))
            .collect()
    };
    tw_api::PluginView {
        id: a.id.clone(),
        name: a.name.clone(),
        description: m.and_then(|m| m.description.clone()),
        enabled: a.enabled,
        on_error: a.on_error,
        permissions: a.permissions.clone(),
        scope: scope_view(&entry.scope),
        reply_mode: a.reply_mode,
        settings_schema: m.map(schema).unwrap_or_default(),
        settings,
        sha256: entry.sha256.clone(),
        status: status_of(a),
        stats: a.stats.view(),
    }
}

/// 状态：跑不了的原因优先于「停用」—— 停用着的插件文件被人改了，也要看得出来
fn status_of(a: &Active) -> tw_api::PluginStatus {
    match a.broken() {
        Some(Broken::Changed) => tw_api::PluginStatus::Changed,
        Some(Broken::Error(m)) => tw_api::PluginStatus::Error { message: m.clone() },
        None if a.enabled => tw_api::PluginStatus::Ok,
        None => tw_api::PluginStatus::Disabled,
    }
}

fn scope_view(s: &tw_config::PluginScope) -> tw_api::PluginScope {
    tw_api::PluginScope {
        clients: s.clients.clone(),
        models: s.models.clone(),
        upstreams: s.upstreams.clone(),
    }
}

fn schema(m: &Manifest) -> Vec<tw_api::SettingSpecView> {
    m.settings
        .iter()
        .map(|s| tw_api::SettingSpecView {
            key: s.key.clone(),
            kind: s.kind,
            label: s.label.clone(),
            default: from_json(&s.default).unwrap_or(SettingValue::String(String::new())),
        })
        .collect()
}

fn manifest_view(m: &Manifest) -> tw_api::ManifestView {
    tw_api::ManifestView {
        name: m.name.clone(),
        description: m.description.clone(),
        permissions: m.permissions.clone(),
        scope: tw_api::PluginScope {
            clients: m.scope.clients.clone(),
            models: m.scope.models.clone(),
            upstreams: m.scope.upstreams.clone(),
        },
        reply_mode: m.reply_mode,
        settings_schema: schema(m),
        hooks: tw_api::PluginHooks {
            request: m.hooks.request,
            reply_text: m.hooks.reply_text,
            tool_call: m.hooks.tool_call,
        },
    }
}

fn from_json(v: &serde_json::Value) -> Option<SettingValue> {
    match v {
        serde_json::Value::Bool(b) => Some(SettingValue::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(SettingValue::Number),
        serde_json::Value::String(s) => Some(SettingValue::String(s.clone())),
        _ => None,
    }
}

fn from_yaml(v: &Value) -> Option<SettingValue> {
    match v {
        Value::Bool(b) => Some(SettingValue::Bool(*b)),
        Value::Number(n) => n.as_f64().map(SettingValue::Number),
        Value::String(s) => Some(SettingValue::String(s.clone())),
        _ => None,
    }
}

/// 写进配置的样子。**整数写成整数**：界面交来的数字一律是 f64，`3` 不该变成 `3.0`
fn to_yaml(v: &SettingValue) -> Value {
    match v {
        SettingValue::Bool(b) => Value::Bool(*b),
        SettingValue::Number(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => {
            Value::Number((*f as i64).into())
        }
        SettingValue::Number(f) => Value::Number((*f).into()),
        SettingValue::String(s) => Value::String(s.clone()),
    }
}

/// 一份源码编出来的样子。**不留任何东西**
async fn inspect(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PluginSource>,
) -> Result<Json<tw_api::PluginInspection>, Fail> {
    let sha256 = sha256_hex(req.source.as_bytes());
    Ok(Json(
        match load(&s, req.source.into_bytes(), false).await? {
            Ok(m) => tw_api::PluginInspection {
                manifest: Some(manifest_view(&m)),
                sha256,
                error: None,
            },
            Err(e) => tw_api::PluginInspection {
                manifest: None,
                sha256,
                error: Some(load_error(&e)),
            },
        },
    ))
}

fn load_error(e: &LoadError) -> tw_api::PluginLoadError {
    let (line, column) = match e {
        LoadError::Syntax { line, column, .. } => (*line, *column),
        _ => (None, None),
    };
    tw_api::PluginLoadError {
        message: e.msg(),
        line,
        column,
    }
}

/// 编一遍：**放到阻塞线程上**，编译是实打实的 CPU 活。`keep`：结果留进缓存（马上
/// 要装上的那一份），否则什么都不留（只是看看）
async fn load(
    s: &ControlState,
    source: Vec<u8>,
    keep: bool,
) -> Result<Result<Manifest, LoadError>, Fail> {
    let plugins = s.gateway.plugins.clone();
    tokio::task::spawn_blocking(move || {
        let compiled = if keep {
            plugins.prepare(&source)
        } else {
            plugins.inspect(&source)
        };
        compiled.map(|host| host.manifest().clone())
    })
    .await
    .map_err(internal)
}

/// 编一遍，编不成就拒绝这次写入（装、换源码、批准都要编得成）
async fn load_or_refuse(s: &ControlState, source: Vec<u8>) -> Result<Manifest, Fail> {
    load(s, source, true)
        .await?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e.msg()))
}

async fn source_diff(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<tw_api::PluginSourceView>, Fail> {
    let rt = s.gateway.runtime();
    let p = rt
        .config
        .plugins
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| not_found(&id))?;
    let dir = config_dir(&s);
    // 底稿只在它就是批准的那一份时给：被人动过的不能冒充「批准过的」
    let approved = read_capped(&tw_config::plugins::approved_path(&dir, &id))
        .ok()
        .filter(|b| sha256_hex(b) == p.sha256)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let (current, current_sha256) = match read_capped(&p.path_in(&dir)) {
        Ok(b) => (
            Some(String::from_utf8_lossy(&b).into_owned()),
            Some(sha256_hex(&b)),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(e) => return Err(unreadable(&p.file, e)),
    };
    Ok(Json(tw_api::PluginSourceView {
        approved,
        approved_sha256: p.sha256.clone(),
        current,
        current_sha256,
    }))
}

fn unreadable(file: &str, e: std::io::Error) -> Fail {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        msg!(
            "control.plugin.unreadable", file = file, detail = e =>
            "The plugin file {file} cannot be read: {detail}"
        ),
    )
}

async fn logs(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Vec<tw_api::PluginLogEntry>>, Fail> {
    let rt = s.gateway.runtime();
    let a = rt.plugins.get(&id).ok_or_else(|| not_found(&id))?;
    Ok(Json(a.logs.lines()))
}

// ---------------------------------------------------------------- 写

/// 配置里的一条。字段的顺序就是写进文件的顺序
fn entry(
    id: &str,
    sha256: &str,
    enabled: bool,
    on_error: tw_api::OnError,
    scope: &tw_api::PluginScope,
    settings: &BTreeMap<String, SettingValue>,
) -> Mapping {
    let mut m = Mapping::new();
    m.insert("id".into(), id.into());
    m.insert("file".into(), tw_config::Plugin::file_for(id).into());
    m.insert("sha256".into(), sha256.into());
    m.insert("enabled".into(), enabled.into());
    m.insert("on_error".into(), on_error.slug().into());
    let mut sc = Mapping::new();
    for (key, list) in [
        ("clients", &scope.clients),
        ("models", &scope.models),
        ("upstreams", &scope.upstreams),
    ] {
        if !list.is_empty() {
            sc.insert(
                key.into(),
                Value::Sequence(list.iter().map(|x| Value::from(x.trim())).collect()),
            );
        }
    }
    if !sc.is_empty() {
        m.insert("scope".into(), Value::Mapping(sc));
    }
    if !settings.is_empty() {
        m.insert(
            "settings".into(),
            Value::Mapping(
                settings
                    .iter()
                    .map(|(k, v)| (Value::from(k.as_str()), to_yaml(v)))
                    .collect(),
            ),
        );
    }
    m
}

/// 范围里不能有空着的一项（和配置校验同一条）。**写文件之前查**
fn check_scope(scope: &tw_api::PluginScope) -> Result<(), Fail> {
    let blank = scope
        .clients
        .iter()
        .chain(&scope.models)
        .chain(&scope.upstreams)
        .any(|x| x.trim().is_empty());
    if blank {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!(
                "control.plugin.blank_pattern" =>
                "A scope entry is empty. Remove it, or write a name or a pattern with *."
            ),
        ));
    }
    Ok(())
}

/// 交上来的设置对着 manifest 查：插件没声明的键、类型不对的值都拒绝；没给的补上默认
/// 值 —— **配置里每个设置都写明**。和网关加载时同一套判据
fn settings_for(
    m: &Manifest,
    given: &BTreeMap<String, SettingValue>,
) -> Result<BTreeMap<String, SettingValue>, Fail> {
    let all = tw_gateway::plugin::load::settings_of(m, given)
        .map_err(|why| fail(StatusCode::BAD_REQUEST, why))?;
    Ok(all
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), from_json(v)?)))
        .collect())
}

/// 换了一份源码之后的设置：**还对得上的留着**（键还在、类型没变），对不上的丢掉，
/// 新声明的补默认值。换源码、批准改过的文件都不该因为设置而让插件跑不了
fn reconcile(m: &Manifest, old: &BTreeMap<String, Value>) -> BTreeMap<String, SettingValue> {
    m.settings
        .iter()
        .map(|spec| {
            let kept = old
                .get(&spec.key)
                .and_then(from_yaml)
                .filter(|v| v.kind() == spec.kind);
            let v = kept
                .or_else(|| from_json(&spec.default))
                .unwrap_or(SettingValue::String(String::new()));
            (spec.key.clone(), v)
        })
        .collect()
}

/// 新插件的 id：给了就查写法和重名，没给就从名字生成一个不重的
fn new_id(given: Option<&str>, name: &str, taken: &[&str]) -> Result<String, Fail> {
    if let Some(id) = given {
        if !tw_config::plugins::valid_id(id) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "control.plugin.bad_id", plugin = id, max = tw_config::plugins::ID_MAX =>
                    "`{plugin}` is not a valid plugin id: lowercase letters, digits and hyphens, 1 \
                     to {max} characters."
                ),
            ));
        }
        if tw_config::plugins::RESERVED_IDS.contains(&id) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                msg!(
                    "control.plugin.reserved_id", plugin = id =>
                    "`{plugin}` cannot be a plugin id: the control plane uses that word itself."
                ),
            ));
        }
        if taken.contains(&id) {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "control.plugin.id_taken", plugin = id =>
                    "There is already a plugin `{plugin}`."
                ),
            ));
        }
        return Ok(id.to_string());
    }
    Ok(id_from_name(name, taken))
}

/// 从名字生成 id：小写的字母和数字，其余的并成一个连字符。**名字里一个拉丁字母都
/// 没有（「附加日期」）就叫 `plugin`**；重了就在后面加 `-2`、`-3`……
fn id_from_name(name: &str, taken: &[&str]) -> String {
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    base.truncate(tw_config::plugins::ID_MAX);
    let mut base = base.trim_end_matches('-').to_string();
    if base.is_empty() || tw_config::plugins::RESERVED_IDS.contains(&base.as_str()) {
        base = if base.is_empty() {
            "plugin".into()
        } else {
            format!("{base}-plugin")
        };
    }
    let free = |id: &str| !taken.contains(&id);
    if free(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let tail = format!("-{n}");
            let mut head = base.clone();
            head.truncate(tw_config::plugins::ID_MAX - tail.len());
            format!("{}{tail}", head.trim_end_matches('-'))
        })
        .find(|id| free(id))
        .unwrap_or(base)
}

/// 写之前的样子，配置没写成时照它还原。
struct Undo(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Undo {
    fn restore(self) {
        for (path, before) in self.0 {
            let r = match before {
                Some(b) => std::fs::write(&path, b),
                None => std::fs::remove_file(&path),
            };
            if let Err(e) = r {
                tracing::warn!(path = %path.display(), "a plugin file could not be put back: {e}");
            }
        }
    }
}

fn write_failed(path: &Path, e: impl std::fmt::Display) -> Fail {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        msg!(
            "control.plugin.write_failed", path = path.display(), detail = e =>
            "{path} could not be written: {detail}"
        ),
    )
}

/// 写一组文件：**目录只给自己（0700），文件 0600**，原子替换。返回写之前的样子
fn write_files(dir: &Path, files: &[(PathBuf, &[u8])]) -> Result<Undo, Fail> {
    let plugins = tw_config::plugins::dir_in(dir);
    let approved = plugins.join(tw_config::plugins::APPROVED_DIR);
    for d in [&plugins, &approved] {
        tw_config::private_dir::create(d).map_err(|e| write_failed(d, e))?;
    }
    let mut undo = Undo(Vec::new());
    for (path, bytes) in files {
        let before = match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                undo.restore();
                return Err(write_failed(path, e));
            }
        };
        if let Err(e) = write_private(path, bytes) {
            undo.restore();
            return Err(write_failed(path, e));
        }
        undo.0.push((path.clone(), before));
    }
    Ok(undo)
}

/// 原子地写一个只给自己看的文件：**建的那一刻就是 0600**，写完再改名过去。写的是
/// 原样的字节 —— 批准的那一份要和哈希过的一字不差
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 先写文件、再写配置；配置没写成就把文件还原
async fn with_files<F>(
    s: &ControlState,
    files: &[(PathBuf, &[u8])],
    base_version: Option<&str>,
    f: F,
) -> Result<String, Fail>
where
    F: FnOnce(&str, &tw_config::Config) -> Result<String, ApplyError>,
{
    let undo = write_files(&config_dir(s), files)?;
    match s.cfg.transform(base_version, Origin::Ui, f).await {
        Ok(version) => Ok(version),
        Err(e) => {
            undo.restore();
            Err(apply_fail(e))
        }
    }
}

async fn create(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PluginCreate>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let m = load_or_refuse(&s, req.source.clone().into_bytes()).await?;
    check_scope(&req.scope)?;
    let settings = settings_for(&m, &req.settings)?;
    // **id 在拿到写的那把锁之后再定**：两个同名的插件同时装，后一个看得见前一个
    let _edit = s.gateway.plugins.edits.lock().await;
    let id = {
        let rt = s.gateway.runtime();
        let taken: Vec<&str> = rt.config.plugins.iter().map(|p| p.id.as_str()).collect();
        new_id(req.id.as_deref(), &m.name, &taken)?
    };
    let sha = sha256_hex(req.source.as_bytes());
    let item = entry(&id, &sha, req.enabled, req.on_error, &req.scope, &settings);
    let dir = config_dir(&s);
    let src = req.source.as_bytes();
    let version = with_files(
        &s,
        &[
            (tw_config::plugins::file_path(&dir, &id), src),
            (tw_config::plugins::approved_path(&dir, &id), src),
        ],
        req.base_version.as_deref(),
        |text, _| Ok(edit::upsert(text, edit::PLUGINS, None, &item)?),
    )
    .await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 网页调得到的那一条：改得了工具调用的插件只能停用、改出错时怎么办（见模块说明）
async fn update(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginUpdate>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    save(&s, &id, req, false).await
}

/// 同一件事，桌面端在系统的确认框里点过头了：工具调用插件的开关、设置、范围也改得了。
/// **网页不能调**（不在桌面端网页的白名单里）
async fn update_confirmed(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginUpdate>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    save(&s, &id, req, true).await
}

/// 改开关、出错时怎么办、范围、设置。`confirmed`：点过头了（[`update_confirmed`]）
async fn save(
    s: &ControlState,
    id: &str,
    req: tw_api::PluginUpdate,
    confirmed: bool,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    check_scope(&req.scope)?;
    // **攥着写插件的那把锁**：读到的权限和写下去的配置说的是同一份插件 —— 换源码、
    // 批准也攥着它，落不到两者之间
    let _edit = s.gateway.plugins.edits.lock().await;
    // 设置对着它此刻的 manifest 查。读不出 manifest（文件变了、底稿也没了）就照交来的
    // 写：加载时还会再查一遍
    let (approved, manifest, name) = {
        let rt = s.gateway.runtime();
        let approved = rt
            .config
            .plugins
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.sha256.clone());
        let a = rt.plugins.get(id);
        let manifest = a.and_then(|a| a.manifest.clone());
        let name = a.map_or_else(|| id.to_string(), |a| a.name.clone());
        (approved, manifest, name)
    };
    let settings = match &manifest {
        Some(m) => settings_for(m, &req.settings)?,
        None => req.settings.clone(),
    };
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let p = cfg
                .plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| missing(id))?;
            // manifest 得是配置里批准的那一份的；对不上（配置刚被别处改了）就是读不出
            let known = manifest
                .as_ref()
                .filter(|_| approved.as_deref() == Some(p.sha256.as_str()));
            if !confirmed && steers_tool_calls(known) && changes_what_it_does(p, &req, known) {
                return Err(ApplyError::NeedsConfirmation(msg!(
                    "control.plugin.needs_confirmation", plugin = &name =>
                    "Turning on plugin `{plugin}`, or changing its settings or scope, has to be \
                     confirmed in the app, because the plugin may change the tool calls in replies."
                )));
            }
            let item = entry(
                id,
                &p.sha256,
                req.enabled,
                req.on_error,
                &req.scope,
                &settings,
            );
            Ok(edit::upsert(text, edit::PLUGINS, Some(id), &item)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 改得了回答里的工具调用：权限里有 `reply_tool_calls`，**或者读不出它要什么权限**
fn steers_tool_calls(m: Option<&Manifest>) -> bool {
    m.is_none_or(|m| m.permissions.contains(&tw_api::Permission::ReplyToolCalls))
}

/// 这次改动里有没有要点头的：打开它、改设置、改范围。停用、改出错时怎么办都不算。
/// **比的是生效的样子**：配置里没写的设置按默认值算，范围不看顺序和重复
fn changes_what_it_does(
    p: &tw_config::Plugin,
    req: &tw_api::PluginUpdate,
    m: Option<&Manifest>,
) -> bool {
    let turns_on = req.enabled && !p.enabled;
    let norm = |v: &[String]| {
        let mut v: Vec<String> = v.iter().map(|x| x.trim().to_string()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let scope = norm(&p.scope.clients) != norm(&req.scope.clients)
        || norm(&p.scope.models) != norm(&req.scope.models)
        || norm(&p.scope.upstreams) != norm(&req.scope.upstreams);
    let now: BTreeMap<String, SettingValue> = p
        .settings
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), from_yaml(v)?)))
        .collect();
    let effective = |given: &BTreeMap<String, SettingValue>| {
        let all = tw_gateway::plugin::load::settings_of(m?, given).ok()?;
        Some(
            all.iter()
                .filter_map(|(k, v)| Some((k.clone(), from_json(v)?)))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    let settings = match (effective(&now), effective(&req.settings)) {
        (Some(a), Some(b)) => a != b,
        // 算不出生效的样子（读不出 manifest、配置里的设置本来就不对）：照写的比
        _ => now != req.settings,
    };
    turns_on || scope || settings
}

async fn replace_source(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginSourceReplace>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    if !s
        .gateway
        .runtime()
        .config
        .plugins
        .iter()
        .any(|p| p.id == id)
    {
        return Err(not_found(&id));
    }
    let m = load_or_refuse(&s, req.source.clone().into_bytes()).await?;
    let sha = sha256_hex(req.source.as_bytes());
    let dir = config_dir(&s);
    let _edit = s.gateway.plugins.edits.lock().await;
    let src = req.source.as_bytes();
    let version = with_files(
        &s,
        &[
            (tw_config::plugins::file_path(&dir, &id), src),
            (tw_config::plugins::approved_path(&dir, &id), src),
        ],
        req.base_version.as_deref(),
        |text, cfg| {
            let p = cfg
                .plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| missing(&id))?;
            let item = entry(
                &id,
                &sha,
                p.enabled,
                p.on_error.into(),
                &scope_view(&p.scope),
                &reconcile(&m, &p.settings),
            );
            Ok(edit::upsert(text, edit::PLUGINS, Some(&id), &item)?)
        },
    )
    .await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 批准磁盘上改过的文件。**批的是调用方看过的那一份**：读一次，哈希得和交来的一样，
/// 编的、存进底稿的、写进配置的都是这一次读到的字节。
async fn approve(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginApprove>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let file = {
        let rt = s.gateway.runtime();
        let p = rt
            .config
            .plugins
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| not_found(&id))?;
        p.path_in(&config_dir(&s))
    };
    let _edit = s.gateway.plugins.edits.lock().await;
    let bytes = match read_capped(&file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "control.plugin.file_missing", plugin = &id =>
                    "The file of plugin `{plugin}` is gone, so there is nothing to approve. Replace \
                     its source or delete it."
                ),
            ));
        }
        Err(e) => return Err(unreadable(&file.display().to_string(), e)),
    };
    let sha = sha256_hex(&bytes);
    if sha != req.sha256 {
        return Err(fail(
            StatusCode::CONFLICT,
            msg!(
                "control.plugin.file_moved_on", plugin = &id =>
                "The file of plugin `{plugin}` changed again after it was reviewed. Review it again."
            ),
        ));
    }
    let m = load_or_refuse(&s, bytes.clone()).await?;
    let dir = config_dir(&s);
    let version = with_files(
        &s,
        &[(tw_config::plugins::approved_path(&dir, &id), &bytes)],
        req.base_version.as_deref(),
        |text, cfg| {
            let p = cfg
                .plugins
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| missing(&id))?;
            let item = entry(
                &id,
                &sha,
                p.enabled,
                p.on_error.into(),
                &scope_view(&p.scope),
                &reconcile(&m, &p.settings),
            );
            Ok(edit::upsert(text, edit::PLUGINS, Some(&id), &item)?)
        },
    )
    .await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 删掉：**先改配置、再删文件**。配置没改成，文件一个都不动；文件删不掉只记一行
/// 日志 —— 配置里已经没有它了，留下的文件不会再被读
async fn delete(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<tw_api::BaseVersion>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let _edit = s.gateway.plugins.edits.lock().await;
    let version = s
        .cfg
        .transform(q.base_version.as_deref(), Origin::Ui, |text, _| {
            Ok(edit::remove(text, edit::PLUGINS, &id)?)
        })
        .await
        .map_err(apply_fail)?;
    let dir = config_dir(&s);
    for path in [
        tw_config::plugins::file_path(&dir, &id),
        tw_config::plugins::approved_path(&dir, &id),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), "a deleted plugin's file stays: {e}"),
        }
    }
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn reorder(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PluginOrder>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, cfg| {
            let mut now: Vec<&str> = cfg.plugins.iter().map(|p| p.id.as_str()).collect();
            let mut want: Vec<&str> = req.ids.iter().map(String::as_str).collect();
            now.sort_unstable();
            want.sort_unstable();
            if now != want {
                return Err(ApplyError::Invalid(msg!(
                    "control.plugin.order" =>
                    "The new order has to name every plugin exactly once."
                )));
            }
            Ok(edit::reorder(text, edit::PLUGINS, &req.ids)?)
        })
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

// ---------------------------------------------------------------- 试跑

/// 拿一条记下的请求试跑。**不连上游**，也不进插件的计数和日志。
async fn trial(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginTrial>,
) -> Result<Json<tw_api::PluginTrialResult>, Fail> {
    let active = s
        .gateway
        .runtime()
        .plugins
        .get(&id)
        .cloned()
        .ok_or_else(|| not_found(&id))?;
    let store = crate::need_store(&s)?;
    let (row, request, reply) = {
        let g = store.lock().await;
        let row = g.db().get(req.request_id).map_err(crate::records)?.ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                msg!("control.request_not_found", id = req.request_id => "There is no request {id}."),
            )
        })?;
        let request = g.blobs().get(row.at_ms, row.id, tw_store::Which::Request);
        let reply = g.blobs().get(row.at_ms, row.id, tw_store::Which::Response);
        (row, request, reply)
    };
    // 跑不了的插件不试：改过的代码不跑（I9），加载不了的也跑不了
    let host = match &active.state {
        tw_gateway::plugin::State::Ready(h) => h.clone(),
        tw_gateway::plugin::State::Broken(b) => {
            return Ok(Json(refused(match b {
                Broken::Changed => msg!(
                    "control.plugin.trial_changed", plugin = &active.name =>
                    "The file of plugin `{plugin}` changed and has not been approved, so it cannot \
                     be tried."
                ),
                Broken::Error(m) => m.clone(),
            })));
        }
    };
    Ok(Json(
        run_trial(&s, &active, host, &row, request, reply).await,
    ))
}

fn refused(why: Msg) -> tw_api::PluginTrialResult {
    tw_api::PluginTrialResult {
        request: None,
        reply: None,
        logs: Vec::new(),
        error: Some(why),
    }
}

/// 试跑本身在数据面那一侧（视图、写回、占位符都在 [`tw_gateway::plugin::trial`]）。
///
/// 存下来的回答是上游的原话：回答它的那一家说什么格式，看服务它的那一跳转换过没有，
/// 和会话记录读回答是同一个办法。插件的 `ctx` 按这一行的路由给：回答它的那一家，和发给
/// 那一家的模型名
async fn run_trial(
    s: &ControlState,
    active: &Active,
    host: std::sync::Arc<dyn tw_gateway::plugin::PluginHost>,
    row: &tw_store::RequestRow,
    request: Option<Vec<u8>>,
    reply: Option<Vec<u8>>,
) -> tw_api::PluginTrialResult {
    use tw_gateway::plugin::trial::{self, StoredReply, StoredRequest};
    use tw_store::search::text::{client_dialect, dialect_of};
    let upstream = row
        .translated
        .as_deref()
        .and_then(|j| serde_json::from_str::<tw_api::TranslatedView>(j).ok())
        .map(|t| dialect_of(t.to))
        .or_else(|| client_dialect(&row.path));
    let t = trial::run(
        s.gateway.plugin_pool.clone(),
        host,
        &active.settings,
        s.gateway.runtime().redact.clone(),
        request.as_deref().map(|body| StoredRequest {
            path: &row.path,
            query: None,
            body,
            client: row.client_hint.as_deref(),
            upstream: &row.provider,
            sent_model: &row.sent_model,
        }),
        reply
            .as_deref()
            .zip(upstream)
            .map(|(body, upstream)| StoredReply {
                body,
                upstream,
                provider: &row.provider,
            }),
    )
    .await;
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let side = |x: trial::Side| tw_api::TrialSide {
        before: x.before,
        after: x.after,
        outcome: x.outcome,
    };
    tw_api::PluginTrialResult {
        request: t.request.map(side),
        reply: t.reply.map(side),
        logs: t
            .logs
            .into_iter()
            .map(|(hook, l)| tw_api::PluginLogEntry {
                at_ms,
                request_id: Some(row.id as u64),
                hook,
                level: l.level,
                text: l.text,
            })
            .collect(),
        error: t.error,
    }
}

// ---------------------------------------------------------------- 监听

/// 盯着 `plugins/` 目录：**插件文件一动，就把插件重读一遍**（重读文件、重算哈希）。
/// 文件和批准的不一样了，那个插件马上停用、说一声。
///
/// 目录不在就先建出来（只给自己，0700）：盯不住一个不存在的目录。返回的 `Watch`
/// 要留着，扔掉就不盯了。
pub fn spawn_watcher(
    gateway: tw_gateway::AppState,
    config: &Path,
) -> Result<tw_watch::Watch, tw_watch::WatchError> {
    let dir = tw_config::plugins::dir_in(&dir_of(config));
    if let Err(e) = tw_config::private_dir::create(&dir) {
        tracing::warn!(dir = %dir.display(), "the plugin directory could not be created: {e}");
    }
    let (w, mut rx) = tw_watch::watch(
        std::slice::from_ref(&dir),
        tw_config::watch::DEBOUNCE,
        |p| p.extension().is_some_and(|x| x == "js"),
    )?;
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // 控制面正在写插件文件和配置时等它写完：两半之间的样子不作数
            let _edit = gateway.plugins.edits.lock().await;
            let gw = gateway.clone();
            // 重读要读文件、可能还要编译，不占异步线程
            let _ = tokio::task::spawn_blocking(move || gw.reload_plugins()).await;
        }
    });
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_from_names_and_do_not_collide() {
        assert_eq!(id_from_name("Add Date!", &[]), "add-date");
        assert_eq!(id_from_name("附加日期", &[]), "plugin");
        assert_eq!(id_from_name("附加日期", &["plugin"]), "plugin-2");
        assert_eq!(
            id_from_name("附加日期", &["plugin", "plugin-2"]),
            "plugin-3"
        );
        assert_eq!(id_from_name("Order", &[]), "order-plugin");
        let long = "x".repeat(60);
        let id = id_from_name(&long, &[&"x".repeat(40)]);
        assert!(id.len() <= 40 && id.ends_with("-2"), "{id}");
        assert!(tw_config::plugins::valid_id(&id));
    }

    #[test]
    fn whole_numbers_stay_whole_in_the_file() {
        assert_eq!(to_yaml(&SettingValue::Number(3.0)), Value::from(3));
        assert_eq!(to_yaml(&SettingValue::Number(0.5)), Value::from(0.5));
    }
}

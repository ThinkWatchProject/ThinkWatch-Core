//! 脚本插件：装、保存、改写、批准、排顺序、删、试跑、日志。
//!
//! # 插件的配置在插件文件里
//!
//! 出错时怎么办、管哪些请求、设置的值都写在插件文件的 manifest 里（契约附录四）：文件就是
//! 它的配置所在。配置里的那一条只有 id、文件、批准的哈希和开关。界面改这几样是改源码 ——
//! `PluginRewrite` 只换 manifest 那一段字面量、交回改写之后的源码（什么都不写），
//! `SavePlugin` 把源码和开关存下来。
//!
//! 保存时拿新源码和批准的那一份比：manifest 字面量以外一个字节不差、manifest 里只差出错时
//! 怎么办、范围和设置的值，是**只改了数据**；别的都是**改了代码**。
//!
//! # 文件和配置是一件事的两半
//!
//! 插件文件（`plugins/<id>.js`）和它的底稿（`plugins/.approved/<id>.js`）由这里写，
//! 批准的哈希在配置里。**先写文件、再写配置**：配置一落盘，网关就照它重读文件、比
//! 哈希（不变式 I9）。配置没写成（版本对不上、校验没过），刚写的文件按写之前的样子
//! 还原 —— 不留下一个和配置对不上的插件文件。整个过程攥着 `Plugins::edits`，目录
//! 监听不会落在两半之间：保存（哪怕只改了数据）时，文件、底稿和哈希一起换，中间没有
//! 「文件变了」的那一刻。
//!
//! # 什么时候要在系统的确认框里点头
//!
//! **只有改得了回答里工具调用的插件**（权限有 `reply_tool_calls`）：它决定客户端执行什么，
//! 网页里注入的脚本要是能装上它、打开它、改它的代码、批准它磁盘上改过的文件，就能借它改
//! 客户端要跑的命令。这四件事，碰上这种插件（新旧两份里有一份有这个权限）时，网页调得到的
//! 端点（`CreatePlugin`、`SavePlugin`、`ApprovePluginFile`）一律拒绝（403，
//! `control.plugin.needs_confirmation`），要走带 `confirmed` 的那一条 —— 那几条不在桌面端
//! 网页的 `call` 白名单里，桌面端的 Rust 自己再编一遍源码、在系统的确认框里把名字、权限和
//! 要改的地方摆给人看，点了头才发。
//!
//! 别的都不用点头：只改数据（哪怕是工具调用插件的）、停用、删、排顺序，装、打开、改、批准
//! 不碰工具调用的插件。
//!
//! **判断只认真的编出来的 manifest**，不认显示用的缓存（它是用户目录里的一个文件，被人改了
//! 只是显示不对）；**读不出旧的那一份要什么权限的按改得了算**：它此刻跑不了，可一旦又跑得
//! 了（运行时恢复了），网页替它做的事就生效了。所以这里也不假设调用方看过什么：源码在这里
//! 再编一遍，批准时磁盘上的文件得正好是调用方看过的那一份（哈希核对）。

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde_yaml_ng::Mapping;
use tw_api::{SettingValue, ep};
use tw_config::edit;
use tw_config::history::Origin;
use tw_gateway::plugin::load::{read_capped, sha256_hex};
use tw_gateway::plugin::{Active, Broken, LoadError, Manifest, source};
use tw_types::{Msg, msg};

use crate::contract::RouterExt;
use crate::{ApplyError, ControlState, Fail, apply_fail, fail, internal};

pub mod defaults;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new()
        .at(ep::Plugins, list)
        .at(ep::PluginInspect, inspect)
        .at(ep::PluginRewrite, rewrite)
        .at(ep::CreatePlugin, create)
        .at(ep::CreatePluginConfirmed, create_confirmed)
        .at(ep::ReorderPlugins, reorder)
        .at(ep::SavePlugin, save)
        .at(ep::SavePluginConfirmed, save_confirmed)
        .at(ep::DeletePlugin, delete)
        .at(ep::PluginSourceDiff, source_diff)
        .at(ep::ApprovePluginFile, approve)
        .at(ep::ApprovePluginFileConfirmed, approve_confirmed)
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
    tw_api::PluginView {
        id: a.id.clone(),
        name: a.name.clone(),
        description: m.and_then(|m| m.description.clone()),
        enabled: a.enabled,
        on_error: a.on_error,
        permissions: a.permissions.clone(),
        requests: a.requests.clone(),
        scope: scope_view(&a.scope),
        reply_mode: a.reply_mode,
        settings_schema: m.map(schema).unwrap_or_default(),
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

fn scope_view(s: &tw_gateway::plugin::Scope) -> tw_api::PluginScope {
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
            value: from_json(&s.value).unwrap_or(SettingValue::String(String::new())),
        })
        .collect()
}

fn manifest_view(m: &Manifest) -> tw_api::ManifestView {
    tw_api::ManifestView {
        name: m.name.clone(),
        description: m.description.clone(),
        permissions: m.permissions.clone(),
        requests: m.requests.clone(),
        scope: scope_view(&m.scope),
        on_error: m.on_error,
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
    let (line, column) = e.location();
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

/// 编一遍，编不成就拒绝这次写入（装、保存新源码、批准都要编得成）
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

/// 配置里的一条：**只有这四样**。出错时怎么办、范围、设置的值都在插件文件里
fn entry(id: &str, sha256: &str, enabled: bool) -> Mapping {
    let mut m = Mapping::new();
    m.insert("id".into(), id.into());
    m.insert("file".into(), tw_config::Plugin::file_for(id).into());
    m.insert("sha256".into(), sha256.into());
    m.insert("enabled".into(), enabled.into());
    m
}

/// 写插件这一节之前：先去掉 0.58 留下的旧字段（出错时怎么办、范围、设置挪进了插件文件，
/// 见 [`tw_config::plugins::drop_legacy`]）。插件这一节的每一次写都先过它
fn current_shape(text: &str) -> Result<String, ApplyError> {
    Ok(tw_config::plugins::drop_legacy(text)?)
}

/// 新建或者改写配置里的一条
fn upsert(text: &str, current: Option<&str>, item: &Mapping) -> Result<String, ApplyError> {
    Ok(edit::upsert(
        &current_shape(text)?,
        edit::PLUGINS,
        current,
        item,
    )?)
}

/// 范围里不能有空着的一项。**改写之前查**，说的是这一句
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

/// 改写一份源码里的数据：出错时怎么办、范围、设置的值。**什么都不留下**，也用不着运行时
/// —— manifest 是纯数据，照着源码就改得了
async fn rewrite(
    Json(req): Json<tw_api::PluginRewriteRequest>,
) -> Result<Json<tw_api::PluginSource>, Fail> {
    check_scope(&req.scope)?;
    let source = source::rewrite(&req.source, req.on_error, &req.scope, &req.settings)
        .map_err(|why| fail(StatusCode::BAD_REQUEST, why))?;
    Ok(Json(tw_api::PluginSource { source }))
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
    if base.is_empty() {
        base = "plugin".into();
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

/// 磁盘上此刻的那一版配置（版本号），和它里面的这个插件。**判断照它做，写也照它写**：
/// 写的时候版本对不上就是 409，不会照着一份过期的判断写下去
fn on_disk(s: &ControlState, id: &str) -> Result<(String, tw_config::Plugin), Fail> {
    let cur = s
        .cfg
        .current()
        .map_err(|e| apply_fail(ApplyError::Store(e)))?;
    let cfg = tw_config::try_parse(&cur.text).map_err(|r| apply_fail(ApplyError::Rejected(r)))?;
    let p = cfg
        .plugins
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| not_found(id))?;
    Ok((cur.version(), p))
}

/// 改得了回答里的工具调用
fn steers(m: &Manifest) -> bool {
    m.permissions.contains(&tw_api::Permission::ReplyToolCalls)
}

/// 这件事要在系统的确认框里点头（见模块说明）
fn needs_confirmation(name: &str) -> Fail {
    apply_fail(ApplyError::NeedsConfirmation(msg!(
        "control.plugin.needs_confirmation", plugin = name =>
        "Plugin `{plugin}` can change the tool calls in replies, so installing it, turning it on, \
         changing its code or approving a change to its file has to be confirmed in the app."
    )))
}

/// 直接写配置原文的那几条路（整份写回 `PutConfig`、按路径改 `PatchConfig`、回滚
/// `ConfigRollback`）**也是网页调得到的**，同样绕不过确认：照新旧两份配置比，装上（多了一条）、
/// 打开（停用 → 开着）、批准（批准的哈希换了）一个改得了工具调用的插件，一律拒绝（403，
/// `control.plugin.needs_confirmation`）。这几条路没有点过头的那一条：要做这几件事，去插件页。
///
/// 新的那一份磁盘上找不到（哈希对得上的字节没有，插件只会是「文件变了」、跑不起来）的不拦；
/// 打开一个读不出权限的插件、换成一份编不成的，按改得了算。
///
/// **只读插件那一节**，不做整份配置的校验：界面交来的原文里控制面的钥匙是打码的（写的时候才
/// 换回来），整份校验在这里过不去 —— 过不去就放行的话，这道关形同虚设。新的那一份插件那一节
/// 读不成的不在这里管：写的时候整份配置会被拒
pub(crate) async fn guard_raw_write(s: &ControlState, old: &str, new: &str) -> Result<(), Fail> {
    let Some(new) = plugins_in(new) else {
        return Ok(());
    };
    // 旧的那一份读不成（不该发生）：当它一个插件都没有，每一条都按新装的查
    let old = plugins_in(old).unwrap_or_default();
    for n in &new {
        let o = old.iter().find(|p| p.id == n.id);
        let same_code = o.is_some_and(|o| o.sha256 == n.sha256);
        if same_code {
            let turns_on = n.enabled && o.is_some_and(|o| !o.enabled);
            if turns_on && let Some(name) = approved_steers(s, &n.id, &n.sha256).await {
                return Err(needs_confirmation(&name));
            }
            continue;
        }
        // 新装的、批准的换了：磁盘上有这份字节才跑得起来
        let Some(bytes) = approved_bytes(s, &n.id, &n.sha256) else {
            continue;
        };
        match load(s, bytes, true).await? {
            Ok(m) if steers(&m) => return Err(needs_confirmation(&m.name)),
            Ok(_) => {}
            Err(_) => return Err(needs_confirmation(&n.id)),
        }
        if let Some(o) = o
            && let Some(name) = approved_steers(s, &o.id, &o.sha256).await
        {
            return Err(needs_confirmation(&name));
        }
    }
    Ok(())
}

/// 一份配置原文里的插件那一节，别的不管。读不成是 None
fn plugins_in(text: &str) -> Option<Vec<tw_config::Plugin>> {
    #[derive(serde::Deserialize)]
    struct Only {
        #[serde(default)]
        plugins: Vec<tw_config::Plugin>,
    }
    serde_yaml_ng::from_str::<Only>(text)
        .ok()
        .map(|o| o.plugins)
}

/// 批准的那一份改不改得了回答里的工具调用。改得了、**或者读不出来**（批准的那份字节没了、
/// 编不成）就是 `Some(名字)` —— 读不出来按改得了算。真的编一遍，不认显示用的缓存
async fn approved_steers(s: &ControlState, id: &str, sha256: &str) -> Option<String> {
    match compiled_manifest(s, id, sha256).await {
        Some(m) if !steers(&m) => None,
        Some(m) => Some(m.name),
        None => Some(
            s.gateway
                .runtime()
                .plugins
                .get(id)
                .map_or_else(|| id.to_string(), |a| a.name.clone()),
        ),
    }
}

async fn create(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PluginCreate>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    install(&s, req, false).await
}

/// 同一件事，桌面端在系统的确认框里点过头了。**网页不能调**
async fn create_confirmed(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::PluginCreate>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    install(&s, req, true).await
}

/// 装一个：写插件文件和底稿，配置里加一条。`confirmed`：点过头了
async fn install(
    s: &ControlState,
    req: tw_api::PluginCreate,
    confirmed: bool,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let m = load_or_refuse(s, req.source.clone().into_bytes()).await?;
    if !confirmed && steers(&m) {
        return Err(needs_confirmation(&m.name));
    }
    // **id 在拿到写的那把锁之后再定**：两个同名的插件同时装，后一个看得见前一个
    let _edit = s.gateway.plugins.edits.lock().await;
    let id = {
        let rt = s.gateway.runtime();
        let taken: Vec<&str> = rt.config.plugins.iter().map(|p| p.id.as_str()).collect();
        new_id(req.id.as_deref(), &m.name, &taken)?
    };
    let sha = sha256_hex(req.source.as_bytes());
    let item = entry(&id, &sha, req.enabled);
    let dir = config_dir(s);
    let src = req.source.as_bytes();
    let version = with_files(
        s,
        &[
            (tw_config::plugins::file_path(&dir, &id), src),
            (tw_config::plugins::approved_path(&dir, &id), src),
        ],
        req.base_version.as_deref(),
        |text, _| upsert(text, None, &item),
    )
    .await?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 网页调得到的那一条：改得了工具调用的插件只能改数据、停用（见模块说明）
async fn save(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    store(&s, &id, req, false).await
}

/// 同一件事，桌面端在系统的确认框里点过头了：工具调用插件也打开得了、改得了代码。
/// **网页不能调**（不在桌面端网页的白名单里）
async fn save_confirmed(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    store(&s, &id, req, true).await
}

/// 保存源码和开关（见 [`tw_api::PluginSave`]）。`confirmed`：点过头了
async fn store(
    s: &ControlState,
    id: &str,
    req: tw_api::PluginSave,
    confirmed: bool,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    // **攥着写插件的那把锁**：判断时读到的批准的那一份，就是写的时候被换掉的那一份 ——
    // 别的插件写入落不到两者之间；配置被别处改了，版本对不上，写不下去
    let _edit = s.gateway.plugins.edits.lock().await;
    let (version, p) = on_disk(s, id)?;
    let base = req.base_version.clone().unwrap_or(version);
    let new = req.source.into_bytes();
    let approved = approved_bytes(s, id, &p.sha256);
    let turns_on = req.enabled && !p.enabled;

    // 源码和批准的一字不差：只是开关，文件不动（磁盘上被人改过的文件也照旧留着待批）
    if approved.as_deref() == Some(new.as_slice()) {
        if turns_on
            && !confirmed
            && let Some(name) = approved_steers(s, id, &p.sha256).await
        {
            return Err(needs_confirmation(&name));
        }
        let item = entry(id, &p.sha256, req.enabled);
        let version = s
            .cfg
            .transform(Some(base.as_str()), Origin::Ui, |text, _| {
                upsert(text, Some(id), &item)
            })
            .await
            .map_err(apply_fail)?;
        return Ok(Json(tw_api::ConfigWritten { version }));
    }

    let m = load_or_refuse(s, new.clone()).await?;
    // 读不到批准的那份字节，说不出差在哪儿：按改了代码算
    let data_only = approved
        .as_deref()
        .is_some_and(|old| source::same_code(old, &new));
    if !confirmed {
        if data_only {
            // 只改了数据：权限和旧的一模一样，只有打开它要点头
            if turns_on && steers(&m) {
                return Err(needs_confirmation(&m.name));
            }
        } else if steers(&m) {
            return Err(needs_confirmation(&m.name));
        } else if let Some(name) = approved_steers(s, id, &p.sha256).await {
            return Err(needs_confirmation(&name));
        }
    }
    let sha = sha256_hex(&new);
    let item = entry(id, &sha, req.enabled);
    let dir = config_dir(s);
    let version = with_files(
        s,
        &[
            (tw_config::plugins::file_path(&dir, id), &new),
            (tw_config::plugins::approved_path(&dir, id), &new),
        ],
        Some(base.as_str()),
        |text, _| upsert(text, Some(id), &item),
    )
    .await?;
    if data_only {
        defaults::follow(&dir, id, &p.sha256, &sha);
    }
    Ok(Json(tw_api::ConfigWritten { version }))
}

/// 这个插件**真的编出来**的 manifest。开着的插件手里就有；休眠的（停用着、运行时没起）、
/// 加载出错的，把批准的那份字节编一遍。**安全上的判断只认它**，不认显示用的缓存。读不到
/// 批准的那份字节、编不成是 None
async fn compiled_manifest(s: &ControlState, id: &str, sha256: &str) -> Option<Manifest> {
    let held = s
        .gateway
        .runtime()
        .plugins
        .get(id)
        .and_then(|a| a.ready().cloned());
    if let Some(h) = held
        && !h.dormant()
        && tw_gateway::plugin::load::hex(&h.sha256()) == sha256
    {
        return Some(h.manifest().clone());
    }
    let bytes = approved_bytes(s, id, sha256)?;
    load(s, bytes, true).await.ok()?.ok()
}

/// 批准的那份字节：磁盘上的插件文件，文件变了时退回底稿。**哈希都得和配置里的一样**，
/// 都对不上就没有
fn approved_bytes(s: &ControlState, id: &str, sha256: &str) -> Option<Vec<u8>> {
    let dir = config_dir(s);
    [
        tw_config::plugins::file_path(&dir, id),
        tw_config::plugins::approved_path(&dir, id),
    ]
    .iter()
    .filter_map(|p| read_capped(p).ok())
    .find(|b| sha256_hex(b) == sha256)
}

async fn approve(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginApprove>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    approve_file(&s, &id, req, false).await
}

/// 同一件事，桌面端在系统的确认框里点过头了。**网页不能调**
async fn approve_confirmed(
    State(s): State<ControlState>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<tw_api::PluginApprove>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    approve_file(&s, &id, req, true).await
}

/// 批准磁盘上改过的文件。**批的是调用方看过的那一份**：读一次，哈希得和交来的一样，
/// 编的、存进底稿的、写进配置的都是这一次读到的字节。`confirmed`：点过头了
async fn approve_file(
    s: &ControlState,
    id: &str,
    req: tw_api::PluginApprove,
    confirmed: bool,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let _edit = s.gateway.plugins.edits.lock().await;
    let (version, p) = on_disk(s, id)?;
    let base = req.base_version.clone().unwrap_or(version);
    let dir = config_dir(s);
    let file = p.path_in(&dir);
    let bytes = match read_capped(&file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail(
                StatusCode::CONFLICT,
                msg!(
                    "control.plugin.file_missing", plugin = id =>
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
                "control.plugin.file_moved_on", plugin = id =>
                "The file of plugin `{plugin}` changed again after it was reviewed. Review it again."
            ),
        ));
    }
    let m = load_or_refuse(s, bytes.clone()).await?;
    if !confirmed {
        // 新的、旧的（批准过的那一份）有一份改得了工具调用，就要点头；旧的读不出来也算
        if steers(&m) {
            return Err(needs_confirmation(&m.name));
        }
        if let Some(name) = approved_steers(s, id, &p.sha256).await {
            return Err(needs_confirmation(&name));
        }
    }
    let data_only = approved_bytes(s, id, &p.sha256)
        .as_deref()
        .is_some_and(|old| source::same_code(old, &bytes));
    let item = entry(id, &sha, p.enabled);
    let version = with_files(
        s,
        &[(tw_config::plugins::approved_path(&dir, id), &bytes)],
        Some(base.as_str()),
        |text, _| upsert(text, Some(id), &item),
    )
    .await?;
    if data_only {
        defaults::follow(&dir, id, &p.sha256, &sha);
    }
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
            Ok(edit::remove(&current_shape(text)?, edit::PLUGINS, &id)?)
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
            Ok(edit::reorder(
                &current_shape(text)?,
                edit::PLUGINS,
                &req.ids,
            )?)
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
    // 休眠的插件（停用着、运行时没起）：真的编一遍再试，设置按编出来的 manifest 重新对
    let active = if active.ready().is_some_and(|h| h.dormant()) {
        match awaken(&s, &active).await {
            Ok(a) => std::sync::Arc::new(a),
            Err(why) => return Ok(Json(refused(why))),
        }
    } else {
        active
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

/// 把一个休眠的插件真的编出来（试跑之前）：批准的那份字节编出来的宿主，出错时怎么办、
/// 范围、设置都照它的 manifest。读不到批准的那份字节、编不成就是试不了的原因
async fn awaken(s: &ControlState, a: &Active) -> Result<Active, Msg> {
    let entry = s
        .gateway
        .runtime()
        .config
        .plugins
        .iter()
        .find(|p| p.id == a.id)
        .cloned()
        .ok_or_else(|| not_found(&a.id).1.0)?;
    let bytes = approved_bytes(s, &a.id, &entry.sha256).ok_or_else(|| {
        msg!(
            "control.plugin.trial_changed", plugin = &a.name =>
            "The file of plugin `{plugin}` changed and has not been approved, so it cannot \
             be tried."
        )
    })?;
    let plugins = s.gateway.plugins.clone();
    let host = tokio::task::spawn_blocking(move || plugins.prepare(&bytes))
        .await
        .map_err(|e| internal(e).1.0)?
        .map_err(|e| e.msg())?;
    let m = host.manifest().clone();
    Ok(Active {
        id: a.id.clone(),
        name: m.name.clone(),
        enabled: a.enabled,
        on_error: m.on_error,
        scope: m.scope.clone(),
        permissions: m.permissions.clone(),
        requests: m.requests.clone(),
        reply_mode: m.reply_mode,
        hooks: m.hooks,
        settings: tw_gateway::plugin::load::values_of(&m),
        manifest: Some(m),
        state: tw_gateway::plugin::State::Ready(host),
        stats: a.stats.clone(),
        logs: a.logs.clone(),
    })
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
        assert_eq!(id_from_name("Order", &[]), "order");
        let long = "x".repeat(60);
        let id = id_from_name(&long, &[&"x".repeat(40)]);
        assert!(id.len() <= 40 && id.ends_with("-2"), "{id}");
        assert!(tw_config::plugins::valid_id(&id));
    }
}

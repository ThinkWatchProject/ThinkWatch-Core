//! 插件清单：从沙箱里读出来，在这边按约定逐项核对。
//!
//! 桥（`bridge.js`）把模块的导出整理成一份 JSON 交过来；这里**不信**它，所有
//! 规则都在这边重新判一遍。错误消息给插件作者看，说清楚哪一项、为什么。
//!
//! 核对之前先对一遍源码：manifest 得是纯数据，沙箱求值出来的得正好是源码里写的那一份
//! （[`crate::literal`]，由调用方给的 `literal` 做）。

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::{
    Hooks, LoadError, Manifest, OnError, Permission, ReplyMode, RequestKind, Scope, SettingKind,
    SettingSpec,
};

const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 500;
const MAX_SETTINGS: usize = 20;
const MAX_SETTING_KEY: usize = 64;
const MAX_LABEL: usize = 100;
/// 字符串设置的值最多几个字符
pub(crate) const MAX_STRING_VALUE: usize = 10_000;
const MAX_GLOBS: usize = 100;
const MAX_GLOB: usize = 200;

/// 沙箱里的桥交回来的那份
#[derive(serde::Deserialize)]
struct LoadInfo {
    hooks: Map<String, Value>,
    manifest_kind: String,
    manifest: Value,
    manifest_error: Option<String>,
    settings_order: Option<Vec<String>>,
    has_default: bool,
}

/// 读出、核对一份 manifest。`literal` 拿到沙箱求值出来的那一份（JSON），核对它和源码里写的
/// 是不是同一份：在别的规则之前做 —— 被模块代码改过的 manifest，按改过的样子报错只会让人
/// 糊涂
pub(crate) fn parse(
    info: &[u8],
    literal: impl FnOnce(&Value) -> Result<(), LoadError>,
) -> Result<Manifest, LoadError> {
    let info: LoadInfo = serde_json::from_slice(info).map_err(|e| {
        LoadError::Engine(format!(
            "the sandbox returned an unreadable module description: {e}"
        ))
    })?;

    let hook = |name: &str| -> Result<bool, LoadError> {
        match info.hooks.get(name).and_then(Value::as_str) {
            Some("function") => Ok(true),
            Some("missing") | None => Ok(false),
            Some(other) => Err(err(format!(
                "{name} is exported but is a {other}, not a function"
            ))),
        }
    };
    let hooks = Hooks {
        request: hook("onRequest")?,
        reply_text: hook("onReplyText")?,
        reply_text_end: hook("onReplyTextEnd")?,
        tool_call: hook("onToolCall")?,
    };

    match info.manifest_kind.as_str() {
        "object" => {}
        "missing" if info.has_default => {
            return Err(err(
                "export the manifest and the hooks by name (`export const manifest = {…}`, \
                 `export function onRequest(…)`), not as a default export",
            ));
        }
        "missing" => {
            return Err(err(
                "the plugin does not export a manifest (`export const manifest = {…}`)",
            ));
        }
        other => {
            return Err(err(format!(
                "the manifest must be an object, not {}",
                article(other)
            )));
        }
    }
    if let Some(e) = info.manifest_error {
        return Err(err(format!("the manifest cannot be read as JSON: {e}")));
    }
    literal(&info.manifest)?;
    let Value::Object(m) = info.manifest else {
        return Err(err("the manifest must be an object"));
    };

    // api 先看：将来的 api 2 可能有这里不认识的字段，那时该说的是「版本不支持」
    let api = match m.get("api") {
        None | Some(Value::Null) => return Err(err("the manifest needs `api: 1`")),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(1) => 1,
            Some(v) => {
                return Err(LoadError::UnsupportedApi(
                    u32::try_from(v).unwrap_or(u32::MAX),
                ));
            }
            None => return Err(err("`api` must be 1")),
        },
        Some(_) => return Err(err("`api` must be the number 1")),
    };

    for key in m.keys() {
        if !matches!(
            key.as_str(),
            "name"
                | "api"
                | "description"
                | "permissions"
                | "requests"
                | "match"
                | "on_error"
                | "reply"
                | "settings"
        ) {
            return Err(err(format!("the manifest has an unknown field `{key}`")));
        }
    }

    let name = match m.get("name") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => return Err(err("the manifest needs a `name`")),
        Some(_) => return Err(err("`name` must be a string")),
    };
    let n = name.chars().count();
    if n == 0 {
        return Err(err("`name` must not be empty"));
    }
    if n > MAX_NAME {
        return Err(err(format!(
            "`name` is {n} characters long; at most {MAX_NAME} are allowed"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(err(
            "`name` must not contain line breaks or other control characters",
        ));
    }

    let description = match m.get("description") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let n = s.chars().count();
            if n > MAX_DESCRIPTION {
                return Err(err(format!(
                    "`description` is {n} characters long; at most {MAX_DESCRIPTION} are allowed"
                )));
            }
            Some(s.clone())
        }
        Some(_) => return Err(err("`description` must be a string")),
    };

    let permissions = permissions(m.get("permissions"))?;
    let requests = requests(m.get("requests"))?;
    let scope = scope(m.get("match"))?;
    let on_error = match m.get("on_error") {
        None | Some(Value::Null) => OnError::Reject,
        Some(Value::String(s)) => OnError::from_manifest(s)
            .ok_or_else(|| err("`on_error` must be \"reject\" or \"skip\""))?,
        Some(_) => return Err(err("`on_error` must be \"reject\" or \"skip\"")),
    };
    let reply_mode = match m.get("reply") {
        None | Some(Value::Null) => ReplyMode::Block,
        Some(Value::String(s)) if s == "block" => ReplyMode::Block,
        Some(Value::String(s)) if s == "stream" => ReplyMode::Stream,
        Some(_) => return Err(err("`reply` must be \"block\" or \"stream\"")),
    };
    let settings = settings(m.get("settings"), info.settings_order.as_deref())?;

    check_hooks(&hooks, &permissions, reply_mode)?;
    check_requests(&requests, &permissions)?;

    Ok(Manifest {
        name,
        api,
        description,
        permissions,
        requests,
        scope,
        on_error,
        reply_mode,
        settings,
        hooks,
    })
}

fn permissions(v: Option<&Value>) -> Result<BTreeSet<Permission>, LoadError> {
    let list = match v {
        Some(Value::Array(a)) => a,
        None | Some(Value::Null) => return Err(err("the manifest needs `permissions`")),
        Some(_) => return Err(err("`permissions` must be a list")),
    };
    if list.is_empty() {
        return Err(err("`permissions` must list at least one permission"));
    }
    let mut out = BTreeSet::new();
    for p in list {
        let Some(s) = p.as_str() else {
            return Err(err("every entry of `permissions` must be a string"));
        };
        let Some(perm) = Permission::from_manifest(s) else {
            return Err(err(format!(
                "unknown permission \"{s}\"; the permissions are {}",
                Permission::ALL
                    .iter()
                    .map(|p| format!("\"{}\"", p.manifest_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        if !out.insert(perm) {
            return Err(err(format!("permission \"{s}\" is listed twice")));
        }
    }
    Ok(out)
}

/// 插件处理哪几种请求。**不写就是只有对话**，写了就得是一张非空的单子
fn requests(v: Option<&Value>) -> Result<BTreeSet<RequestKind>, LoadError> {
    let list = match v {
        None | Some(Value::Null) => return Ok(BTreeSet::from([RequestKind::Conversation])),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(err("`requests` must be a list")),
    };
    if list.is_empty() {
        return Err(err(
            "`requests` must list at least one kind of request; leave it out to handle conversations only",
        ));
    }
    let mut out = BTreeSet::new();
    for r in list {
        let Some(s) = r.as_str() else {
            return Err(err("every entry of `requests` must be a string"));
        };
        let Some(kind) = RequestKind::from_manifest(s) else {
            return Err(err(format!(
                "`requests` lists \"{s}\", which is not a kind of request; the kinds are {}",
                RequestKind::ALL
                    .iter()
                    .map(|k| format!("\"{}\"", k.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        if !out.insert(kind) {
            return Err(err(format!("\"{s}\" is listed twice in `requests`")));
        }
    }
    Ok(out)
}

/// 声明的请求和申请的权限对得上：**每一种请求都有权限碰得到它的视图**，**每个权限都在
/// 某一种声明了的请求上用得着** —— 和钩子、权限一一对应是同一个道理，什么都不白要。
///
/// 嵌入和旧版补全的视图只有 `messages` 和 `params`，回答钩子也只在对话上跑：只要了
/// `system` 的插件处理不了嵌入，不处理对话的插件用不着 `system`、`tools` 和回答钩子
fn check_requests(
    requests: &BTreeSet<RequestKind>,
    perms: &BTreeSet<Permission>,
) -> Result<(), LoadError> {
    for kind in requests {
        if !kind.reached_by().iter().any(|p| perms.contains(p)) {
            return Err(err(format!(
                "`requests` lists \"{}\", but none of the permissions applies to those requests; \
                 they show only \"messages\" and \"params\"",
                kind.as_str()
            )));
        }
    }
    for p in perms {
        if !requests.iter().any(|k| k.reached_by().contains(p)) {
            return Err(err(format!(
                "permission \"{}\" only applies to conversations, and `requests` does not list \
                 \"conversation\"",
                p.manifest_name()
            )));
        }
    }
    Ok(())
}

/// 一份 `match`（`{ clients, models, upstreams }`）合不合规矩，和加载时同一套判据。
/// 改写 manifest 之前用（[`crate::literal::rewrite`]）
pub(crate) fn check_scope(v: &Value) -> Result<(), String> {
    match scope(Some(v)) {
        Ok(_) => Ok(()),
        Err(LoadError::Manifest(why)) => Err(why),
        Err(e) => Err(e.to_string()),
    }
}

fn scope(v: Option<&Value>) -> Result<Scope, LoadError> {
    let m = match v {
        None | Some(Value::Null) => return Ok(Scope::default()),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(err("`match` must be an object")),
    };
    for key in m.keys() {
        if !matches!(key.as_str(), "clients" | "models" | "upstreams") {
            return Err(err(format!(
                "`match` has an unknown field `{key}`; it takes clients, models and upstreams"
            )));
        }
    }
    let globs = |key: &str| -> Result<Vec<String>, LoadError> {
        let list = match m.get(key) {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(a)) => a,
            Some(_) => return Err(err(format!("`match.{key}` must be a list of strings"))),
        };
        if list.len() > MAX_GLOBS {
            return Err(err(format!(
                "`match.{key}` has more than {MAX_GLOBS} entries"
            )));
        }
        let mut out = Vec::with_capacity(list.len());
        for g in list {
            let Some(s) = g.as_str() else {
                return Err(err(format!("`match.{key}` must be a list of strings")));
            };
            if s.trim().is_empty() {
                return Err(err(format!("`match.{key}` has an empty entry")));
            }
            if s.chars().count() > MAX_GLOB || s.chars().any(char::is_control) {
                return Err(err(format!(
                    "`match.{key}` has an entry that is too long or not plain text"
                )));
            }
            out.push(s.to_string());
        }
        Ok(out)
    };
    Ok(Scope {
        clients: globs("clients")?,
        models: globs("models")?,
        upstreams: globs("upstreams")?,
    })
}

fn settings(v: Option<&Value>, order: Option<&[String]>) -> Result<Vec<SettingSpec>, LoadError> {
    let m = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(err("`settings` must be an object of setting definitions")),
    };
    if m.len() > MAX_SETTINGS {
        return Err(err(format!(
            "`settings` has {} entries; at most {MAX_SETTINGS} are allowed",
            m.len()
        )));
    }
    // 作者写的先后（JSON 对象在这边按键排序，先后只能从沙箱里带过来）
    let mut keys: Vec<&String> = Vec::with_capacity(m.len());
    if let Some(order) = order {
        for k in order {
            if let Some((key, _)) = m.get_key_value(k)
                && !keys.contains(&key)
            {
                keys.push(key);
            }
        }
    }
    for k in m.keys() {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }

    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        if !valid_key(key) {
            return Err(err(format!(
                "setting `{key}` has an invalid name: use letters, digits and _ (up to {MAX_SETTING_KEY}), not starting with a digit"
            )));
        }
        let Some(Value::Object(spec)) = m.get(key) else {
            return Err(err(format!(
                "setting `{key}` must be an object like {{ type: \"string\", label: \"…\", value: \"…\" }}"
            )));
        };
        for f in spec.keys() {
            if !matches!(f.as_str(), "type" | "label" | "value") {
                return Err(err(format!(
                    "setting `{key}` has an unknown field `{f}`; it takes type, label and value"
                )));
            }
        }
        let kind = match spec.get("type").and_then(Value::as_str) {
            Some("string") => SettingKind::String,
            Some("number") => SettingKind::Number,
            Some("boolean") => SettingKind::Boolean,
            _ => {
                return Err(err(format!(
                    "setting `{key}` needs a type: \"string\", \"number\" or \"boolean\""
                )));
            }
        };
        let label = match spec.get("label") {
            None | Some(Value::Null) => key.clone(),
            Some(Value::String(s)) => {
                let s = s.trim();
                if s.is_empty() {
                    key.clone()
                } else if s.chars().count() > MAX_LABEL || s.chars().any(char::is_control) {
                    return Err(err(format!(
                        "the label of setting `{key}` must be plain text of at most {MAX_LABEL} characters"
                    )));
                } else {
                    s.to_string()
                }
            }
            Some(_) => {
                return Err(err(format!(
                    "the label of setting `{key}` must be a string"
                )));
            }
        };
        // 设置此刻的值。没写就是这种类型的空值
        let value = match (kind, spec.get("value")) {
            (SettingKind::String, None | Some(Value::Null)) => Value::String(String::new()),
            (SettingKind::Number, None | Some(Value::Null)) => Value::from(0),
            (SettingKind::Boolean, None | Some(Value::Null)) => Value::Bool(false),
            (SettingKind::String, Some(Value::String(s))) => {
                if s.chars().count() > MAX_STRING_VALUE {
                    return Err(err(format!("the value of setting `{key}` is too long")));
                }
                Value::String(s.clone())
            }
            (SettingKind::Number, Some(Value::Number(n))) => Value::Number(n.clone()),
            (SettingKind::Boolean, Some(Value::Bool(b))) => Value::Bool(*b),
            (kind, Some(_)) => {
                return Err(err(format!(
                    "the value of setting `{key}` must be a {}",
                    kind.as_str()
                )));
            }
        };
        out.push(SettingSpec {
            key: key.clone(),
            kind,
            label,
            value,
        });
    }
    Ok(out)
}

fn valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    k.len() <= MAX_SETTING_KEY
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 钩子和权限一一对应：导出了钩子就得申请它的权限，申请了权限就得有钩子用它
fn check_hooks(
    hooks: &Hooks,
    perms: &BTreeSet<Permission>,
    mode: ReplyMode,
) -> Result<(), LoadError> {
    let request_perms = [
        Permission::System,
        Permission::Messages,
        Permission::Tools,
        Permission::Params,
    ];
    if hooks.request {
        if !request_perms.iter().any(|p| perms.contains(p)) {
            return Err(err(
                "onRequest is exported but the manifest requests none of \"system\", \"messages\", \"tools\", \"params\"",
            ));
        }
    } else if let Some(p) = request_perms.iter().find(|p| perms.contains(p)) {
        return Err(err(format!(
            "permission \"{}\" is requested but onRequest is not exported",
            p.manifest_name()
        )));
    }

    match (hooks.reply_text, perms.contains(&Permission::ReplyText)) {
        (true, false) => {
            return Err(err(
                "onReplyText is exported but the manifest does not request \"reply.text\"",
            ));
        }
        (false, true) => {
            return Err(err(
                "permission \"reply.text\" is requested but onReplyText is not exported",
            ));
        }
        _ => {}
    }
    if hooks.reply_text_end {
        if !hooks.reply_text {
            return Err(err("onReplyTextEnd is exported without onReplyText"));
        }
        if mode != ReplyMode::Stream {
            return Err(err(
                "onReplyTextEnd is only called in stream mode; set `reply: \"stream\"` in the manifest",
            ));
        }
    }

    match (hooks.tool_call, perms.contains(&Permission::ReplyToolCalls)) {
        (true, false) => {
            return Err(err(
                "onToolCall is exported but the manifest does not request \"reply.tool_calls\"",
            ));
        }
        (false, true) => {
            return Err(err(
                "permission \"reply.tool_calls\" is requested but onToolCall is not exported",
            ));
        }
        _ => {}
    }

    if !hooks.request && !hooks.reply_text && !hooks.tool_call {
        return Err(err(
            "the plugin exports no hook; export at least one of onRequest, onReplyText, onToolCall",
        ));
    }
    Ok(())
}

fn article(kind: &str) -> String {
    match kind {
        "null" => "null".into(),
        "array" => "an array".into(),
        "undefined" => "undefined".into(),
        other => format!("a {other}"),
    }
}

fn err(msg: impl Into<String>) -> LoadError {
    LoadError::Manifest(msg.into())
}

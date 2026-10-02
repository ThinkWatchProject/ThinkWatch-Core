//! **测试用的假引擎**：不跑 JavaScript，只照约定的写法读出 manifest 和导出了哪些钩子。
//!
//! 管理面的测试（装、换、批准、文件变了）要一个能「编译」的引擎，而真的沙箱编译慢、
//! 还要 wasm 工具链。假引擎认的源码长这样 —— manifest 是**一行 JSON**：
//!
//! ```text
//! export const manifest = {"name":"附加日期","api":1,"permissions":["system"]};
//! export function onRequest(req, ctx) {}
//! ```
//!
//! 校验照插件约定的那几条做（权限和钩子对得上、至少一个钩子、名字长度……），错了
//! 给 [`LoadError::Manifest`]；有一行写着 `@@syntax@@` 的算语法错，行号就是那一行。

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::plugin::engine::{Engine, Hooks, LoadError, MAX_SOURCE, Manifest, SettingSpec};
use crate::plugin::host::PluginHost;
use crate::plugin::set::Scope;

/// 假引擎。
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeEngine;

/// 假引擎「编」出来的插件。
#[derive(Debug)]
pub struct FakeHost {
    manifest: Manifest,
    sha256: [u8; 32],
}

impl PluginHost for FakeHost {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
}

impl Engine for FakeEngine {
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        if source.len() > MAX_SOURCE {
            return Err(LoadError::TooLarge);
        }
        let text = std::str::from_utf8(source).map_err(|e| LoadError::Syntax {
            message: format!("the file is not UTF-8: {e}"),
            line: None,
            column: None,
        })?;
        if let Some((i, _)) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("@@syntax@@"))
        {
            return Err(LoadError::Syntax {
                message: "Unexpected token".into(),
                line: Some(i as u32 + 1),
                column: Some(1),
            });
        }
        let manifest = manifest_of(text)?;
        Ok(Arc::new(FakeHost {
            manifest,
            sha256: Sha256::digest(source).into(),
        }))
    }
}

/// 一份假源码：manifest（一行 JSON）加上给定的钩子。
pub fn source(manifest: serde_json::Value, hooks: &[&str]) -> String {
    let mut s = format!("export const manifest = {manifest};\n");
    for h in hooks {
        s.push_str(&format!("export function {h}(x, ctx) {{}}\n"));
    }
    s
}

fn bad(why: impl Into<String>) -> LoadError {
    LoadError::Manifest(why.into())
}

fn manifest_of(text: &str) -> Result<Manifest, LoadError> {
    const PREFIX: &str = "export const manifest = ";
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix(PREFIX))
        .ok_or_else(|| bad("the plugin does not export a manifest"))?;
    let json = line.trim().trim_end_matches(';');
    let m: serde_json::Value =
        serde_json::from_str(json).map_err(|e| bad(format!("the manifest is not valid: {e}")))?;

    let name = m["name"]
        .as_str()
        .ok_or_else(|| bad("manifest.name is required"))?;
    let chars = name.chars().count();
    if !(1..=64).contains(&chars) {
        return Err(bad("manifest.name has to be 1 to 64 characters"));
    }
    let api = m["api"]
        .as_u64()
        .ok_or_else(|| bad("manifest.api is required"))? as u32;
    if api != 1 {
        return Err(LoadError::UnsupportedApi(api));
    }
    let description = match &m["description"] {
        serde_json::Value::Null => None,
        serde_json::Value::String(d) if d.chars().count() <= 500 => Some(d.clone()),
        _ => {
            return Err(bad(
                "manifest.description has to be a string of up to 500 characters",
            ));
        }
    };

    let mut permissions = Vec::new();
    for p in m["permissions"]
        .as_array()
        .ok_or_else(|| bad("manifest.permissions is required"))?
    {
        let word = p.as_str().unwrap_or_default();
        let perm = match word {
            "system" => tw_api::Permission::System,
            "messages" => tw_api::Permission::Messages,
            "tools" => tw_api::Permission::Tools,
            "params" => tw_api::Permission::Params,
            "reply.text" => tw_api::Permission::ReplyText,
            "reply.tool_calls" => tw_api::Permission::ReplyToolCalls,
            other => return Err(bad(format!("`{other}` is not a permission"))),
        };
        if !permissions.contains(&perm) {
            permissions.push(perm);
        }
    }
    if permissions.is_empty() {
        return Err(bad("manifest.permissions cannot be empty"));
    }
    permissions.sort_by_key(|p| tw_api::Permission::ALL.iter().position(|x| x == p));

    let reply_mode = match m["reply"].as_str() {
        None | Some("block") => tw_api::ReplyMode::Block,
        Some("stream") => tw_api::ReplyMode::Stream,
        Some(other) => return Err(bad(format!("`{other}` is not a reply mode"))),
    };

    let hooks = Hooks {
        request: text.contains("export function onRequest("),
        reply_text: text.contains("export function onReplyText("),
        reply_text_end: text.contains("export function onReplyTextEnd("),
        tool_call: text.contains("export function onToolCall("),
    };
    let request_perm = permissions.iter().any(|p| {
        matches!(
            p,
            tw_api::Permission::System
                | tw_api::Permission::Messages
                | tw_api::Permission::Tools
                | tw_api::Permission::Params
        )
    });
    let has = |p| permissions.contains(&p);
    if hooks.request != request_perm {
        return Err(bad(
            "onRequest and the permissions system, messages, tools and params go together",
        ));
    }
    if hooks.reply_text != has(tw_api::Permission::ReplyText) {
        return Err(bad("onReplyText and the permission reply.text go together"));
    }
    if hooks.tool_call != has(tw_api::Permission::ReplyToolCalls) {
        return Err(bad(
            "onToolCall and the permission reply.tool_calls go together",
        ));
    }
    if hooks.reply_text_end && (reply_mode != tw_api::ReplyMode::Stream || !hooks.reply_text) {
        return Err(bad(
            "onReplyTextEnd is only used with reply: \"stream\" and onReplyText",
        ));
    }

    // 处理哪几种请求：没写是只有对话。每一种都要有权限碰得到它，每个权限都要用得上
    let requests = match &m["requests"] {
        serde_json::Value::Null => crate::plugin::engine::DEFAULT_REQUESTS.to_vec(),
        serde_json::Value::Array(a) if !a.is_empty() => {
            let mut out = Vec::new();
            for r in a {
                let kind = r
                    .as_str()
                    .and_then(tw_api::RequestKind::from_slug)
                    .ok_or_else(|| bad(format!("`{r}` in requests is not a kind of request")))?;
                if out.contains(&kind) {
                    return Err(bad(format!("`{r}` is listed twice in requests")));
                }
                out.push(kind);
            }
            out.sort_by_key(|k| tw_api::RequestKind::ALL.iter().position(|x| x == k));
            out
        }
        _ => return Err(bad("requests has to be a non-empty list")),
    };
    let inputs = [tw_api::Permission::Messages, tw_api::Permission::Params];
    let conversation = requests.contains(&tw_api::RequestKind::Conversation);
    if requests
        .iter()
        .any(|k| *k != tw_api::RequestKind::Conversation && !inputs.iter().any(|p| has(*p)))
    {
        return Err(bad(
            "embeddings and completions requests need the permission messages or params",
        ));
    }
    if !conversation && permissions.iter().any(|p| !inputs.contains(p)) {
        return Err(bad(
            "a permission other than messages and params needs \"conversation\" in requests",
        ));
    }

    let list = |v: &serde_json::Value| -> Result<Vec<String>, LoadError> {
        match v {
            serde_json::Value::Null => Ok(Vec::new()),
            serde_json::Value::Array(a) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| bad("a match entry has to be a string"))
                })
                .collect(),
            _ => Err(bad("a match list has to be a list")),
        }
    };
    let scope = Scope {
        clients: list(&m["match"]["clients"])?,
        models: list(&m["match"]["models"])?,
        upstreams: list(&m["match"]["upstreams"])?,
    };

    let mut settings = Vec::new();
    if let Some(obj) = m["settings"].as_object() {
        if obj.len() > 20 {
            return Err(bad("a plugin has at most 20 settings"));
        }
        for (key, spec) in obj {
            let kind = match spec["type"].as_str() {
                Some("string") => tw_api::SettingKind::String,
                Some("number") => tw_api::SettingKind::Number,
                Some("boolean") => tw_api::SettingKind::Boolean,
                _ => return Err(bad(format!("setting `{key}` has no valid type"))),
            };
            let default = match (&kind, &spec["default"]) {
                (tw_api::SettingKind::String, serde_json::Value::Null) => "".into(),
                (tw_api::SettingKind::Number, serde_json::Value::Null) => 0.into(),
                (tw_api::SettingKind::Boolean, serde_json::Value::Null) => false.into(),
                (tw_api::SettingKind::String, v @ serde_json::Value::String(_))
                | (tw_api::SettingKind::Number, v @ serde_json::Value::Number(_))
                | (tw_api::SettingKind::Boolean, v @ serde_json::Value::Bool(_)) => v.clone(),
                _ => {
                    return Err(bad(format!(
                        "the default of setting `{key}` is not of its type"
                    )));
                }
            };
            settings.push(SettingSpec {
                key: key.clone(),
                kind,
                label: spec["label"].as_str().unwrap_or(key).to_string(),
                default,
            });
        }
    }

    Ok(Manifest {
        name: name.to_string(),
        api,
        description,
        permissions,
        requests,
        scope,
        reply_mode,
        settings,
        hooks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_good_source_loads_with_its_manifest_hooks_and_hash() {
        let src = source(
            json!({"name": "附加日期", "api": 1, "permissions": ["system"],
                   "match": {"models": ["claude-*"]},
                   "settings": {"note": {"type": "string", "label": "附加内容", "default": "x"}}}),
            &["onRequest"],
        );
        let host = FakeEngine.load(src.as_bytes()).unwrap();
        let m = host.manifest();
        assert_eq!(m.name, "附加日期");
        assert_eq!(m.permissions, [tw_api::Permission::System]);
        assert!(m.hooks.request && !m.hooks.on_reply());
        assert_eq!(m.scope.models, ["claude-*"]);
        assert_eq!(m.settings[0].default, json!("x"));
        let want: [u8; 32] = Sha256::digest(src.as_bytes()).into();
        assert_eq!(host.sha256(), want);
    }

    #[test]
    fn permissions_and_hooks_have_to_go_together() {
        let only_perm = source(
            json!({"name": "n", "api": 1, "permissions": ["system"]}),
            &[],
        );
        assert!(matches!(
            FakeEngine.load(only_perm.as_bytes()),
            Err(LoadError::Manifest(_))
        ));
        let tool = source(
            json!({"name": "n", "api": 1, "permissions": ["reply.text"]}),
            &["onToolCall"],
        );
        assert!(matches!(
            FakeEngine.load(tool.as_bytes()),
            Err(LoadError::Manifest(_))
        ));
    }

    /// `requests` 照沙箱的规矩读：没写是只有对话；每一种都要有权限碰得到它
    #[test]
    fn requests_default_to_conversations_and_follow_the_sandbox_rules() {
        use tw_api::RequestKind::*;
        let load = |m: serde_json::Value| FakeEngine.load(source(m, &["onRequest"]).as_bytes());
        let m = load(json!({"name": "n", "api": 1, "permissions": ["messages"]})).unwrap();
        assert_eq!(m.manifest().requests, [Conversation]);
        let m = load(json!({"name": "n", "api": 1, "permissions": ["messages"],
                            "requests": ["embeddings", "conversation"]}))
        .unwrap();
        assert_eq!(m.manifest().requests, [Conversation, Embeddings]);
        for requests in [
            json!([]),
            json!(["images"]),
            json!(["completions", "completions"]),
        ] {
            let r = load(json!({"name": "n", "api": 1, "permissions": ["messages"],
                                "requests": requests}));
            assert!(matches!(r, Err(LoadError::Manifest(_))), "{requests}");
        }
        let only_system = load(json!({"name": "n", "api": 1, "permissions": ["system"],
                                      "requests": ["conversation", "embeddings"]}));
        assert!(matches!(only_system, Err(LoadError::Manifest(_))));
        let no_conversation = load(json!({"name": "n", "api": 1,
                                          "permissions": ["system", "messages"],
                                          "requests": ["embeddings"]}));
        assert!(matches!(no_conversation, Err(LoadError::Manifest(_))));
    }

    #[test]
    fn a_syntax_marker_is_a_syntax_error_on_its_line() {
        let src = format!(
            "{}\n// @@syntax@@\n",
            source(
                json!({"name": "n", "api": 1, "permissions": ["system"]}),
                &["onRequest"]
            )
        );
        let Err(LoadError::Syntax { line, .. }) = FakeEngine.load(src.as_bytes()) else {
            panic!("no syntax error");
        };
        assert_eq!(line, Some(4));
    }

    #[test]
    fn another_api_version_is_refused() {
        let src = source(
            json!({"name": "n", "api": 2, "permissions": ["system"]}),
            &["onRequest"],
        );
        assert!(matches!(
            FakeEngine.load(src.as_bytes()),
            Err(LoadError::UnsupportedApi(2))
        ));
    }
}

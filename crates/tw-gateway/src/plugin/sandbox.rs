//! 真的运行时：`tw-plugin`（QuickJS 跑在 Wasmtime 里）接到 [`Engine`] 和 [`PluginHost`] 上。
//!
//! **沙箱用到时才起**：第一次编插件（装了插件的配置换进来、或者有人试编一份源码）时
//! 才建运行时，之后一直用这一个，换配置也不重建。没装插件的 core、每个建一份网关状态的
//! 测试都不付这个钱。起不来（这台机器上的 Wasmtime 用不了）时，每个插件都「加载不了」，
//! 原因就是起不来的那句话。
//!
//! **编插件在自己的线程上**，栈给足（[`STACK`]）：沙箱要 2 MiB 以上的栈，而调用方可能是
//! 任何线程 —— 换配置的那一路、Windows 上只有 1 MiB 栈的主线程（启动时第一次读插件）。
//! 跑钩子的那些调用在数据面自己的线程池上（`plugin::pool`）。

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::plugin::engine::{Engine, Hooks, LoadError, Manifest, SettingSpec};
use crate::plugin::host::{
    Invocation, PluginHost, ReplyHost, RequestOutcome, RunError, ToolCallOutcome,
};
use crate::plugin::set::{LogLine, Scope};

/// 编插件的那个线程的栈。沙箱自己的 wasm 栈最多 1 MiB，宿主这一侧另要余量
pub const STACK: usize = 8 * 1024 * 1024;

/// 进程里的那一个沙箱运行时。
pub struct Sandbox {
    limits: tw_plugin::Limits,
    runtime: OnceLock<Result<tw_plugin::Runtime, String>>,
}

impl Sandbox {
    pub fn new(limits: tw_plugin::Limits) -> Self {
        Self {
            limits,
            runtime: OnceLock::new(),
        }
    }

    fn runtime(&self) -> Result<&tw_plugin::Runtime, LoadError> {
        self.runtime
            .get_or_init(|| tw_plugin::Runtime::new(self.limits.clone()).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| LoadError::Engine(e.clone()))
    }
}

/// 沙箱编好的一个插件
pub struct Loaded {
    plugin: tw_plugin::Plugin,
    manifest: Manifest,
}

impl PluginHost for Loaded {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn sha256(&self) -> [u8; 32] {
        self.plugin.sha256()
    }

    fn on_request(&self, view: Value, ctx: Value) -> Invocation<RequestOutcome> {
        invocation(self.plugin.on_request(view, ctx), |o| match o {
            tw_plugin::RequestOutcome::Unchanged => RequestOutcome::Unchanged,
            tw_plugin::RequestOutcome::Changed(v) => RequestOutcome::Changed(v),
            tw_plugin::RequestOutcome::Rejected(why) => RequestOutcome::Rejected(why),
        })
    }

    fn reply(&self, ctx: Value) -> Result<Box<dyn ReplyHost>, RunError> {
        let reply = self.plugin.reply(ctx).map_err(run_error)?;
        Ok(Box::new(LoadedReply(reply)))
    }
}

/// 一次回答的那一个实例（不变式 I3：这次回答的钩子共用它，回答完就扔）
struct LoadedReply(tw_plugin::Reply);

impl ReplyHost for LoadedReply {
    fn on_text(&mut self, text: &str) -> Invocation<Option<String>> {
        invocation(self.0.on_text(text), |t| t)
    }

    fn on_text_end(&mut self) -> Invocation<Option<String>> {
        invocation(self.0.on_text_end(), |t| t)
    }

    fn on_tool_call(&mut self, call: Value) -> Invocation<ToolCallOutcome> {
        invocation(self.0.on_tool_call(call), |o| match o {
            tw_plugin::ToolCallOutcome::Unchanged => ToolCallOutcome::Unchanged,
            tw_plugin::ToolCallOutcome::Replace(calls) => ToolCallOutcome::Replace(calls),
            tw_plugin::ToolCallOutcome::Drop => ToolCallOutcome::Drop,
        })
    }
}

/// 沙箱的一次调用换成网关的：结果、日志、CPU 时间一样不少
fn invocation<T, U>(i: tw_plugin::Invocation<T>, f: impl FnOnce(T) -> U) -> Invocation<U> {
    Invocation {
        result: i.result.map(f).map_err(run_error),
        logs: i
            .logs
            .into_iter()
            .map(|l| LogLine {
                level: match l.level {
                    tw_plugin::LogLevel::Log => tw_api::PluginLogLevel::Log,
                    tw_plugin::LogLevel::Info => tw_api::PluginLogLevel::Info,
                    tw_plugin::LogLevel::Warn => tw_api::PluginLogLevel::Warn,
                    tw_plugin::LogLevel::Error => tw_api::PluginLogLevel::Error,
                },
                text: l.text,
            })
            .collect(),
        cpu: i.cpu,
    }
}

fn run_error(e: tw_plugin::RunError) -> RunError {
    match e {
        tw_plugin::RunError::CpuLimit => RunError::CpuLimit,
        tw_plugin::RunError::MemoryLimit => RunError::MemoryLimit,
        tw_plugin::RunError::OutputLimit => RunError::OutputLimit,
        tw_plugin::RunError::Threw { message, stack } => RunError::Threw { message, stack },
        tw_plugin::RunError::BadOutput(d) => RunError::BadOutput(d),
        tw_plugin::RunError::Trap(d) => RunError::Trap(d),
    }
}

impl Sandbox {
    /// 在调用它的这个线程上编。**栈要够**，见 [`STACK`]
    fn load_here(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        let plugin = self.runtime()?.load(source).map_err(load_error)?;
        let manifest = manifest(plugin.manifest());
        Ok(Arc::new(Loaded { plugin, manifest }))
    }
}

impl Engine for Sandbox {
    /// 编的就是交来的这份字节（不变式 I9），在一个栈给足的线程上编
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("tw-plugin-load".into())
                .stack_size(STACK)
                .spawn_scoped(scope, || self.load_here(source))
                .map_err(|e| LoadError::Engine(format!("no thread to load the plugin on: {e}")))?
                .join()
                .unwrap_or_else(|_| Err(LoadError::Engine("loading the plugin panicked".into())))
        })
    }
}

fn load_error(e: tw_plugin::LoadError) -> LoadError {
    match e {
        tw_plugin::LoadError::TooLarge => LoadError::TooLarge,
        tw_plugin::LoadError::Syntax {
            message,
            line,
            column,
        } => LoadError::Syntax {
            message,
            line,
            column,
        },
        tw_plugin::LoadError::Manifest(m) => LoadError::Manifest(m),
        tw_plugin::LoadError::UnsupportedApi(v) => LoadError::UnsupportedApi(v),
        tw_plugin::LoadError::Engine(e) => LoadError::Engine(e),
    }
}

fn permission(p: tw_plugin::Permission) -> tw_api::Permission {
    match p {
        tw_plugin::Permission::System => tw_api::Permission::System,
        tw_plugin::Permission::Messages => tw_api::Permission::Messages,
        tw_plugin::Permission::Tools => tw_api::Permission::Tools,
        tw_plugin::Permission::Params => tw_api::Permission::Params,
        tw_plugin::Permission::ReplyText => tw_api::Permission::ReplyText,
        tw_plugin::Permission::ReplyToolCalls => tw_api::Permission::ReplyToolCalls,
    }
}

fn manifest(m: &tw_plugin::Manifest) -> Manifest {
    let mut permissions: Vec<tw_api::Permission> =
        m.permissions.iter().copied().map(permission).collect();
    permissions.sort_by_key(|p| tw_api::Permission::ALL.iter().position(|x| x == p));
    Manifest {
        name: m.name.clone(),
        api: m.api,
        description: m.description.clone(),
        permissions,
        scope: Scope {
            clients: m.scope.clients.clone(),
            models: m.scope.models.clone(),
            upstreams: m.scope.upstreams.clone(),
        },
        reply_mode: match m.reply_mode {
            tw_plugin::ReplyMode::Block => tw_api::ReplyMode::Block,
            tw_plugin::ReplyMode::Stream => tw_api::ReplyMode::Stream,
        },
        settings: m
            .settings
            .iter()
            .map(|s| SettingSpec {
                key: s.key.clone(),
                kind: match s.kind {
                    tw_plugin::SettingKind::String => tw_api::SettingKind::String,
                    tw_plugin::SettingKind::Number => tw_api::SettingKind::Number,
                    tw_plugin::SettingKind::Boolean => tw_api::SettingKind::Boolean,
                },
                label: s.label.clone(),
                default: s.default.clone(),
            })
            .collect(),
        hooks: Hooks {
            request: m.hooks.request,
            reply_text: m.hooks.reply_text,
            reply_text_end: m.hooks.reply_text_end,
            tool_call: m.hooks.tool_call,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const ADD_DATE: &str = r#"export const manifest = {
  name: "附加日期",
  api: 1,
  description: "在系统提示里写上今天的日期",
  permissions: ["system"],
  match: { clients: ["claude-code"], models: ["claude-*"] },
  settings: {
    note: { type: "string", label: "附加内容", default: "今天" },
    days: { type: "number", label: "天数", default: 1 },
  },
};
export function onRequest(req, ctx) {
  return { ...req, system: req.system + " " + ctx.settings.note };
}
"#;

    #[test]
    fn a_real_plugin_loads_with_its_manifest_and_the_hash_of_its_bytes() {
        let engine = Sandbox::new(tw_plugin::Limits::default());
        let host = engine.load(ADD_DATE.as_bytes()).unwrap();
        let m = host.manifest();
        assert_eq!(m.name, "附加日期");
        assert_eq!(m.api, 1);
        assert_eq!(m.description.as_deref(), Some("在系统提示里写上今天的日期"));
        assert_eq!(m.permissions, [tw_api::Permission::System]);
        assert_eq!(m.scope.clients, ["claude-code"]);
        assert_eq!(m.scope.models, ["claude-*"]);
        assert!(m.scope.upstreams.is_empty());
        assert_eq!(m.reply_mode, tw_api::ReplyMode::Block);
        assert!(m.hooks.request && !m.hooks.on_reply());
        let keys: Vec<&str> = m.settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["note", "days"], "the author's order");
        assert_eq!(m.settings[1].kind, tw_api::SettingKind::Number);
        assert_eq!(m.settings[0].default, serde_json::json!("今天"));
        let want: [u8; 32] = Sha256::digest(ADD_DATE.as_bytes()).into();
        assert_eq!(host.sha256(), want);
    }

    /// 钩子经过这一层原样到达沙箱、原样回来：改过的视图、日志、CPU 时间
    #[test]
    fn hooks_run_in_the_sandbox_through_the_adapter() {
        let engine = Sandbox::new(tw_plugin::Limits::default());
        let host = engine.load(ADD_DATE.as_bytes()).unwrap();
        let view = serde_json::json!({"format": "anthropic", "model": "m", "system": "你好"});
        let ctx = serde_json::json!({"client": null, "model": "m", "format": "anthropic",
                                     "upstream": null, "settings": {"note": "今天", "days": 1}});
        let inv = host.on_request(view, ctx);
        let Ok(RequestOutcome::Changed(v)) = inv.result else {
            panic!("{:?}", inv.result);
        };
        assert_eq!(v["system"], "你好 今天");

        let shout = r#"export const manifest = { name: "Shout", api: 1, permissions: ["reply.text"] };
export function onReplyText(text, ctx) { console.warn("seen", text.length); return text.toUpperCase(); }
"#;
        let host = engine.load(shout.as_bytes()).unwrap();
        let mut reply = host
            .reply(
                serde_json::json!({"client": null, "model": "m", "format": "anthropic",
                                      "upstream": "u", "settings": {}}),
            )
            .unwrap();
        let inv = reply.on_text("hi");
        assert_eq!(inv.result, Ok(Some("HI".to_string())));
        assert_eq!(inv.logs.len(), 1);
        assert_eq!(inv.logs[0].level, tw_api::PluginLogLevel::Warn);
    }

    /// 调用方的栈再小也编得了：编在自己的线程上
    #[test]
    fn a_small_caller_stack_is_enough() {
        let engine = Sandbox::new(tw_plugin::Limits::default());
        let loaded = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                engine
                    .load(ADD_DATE.as_bytes())
                    .map(|h| h.manifest().name.clone())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(loaded.unwrap(), "附加日期");
    }

    #[test]
    fn a_syntax_error_says_where() {
        let engine = Sandbox::new(tw_plugin::Limits::default());
        let src = "export const manifest = { name: \"x\", api: 1, permissions: [\"system\"] };\nexport function onRequest(req) {\n  return req +;\n}\n";
        let Err(LoadError::Syntax { line, .. }) = engine.load(src.as_bytes()) else {
            panic!("a syntax error loaded");
        };
        assert_eq!(line, Some(3));
    }

    #[test]
    fn a_manifest_that_asks_for_more_than_it_uses_is_refused() {
        let engine = Sandbox::new(tw_plugin::Limits::default());
        let src = "export const manifest = { name: \"x\", api: 1, permissions: [\"system\", \"tools\", \"reply.text\"] };\nexport function onRequest(req) {}\n";
        assert!(matches!(
            engine.load(src.as_bytes()),
            Err(LoadError::Manifest(_))
        ));
    }
}

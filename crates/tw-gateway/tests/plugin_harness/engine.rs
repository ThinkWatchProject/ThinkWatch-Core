//! 真的沙箱（`tw-plugin`）接在网关的引擎接缝上：类型一样一样对过去。
//!
//! 端到端测试要的是**真的 JS 跑在真的 Wasmtime 里**，不是替身 —— 插件能不能看到密钥、
//! 能不能越权，答案取决于桥和沙箱真正做了什么。

use std::sync::{Arc, OnceLock};

use serde_json::Value;
use tw_gateway::plugin::engine::{Engine, Hooks, LoadError, Manifest, SettingSpec};
use tw_gateway::plugin::host::{
    Invocation, PluginHost, ReplyHost, RequestOutcome, RunError, ToolCallOutcome,
};
use tw_gateway::plugin::set::{LogLine, Scope};

/// 一个进程一个运行时
pub fn runtime() -> tw_plugin::Runtime {
    static RT: OnceLock<tw_plugin::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tw_plugin::Runtime::new(tw_plugin::Limits::default()).expect("the plugin runtime starts")
    })
    .clone()
}

pub struct Sandbox(pub tw_plugin::Runtime);

impl Engine for Sandbox {
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        let plugin = self.0.load(source).map_err(load_error)?;
        let manifest = manifest(plugin.manifest());
        Ok(Arc::new(Hosted { plugin, manifest }))
    }
}

struct Hosted {
    plugin: tw_plugin::Plugin,
    manifest: Manifest,
}

impl PluginHost for Hosted {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn sha256(&self) -> [u8; 32] {
        self.plugin.sha256()
    }

    fn on_request(&self, view: Value, ctx: Value) -> Invocation<RequestOutcome> {
        let inv = self.plugin.on_request(view, ctx);
        convert(inv, |o| match o {
            tw_plugin::RequestOutcome::Unchanged => RequestOutcome::Unchanged,
            tw_plugin::RequestOutcome::Changed(v) => RequestOutcome::Changed(v),
            tw_plugin::RequestOutcome::Rejected(r) => RequestOutcome::Rejected(r),
        })
    }

    fn reply(&self, ctx: Value) -> Result<Box<dyn ReplyHost>, RunError> {
        let r = self.plugin.reply(ctx).map_err(run_error)?;
        Ok(Box::new(HostedReply(r)))
    }
}

struct HostedReply(tw_plugin::Reply);

impl ReplyHost for HostedReply {
    fn on_text(&mut self, text: &str) -> Invocation<Option<String>> {
        convert(self.0.on_text(text), |o| o)
    }

    fn on_text_end(&mut self) -> Invocation<Option<String>> {
        convert(self.0.on_text_end(), |o| o)
    }

    fn on_tool_call(&mut self, call: Value) -> Invocation<ToolCallOutcome> {
        convert(self.0.on_tool_call(call), |o| match o {
            tw_plugin::ToolCallOutcome::Unchanged => ToolCallOutcome::Unchanged,
            tw_plugin::ToolCallOutcome::Replace(c) => ToolCallOutcome::Replace(c),
            tw_plugin::ToolCallOutcome::Drop => ToolCallOutcome::Drop,
        })
    }
}

fn convert<T, U>(inv: tw_plugin::Invocation<T>, f: impl FnOnce(T) -> U) -> Invocation<U> {
    Invocation {
        result: inv.result.map(f).map_err(run_error),
        logs: inv
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
        cpu: inv.cpu,
    }
}

fn run_error(e: tw_plugin::RunError) -> RunError {
    match e {
        tw_plugin::RunError::CpuLimit => RunError::CpuLimit,
        tw_plugin::RunError::MemoryLimit => RunError::MemoryLimit,
        tw_plugin::RunError::OutputLimit => RunError::OutputLimit,
        tw_plugin::RunError::Threw { message, stack } => RunError::Threw { message, stack },
        tw_plugin::RunError::BadOutput(m) => RunError::BadOutput(m),
        tw_plugin::RunError::Trap(m) => RunError::Trap(m),
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
        tw_plugin::LoadError::Engine(m) => LoadError::Engine(m),
    }
}

fn manifest(m: &tw_plugin::Manifest) -> Manifest {
    Manifest {
        name: m.name.clone(),
        api: m.api,
        description: m.description.clone(),
        permissions: tw_api::Permission::ALL
            .iter()
            .copied()
            .filter(|p| m.permissions.iter().any(|q| q.as_str() == p.slug()))
            .collect(),
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
                kind: tw_api::SettingKind::from_slug(s.kind.as_str()).expect("a known kind"),
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

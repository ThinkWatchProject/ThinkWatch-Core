//! 真的插件运行时：`tw-plugin` 的沙箱（QuickJS 跑在 Wasmtime 里）接到 [`Engine`] 这道
//! 接缝上。
//!
//! **只是一层把类型对上的适配**：manifest、钩子的结局、错误、日志，一样一样换成网关的
//! 那一份，语义一点不改 —— 怎么跑、上限多少都是 `tw-plugin` 的事，视图、权限、写回
//! 都是网关的事。
//!
//! 运行时一个进程一份，**第一次编插件时才起**：起它要加载沙箱、起一个计时线程，没装
//! 插件的进程（以及绝大多数测试）不该为它付这个钱。起不来（比如地址空间受限的机器）
//! 就和没有运行时一样：每个插件都加载不了，原因照实说，管得着的请求按 `on_error` 处置。

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::plugin::engine::{Engine, Hooks, LoadError, Manifest, SettingSpec};
use crate::plugin::host::{
    Invocation, PluginHost, ReplyHost, RequestOutcome, RunError, ToolCallOutcome,
};
use crate::plugin::set::{LogLine, Scope};

/// 进程里那一份运行时。起不来时是起不来的原因
fn runtime() -> Result<&'static tw_plugin::Runtime, LoadError> {
    static RT: OnceLock<Result<tw_plugin::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        tw_plugin::Runtime::new(tw_plugin::Limits::default()).map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| LoadError::Engine(e.clone()))
}

/// `tw-plugin` 的沙箱。生产上用的就是它（见 [`crate::plugin::default_engine`]）。
#[derive(Debug, Default, Clone, Copy)]
pub struct Sandbox;

impl Engine for Sandbox {
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        let rt = runtime()?;
        // 编译要在沙箱里跑一遍模块顶层，**调用方的线程栈未必够**：插件是在换配置的那一路
        // 上编的，那可能是主线程（Windows 上只有 1 MiB）。在一根栈给足了的线程上编 ——
        // 编插件只在换配置、装插件时发生，多起一根线程不算什么
        let plugin = std::thread::scope(|s| {
            let compiling = std::thread::Builder::new()
                .name("tw-plugin-load".into())
                .stack_size(crate::plugin::pool::STACK)
                .spawn_scoped(s, || rt.load(source).map_err(load_error))
                .map_err(|e| {
                    LoadError::Engine(format!("cannot start a thread to compile the plugin: {e}"))
                })?;
            compiling
                .join()
                .unwrap_or_else(|_| Err(LoadError::Engine("compiling the plugin crashed".into())))
        })?;
        let manifest = manifest(plugin.manifest());
        Ok(Arc::new(Host { plugin, manifest }))
    }
}

/// 编好的一个插件。克隆、跨线程共享都便宜（`tw_plugin::Plugin` 里是一个 `Arc`）
struct Host {
    plugin: tw_plugin::Plugin,
    /// 换成网关那一份的 manifest。编的时候换一次，之后每次问都是它
    manifest: Manifest,
}

impl PluginHost for Host {
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
            tw_plugin::RequestOutcome::Rejected(r) => RequestOutcome::Rejected(r),
        })
    }

    fn reply(&self, ctx: Value) -> Result<Box<dyn ReplyHost>, RunError> {
        match self.plugin.reply(ctx) {
            Ok(r) => Ok(Box::new(Reply(r))),
            Err(e) => Err(run_error(e)),
        }
    }
}

/// 一个回答的实例
struct Reply(tw_plugin::Reply);

impl ReplyHost for Reply {
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

// ── 换类型 ───────────────────────────────────────────────────────

fn invocation<T, U>(inv: tw_plugin::Invocation<T>, f: impl FnOnce(T) -> U) -> Invocation<U> {
    Invocation {
        result: inv.result.map(f).map_err(run_error),
        logs: inv.logs.into_iter().map(log_line).collect(),
        cpu: inv.cpu,
    }
}

fn log_line(l: tw_plugin::LogLine) -> LogLine {
    LogLine {
        level: match l.level {
            tw_plugin::LogLevel::Log => tw_api::PluginLogLevel::Log,
            tw_plugin::LogLevel::Info => tw_api::PluginLogLevel::Info,
            tw_plugin::LogLevel::Warn => tw_api::PluginLogLevel::Warn,
            tw_plugin::LogLevel::Error => tw_api::PluginLogLevel::Error,
        },
        text: l.text,
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
        tw_plugin::LoadError::Manifest(d) => LoadError::Manifest(d),
        tw_plugin::LoadError::UnsupportedApi(api) => LoadError::UnsupportedApi(api),
        tw_plugin::LoadError::NotData {
            message,
            line,
            column,
        } => LoadError::NotData {
            message,
            line,
            column,
        },
        tw_plugin::LoadError::Engine(d) => LoadError::Engine(d),
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

fn request_kind(k: tw_plugin::RequestKind) -> tw_api::RequestKind {
    match k {
        tw_plugin::RequestKind::Conversation => tw_api::RequestKind::Conversation,
        tw_plugin::RequestKind::Embeddings => tw_api::RequestKind::Embeddings,
        tw_plugin::RequestKind::Completions => tw_api::RequestKind::Completions,
    }
}

pub(crate) fn on_error(o: tw_plugin::OnError) -> tw_api::OnError {
    match o {
        tw_plugin::OnError::Reject => tw_api::OnError::Reject,
        tw_plugin::OnError::Skip => tw_api::OnError::Skip,
    }
}

fn manifest(m: &tw_plugin::Manifest) -> Manifest {
    let granted: Vec<tw_api::Permission> = m.permissions.iter().copied().map(permission).collect();
    let handled: Vec<tw_api::RequestKind> = m.requests.iter().copied().map(request_kind).collect();
    Manifest {
        name: m.name.clone(),
        api: m.api,
        description: m.description.clone(),
        // 网关这边按 `Permission::ALL` 的顺序排
        permissions: tw_api::Permission::ALL
            .iter()
            .copied()
            .filter(|p| granted.contains(p))
            .collect(),
        requests: tw_api::RequestKind::ALL
            .iter()
            .copied()
            .filter(|k| handled.contains(k))
            .collect(),
        scope: Scope {
            clients: m.scope.clients.clone(),
            models: m.scope.models.clone(),
            upstreams: m.scope.upstreams.clone(),
        },
        on_error: on_error(m.on_error),
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
                value: s.value.clone(),
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
mod tests;

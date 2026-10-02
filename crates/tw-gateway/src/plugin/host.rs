//! 一个编好的插件。**数据面通过它跑钩子**（请求钩子、回答钩子），由运行时的适配层
//! 实现；测试有自己的替身（[`double`]）。
//!
//! 跑钩子的那几样照着 `tw-plugin` 的 Rust 接口写（约定第 5 节），一样一个：真正的
//! 运行时接进来只是一层把类型对上的适配。所有调用都是阻塞的、吃 CPU 的 —— 调用方
//! 一律放在 [`crate::plugin::pool`] 上跑，不在 tokio 的线程上调。

use std::time::Duration;

use serde_json::Value;

use crate::plugin::engine::Manifest;
use crate::plugin::set::LogLine;

pub trait PluginHost: Send + Sync {
    /// 编译时读到的 manifest，校验过的
    fn manifest(&self) -> &Manifest;
    /// 编出它的那一份字节的 SHA-256（不变式 I9 比对的就是它）
    fn sha256(&self) -> [u8; 32];

    /// **休眠的插件**：停用着、运行时没起，还没真的编过（见 [`crate::plugin::load`]）。
    /// 它跑不了任何钩子，`manifest()` 是缓存里的那一份（或者只有名字的占位），**只拿来
    /// 显示** —— 要跑它（试跑）、要按它的权限做判断，先真的编一遍
    fn dormant(&self) -> bool {
        false
    }

    /// 请求钩子：每次调用一个新实例（不变式 I3）。
    ///
    /// **没有实现的宿主跑不了钩子**（只拿来加载、展示的那些）：报一个沙箱错误，按插件
    /// 的 `on_error` 处置
    fn on_request(&self, view: Value, ctx: Value) -> Invocation<RequestOutcome> {
        let _ = (view, ctx);
        Invocation::err(RunError::Trap("this plugin host cannot run hooks".into()))
    }

    /// 给一次回答起一个实例，这次回答的所有回答钩子共用它，回答结束就扔掉
    fn reply(&self, ctx: Value) -> Result<Box<dyn ReplyHost>, RunError> {
        let _ = ctx;
        Err(RunError::Trap("this plugin host cannot run hooks".into()))
    }
}

/// 一次回答的插件实例。**只给这一次回答用**。
pub trait ReplyHost: Send {
    /// `None` 是没改
    fn on_text(&mut self, text: &str) -> Invocation<Option<String>>;
    /// 流式一块文字结束。`None` 是什么都不补
    fn on_text_end(&mut self) -> Invocation<Option<String>>;
    fn on_tool_call(&mut self, call: Value) -> Invocation<ToolCallOutcome>;
}

/// 一次调用的结果，连同这次调用写的日志和用掉的 CPU 时间。
#[derive(Debug)]
pub struct Invocation<T> {
    pub result: Result<T, RunError>,
    pub logs: Vec<LogLine>,
    pub cpu: Duration,
}

impl<T> Invocation<T> {
    pub fn ok(v: T) -> Self {
        Self {
            result: Ok(v),
            logs: Vec::new(),
            cpu: Duration::ZERO,
        }
    }

    pub fn err(e: RunError) -> Self {
        Self {
            result: Err(e),
            logs: Vec::new(),
            cpu: Duration::ZERO,
        }
    }
}

/// `onRequest` 的结局。
#[derive(Debug, Clone, PartialEq)]
pub enum RequestOutcome {
    /// 返回了 `undefined`
    Unchanged,
    /// 返回的视图（还没核对过）
    Changed(Value),
    /// 调了 `reject(原因)`
    Rejected(String),
}

/// `onToolCall` 的结局。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallOutcome {
    Unchanged,
    /// 换成这几个调用（一个对象也包成一个）。每个还没核对过
    Replace(Vec<Value>),
    Drop,
}

/// 插件没跑完。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    CpuLimit,
    MemoryLimit,
    OutputLimit,
    Threw {
        message: String,
        stack: Option<String>,
    },
    /// 返回值的形状不对（运行时查出来的那些：类型、能不能写成 JSON）
    BadOutput(String),
    Trap(String),
}

impl RunError {
    /// 记在这次运行上、报给客户端的那一句
    pub fn msg(&self) -> tw_types::Msg {
        use tw_types::msg;
        match self {
            RunError::CpuLimit => msg!(
                "gw.plugin.cpu_limit" => "The plugin used more CPU time than it is allowed."
            ),
            RunError::MemoryLimit => msg!(
                "gw.plugin.memory_limit" => "The plugin used more memory than it is allowed."
            ),
            RunError::OutputLimit => msg!(
                "gw.plugin.output_limit" => "The plugin returned more output than it is allowed."
            ),
            RunError::Threw { message, .. } => msg!(
                "gw.plugin.threw", message = message.clone() =>
                "The plugin threw an error: {message}"
            ),
            RunError::BadOutput(detail) => msg!(
                "gw.plugin.bad_output", detail = detail.clone() =>
                "The plugin returned something invalid: {detail}"
            ),
            RunError::Trap(detail) => msg!(
                "gw.plugin.trap", detail = detail.clone() =>
                "The sandbox stopped the plugin: {detail}"
            ),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg().text)
    }
}

/// 测试用的替身：钩子是 Rust 闭包。
///
/// **不在 `cfg(test)` 后面**：网关的集成测试（`tests/`）和管理面的测试都要用它，
/// 而那些只看得到公开的接口。
pub mod double {
    use std::sync::Arc;

    use super::*;
    use crate::plugin::engine::Hooks;
    use crate::plugin::set::Scope;

    type RequestFn = dyn Fn(Value, Value) -> Invocation<RequestOutcome> + Send + Sync;
    type ReplyFactory = dyn Fn(Value) -> Result<Box<dyn ReplyHost>, RunError> + Send + Sync;

    /// 一个插件替身。
    #[derive(Clone)]
    pub struct Double {
        manifest: Manifest,
        on_request: Option<Arc<RequestFn>>,
        reply: Option<Arc<ReplyFactory>>,
    }

    impl Double {
        /// 一个什么钩子都没有、什么权限都没要的插件。用下面几个方法补上
        pub fn new(name: &str) -> Self {
            Self {
                manifest: Manifest {
                    name: name.to_string(),
                    api: 1,
                    description: None,
                    permissions: Vec::new(),
                    scope: Scope::default(),
                    reply_mode: tw_api::ReplyMode::Block,
                    settings: Vec::new(),
                    hooks: Hooks::default(),
                },
                on_request: None,
                reply: None,
            }
        }

        pub fn permit(mut self, perms: &[tw_api::Permission]) -> Self {
            for p in perms {
                if !self.manifest.permissions.contains(p) {
                    self.manifest.permissions.push(*p);
                }
            }
            self
        }

        pub fn mode(mut self, mode: tw_api::ReplyMode) -> Self {
            self.manifest.reply_mode = mode;
            self
        }

        /// 请求钩子
        pub fn on_request(
            mut self,
            f: impl Fn(Value, Value) -> Invocation<RequestOutcome> + Send + Sync + 'static,
        ) -> Self {
            self.manifest.hooks.request = true;
            self.on_request = Some(Arc::new(f));
            self
        }

        /// 回答钩子：每次回答调一次 `factory` 起一个实例。三个开关说这个实例导出了哪几个
        pub fn on_reply(
            mut self,
            text: bool,
            text_end: bool,
            tool_call: bool,
            factory: impl Fn(Value) -> Result<Box<dyn ReplyHost>, RunError> + Send + Sync + 'static,
        ) -> Self {
            self.manifest.hooks.reply_text = text;
            self.manifest.hooks.reply_text_end = text_end;
            self.manifest.hooks.tool_call = tool_call;
            self.reply = Some(Arc::new(factory));
            self
        }

        /// 只改文字、没有状态的回答钩子：`f` 返回 `None` 是没改
        pub fn on_text(self, f: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
            let f = Arc::new(f);
            self.on_reply(true, false, false, move |_| {
                let f = f.clone();
                Ok(Box::new(Closures {
                    text: Box::new(move |t| Invocation::ok(f(t))),
                    end: Box::new(|| Invocation::ok(None)),
                    tool: Box::new(|_| Invocation::ok(ToolCallOutcome::Unchanged)),
                }))
            })
        }

        /// 只管工具调用、没有状态的回答钩子
        pub fn on_tool_call(
            self,
            f: impl Fn(Value) -> ToolCallOutcome + Send + Sync + 'static,
        ) -> Self {
            let f = Arc::new(f);
            self.on_reply(false, false, true, move |_| {
                let f = f.clone();
                Ok(Box::new(Closures {
                    text: Box::new(|_| Invocation::ok(None)),
                    end: Box::new(|| Invocation::ok(None)),
                    tool: Box::new(move |c| Invocation::ok(f(c))),
                }))
            })
        }

        pub fn into_host(self) -> Arc<dyn PluginHost> {
            Arc::new(self)
        }
    }

    impl PluginHost for Double {
        fn manifest(&self) -> &Manifest {
            &self.manifest
        }

        fn sha256(&self) -> [u8; 32] {
            [0; 32]
        }

        fn on_request(&self, view: Value, ctx: Value) -> Invocation<RequestOutcome> {
            match &self.on_request {
                Some(f) => f(view, ctx),
                None => Invocation::err(RunError::Trap("the plugin has no onRequest".into())),
            }
        }

        fn reply(&self, ctx: Value) -> Result<Box<dyn ReplyHost>, RunError> {
            match &self.reply {
                Some(f) => f(ctx),
                None => Err(RunError::Trap("the plugin has no reply hooks".into())),
            }
        }
    }

    type TextFn = dyn FnMut(&str) -> Invocation<Option<String>> + Send;
    type EndFn = dyn FnMut() -> Invocation<Option<String>> + Send;
    type ToolFn = dyn FnMut(Value) -> Invocation<ToolCallOutcome> + Send;

    /// 一个回答实例的替身：三个闭包，可以带状态（流式扣住文字的插件就要）。
    pub struct Closures {
        pub text: Box<TextFn>,
        pub end: Box<EndFn>,
        pub tool: Box<ToolFn>,
    }

    impl ReplyHost for Closures {
        fn on_text(&mut self, text: &str) -> Invocation<Option<String>> {
            (self.text)(text)
        }

        fn on_text_end(&mut self) -> Invocation<Option<String>> {
            (self.end)()
        }

        fn on_tool_call(&mut self, call: Value) -> Invocation<ToolCallOutcome> {
            (self.tool)(call)
        }
    }

    /// 测试里装一个插件：能跑、启用、范围不挑、出错时拒绝
    pub fn active(id: &str, d: Double) -> crate::plugin::set::Active {
        let m = d.manifest.clone();
        crate::plugin::set::Active {
            id: id.to_string(),
            name: m.name.clone(),
            enabled: true,
            on_error: tw_api::OnError::Reject,
            scope: Scope::default(),
            permissions: m.permissions.clone(),
            reply_mode: m.reply_mode,
            hooks: m.hooks,
            settings: Default::default(),
            manifest: Some(m),
            state: crate::plugin::set::State::Ready(Arc::new(d)),
            stats: Default::default(),
            logs: Default::default(),
        }
    }
}

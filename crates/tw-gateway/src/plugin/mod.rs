//! 脚本插件：请求发往上游之前改写请求，回答到达客户端之前改写回答。
//!
//! 插件是 JavaScript，**只在沙箱里跑**（`tw-plugin`，Wasmtime）。网关这一侧分三块：
//!
//! - [`engine`]：网关要从运行时那里拿到的东西 —— 把一份源码编成一个插件，读出它的
//!   manifest。真的运行时在 [`sandbox`]，测试可以换一个假的；
//! - [`host`]：一个编好的插件能做什么（跑请求钩子、回答钩子），数据面调它；
//! - [`set`]：跟着配置一起换的那一份 —— 每个配置了的插件此刻的样子（能跑、文件
//!   变了、加载出错）、范围、出错时怎么办，以及跨重载存活的计数和日志。
//!
//! **顺序就是配置里的顺序**：`plugins` 那一节从上到下，就是请求上一个接一个跑的
//! 顺序。
//!
//! 数据面跑插件的那几块：
//!
//! - [`view`]：把客户端那种格式的请求体读成插件看到的视图（每一项带一个网关发的
//!   `key`），按权限裁掉没给的部分；插件交回来之后按改写规则逐条核对，再**只改
//!   动过的那几项**写回原来的 JSON —— 缓存断点、签名、图片和不认识的字段原样留着，
//!   什么都没改时一个字节都不动。
//! - [`bridge`]：插件永远看不到真的密钥（不变式 I5）。进插件之前按出站脱敏的规则把
//!   认得出的密钥换成占位符，出来之后换回去；**不看脱敏开在哪一档**。
//! - [`request`]：请求钩子。排在路由之后，**每发往一个上游跑一次**（契约附录二的 I7、
//!   I8）：按这一次的客户端、发出去的模型和上游挑插件，从客户端的原话起改；换上游从
//!   原话重来，同一家重发不重跑。改过的请求再过一遍内容审查，然后才转换格式、脱敏。
//! - [`reply`]：回答钩子。排在格式转换之后、工具调用审查和输出长度之前（I7）——
//!   这两道防护看的就是插件改过的那一版。
//! - [`pool`]：插件调用都是阻塞的、吃 CPU 的，放在专用线程池上跑，不占 tokio 的线程。
//! - [`trial`]：对着存下来的请求和回答试跑一个插件。
//!
//! [`defaults`] 是随 core 一起发的那几个插件（清单和源码）。

pub mod bridge;
pub mod defaults;
pub mod engine;
/// 测试用的假引擎（见里面的说明）。**不是给生产用的**
#[doc(hidden)]
pub mod fake;
pub mod host;
pub mod load;
pub mod pool;
pub mod reply;
pub mod request;
pub mod sandbox;
pub mod set;
pub mod trial;
pub mod view;

pub use engine::{Engine, Hooks, LoadError, MAX_SOURCE, Manifest, SettingSpec, Unavailable};
pub use host::{Invocation, PluginHost, ReplyHost, RequestOutcome, RunError, ToolCallOutcome};
pub use load::{Plugins, RUN_CHANNEL_CAP, RunRecord, RunSender};
pub use set::{Active, Broken, LogLine, LogRing, PluginRun, PluginSet, Scope, State, Stats};

/// 客户端格式在插件那一侧的写法（`ctx.format`、视图的 `format`）。
pub fn format_name(d: tw_dialect::ir::Dialect) -> &'static str {
    use tw_dialect::ir::Dialect;
    match d {
        Dialect::Anthropic => "anthropic",
        Dialect::Chat => "openai_chat",
        Dialect::Responses => "openai_responses",
        Dialect::Gemini => "gemini",
        // 客户端不会说这种格式（Bedrock 只是上游）
        Dialect::Bedrock => "bedrock",
    }
}

use tw_types::Msg;

impl crate::AppState {
    /// 记一次插件运行（不变式 I10：每一次运行的结果都记在请求上、界面看得到）。
    ///
    /// 计数、日志进这个插件自己的那一份（跨重载存活，见 [`Stats`]、[`LogRing`]）；
    /// 运行记录交给存储层落在那个请求上（`plugin_runs`）；出了错的再发一条
    /// `plugin_failed` 给通知用。**数据面每跑完一个插件调一次**，跳过的（插件没加载
    /// 起来、设的是出错时跳过）也调：那也是这个请求上发生过的事。
    pub fn plugin_ran(&self, request_id: u64, active: &Active, run: PluginRun, logs: Vec<LogLine>) {
        let at_ms = now_ms();
        active.stats.note(&run, at_ms);
        active.logs.extend(at_ms, Some(request_id), run.hook, logs);
        if run.outcome == tw_api::PluginOutcome::Error {
            let message = run.error.clone().unwrap_or_else(unknown_failure);
            self.bus.emit(tw_api::Event::PluginFailed {
                id: self.bus.next_id(),
                plugin_id: active.id.clone(),
                plugin_name: active.name.clone(),
                request_id: Some(request_id),
                message,
                at_ms,
            });
        }
        self.plugins.offer(RunRecord {
            request_id,
            at_ms,
            run,
        });
    }

    /// 接上运行记录的去处。**观测层起来之后才调** —— 在那之前只计数、不落库
    pub fn set_plugin_sink(&self, tx: RunSender) {
        self.plugins.set_sink(tx);
    }
}

/// 这个进程用的插件运行时：`tw-plugin` 的沙箱（见 [`sandbox`]）。**第一次编插件时
/// 才真的起来**；起不来时每个插件都「加载不了」，管得着的请求照它的 `on_error` 处置。
pub fn default_engine() -> std::sync::Arc<dyn Engine> {
    std::sync::Arc::new(sandbox::Sandbox)
}

/// 出错却没说为什么。数据面总该给一句，这里只是不让通知空着
fn unknown_failure() -> Msg {
    tw_types::msg!("gw.plugin.failed" => "The plugin failed.")
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn state() -> crate::AppState {
        crate::AppState::new(tw_config::Config {
            clients: vec![tw_config::Client {
                name: "c".into(),
                key: "tw-k".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap()
    }

    fn active() -> Active {
        Active {
            id: "add-date".into(),
            name: "附加日期".into(),
            enabled: true,
            on_error: tw_api::OnError::Reject,
            scope: Scope::default(),
            permissions: vec![tw_api::Permission::System],
            reply_mode: tw_api::ReplyMode::Block,
            hooks: Hooks {
                request: true,
                ..Default::default()
            },
            settings: Default::default(),
            manifest: None,
            state: State::Broken(Broken::Changed),
            stats: Arc::default(),
            logs: Arc::default(),
        }
    }

    fn run(outcome: tw_api::PluginOutcome) -> PluginRun {
        PluginRun {
            plugin_id: "add-date".into(),
            plugin_name: "附加日期".into(),
            hook: tw_api::PluginHook::Request,
            outcome,
            error: (outcome == tw_api::PluginOutcome::Error)
                .then(|| tw_types::msg!("t.plugin_threw" => "it threw")),
            cpu_us: 120,
            detail: None,
        }
    }

    /// 一次运行进计数、进日志；出错的再发一条通知，说清是哪个插件、哪个请求
    #[tokio::test]
    async fn a_failed_run_is_counted_logged_and_announced() {
        let s = state();
        let mut rx = s.bus.subscribe();
        let a = active();
        s.plugin_ran(
            41,
            &a,
            run(tw_api::PluginOutcome::Unchanged),
            vec![LogLine {
                level: tw_api::PluginLogLevel::Info,
                text: "hello".into(),
            }],
        );
        s.plugin_ran(42, &a, run(tw_api::PluginOutcome::Error), Vec::new());

        let stats = a.stats.view();
        assert_eq!((stats.calls, stats.errors), (2, 1));
        let logs = a.logs.lines();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            (logs[0].request_id, logs[0].text.as_str()),
            (Some(41), "hello")
        );

        let ev = rx.try_recv().expect("no plugin_failed event");
        let tw_api::Event::PluginFailed {
            plugin_id,
            plugin_name,
            request_id,
            message,
            ..
        } = ev
        else {
            panic!("another event came first: {ev:?}");
        };
        assert_eq!(plugin_id, "add-date");
        assert_eq!(plugin_name, "附加日期");
        assert_eq!(request_id, Some(42));
        assert_eq!(message.code, "t.plugin_threw");
        assert!(rx.try_recv().is_err(), "an unchanged run was announced");
    }
}

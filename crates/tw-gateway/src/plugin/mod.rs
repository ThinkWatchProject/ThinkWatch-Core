//! 脚本插件：请求发往上游之前改写请求，回答到达客户端之前改写回答。
//!
//! 插件是 JavaScript，**只在沙箱里跑**（`tw-plugin`，Wasmtime）。网关这一侧分三块：
//!
//! - [`engine`]：网关要从运行时那里拿到的东西 —— 把一份源码编成一个插件，读出它的
//!   manifest。运行时还没接上时由一个替身顶着，所有插件都是「加载不了」；
//! - [`host`]：一个编好的插件能做什么（跑请求钩子、回答钩子），数据面调它；
//! - [`set`]：跟着配置一起换的那一份 —— 每个配置了的插件此刻的样子（能跑、文件
//!   变了、加载出错）、范围、出错时怎么办，以及跨重载存活的计数和日志。
//!
//! **顺序就是配置里的顺序**：`plugins` 那一节从上到下，就是请求上一个接一个跑的
//! 顺序。

pub mod engine;
pub mod host;
pub mod set;

pub use engine::{Engine, Hooks, LoadError, Manifest, SettingSpec, Unavailable};
pub use host::PluginHost;
pub use set::{Active, Broken, LogLine, LogRing, PluginRun, PluginSet, Scope, State, Stats};

use tw_types::Msg;

impl crate::AppState {
    /// 记一次插件运行（不变式 I10：每一次运行的结果都记在请求上、界面看得到）。
    ///
    /// 计数、日志进这个插件自己的那一份（跨重载存活，见 [`Stats`]、[`LogRing`]）；
    /// 出了错的再发一条 `plugin_failed` 给通知用。**数据面每跑完一个插件调一次**，
    /// 跳过的（插件没加载起来、设的是出错时跳过）也调：那也是这个请求上发生过的事。
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
    }
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

//! 网关要从插件运行时那里拿到的东西：把一份源码编成插件，读出它的 manifest。
//!
//! **这里是一道接缝**：沙箱在 `tw-plugin` 里（接上它的是 [`crate::plugin::sandbox`]），
//! 网关只认这个 trait。没有运行时可用时由 [`Unavailable`] 顶着 —— 每个插件都「加载
//! 不了」，有一个就拒一个请求（出错时拒绝是出厂的做法），而不是悄悄放过。测试拿一个
//! 假的引擎接在这里。

use std::sync::Arc;

use tw_types::{Msg, msg};

use crate::plugin::host::PluginHost;
use crate::plugin::set::Scope;

/// 一个插件文件最多多大。**读文件时也按它截**：再大的文件反正编不了，不必整个读进来
pub const MAX_SOURCE: usize = 1024 * 1024;

/// 插件文件里 `manifest` 写的东西，加上它导出了哪些钩子。**由运行时读出来、校验过**：
/// 权限和钩子对得上、设置项不超过上限，这里拿到的都是合规的。
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// 插件自己起的名字。**插件写的字**：界面当纯文本显示
    pub name: String,
    /// 插件接口的版本。只有 1
    pub api: u32,
    pub description: Option<String>,
    /// 按 [`tw_api::Permission::ALL`] 的顺序，不重复
    pub permissions: Vec<tw_api::Permission>,
    /// 插件处理哪几种请求（manifest 的 `requests`，没写是只有对话）。按
    /// [`tw_api::RequestKind::ALL`] 的顺序，不重复、不空。**别的种类的请求不过它**（见
    /// [`crate::plugin::set::PluginSet::for_request`]）
    pub requests: Vec<tw_api::RequestKind>,
    /// 插件建议的范围。装上时照它填进配置，之后以配置为准
    pub scope: Scope,
    pub reply_mode: tw_api::ReplyMode,
    /// 按插件写的顺序
    pub settings: Vec<SettingSpec>,
    pub hooks: Hooks,
}

/// 一个设置项。
#[derive(Debug, Clone, PartialEq)]
pub struct SettingSpec {
    pub key: String,
    pub kind: tw_api::SettingKind,
    /// 插件写的字
    pub label: String,
    /// 和 `kind` 同一种类型
    pub default: serde_json::Value,
}

/// 插件导出了哪些钩子。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hooks {
    /// `onRequest`
    pub request: bool,
    /// `onReplyText`
    pub reply_text: bool,
    /// `onReplyTextEnd`，只在 stream 模式下有
    pub reply_text_end: bool,
    /// `onToolCall`
    pub tool_call: bool,
}

impl Hooks {
    /// 回答那一段有没有它的事
    pub fn on_reply(&self) -> bool {
        self.reply_text || self.tool_call
    }
}

/// 不写 `requests` 的插件处理的那几种：只有对话。读不出 manifest 的插件也按它算
pub const DEFAULT_REQUESTS: &[tw_api::RequestKind] = &[tw_api::RequestKind::Conversation];

/// 编不成的原因。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LoadError {
    #[error("the plugin file is larger than the limit")]
    TooLarge,
    /// 语法错。行列从 1 起，运行时说得出来才有
    #[error("{message}")]
    Syntax {
        message: String,
        line: Option<u32>,
        column: Option<u32>,
    },
    /// manifest 不合规矩：缺字段、权限和钩子对不上、设置项写错了……原话是运行时的
    #[error("{0}")]
    Manifest(String),
    #[error("the plugin is written for plugin API {0}, and only API 1 is supported")]
    UnsupportedApi(u32),
    /// 运行时自己出了问题，或者根本没有运行时（见 [`Unavailable`]）
    #[error("{0}")]
    Engine(String),
}

impl LoadError {
    /// 给人看的那句话，带码。语法错和 manifest 的原话是运行时的，放在 `detail` 里
    pub fn msg(&self) -> Msg {
        match self {
            LoadError::TooLarge => msg!(
                "gw.plugin.too_large", max = MAX_SOURCE =>
                "The plugin file is larger than {max} bytes."
            ),
            LoadError::Syntax {
                message,
                line: Some(line),
                column,
            } => msg!(
                "gw.plugin.syntax_at", line = line, column = column.unwrap_or(1), detail = message =>
                "The plugin has a syntax error at line {line}, column {column}: {detail}"
            ),
            LoadError::Syntax { message, .. } => msg!(
                "gw.plugin.syntax", detail = message =>
                "The plugin has a syntax error: {detail}"
            ),
            LoadError::Manifest(d) => msg!(
                "gw.plugin.manifest", detail = d =>
                "The plugin's manifest is not valid: {detail}"
            ),
            LoadError::UnsupportedApi(api) => msg!(
                "gw.plugin.api", api = api =>
                "The plugin is written for plugin API {api}, and only API 1 is supported."
            ),
            LoadError::Engine(d) => msg!(
                "gw.plugin.engine", detail = d =>
                "The plugin engine cannot load plugins: {detail}"
            ),
        }
    }
}

/// 插件运行时。**一个进程一个**，所有插件共用。
pub trait Engine: Send + Sync {
    /// 用**正好这些字节**编一个插件（不变式 I9：跑的就是哈希过、比对过的那一份）。
    fn load(&self, source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError>;
}

/// 没有运行时：每个插件都加载不了，原因就是 `reason`。
#[derive(Debug, Clone)]
pub struct Unavailable(pub String);

impl Default for Unavailable {
    fn default() -> Self {
        Self("the plugin engine is not available in this build".into())
    }
}

impl Engine for Unavailable {
    fn load(&self, _source: &[u8]) -> Result<Arc<dyn PluginHost>, LoadError> {
        Err(LoadError::Engine(self.0.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_an_engine_every_plugin_fails_to_load_and_says_why() {
        let e = Unavailable::default();
        let Err(LoadError::Engine(why)) = e.load(b"export const manifest = {}") else {
            panic!("the stand-in engine loaded something");
        };
        assert!(why.contains("not available"), "{why}");
    }

    #[test]
    fn only_text_and_tool_call_hooks_make_a_plugin_part_of_the_reply() {
        assert!(!Hooks::default().on_reply());
        assert!(
            !Hooks {
                request: true,
                ..Default::default()
            }
            .on_reply()
        );
        assert!(
            Hooks {
                tool_call: true,
                ..Default::default()
            }
            .on_reply()
        );
    }
}

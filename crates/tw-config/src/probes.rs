//! 客户端自己发的辅助请求怎么处理（DESIGN.md §4.8）。
//!
//! Claude Code 发出的请求里有相当一部分不是用户发的：连通性探测、预热、
//! 给会话起标题、话题检测。它们都走完整的计费链路 —— 占额度、算钱、
//! 可能撞限流。
//!
//! **但它们不是同一类东西。**分水岭是「拦掉之后用户会不会少一样东西」：
//!
//! - A 类（探测、预热）没有用户可见产物，本地应答是纯赚；
//! - B 类（标题、话题、建议）有。对标题请求一律回一个固定字符串，
//!   意味着用户在 `/resume` 里看到的每个会话都叫同一个名字 —— 那不是
//!   省钱，那是把一个功能关掉了。
//!
//! 所以默认只拦 A 类。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeAction {
    /// 本地应答，一个字节都不发给上游
    Intercept,
    /// 原样放行
    Passthrough,
    /// 交给路由规则。**前提是你手里真有一个更便宜的地方**（§4.8）——
    /// 这类请求本来就走客户端的小模型档，在同一个上游内部已经没有更
    /// 便宜的可换了。
    Route,
}

/// 五种辅助请求各自怎么处理。
///
/// **默认值的分布本身就是那条判据**：A 类拦、B 类放行。改这个默认值
/// 之前先回答「拦掉之后用户会不会少一样东西」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientProbes {
    /// A 类。`max_tokens: 1` 的连通性检查
    #[serde(default = "intercept")]
    pub health_check: ProbeAction,
    /// A 类。正文恰好是 `Warmup`
    #[serde(default = "intercept")]
    pub warmup: ProbeAction,
    /// **B 类，默认放行。**拦了的话每个会话都叫同一个名字。
    #[serde(default = "passthrough")]
    pub titling: ProbeAction,
    /// B 类
    #[serde(default = "passthrough")]
    pub topic_detect: ProbeAction,
    /// B 类
    #[serde(default = "passthrough")]
    pub suggestion: ProbeAction,
}

fn intercept() -> ProbeAction {
    ProbeAction::Intercept
}
fn passthrough() -> ProbeAction {
    ProbeAction::Passthrough
}

impl Default for ClientProbes {
    fn default() -> Self {
        Self {
            health_check: ProbeAction::Intercept,
            warmup: ProbeAction::Intercept,
            titling: ProbeAction::Passthrough,
            topic_detect: ProbeAction::Passthrough,
            suggestion: ProbeAction::Passthrough,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_is_intercepted_and_b_class_is_not() {
        // 这条测的是一个产品决定，不是一个实现细节：B 类有用户可见的
        // 产物，拦掉等于把功能关了。
        let d = ClientProbes::default();
        assert_eq!(d.health_check, ProbeAction::Intercept);
        assert_eq!(d.warmup, ProbeAction::Intercept);
        assert_eq!(d.titling, ProbeAction::Passthrough);
        assert_eq!(d.topic_detect, ProbeAction::Passthrough);
        assert_eq!(d.suggestion, ProbeAction::Passthrough);
    }

    #[test]
    fn only_the_key_you_wrote_moves_the_rest_stay_default() {
        // 配置文件里写一行不该把另外四行也重置掉。
        let c: ClientProbes = serde_yaml_ng::from_str("titling: route").unwrap();
        assert_eq!(c.titling, ProbeAction::Route);
        assert_eq!(c.health_check, ProbeAction::Intercept);
        assert_eq!(c.suggestion, ProbeAction::Passthrough);
    }

    #[test]
    fn a_misspelled_probe_name_is_an_error() {
        // `warmup` 写成 `warm_up` 被静默忽略的话，用户会以为预热请求
        // 已经被拦住了，而它一直在计费。
        assert!(serde_yaml_ng::from_str::<ClientProbes>("warm_up: route").is_err());
    }
}

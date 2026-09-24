//! 给人看的话：[`Msg`] 和造它的 [`msg!`]。
//!
//! 桌面网关里会造消息的 crate 有一半够不着 tw-api，所以它单独住一个零业务
//! 依赖的 crate。企业版不用它。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一条给人看的话：一个稳定的码、填进句子的参数，以及英文原句。
///
/// **core 不翻译，只出英文。**桌面版有中英两套界面，而这些句子是从这里
/// 发出去的。在这里翻译等于把「界面现在是什么语言」塞进一个同时服务
/// 命令行、桌面版和企业版的网关进程 —— 那个问题在这一层没有答案。
///
/// 所以这里给的是码加参数：界面拿 `code` 去自己的词表里找句子，用 `args`
/// 填空。没有词表的一方（命令行、第三方客户端）显示 `text` —— 一句英文
/// 总好过一个码。
///
/// **码是契约，句子不是。**改措辞不用动码，界面那边什么都不用做；只有
/// 语义变了才换码 —— 那时界面里那条旧文案会随着码一起失效，而不是
/// 悄悄留在那里说着一件不再为真的事。
///
/// 码的写法是点分小写，从粗到细：`l1.dns.timeout`、`l1.proxy.auth_rejected`。
/// 第一段是发出它的那一层。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub code: String,
    /// 填进句子里的参数，按名字取。**值已经写成字符串** —— 界面只是
    /// 把它们放进自己那句话里，不需要知道原来是什么类型
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
    /// 英文原句，参数已经填好
    pub text: String,
}

impl std::fmt::Display for Msg {
    /// 命令行和日志里就用英文原句。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Msg {
    /// 取一个参数。**没有就是空串** —— 调用方是在拼一句话，为一个缺席的
    /// 参数 panic 没有意义
    pub fn arg(&self, name: &str) -> &str {
        self.args.get(name).map(String::as_str).unwrap_or_default()
    }
}

/// 造一条 [`Msg`]。
///
/// ```ignore
/// msg!("l1.dns.no_records", host = host => "`{host}` resolved, but to no addresses");
/// ```
///
/// 参数先 `let` 出来，所以句子里直接写 `{host}` 就能取到，同时它们原样
/// 进 `args` —— **两边不会说不同的话**，这正是分开写最容易出错的地方。
#[macro_export]
macro_rules! msg {
    ($code:expr $(, $name:ident = $value:expr)* $(,)? => $($fmt:tt)+) => {{
        $(let $name = $value;)*
        $crate::Msg {
            code: ($code).into(),
            args: ::std::collections::BTreeMap::from([
                $((
                    ::std::string::String::from(stringify!($name)),
                    ::std::string::ToString::to_string(&$name),
                ),)*
            ]),
            text: format!($($fmt)+),
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_carries_its_arguments_beside_the_sentence() {
        // 界面要照自己的语序重写这句话，所以参数必须单独拿得到 ——
        // 从成句的英文里再切出主机名是切不准的。
        let m = msg!(
            "l1.dns.timeout", host = "api.example.com", seconds = 8u64 =>
            "Resolving {host} did not finish within {seconds} s."
        );
        assert_eq!(m.code, "l1.dns.timeout");
        assert_eq!(m.arg("host"), "api.example.com");
        assert_eq!(m.arg("seconds"), "8");
        assert_eq!(
            m.text,
            "Resolving api.example.com did not finish within 8 s."
        );
        // 没有的参数是空串，不是 panic
        assert_eq!(m.arg("port"), "");
    }

    #[test]
    fn a_message_without_arguments_leaves_the_map_out_of_the_wire() {
        let m = msg!("l1.config.no_host" => "The endpoint address has no host name.");
        assert!(m.args.is_empty());
        let wire = serde_json::to_value(&m).unwrap();
        assert!(wire.get("args").is_none(), "{wire}");
        assert_eq!(
            serde_json::from_value::<Msg>(wire).unwrap(),
            m,
            "少了 args 也要读得回来"
        );
    }
}

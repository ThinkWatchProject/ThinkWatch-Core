//! 防护：出站脱敏、对上游返回的工具调用的审查、按规则过滤调用方发来的正文。
//!
//! **两个产品的规则模型只在这里定义一份。**档位、内置规则目录、启停、处置、自定义规则、
//! 校验（[`policy`]），规则视图（[`view`]）和「测试…」（[`trial`]）：桌面版写进
//! `config.yaml`，企业版存进系统设置，结构相同，管理接口返回同一份 JSON。两边各管的只是
//! 存在哪儿、错误怎么报、命中之后记在哪儿。
//!
//! 引擎也在这里：「怎么找、怎么换、怎么在流里换回来」（[`redact`]），「怎么审查一个工具
//! 调用」（[`tools`]），「查调用方的哪些正文、怎么匹配、怎么删」（[`content`]）。扫客户端
//! 配置文件用的隐藏字符检测在 [`hidden`]（桌面版的配置扫描用它）。

/// 取值是一个固定集合的枚举：配置里、线上写的就是那个词。和桌面版控制面契约里
/// `slug_enum!` 给的是同一套：全部取值、词、从词读回、写出来就是词、和词比
macro_rules! words {
    ($t:ty { $($v:ident = $s:literal),+ $(,)? }) => {
        impl $t {
            /// 全部取值，按声明的顺序
            pub const ALL: &'static [Self] = &[$(Self::$v),+];
            /// 写出来的那个词
            pub fn slug(self) -> &'static str {
                match self {
                    $(Self::$v => $s,)+
                }
            }
            /// 反过来。不在集合里的是 `None`
            pub fn from_slug(s: &str) -> Option<Self> {
                match s {
                    $($s => Some(Self::$v),)+
                    _ => None,
                }
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.slug())
            }
        }
        impl PartialEq<str> for $t {
            fn eq(&self, other: &str) -> bool {
                self.slug() == other
            }
        }
        impl PartialEq<&str> for $t {
            fn eq(&self, other: &&str) -> bool {
                self.slug() == *other
            }
        }
    };
}

pub mod content;
pub mod hidden;
pub mod policy;
pub mod redact;
pub mod tools;
pub mod trial;
pub mod view;

/// 编一条调用方给的正则。**编译后的大小有上限**（NFA 和 DFA 各 1 MiB）——
/// 这条正则要在每个请求上跑，而 `(a|aa){200}` 这种写法在默认的 10 MiB 上限下
/// 要编好几秒、占几 MB，然后每个请求都付一遍。
fn bounded(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
}

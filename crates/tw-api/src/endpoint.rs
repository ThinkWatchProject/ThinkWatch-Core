//! 控制面的每一个端点：方法、路径、请求和响应的类型，**只在这里写一次**。
//!
//! 以前一个端点要写四遍：core 注册路由一遍、桌面端的 Rust 客户端拼路径一遍、
//! Tauri 命令一遍、前端再一遍。路径是字符串，两边各拼各的，拼错的表现是
//! 运行时的 404 —— 编译器看不见。现在路径和类型住在这里：core 按它注册，
//! 客户端按它拼，改了哪一边另一边都编不过。
//!
//! 请求放在哪儿由方法决定：**GET 和 DELETE 走查询串，其余走 JSON 请求体**。
//! 两样都不需要的端点，`Req` 是 `()`。
//!
//! 失败时的响应体一律是 [`ErrorBody`]，每个端点都一样，所以不在描述里重复。

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 控制面上所有非 2xx 响应的响应体：一条带码的 [`Msg`](crate::Msg)。
///
/// **每个端点都是它，包括框架替我们回的那些**（请求体解析不了、路径不存在、
/// 方法不对、没带凭据）—— core 在最外层把它们也换成了这个形状。客户端拿到
/// 非 2xx 就按它解析，不用看响应体是不是以 `{` 开头。
pub type ErrorBody = crate::Msg;

/// HTTP 方法。**只列控制面用到的这几个。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// 请求放在查询串里吗。否则是 JSON 请求体。
    pub const fn query(self) -> bool {
        matches!(self, Method::Get | Method::Delete)
    }
}

/// 成功的响应体长什么样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `Res` 的 JSON。绝大多数端点
    Json,
    /// 一段纯文本，`Res` 是 `String`（诊断包、回放用例）
    Text,
    /// Server-Sent Events，每条 `data:` 是一个 `Res` 的 JSON。连着不断
    Events,
}

impl Format {
    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Text => "text",
            Format::Events => "events",
        }
    }
}

/// 一个控制面端点。实现都在 [`ep`](crate::ep) 里，由 `endpoints!` 生成。
///
/// ```ignore
/// use tw_api::{Endpoint, ep};
/// // 路径带参数的，拼路径用生成的 `path`，参数会做百分号编码
/// let path = ep::UpdateProvider::path("relay cn"); // "/providers/relay%20cn"
/// let _: <ep::UpdateProvider as Endpoint>::Req;    // tw_api::ProviderSave
/// ```
pub trait Endpoint {
    const METHOD: Method;
    /// 路径模板，参数写成 `{name}`（和 axum 同一种写法，core 直接拿它注册）
    const PATH: &'static str;
    /// 模板里的参数名，按出现的顺序
    const PARAMS: &'static [&'static str];
    const FORMAT: Format;
    /// 名字，和 [`ep`](crate::ep) 里的类型名一样。导出给前端时用它当键
    const NAME: &'static str;
    /// GET/DELETE 的查询串，或者其余方法的 JSON 请求体。没有就是 `()`
    type Req: Serialize + DeserializeOwned;
    /// 成功时的响应体。`Format::Events` 时是每一条事件
    type Res: Serialize + DeserializeOwned;
}

/// 一个端点的静态描述，[`ep::ALL`](crate::ep::ALL) 里一个端点一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub name: &'static str,
    pub method: Method,
    pub path: &'static str,
    pub params: &'static [&'static str],
    pub format: Format,
    /// 请求类型在 Rust 里的写法，`()` 是没有请求。给人看、给测试用
    pub req: &'static str,
    /// 响应类型在 Rust 里的写法
    pub res: &'static str,
}

impl Info {
    /// 这个端点带请求吗（查询串或者请求体）
    pub fn has_req(&self) -> bool {
        self.req != "()"
    }
}

/// 把参数填进路径模板。**每个值都做百分号编码**：名字里可以有空格、斜杠，
/// 原样拼进去的话 `a/b` 会变成两段路径。
///
/// 模板里没有的参数会 panic —— 那是 `endpoints!` 里写错了，测试里就会炸。
pub fn fill(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in params {
        let slot = format!("{{{name}}}");
        assert!(
            out.contains(&slot),
            "`{template}` has no parameter `{name}`"
        );
        out = out.replace(&slot, &encode(value));
    }
    out
}

/// RFC 3986 的 unreserved 以外一律编码。
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 一个端点一行：
///
/// ```text
/// 名字: 方法 "路径" [路径参数…], 请求 => 响应, 格式;
/// ```
///
/// 格式缺省是 JSON。
macro_rules! endpoints {
    (@method GET) => { $crate::endpoint::Method::Get };
    (@method POST) => { $crate::endpoint::Method::Post };
    (@method PUT) => { $crate::endpoint::Method::Put };
    (@method PATCH) => { $crate::endpoint::Method::Patch };
    (@method DELETE) => { $crate::endpoint::Method::Delete };
    (@format) => { $crate::endpoint::Format::Json };
    (@format text) => { $crate::endpoint::Format::Text };
    (@format events) => { $crate::endpoint::Format::Events };
    ($(
        $(#[$meta:meta])*
        $name:ident: $method:ident $path:literal $([$($param:ident),+])?, $req:ty => $res:ty $(, $fmt:ident)?;
    )*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy)]
            pub struct $name;

            impl $crate::endpoint::Endpoint for $name {
                const METHOD: $crate::endpoint::Method = endpoints!(@method $method);
                const PATH: &'static str = $path;
                const PARAMS: &'static [&'static str] = &[$($(stringify!($param)),+)?];
                const FORMAT: $crate::endpoint::Format = endpoints!(@format $($fmt)?);
                const NAME: &'static str = stringify!($name);
                type Req = $req;
                type Res = $res;
            }

            impl $name {
                /// 填好参数的路径
                #[allow(clippy::too_many_arguments)]
                pub fn path($($($param: impl ::std::fmt::Display),+)?) -> String {
                    $crate::endpoint::fill(
                        $path,
                        &[$($((stringify!($param), $param.to_string().as_str())),+)?],
                    )
                }
            }
        )*

        /// 所有端点，按声明的顺序。
        pub const ALL: &[$crate::endpoint::Info] = &[
            $($crate::endpoint::Info {
                name: stringify!($name),
                method: endpoints!(@method $method),
                path: $path,
                params: &[$($(stringify!($param)),+)?],
                format: endpoints!(@format $($fmt)?),
                req: stringify!($req),
                res: stringify!($res),
            }),*
        ];

        /// 每个端点的请求和响应类型都走一遍。导出 TypeScript 用
        #[cfg(feature = "ts")]
        pub(crate) fn visit_types(v: &mut impl $crate::ts::Visit) {
            $(
                v.endpoint::<$name>();
            )*
        }
    };
}

pub(crate) use endpoints;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_percent_encoded_so_they_stay_one_segment() {
        assert_eq!(
            fill("/providers/{name}", &[("name", "relay cn/中")]),
            "/providers/relay%20cn%2F%E4%B8%AD"
        );
        assert_eq!(
            fill(
                "/security/{guard}/custom/{name}",
                &[("guard", "outbound"), ("name", "a.b-c_d~e")]
            ),
            "/security/outbound/custom/a.b-c_d~e"
        );
    }

    #[test]
    #[should_panic(expected = "has no parameter")]
    fn a_parameter_the_template_does_not_have_is_a_bug() {
        fill("/providers/{name}", &[("id", "x")]);
    }
}

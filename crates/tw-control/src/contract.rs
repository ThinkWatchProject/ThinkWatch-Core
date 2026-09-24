//! 按 tw-api 的端点描述挂处理函数，**并让编译器核对两边说的是同一件事**。
//!
//! 路径和方法只在 [`tw_api::ep`] 里写一次。挂的时候（[`RouterExt::at`]）还要求：
//!
//! - 处理函数的最后一个参数是 `Json<E::Req>` / `Query<E::Req>`；没有请求的端点
//!   （`Req = ()`）最后一个参数是 `State` 或 `Path`，或者干脆没有参数；
//! - 它的返回值是 `E::Res` 的 JSON（或文本、事件流）。
//!
//! 所以描述里的类型和处理函数对不上时，编不过 —— 而不是等客户端解析失败。
//!
//! 另外，[`errors_are_messages`] 把框架替我们回的那些错误（请求体解析不了、
//! 路径不存在、方法不对）也换成 [`tw_api::ErrorBody`]，客户端碰到的每一个
//! 非 2xx 都是同一个形状。

use std::future::Future;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::handler::Handler;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::Sse;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use tw_api::{Endpoint, Method};
use tw_types::msg;

use crate::{ControlState, Fail};

pub(crate) trait RouterExt {
    /// 把 `h` 挂到端点 `E` 上：路径、方法都取自 `E`。
    fn at<E, H, T>(self, e: E, h: H) -> Self
    where
        E: Endpoint,
        H: Handler<T, ControlState> + Serves<E, T>,
        T: 'static;
}

impl RouterExt for axum::Router<ControlState> {
    fn at<E, H, T>(self, _: E, h: H) -> Self
    where
        E: Endpoint,
        H: Handler<T, ControlState> + Serves<E, T>,
        T: 'static,
    {
        self.route(E::PATH, on(filter(E::METHOD), h))
    }
}

fn filter(m: Method) -> MethodFilter {
    match m {
        Method::Get => MethodFilter::GET,
        Method::Post => MethodFilter::POST,
        Method::Put => MethodFilter::PUT,
        Method::Patch => MethodFilter::PATCH,
        Method::Delete => MethodFilter::DELETE,
    }
}

/// 这个处理函数服务得了端点 `E`。`T` 和 axum 的 `Handler<T, _>` 是同一个元组。
pub(crate) trait Serves<E, T> {}

/// 处理函数的最后一个参数收的是 `E` 的请求。
pub(crate) trait Takes<E> {}

impl<E: Endpoint> Takes<E> for Json<E::Req> {}
impl<E: Endpoint> Takes<E> for Query<E::Req> {}
impl<E: Endpoint<Req = ()>, S> Takes<E> for State<S> {}
impl<E: Endpoint<Req = ()>, P> Takes<E> for Path<P> {}

/// 处理函数的返回值是 `E` 的响应。
pub(crate) trait Answer<E> {}

impl<E: Endpoint> Answer<E> for Json<E::Res> {}
impl<E: Endpoint> Answer<E> for Result<Json<E::Res>, Fail> {}
impl<E: Endpoint> Answer<E> for (StatusCode, Json<E::Res>) {}
impl<E: Endpoint<Res = String>> Answer<E> for String {}
impl<E: Endpoint<Res = String>> Answer<E> for Result<String, Fail> {}
impl<E: Endpoint<Res = tw_api::Event>, S> Answer<E> for Sse<S> {}

macro_rules! serves {
    ($($a:ident),*) => {
        impl<E, F, Fut, M, $($a,)* Last> Serves<E, (M, $($a,)* Last,)> for F
        where
            E: Endpoint,
            F: FnOnce($($a,)* Last) -> Fut,
            Fut: Future,
            Fut::Output: Answer<E>,
            Last: Takes<E>,
        {
        }
    };
}

/// 一个参数都不收的：只能是没有请求的端点
impl<E, F, Fut> Serves<E, ((),)> for F
where
    E: Endpoint<Req = ()>,
    F: FnOnce() -> Fut,
    Fut: Future,
    Fut::Output: Answer<E>,
{
}

serves!();
serves!(A1);
serves!(A1, A2);
serves!(A1, A2, A3);

/// 框架自己回的错误也换成 [`tw_api::ErrorBody`]。
///
/// 处理函数的失败本来就是带码的 JSON（[`crate::fail`]）。剩下的是 axum 在走到
/// 处理函数之前就回掉的：请求体不是合法 JSON、查询串对不上类型、路径不存在、
/// 方法不对 —— 那些是纯文本或者空的，客户端只能看响应体是不是以 `{` 开头来猜。
pub(crate) async fn errors_are_messages(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    let status = res.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return res;
    }
    let json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if json {
        return res;
    }
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();
    let m = match status {
        StatusCode::METHOD_NOT_ALLOWED => msg!(
            "control.method_not_allowed", method = method, path = path =>
            "The control plane does not accept {method} on {path}. The desktop app and the gateway may be different versions."
        ),
        _ => msg!(
            "control.request_rejected", detail = body =>
            "The control plane could not read the request: {detail}"
        ),
    };
    (status, Json(m)).into_response()
}

/// 没有这个端点。**不靠上面那层**：axum 的 `layer` 只包已经注册的路由，
/// 走到兜底的请求不经过它。
pub(crate) async fn no_such_endpoint(method: axum::http::Method, uri: axum::http::Uri) -> Fail {
    crate::fail(
        StatusCode::NOT_FOUND,
        msg!(
            "control.no_such_endpoint", method = method, path = uri.path() =>
            "The control plane has no endpoint {method} {path}. The desktop app and the gateway may be different versions."
        ),
    )
}

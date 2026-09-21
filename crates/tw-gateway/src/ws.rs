//! WebSocket 升级代理（第三个集成细节）。
//!
//! sub2api 给 Codex CLI 提供 `/backend-api/codex/responses` 的 WS 桥接
//! （客户端 WS ↔ 上游 HTTP/SSE）。要覆盖这条链路，数据面得能代理一次
//! 升级，而不只是 HTTP + SSE。
//!
//! # 这一层最容易犯的错：把它做成一根管子
//!
//! 直接把两边的帧对着倒，是最省事的写法，也是**一条绕过整条管线的
//! 合法后门** —— 出站脱敏、工具墙全都不会发生，而
//! 用户完全看不出这条路和别的路有什么不同。重放那次已经踩过一模一样
//! 的坑：**任何绕过主管线的路径都要把管线上的保护重新点一遍。**
//!
//! 所以这里每一帧文本都过同一套：
//!
//! - 客户端 → 上游：出站脱敏，和普通请求同一套函数、同一份全局规则 ——
//!   观察档记录，拦截档替换；
//! - 上游 → 客户端：先把占位符换回去，再喂给工具调用审查。
//!
//! # 两条明说的边界
//!
//! **一、走代理的上游不代理 WS。**代理是给 reqwest 配的，而这里
//! 是自己建连。悄悄绕过用户配的代理，等于把他以为在代理后面的流量直接
//! 发出去 —— 那比不支持严重得多，所以宁可明确拒绝。
//!
//! **二、二进制帧原样转发。**脱敏规则是文本规则，对二进制没有意义；
//! 而假装检查过它，比说清楚没检查更糟。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::Message as UpMsg;

use crate::server::AppState;
use tw_types::{Msg, msg};

/// `Option<WebSocketUpgrade>` 的替身。
///
/// axum 0.8 只给 `WebSocketUpgrade` 实现了 `FromRequestParts`，没有
/// `OptionalFromRequestParts` —— 而**这个 handler 是全局的 fallback**，
/// 绝大多数请求根本不是升级请求。提取失败在这里不是错误，是常态。
pub struct MaybeUpgrade(pub Option<axum::extract::WebSocketUpgrade>);

impl<S> axum::extract::FromRequestParts<S> for MaybeUpgrade
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        if !is_upgrade(&parts.headers) {
            return Ok(MaybeUpgrade(None));
        }
        Ok(MaybeUpgrade(
            axum::extract::WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// 这是一次 WebSocket 升级请求吗。
pub fn is_upgrade(headers: &axum::http::HeaderMap) -> bool {
    let has = |k: &str, v: &str| {
        headers
            .get(k)
            .and_then(|x| x.to_str().ok())
            .is_some_and(|s| s.to_ascii_lowercase().contains(v))
    };
    has("upgrade", "websocket") && has("connection", "upgrade")
}

/// 把 `http(s)://host/path` 换成 `ws(s)://host/path`。
pub fn upstream_url(base: &str, path: &str, query: Option<&str>) -> String {
    let scheme = if base.starts_with("https://") {
        "wss://"
    } else {
        "ws://"
    };
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    match query {
        Some(q) if !q.is_empty() => format!("{scheme}{host}{path}?{q}"),
        _ => format!("{scheme}{host}{path}"),
    }
}

/// 两项防护此刻的档位和规则。**升级那一刻取一次**：一条连接活多久，就按
/// 它开始时的配置走多久，和普通请求按开始时的运行时走是同一个道理。
pub struct Rules {
    pub redact_mode: tw_config::SecurityMode,
    pub redact: Arc<tw_redact::rules::RuleSet>,
    pub inspect_mode: tw_config::SecurityMode,
    pub tools: Arc<tw_scan::rules::Rules>,
}

/// 一次连接里两个方向各自的状态。
struct Pipes {
    /// **整条连接一本账。**每帧各起一本的话，第二帧的
    /// `<<TW_SECRET_1>>` 会和第一帧的撞车（见 `redact_into` 的注释）
    ledger: tw_redact::redact::Ledger,
    /// 工具调用审查关着的时候没有它
    wall: Option<crate::toolwall::Wall>,
    rules: Rules,
    provider: String,
    id: u64,
}

/// 接管一次升级。
///
/// 路由、鉴权都在调用方做完了 —— 这里只负责把两条流接起来，并且**在每一帧
/// 上重新点一遍管线的保护**。
///
/// **这条连接怎么断的，就是这个请求的结局**（`ending`）。每一条收场的
/// 路径都先报结局、再去关连接：关连接要等对面，而对面可能已经不在了。
#[allow(clippy::too_many_arguments)]
pub async fn proxy(
    state: AppState,
    client: WebSocket,
    upstream_url: String,
    upstream_headers: Vec<(String, String)>,
    provider: tw_config::Provider,
    rules: Rules,
    id: u64,
    ending: crate::ending::Ending,
) {
    let mut req =
        match tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            upstream_url.as_str(),
        ) {
            Ok(r) => r,
            Err(e) => {
                let why = msg!(
                    "gw.ws.bad_url", detail = e =>
                    "The upstream address is not a valid WebSocket address: {detail}"
                );
                let text = why.text.clone();
                ending.failed("config", why);
                close_with(client, &text).await;
                return;
            }
        };
    for (name, value) in &upstream_headers {
        let parsed = (
            name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>(),
            value.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>(),
        );
        match parsed {
            (Ok(n), Ok(v)) => {
                req.headers_mut().insert(n, v);
            }
            _ => {
                let why = msg!(
                    "gw.ws.bad_header", header = name =>
                    "The upstream header `{header}` contains characters a header may not carry."
                );
                let text = why.text.clone();
                ending.failed("config", why);
                close_with(client, &text).await;
                return;
            }
        }
    }
    let up = match dial(&upstream_url, req).await {
        Ok(x) => x,
        Err(e) => {
            let why = msg!(
                "gw.ws.connect_failed", detail = e =>
                "The upstream WebSocket could not be connected: {detail}"
            );
            let text = why.text.clone();
            ending.failed("upstream", why);
            close_with(client, &text).await;
            return;
        }
    };
    let mut p = Pipes {
        ledger: tw_redact::redact::Ledger::default(),
        wall: rules
            .inspect_mode
            .detects()
            .then(|| crate::toolwall::Wall::new(rules.tools.clone())),
        rules,
        provider: provider.name.clone(),
        id,
    };
    pump(state, client, up, &mut p, ending).await;
}

/// 建连。
///
/// **不用 tokio-tungstenite 自带的 `connect_async`。**那会带进另一套 TLS
/// 信任根，于是「数据面信任的证书」和「WS 信任的」变成两回事 —— 而那种
/// 不一致的表现是「HTTP 通、WS 报证书错误」，最难查的一类（L1 那边为了
/// 同一个理由也是复用这份配置）。
async fn dial(
    url: &str,
    req: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Result<Stream, String> {
    let tls = url.starts_with("wss://");
    let hostport = url
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .to_string();
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (
            h.to_string(),
            p.parse().unwrap_or(if tls { 443 } else { 80 }),
        ),
        _ => (hostport.clone(), if tls { 443 } else { 80 }),
    };
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("{host}:{port} could not be reached: {e}"))?;
    let io: Box<dyn Io> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| format!("{host} is not a valid TLS host name"))?;
        let conn = tokio_rustls::TlsConnector::from(crate::l1::tls_config());
        Box::new(conn.connect(name, tcp).await.map_err(|e| e.to_string())?)
    } else {
        Box::new(tcp)
    };
    let (s, _) = tokio_tungstenite::client_async(req, io)
        .await
        .map_err(|e| e.to_string())?;
    Ok(s)
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

type Stream = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

/// 一条连接是怎么断的。
enum End {
    /// 有一边收场了：发了关闭帧，或者把连接收掉了。**客户端那一边怎么走
    /// 都算这一种** —— 一次会话就是由客户端结束的，那是正常收场
    Closed,
    /// 上游那边出错断了，或者写不过去了
    Broke(Msg),
    /// 上游返回了高危工具调用，被切断了
    Cut(Msg),
}

async fn pump(
    state: AppState,
    client: WebSocket,
    up: Stream,
    p: &mut Pipes,
    mut ending: crate::ending::Ending,
) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = up.split();
    let end = loop {
        tokio::select! {
            // 客户端 → 上游：**和普通请求同一个脱敏函数**
            msg = c_rx.next() => {
                let Some(Ok(m)) = msg else { break End::Closed };
                let out = match m {
                    Message::Text(t) => {
                        let mode = p.rules.redact_mode;
                        let found = crate::guard::find(mode, &p.rules.redact, t.as_bytes());
                        if found.is_empty() {
                            UpMsg::Text(t.as_str().into())
                        } else {
                            state.bus.emit(tw_api::Event::SecretsFound {
                                id: p.id,
                                provider: p.provider.clone(),
                                replaced: mode.acts(),
                                items: crate::guard::items(&found),
                                at_ms: crate::server::now_ms(),
                            });
                            if mode.acts() {
                                let r = tw_redact::redact::redact_into(
                                    t.as_str(),
                                    &p.rules.redact,
                                    std::mem::take(&mut p.ledger),
                                );
                                p.ledger = r.ledger;
                                UpMsg::Text(r.text.into())
                            } else {
                                UpMsg::Text(t.as_str().into())
                            }
                        }
                    }
                    // 二进制不检查，也不假装检查过
                    Message::Binary(b) => UpMsg::Binary(b),
                    Message::Ping(b) => UpMsg::Ping(b),
                    Message::Pong(b) => UpMsg::Pong(b),
                    Message::Close(_) => break End::Closed,
                };
                if let Err(e) = u_tx.send(out).await {
                    break End::Broke(msg!(
                "gw.ws.send_failed", detail = e => "Sending to the upstream failed: {detail}"
            ));
                }
            }
            // 上游 → 客户端：先还原占位符，再过工具墙
            msg = u_rx.next() => {
                let m = match msg {
                    Some(Ok(m)) => m,
                    // 上游把连接收掉了，没有关闭帧也算收场
                    None => break End::Closed,
                    Some(Err(e)) => {
                    break End::Broke(msg!(
                        "gw.ws.upstream_broke", detail = e =>
                        "The upstream connection broke: {detail}"
                    ));
                }
                };
                let out = match m {
                    UpMsg::Text(t) => {
                        let restored = tw_redact::redact::restore(t.as_str(), &p.ledger);
                        let hits = match p.wall.as_mut() {
                            Some(w) => w.feed(as_sse(&restored).as_bytes()),
                            None => Vec::new(),
                        };
                        // **和主管线一模一样的判据**：规则是切断 + 拦截档
                        let acts = p.rules.inspect_mode.acts();
                        let mut deadly = false;
                        let mut why: Option<Msg> = None;
                        for h in &hits {
                            let blocked = h.cut && acts;
                            if blocked && why.is_none() {
                                why = Some(msg!(
                                    "gw.ws.toolcall_cut",
                                    upstream = p.provider.clone(), tool = h.tool.clone(),
                                    rule = h.rule.clone(), detail = h.why.clone() =>
                                    "The {tool} call returned by upstream `{upstream}` matched \
                                     rule `{rule}` ({detail}), so the connection was cut."
                                ));
                            }
                            deadly |= blocked;
                            state.bus.emit(crate::server::flagged(p.id, &p.provider, h, blocked));
                        }
                        if deadly {
                            // **命中那一帧不发。**和 SSE 那条路同一条纪律：
                            // 先判断再转发，而不是发完再说
                            let _ = c_tx.send(Message::Text(
                                "[ThinkWatch] the upstream returned a dangerous tool call; the connection was cut".into(),
                            )).await;
                            break End::Cut(why.expect("set on the same pass that set deadly"));
                        }
                        ending.count(restored.len());
                        Message::Text(restored.into())
                    }
                    UpMsg::Binary(b) => {
                        ending.count(b.len());
                        Message::Binary(b)
                    }
                    UpMsg::Ping(b) => Message::Ping(b),
                    UpMsg::Pong(b) => Message::Pong(b),
                    UpMsg::Close(_) => break End::Closed,
                    UpMsg::Frame(_) => continue,
                };
                // 发不给客户端，就是客户端已经走了
                if c_tx.send(out).await.is_err() { break End::Closed }
            }
        }
    };
    // **先报结局，再关连接。**关连接要等对面回话，而对面可能早就不在了
    match end {
        End::Closed => ending.finished(101),
        End::Broke(why) => ending.failed("upstream", why),
        End::Cut(why) => ending.failed("denied", why),
    }
    let _ = c_tx.close().await;
    let _ = u_tx.close().await;
}

/// 把一帧喂成工具墙认得的样子。
///
/// 工具墙是按 SSE 写的（`data: {…}` 行），而 WS 上一帧就是一个 JSON
/// 对象。包一层比给工具墙开第二个入口好 —— **两个入口迟早会在「哪些
/// 规则跑」这件事上分叉**，而那时只有一条路是安全的。
///
/// 桥接本身可能已经在转发 SSE 文本（sub2api 那条链路是 WS ↔ HTTP/SSE），
/// 所以已经是那个形状的就原样过去。
///
/// > **这里的帧形状没有实测过。**手上没有跑着的 sub2api WS 桥，所以
/// > 两种形状都喂 —— 猜错一种的代价是漏检，而漏检正是这一层要防的。
fn as_sse(frame: &str) -> String {
    if frame.lines().any(|l| l.starts_with("data: ")) {
        return frame.to_string();
    }
    format!("data: {frame}\n\n")
}

async fn close_with(client: WebSocket, why: &str) {
    let mut c = client;
    // **说清楚为什么。**一个默默断掉的 WebSocket，客户端只会显示
    // 「连接已关闭」，而用户完全无从下手
    let _ = c
        .send(Message::Text(format!("[ThinkWatch] {why}").into()))
        .await;
    let _ = c.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upgrade_request_is_recognised_case_insensitively() {
        let mut h = axum::http::HeaderMap::new();
        assert!(!is_upgrade(&h));
        h.insert("upgrade", "WebSocket".parse().unwrap());
        // 只有 upgrade 不算 —— 两个头都要在
        assert!(!is_upgrade(&h));
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(is_upgrade(&h));
    }

    #[test]
    fn the_scheme_follows_the_base_url() {
        assert_eq!(
            upstream_url(
                "https://a.example.com",
                "/backend-api/codex/responses",
                None
            ),
            "wss://a.example.com/backend-api/codex/responses"
        );
        assert_eq!(
            upstream_url("http://127.0.0.1:8080/", "/x", Some("a=1")),
            "ws://127.0.0.1:8080/x?a=1"
        );
        // 空 query 不该留一个光秃秃的问号
        assert_eq!(upstream_url("http://h", "/x", Some("")), "ws://h/x");
    }
}

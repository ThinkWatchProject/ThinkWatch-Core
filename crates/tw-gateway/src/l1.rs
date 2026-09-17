//! L1 · 连通性：DNS → TCP → TLS 的分段耗时。
//!
//! **不发任何业务请求，零成本零副作用**，可以随便点、可以定期自动跑。
//!
//! 分段是这一层的全部价值。一个笼统的「连接失败」说不出该去修什么，而
//! 「DNS 12ms / TCP 4800ms」一眼就能看出是网络不通还是域名解析出了问题。
//! 走代理时尤其如此：到代理的那一次 TCP 握手，是「代理本身还活着吗」的
//! 直接答案。
//!
//! 为什么不复用 reqwest：它把建连过程整个包起来，只给出一个总时间。
//! 这里要的恰恰是里面的分段，所以握手是自己走的 —— 包括 SOCKS5 和
//! HTTP CONNECT。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 每一段都单独限时，这样超时错误能说出**卡在哪一段**。
/// 一句「8 秒超时」和「TLS 握手 8 秒没完成」的可修复性差得远。
const PHASE_TIMEOUT: Duration = Duration::from_secs(8);

/// 建连的哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 地址或代理配置用不了，还没有开始建连
    Config,
    Dns,
    Tcp,
    Tls,
    /// 代理协议的握手，含认证
    Handshake,
}

impl Step {
    pub fn slug(&self) -> &'static str {
        match self {
            Step::Config => "config",
            Step::Dns => "dns",
            Step::Tcp => "tcp",
            Step::Tls => "tls",
            Step::Handshake => "handshake",
        }
    }
}

/// 这一步对着谁。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Peer {
    Upstream,
    Proxy,
}

impl Peer {
    pub fn slug(&self) -> &'static str {
        match self {
            Peer::Upstream => "upstream",
            Peer::Proxy => "proxy",
        }
    }
}

/// 建连的一步，连同它对着谁。**失败时报的就是它**：「卡在到代理的 TCP
/// 握手」和「卡在上游的 TLS 握手」该去修的地方完全不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub step: Step,
    pub peer: Peer,
}

impl Stage {
    const fn new(step: Step, peer: Peer) -> Self {
        Self { step, peer }
    }

    /// 命令行里的说法。**界面不用它**，界面按 `step` / `peer` 自己说。
    pub fn label(&self) -> String {
        let step = match self.step {
            Step::Config => "配置",
            Step::Dns => "DNS 解析",
            Step::Tcp => "TCP 握手",
            Step::Tls => "TLS 握手",
            Step::Handshake => "代理握手",
        };
        match (self.step, self.peer) {
            (Step::Handshake, _) | (_, Peer::Upstream) => step.to_string(),
            (_, Peer::Proxy) => format!("{step} · 代理"),
        }
    }
}

/// 某一步为什么没有出现在分段里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// `http://` 地址没有 TLS
    PlainHttp,
    /// 地址已经是 IP，不需要解析
    IpAddress,
    /// `socks5h` 和 HTTP CONNECT 把域名交给代理解析
    ProxyResolves,
}

impl SkipReason {
    pub fn slug(&self) -> &'static str {
        match self {
            SkipReason::PlainHttp => "plain_http",
            SkipReason::IpAddress => "ip_address",
            SkipReason::ProxyResolves => "proxy_resolves",
        }
    }

    /// 命令行里的说法。
    pub fn label(&self) -> &'static str {
        match self {
            SkipReason::PlainHttp => "http:// 地址不进行 TLS 握手",
            SkipReason::IpAddress => "地址已是 IP，无需 DNS 解析",
            SkipReason::ProxyResolves => "域名由代理解析，本机不进行 DNS 解析",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub stage: Stage,
    pub ms: u64,
}

/// 没出现在分段里的一步。**不说的话，缺一段看起来就像 bug。**
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skip {
    pub stage: Stage,
    pub reason: SkipReason,
}

/// **分段是个列表而不是固定的三个字段**，因为走代理时的形状本来就不同：
/// 多出代理握手，而 `socks5h` 下根本没有本地 DNS 这一段。用
/// `Option<dns_ms>` 表达会把「测不到」和「0 毫秒」混成一件事。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Result {
    pub ok: bool,
    pub segments: Vec<Segment>,
    pub total_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skip>,
    /// 失败在哪一步
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<Stage>,
    /// 失败的原因和下一步
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 代理怎么走。调用方从 `tw_config::Proxy` 解析好再传进来 —— 解析密码
/// 是这一层之外的事，不该混进计时里。
#[derive(Debug, Clone)]
pub struct ProxyHop {
    pub kind: tw_config::ProxyKind,
    /// `host:port`
    pub addr: String,
    pub auth: Option<(String, String)>,
}

/// 把 provider 的代理名解析成一跳。
///
/// **数据面和控制面共用这一份。**各写一份的话，L1 测速走的路和
/// `url-test` 垫底走的路会不一样，而那时两个数字都对不上真实转发。
///
/// **`system` 这里测不了**：跟随系统代理是 reqwest 在建连时才去查环境的，
/// 我们没有那份地址可以去握手。说出来，而不是假装直连测一遍给个漂亮
/// 数字 —— 那个数字测的根本不是用户实际会走的路。
pub fn hop_for(
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> Result<Option<ProxyHop>, String> {
    match p.proxy.as_str() {
        tw_config::DIRECT => Ok(None),
        tw_config::SYSTEM => Err(
            "该上游使用系统代理，代理地址在建立连接时才能确定，链路测速无法测量。如需测速，请将代理配置为命名代理。"
                .into(),
        ),
        name => {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .ok_or_else(|| format!("上游「{}」使用的代理「{name}」未在 proxies 中定义。", p.name))?;
            hop_of(px).map(Some)
        }
    }
}

/// 一个代理条目解析成一跳。**密码在这里取出来**，取不出来（环境变量没设）
/// 直接说是哪个代理的哪一项。
pub fn hop_of(px: &tw_config::Proxy) -> Result<ProxyHop, String> {
    let auth = match &px.auth {
        None => None,
        Some(a) => Some((
            a.user.clone(),
            a.pass
                .resolve()
                .map_err(|e| format!("无法读取代理「{}」的密码：{e}", px.name))?,
        )),
    };
    Ok(ProxyHop {
        kind: px.kind,
        addr: px.addr.clone(),
        auth,
    })
}

/// 测一个代理时让它去连哪儿。
///
/// SOCKS5 和 HTTP CONNECT 的握手都是「帮我连到某处」，所以握手必须带一个
/// 目标。**用第一家走这个代理的上游** —— 测的就是它实际要替谁连；还没有
/// 上游用它时，退到 `api.anthropic.com:443`。
pub fn proxy_target(cfg: &tw_config::Config, proxy: &str) -> (String, u16) {
    cfg.providers
        .iter()
        .filter(|p| p.proxy == proxy)
        .find_map(|p| target_of(&p.base_url).ok())
        .map(|(host, port, _)| (host, port))
        .unwrap_or_else(|| ("api.anthropic.com".to_string(), 443))
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

struct Timer {
    segments: Vec<Segment>,
    skipped: Vec<Skip>,
    started: Instant,
    last: Instant,
}

impl Timer {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            segments: Vec::new(),
            skipped: Vec::new(),
            started: now,
            last: now,
        }
    }
    fn mark(&mut self, stage: Stage) {
        let now = Instant::now();
        self.segments.push(Segment {
            stage,
            ms: now.duration_since(self.last).as_millis() as u64,
        });
        self.last = now;
    }
    fn skip(&mut self, stage: Stage, reason: SkipReason) {
        self.skipped.push(Skip { stage, reason });
    }
    fn total(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn fail(self, stage: Stage, msg: impl std::fmt::Display) -> L1Result {
        L1Result {
            ok: false,
            total_ms: self.total(),
            segments: self.segments,
            skipped: self.skipped,
            failed: Some(stage),
            error: Some(msg.to_string()),
        }
    }
    fn ok(self) -> L1Result {
        L1Result {
            ok: true,
            total_ms: self.total(),
            segments: self.segments,
            skipped: self.skipped,
            failed: None,
            error: None,
        }
    }
}

/// 某一步超时的说法。**它同时是判据**：解析超时和解析失败要说成两句话。
fn timed_out() -> String {
    format!("{} 秒内未完成", PHASE_TIMEOUT.as_secs())
}

async fn phase<T>(f: impl std::future::Future<Output = std::io::Result<T>>) -> Result<T, String> {
    match tokio::time::timeout(PHASE_TIMEOUT, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(timed_out()),
    }
}

/// 拆出 host / port / 要不要 TLS。
fn target_of(base_url: &str) -> Result<(String, u16, bool), String> {
    let u = reqwest::Url::parse(base_url).map_err(|e| format!("接口地址不是合法的 URL：{e}"))?;
    let tls = match u.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(format!(
                "接口地址的协议 {other} 不受支持，仅支持 http 和 https"
            ));
        }
    };
    let host = u
        .host_str()
        .ok_or("接口地址中缺少主机名")?
        .trim_matches(['[', ']'])
        .to_string();
    Ok((host, u.port_or_known_default().unwrap_or(443), tls))
}

/// 量一条线通不通、每一段花了多久。
pub async fn l1(base_url: &str, proxy: Option<&ProxyHop>) -> L1Result {
    let mut t = Timer::new();
    let (host, port, tls) = match target_of(base_url) {
        Ok(v) => v,
        Err(e) => return t.fail(Stage::new(Step::Config, Peer::Upstream), e),
    };

    let stream: Box<dyn Io> = match proxy {
        None => match connect_direct(&mut t, &host, port).await {
            Ok(s) => Box::new(s),
            Err(r) => return r,
        },
        Some(p) => match connect_via_proxy(&mut t, p, &host, port).await {
            Ok(s) => s,
            Err(r) => return r,
        },
    };

    let tls_stage = Stage::new(Step::Tls, Peer::Upstream);
    if !tls {
        t.skip(tls_stage, SkipReason::PlainHttp);
        return t.ok();
    }
    if let Err(e) = tls_handshake(stream, &host).await {
        return t.fail(tls_stage, e);
    }
    t.mark(tls_stage);
    t.ok()
}

/// 只量代理这一跳：连上代理，并**完成它的握手（含认证）**为止。
///
/// **握手必须做完。**以前这里只量到 TCP：密码错了、或者那个端口上跑的
/// 根本不是这种代理，结果照样是「通」—— 然后走它的上游全部连不上，而
/// 用户刚看过一个绿色的对勾。
///
/// 握手的目标见 [`proxy_target`]。上游那一段（TLS 以及之后）不在这里测，
/// 那是链路测速对每个上游做的事。
pub async fn l1_proxy(p: &ProxyHop, host: &str, port: u16) -> L1Result {
    let mut t = Timer::new();
    match connect_via_proxy(&mut t, p, host, port).await {
        Ok(_) => t.ok(),
        Err(r) => r,
    }
}

async fn connect_direct(t: &mut Timer, host: &str, port: u16) -> Result<TcpStream, L1Result> {
    let dns = Stage::new(Step::Dns, Peer::Upstream);
    let addr = match resolve(t, host, port, dns).await {
        Ok(a) => a,
        Err(e) => return Err(std::mem::replace(t, Timer::new()).fail(dns, e)),
    };
    let tcp = Stage::new(Step::Tcp, Peer::Upstream);
    match phase(TcpStream::connect(addr)).await {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            t.mark(tcp);
            Ok(s)
        }
        Err(e) => Err(std::mem::replace(t, Timer::new()).fail(tcp, tcp_message(&e, addr))),
    }
}

/// 解析一个 `host:port`。**已经是 IP 时不记这一段** —— 记一个 0ms 的
/// DNS 解析会让人以为解析快得离谱，而实际是根本没发生。
async fn resolve(t: &mut Timer, host: &str, port: u16, stage: Stage) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        t.skip(stage, SkipReason::IpAddress);
        t.last = Instant::now();
        return Ok(SocketAddr::new(ip, port));
    }
    // **解析失败的系统原话是英文的、而且没提到域名。**「nodename nor
    // servname provided」对着一个中文界面出现，用户既不知道它在说谁，
    // 也不知道该改哪里。
    let mut it = phase(tokio::net::lookup_host((host, port)))
        .await
        .map_err(|e| {
            if e == timed_out() {
                format!(
                    "解析 {host} 超过 {} 秒未完成，DNS 服务器可能无法访问。",
                    PHASE_TIMEOUT.as_secs()
                )
            } else {
                format!("无法解析 {host}。请检查域名拼写；如果该地址需要经代理访问，请先配置代理。")
            }
        })?;
    let addr = it
        .next()
        .ok_or_else(|| format!("{host} 的解析结果中没有地址"))?;
    t.mark(stage);
    Ok(addr)
}

async fn connect_via_proxy(
    t: &mut Timer,
    p: &ProxyHop,
    host: &str,
    port: u16,
) -> Result<Box<dyn Io>, L1Result> {
    use tw_config::ProxyKind::*;

    let (phost, pport) = match split_hostport(&p.addr) {
        Ok(v) => v,
        Err(e) => {
            let stage = Stage::new(Step::Config, Peer::Proxy);
            return Err(std::mem::replace(t, Timer::new()).fail(stage, e));
        }
    };

    // socks5（不带 h）在本地解析目标域名 —— **这一段正是会被污染的那一段**，
    // 所以它值得单独计时。socks5h 和 CONNECT 都把域名原样交给代理。
    let upstream_dns = Stage::new(Step::Dns, Peer::Upstream);
    let target_ip = if p.kind == Socks5 {
        match resolve(t, host, port, upstream_dns).await {
            Ok(a) => Some(a),
            Err(e) => return Err(std::mem::replace(t, Timer::new()).fail(upstream_dns, e)),
        }
    } else {
        t.skip(upstream_dns, SkipReason::ProxyResolves);
        None
    };

    let proxy_dns = Stage::new(Step::Dns, Peer::Proxy);
    let paddr = match resolve(t, &phost, pport, proxy_dns).await {
        Ok(a) => a,
        Err(e) => return Err(std::mem::replace(t, Timer::new()).fail(proxy_dns, e)),
    };
    let proxy_tcp = Stage::new(Step::Tcp, Peer::Proxy);
    let tcp = match phase(TcpStream::connect(paddr)).await {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            t.mark(proxy_tcp);
            s
        }
        Err(e) => {
            let msg = tcp_message(&e, paddr);
            return Err(std::mem::replace(t, Timer::new()).fail(proxy_tcp, msg));
        }
    };

    // https 代理：到代理这一跳本身也是 TLS，CONNECT 走在里面。
    let proxy_tls = Stage::new(Step::Tls, Peer::Proxy);
    let mut io: Box<dyn Io> = if p.kind == Https {
        match tls_handshake_boxed(Box::new(tcp), &phost).await {
            Ok(s) => {
                t.mark(proxy_tls);
                s
            }
            Err(e) => return Err(std::mem::replace(t, Timer::new()).fail(proxy_tls, e)),
        }
    } else {
        Box::new(tcp)
    };

    let r = match p.kind {
        Socks5 | Socks5h => socks5_connect(&mut io, p, host, port, target_ip).await,
        Http | Https => http_connect(&mut io, p, host, port).await,
    };
    let handshake = Stage::new(Step::Handshake, Peer::Proxy);
    match r {
        Ok(()) => {
            t.mark(handshake);
            Ok(io)
        }
        Err(e) => Err(std::mem::replace(t, Timer::new()).fail(handshake, e)),
    }
}

/// TCP 建连失败的系统原话是英文的，而且只有一个 errno。
/// 「Connection refused (os error 61)」和「那个端口上没有东西在听」
/// 之间隔着一次搜索。
fn tcp_message(e: &str, addr: SocketAddr) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("refused") {
        format!("{addr} 拒绝连接，该端口上没有服务在监听。请检查地址和端口。")
    } else if e == timed_out() || l.contains("timed out") {
        format!("连接 {addr} 没有响应。请检查网络，或确认该地址是否需要经代理访问。")
    } else if l.contains("unreachable") {
        format!("无法访问 {addr} 所在的网络，请检查本机网络。")
    } else {
        format!("无法连接 {addr}：{e}")
    }
}

fn split_hostport(s: &str) -> Result<(String, u16), String> {
    // IPv6 字面量要带方括号
    if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest
            .split_once("]:")
            .ok_or("IPv6 代理地址应写成 [::1]:1080 的形式")?;
        return Ok((h.to_string(), p.parse().map_err(|_| "端口必须是数字")?));
    }
    let (h, p) = s
        .rsplit_once(':')
        .ok_or("代理地址应写成 主机:端口 的形式")?;
    // **裸的 IPv6 要报错，不能猜。**`fe80::1` 会被切成主机 `fe80:` 加
    // 端口 `1` —— 一个语法上完全合法、语义上彻底错掉的结果，而它的表现
    // 是「代理连不上」，没有任何线索指向这里。
    if h.contains(':') {
        return Err(format!("{s} 中的 IPv6 地址需要加方括号，应写成 [{h}]:{p}"));
    }
    Ok((
        h.to_string(),
        p.parse().map_err(|_| format!("端口 {p} 不是数字"))?,
    ))
}

async fn socks5_connect(
    io: &mut Box<dyn Io>,
    p: &ProxyHop,
    host: &str,
    port: u16,
    target_ip: Option<SocketAddr>,
) -> Result<(), String> {
    // 打招呼。**总是把「无认证」也报上去** —— 配了用户名密码但代理不要，
    // 只报 0x02 会被拒，而那个失败看起来像密码错了。
    let greeting: &[u8] = if p.auth.is_some() {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    write_all(io, greeting).await?;
    let mut m = [0u8; 2];
    read_exact(io, &mut m).await?;
    if m[0] != 0x05 {
        return Err(format!(
            "代理的响应不是 SOCKS5 协议（版本字节 0x{:02x}），该端口上可能是 HTTP 代理。",
            m[0]
        ));
    }
    match m[1] {
        0x00 => {}
        0x02 => {
            let Some((u, pw)) = &p.auth else {
                return Err("代理要求用户名和密码认证，但该代理未配置认证信息".into());
            };
            if u.len() > 255 || pw.len() > 255 {
                return Err("SOCKS5 的用户名和密码均不能超过 255 字节".into());
            }
            let mut buf = vec![0x01, u.len() as u8];
            buf.extend_from_slice(u.as_bytes());
            buf.push(pw.len() as u8);
            buf.extend_from_slice(pw.as_bytes());
            write_all(io, &buf).await?;
            let mut r = [0u8; 2];
            read_exact(io, &mut r).await?;
            if r[1] != 0x00 {
                return Err("代理拒绝了用户名和密码".into());
            }
        }
        0xFF => return Err("代理不接受所提供的认证方式".into()),
        other => return Err(format!("代理要求的认证方式 0x{other:02x} 不受支持")),
    }

    // CONNECT
    let mut req = vec![0x05, 0x01, 0x00];
    match target_ip.map(|a| a.ip()) {
        Some(std::net::IpAddr::V4(v4)) => {
            req.push(0x01);
            req.extend_from_slice(&v4.octets());
        }
        Some(std::net::IpAddr::V6(v6)) => {
            req.push(0x04);
            req.extend_from_slice(&v6.octets());
        }
        None => {
            if host.len() > 255 {
                return Err("域名超过 255 字节，无法通过 SOCKS5 发送".into());
            }
            req.push(0x03);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    write_all(io, &req).await?;

    let mut head = [0u8; 4];
    read_exact(io, &mut head).await?;
    if head[1] != 0x00 {
        return Err(socks5_reply(head[1]).to_string());
    }
    // **绑定地址必须读掉**，否则它会留在流里，被后面的 TLS 当成握手数据。
    let n = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut l = [0u8; 1];
            read_exact(io, &mut l).await?;
            l[0] as usize
        }
        other => return Err(format!("代理返回了无法识别的地址类型 0x{other:02x}")),
    };
    let mut rest = vec![0u8; n + 2];
    read_exact(io, &mut rest).await?;
    Ok(())
}

fn socks5_reply(code: u8) -> &'static str {
    match code {
        0x01 => "代理内部错误",
        0x02 => "代理规则不允许连接该地址",
        0x03 => "网络不可达",
        0x04 => "主机不可达",
        0x05 => "目标拒绝连接",
        0x06 => "TTL 已过期",
        0x07 => "代理不支持 CONNECT",
        0x08 => "代理不支持该地址类型",
        _ => "代理拒绝了连接",
    }
}

async fn http_connect(
    io: &mut Box<dyn Io>,
    p: &ProxyHop,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let hostport = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {hostport} HTTP/1.1\r\nHost: {hostport}\r\n");
    if let Some((u, pw)) = &p.auth {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{pw}"));
        req.push_str(&format!("Proxy-Authorization: Basic {b}\r\n"));
    }
    req.push_str("\r\n");
    write_all(io, req.as_bytes()).await?;

    // **一个字节一个字节读到头部结束。**多读一个字节就会吃掉 TLS 握手
    // 的开头，而那个错误会表现成一个完全无关的「TLS 失败」。
    let mut buf = Vec::with_capacity(256);
    loop {
        let mut b = [0u8; 1];
        read_exact(io, &mut b).await?;
        buf.push(b[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err("代理的响应头超过 8 KB，该端口上可能不是 HTTP 代理".into());
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next().unwrap_or("");
    let code = line.split_whitespace().nth(1).unwrap_or("");
    match code {
        "200" => Ok(()),
        "407" => Err("代理要求认证（HTTP 407），请检查用户名和密码。".into()),
        "" => Err(format!("代理返回了无法识别的响应：{line}")),
        c => Err(format!("代理拒绝了 CONNECT 请求（HTTP {c}）：{line}")),
    }
}

async fn write_all(io: &mut Box<dyn Io>, buf: &[u8]) -> Result<(), String> {
    phase(io.write_all(buf)).await
}

async fn read_exact(io: &mut Box<dyn Io>, buf: &mut [u8]) -> Result<(), String> {
    phase(async { io.read_exact(buf).await.map(|_| ()) }).await
}

/// TLS 配置。**用平台的信任根**，和数据面走的是同一套验证 —— 否则 L1
/// 说通、真实请求却因为证书被拒，那是最难查的一类不一致。
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        // 进程里可能一个都没装（rustls 同时开着两个 provider 时
        // `builder()` 会 panic），也可能 reqwest 已经装过了 —— 装不上就
        // 说明已经有了，忽略即可。
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        use rustls_platform_verifier::BuilderVerifierExt;
        let c = rustls::ClientConfig::builder()
            .with_platform_verifier()
            .expect("无法创建平台证书验证器")
            .with_no_client_auth();
        Arc::new(c)
    })
    .clone()
}

async fn tls_handshake(stream: Box<dyn Io>, host: &str) -> Result<(), String> {
    tls_handshake_boxed(stream, host).await.map(|_| ())
}

async fn tls_handshake_boxed(stream: Box<dyn Io>, host: &str) -> Result<Box<dyn Io>, String> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| format!("{host} 不是有效的 TLS 主机名"))?;
    let conn = tokio_rustls::TlsConnector::from(tls_config());
    match tokio::time::timeout(PHASE_TIMEOUT, conn.connect(name, stream)).await {
        Ok(Ok(s)) => Ok(Box::new(s)),
        Ok(Err(e)) => Err(tls_message(&e)),
        Err(_) => Err(timed_out()),
    }
}

/// 把 TLS 失败翻成一句能行动的话。
///
/// rustls 的原话是 `invalid peer certificate: Other(OtherError("…"))` ——
/// 里面那句才是平台验证器说的，外面两层包装对用户没有任何信息量。
/// 而证书失败在这个产品的目标场景里**多半不是证书过期，是中间人**：
/// 公司根证书、抓包工具、或者一个在劫持 TLS 的「代理」。
fn tls_message(e: &std::io::Error) -> String {
    let Some(rustls::Error::InvalidCertificate(ce)) =
        e.get_ref().and_then(|r| r.downcast_ref::<rustls::Error>())
    else {
        return e.to_string();
    };
    let detail = match ce {
        rustls::CertificateError::Other(o) => o.to_string(),
        other => other.to_string(),
    };
    // **按文字判，不按 rustls 的枚举判。**macOS 的平台验证器把一切都
    // 塞进 `Other`，那些结构化变体在这个平台上永远不会出现 —— 按枚举
    // 匹配的结果是每一种失败都拿到那句最泛的提示，包括证书只是过期时。
    let d = detail.to_ascii_lowercase();
    let hint = if d.contains("expired")
        || d.contains("not valid yet")
        || d.contains("not yet valid")
    {
        "证书已过期或尚未生效，也可能是本机时间不准确，请先核对系统时间。"
    } else if d.contains("issuer")
        || d.contains("self-signed")
        || d.contains("self signed")
        || d.contains("untrusted")
        || d.contains("not trusted")
    {
        "证书的签发者不在系统信任列表中。如果本机安装了抓包工具或企业根证书，证书链可能已被替换。"
    } else {
        "如果网络中有抓包工具或企业代理拦截 TLS 连接，也会出现此错误。"
    };
    format!("证书验证未通过：{detail}。{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn stages(r: &L1Result) -> Vec<Stage> {
        r.segments.iter().map(|s| s.stage).collect()
    }

    fn skipped(r: &L1Result, step: Step, peer: Peer, reason: SkipReason) -> bool {
        r.skipped.contains(&Skip {
            stage: Stage::new(step, peer),
            reason,
        })
    }

    /// 起一个只接受连接、什么都不说的 TCP 端口。
    async fn dead_ear() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if l.accept().await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        a
    }

    #[tokio::test]
    async fn a_plain_http_target_has_no_tls_segment_and_says_so() {
        let a = dead_ear().await;
        let r = l1(&format!("http://{a}"), None).await;
        assert!(r.ok, "{r:?}");
        assert_eq!(stages(&r), [Stage::new(Step::Tcp, Peer::Upstream)]);
        // 缺一段必须有交代，否则看起来像 bug
        assert!(
            skipped(&r, Step::Tls, Peer::Upstream, SkipReason::PlainHttp),
            "{:?}",
            r.skipped
        );
        assert!(
            skipped(&r, Step::Dns, Peer::Upstream, SkipReason::IpAddress),
            "{:?}",
            r.skipped
        );
    }

    #[tokio::test]
    async fn a_refused_port_fails_at_tcp_and_the_error_names_that_phase() {
        // 一句「连接失败」说不出该修什么。
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        let r = l1(&format!("http://{a}"), None).await;
        assert!(!r.ok);
        assert_eq!(
            r.failed,
            Some(Stage::new(Step::Tcp, Peer::Upstream)),
            "{r:?}"
        );
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn a_hostname_that_does_not_resolve_fails_at_dns() {
        let r = l1("https://no-such-host.invalid", None).await;
        assert!(!r.ok);
        assert_eq!(
            r.failed,
            Some(Stage::new(Step::Dns, Peer::Upstream)),
            "{r:?}"
        );
        assert!(r.segments.is_empty(), "还没走到 TCP 就不该有 TCP 这一段");
    }

    #[tokio::test]
    async fn a_bad_base_url_is_refused_before_any_socket_is_opened() {
        for bad in ["ftp://x", "not a url", "https://"] {
            let r = l1(bad, None).await;
            assert!(!r.ok, "{bad} 该被拒");
            assert_eq!(
                r.failed,
                Some(Stage::new(Step::Config, Peer::Upstream)),
                "{r:?}"
            );
        }
    }

    #[test]
    fn an_ipv6_proxy_address_needs_its_brackets() {
        assert_eq!(
            split_hostport("[::1]:1080").unwrap(),
            ("::1".to_string(), 1080)
        );
        assert_eq!(
            split_hostport("127.0.0.1:7890").unwrap(),
            ("127.0.0.1".to_string(), 7890)
        );
        // 不带方括号的 IPv6 会被 rsplit 切在最后一个冒号上：`fe80::1`
        // 变成主机 `fe80:` 加端口 `1`，语法合法、语义全错，而表现只是
        // 「代理连不上」。**报错好过悄悄猜。**
        let e = split_hostport("fe80::1").unwrap_err();
        assert!(e.contains("方括号"), "{e}");
        assert!(split_hostport("::1:1080").is_err());
        assert!(split_hostport("no-port").is_err());
    }

    // ── 假的 SOCKS5 代理 ────────────────────────────────────────────
    struct Socks5Opts {
        need_auth: bool,
        /// 回复里的绑定地址用域名形式 —— 长度可变，最容易读漏
        domain_bound: bool,
    }

    /// 握手完成后往流里写一个哨兵字节。**读漏了绑定地址的话，哨兵就对
    /// 不上** —— 这是「必须把绑定地址读干净」那条的唯一可靠证据。
    async fn fake_socks5(o: Socks5Opts) -> (SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut n = [0u8; 2];
            s.read_exact(&mut n).await.unwrap();
            let mut methods = vec![0u8; n[1] as usize];
            s.read_exact(&mut methods).await.unwrap();
            let mut tx = Some(tx);
            if o.need_auth {
                s.write_all(&[0x05, 0x02]).await.unwrap();
                let mut h = [0u8; 2];
                s.read_exact(&mut h).await.unwrap();
                let mut u = vec![0u8; h[1] as usize];
                s.read_exact(&mut u).await.unwrap();
                let mut pl = [0u8; 1];
                s.read_exact(&mut pl).await.unwrap();
                let mut p = vec![0u8; pl[0] as usize];
                s.read_exact(&mut p).await.unwrap();
                s.write_all(&[0x01, 0x00]).await.unwrap();
                let _ = tx.take().unwrap().send(format!(
                    "{}:{}",
                    String::from_utf8_lossy(&u),
                    String::from_utf8_lossy(&p)
                ));
            } else {
                s.write_all(&[0x05, 0x00]).await.unwrap();
            }
            let mut head = [0u8; 4];
            s.read_exact(&mut head).await.unwrap();
            let asked = match head[3] {
                0x01 => {
                    let mut b = [0u8; 4];
                    s.read_exact(&mut b).await.unwrap();
                    std::net::Ipv4Addr::from(b).to_string()
                }
                0x03 => {
                    let mut l = [0u8; 1];
                    s.read_exact(&mut l).await.unwrap();
                    let mut d = vec![0u8; l[0] as usize];
                    s.read_exact(&mut d).await.unwrap();
                    String::from_utf8_lossy(&d).to_string()
                }
                _ => "?".into(),
            };
            let mut port = [0u8; 2];
            s.read_exact(&mut port).await.unwrap();
            if let Some(tx) = tx.take() {
                let _ = tx.send(asked);
            }
            if o.domain_bound {
                let d = b"proxy.internal";
                let mut r = vec![0x05, 0x00, 0x00, 0x03, d.len() as u8];
                r.extend_from_slice(d);
                r.extend_from_slice(&[0x1f, 0x90]);
                s.write_all(&r).await.unwrap();
            } else {
                s.write_all(&[0x05, 0x00, 0x00, 0x01, 1, 2, 3, 4, 0x1f, 0x90])
                    .await
                    .unwrap();
            }
            s.write_all(b"SENTINEL").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        (a, rx)
    }

    async fn dial(a: SocketAddr) -> Box<dyn Io> {
        Box::new(TcpStream::connect(a).await.unwrap())
    }

    fn hop(kind: tw_config::ProxyKind, a: SocketAddr, auth: Option<(&str, &str)>) -> ProxyHop {
        ProxyHop {
            kind,
            addr: a.to_string(),
            auth: auth.map(|(u, p)| (u.into(), p.into())),
        }
    }

    #[tokio::test]
    async fn the_bound_address_is_consumed_so_the_next_read_is_real_data() {
        // **这是这段代码最容易错的地方。**绑定地址留在流里的话，它会被
        // 后面的 TLS 握手当成对方的第一帧，而那个失败看起来和证书问题
        // 一模一样 —— 完全指错方向。
        for domain_bound in [false, true] {
            let (a, _rx) = fake_socks5(Socks5Opts {
                need_auth: false,
                domain_bound,
            })
            .await;
            let mut io = dial(a).await;
            let p = hop(tw_config::ProxyKind::Socks5h, a, None);
            socks5_connect(&mut io, &p, "example.com", 443, None)
                .await
                .unwrap();
            let mut buf = [0u8; 8];
            io.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"SENTINEL", "domain_bound={domain_bound}");
        }
    }

    #[tokio::test]
    async fn socks5h_sends_the_domain_and_socks5_sends_the_ip() {
        // 这个差别是唯一真正重要的一条：本地 DNS 被污染时，
        // socks5 解析出来的 IP 根本连不通，即使代理本身是好的。
        let (a, rx) = fake_socks5(Socks5Opts {
            need_auth: false,
            domain_bound: false,
        })
        .await;
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Socks5h, a, None);
        socks5_connect(&mut io, &p, "example.com", 443, None)
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap(), "example.com");

        let (a, rx) = fake_socks5(Socks5Opts {
            need_auth: false,
            domain_bound: false,
        })
        .await;
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Socks5, a, None);
        let ip: SocketAddr = "93.184.216.34:443".parse().unwrap();
        socks5_connect(&mut io, &p, "example.com", 443, Some(ip))
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap(), "93.184.216.34");
    }

    #[tokio::test]
    async fn a_configured_password_is_offered_but_no_auth_is_offered_too() {
        // 配了密码而代理不要，只报 0x02 会被拒 —— 而那个失败看起来
        // 像密码错了。
        let (a, rx) = fake_socks5(Socks5Opts {
            need_auth: true,
            domain_bound: false,
        })
        .await;
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Socks5h, a, Some(("alice", "hunter2")));
        socks5_connect(&mut io, &p, "example.com", 443, None)
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap(), "alice:hunter2");
    }

    #[tokio::test]
    async fn an_http_proxy_on_a_socks_port_says_which_mistake_it_looks_like() {
        // 端口填串了是最常见的配置错误。「握手失败」帮不上忙。
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0u8; 3];
            let _ = s.read_exact(&mut b).await;
            let _ = s.write_all(b"HT").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Socks5h, a, None);
        let e = socks5_connect(&mut io, &p, "example.com", 443, None)
            .await
            .unwrap_err();
        assert!(e.contains("HTTP 代理"), "{e}");
    }

    // ── 假的 HTTP CONNECT 代理 ──────────────────────────────────────
    async fn fake_connect_proxy(
        status: &'static str,
    ) -> (SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = Vec::new();
            loop {
                let mut b = [0u8; 1];
                if s.read_exact(&mut b).await.is_err() {
                    return;
                }
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&head).to_string());
            s.write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            s.write_all(b"SENTINEL").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        (a, rx)
    }

    #[tokio::test]
    async fn connect_stops_exactly_at_the_blank_line() {
        // 多读一个字节就会吃掉 TLS 握手的开头，而那个错误会表现成一个
        // 完全无关的「TLS 失败」。
        let (a, rx) = fake_connect_proxy("200 Connection established").await;
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Http, a, Some(("bob", "s3cret")));
        http_connect(&mut io, &p, "api.anthropic.com", 443)
            .await
            .unwrap();
        let sent = rx.await.unwrap();
        assert!(
            sent.starts_with("CONNECT api.anthropic.com:443 HTTP/1.1"),
            "{sent}"
        );
        assert!(sent.contains("Host: api.anthropic.com:443"), "{sent}");
        // Basic Ym9iOnMzY3JldA== 就是 bob:s3cret
        assert!(
            sent.contains("Proxy-Authorization: Basic Ym9iOnMzY3JldA=="),
            "{sent}"
        );
        let mut buf = [0u8; 8];
        io.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"SENTINEL");
    }

    #[tokio::test]
    async fn a_407_says_it_is_about_credentials() {
        let (a, _rx) = fake_connect_proxy("407 Proxy Authentication Required").await;
        let mut io = dial(a).await;
        let p = hop(tw_config::ProxyKind::Http, a, None);
        let e = http_connect(&mut io, &p, "x.com", 443).await.unwrap_err();
        assert!(e.contains("用户名和密码"), "{e}");
    }

    #[tokio::test]
    async fn going_through_a_proxy_shows_the_proxy_hop_as_its_own_segment() {
        // 到代理的那一次握手，是「代理本身还活着吗」的直接答案 ——
        // 它必须单独成段，不能和别的混在一起。
        let (a, _rx) = fake_socks5(Socks5Opts {
            need_auth: false,
            domain_bound: false,
        })
        .await;
        let p = hop(tw_config::ProxyKind::Socks5h, a, None);
        let r = l1("http://example.com:80", Some(&p)).await;
        assert!(r.ok, "{r:?}");
        // 代理地址就是个 IP，所以没有解析这一段 —— 而这件事有话交代。
        assert_eq!(
            stages(&r),
            [
                Stage::new(Step::Tcp, Peer::Proxy),
                Stage::new(Step::Handshake, Peer::Proxy)
            ]
        );
        assert!(
            skipped(&r, Step::Dns, Peer::Proxy, SkipReason::IpAddress),
            "{:?}",
            r.skipped
        );
        assert!(
            skipped(&r, Step::Dns, Peer::Upstream, SkipReason::ProxyResolves),
            "socks5h 没有本地 DNS 这件事要说出来：{:?}",
            r.skipped
        );
    }

    #[tokio::test]
    async fn a_refused_proxy_port_says_nothing_is_listening_not_just_an_errno() {
        // 「Connection refused (os error 61)」和「那个端口上没有东西在听」
        // 之间隔着一次搜索。
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        let p = hop(tw_config::ProxyKind::Socks5h, a, None);
        let r = l1_proxy(&p, "api.anthropic.com", 443).await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(e.contains("没有服务在监听"), "{e}");
        assert!(!e.contains("os error"), "{e}");
    }

    #[tokio::test]
    async fn testing_a_proxy_completes_its_handshake_with_the_credentials() {
        // 只量 TCP 的话，密码错了也是「通」。握手做完，认证才算验过。
        let (a, creds) = fake_socks5(Socks5Opts {
            need_auth: true,
            domain_bound: false,
        })
        .await;
        let p = hop(tw_config::ProxyKind::Socks5h, a, Some(("svc", "p@ss")));
        let r = l1_proxy(&p, "api.anthropic.com", 443).await;
        assert!(r.ok, "{r:?}");
        assert_eq!(
            stages(&r),
            [
                Stage::new(Step::Tcp, Peer::Proxy),
                Stage::new(Step::Handshake, Peer::Proxy)
            ]
        );
        assert_eq!(creds.await.unwrap(), "svc:p@ss");
    }

    #[test]
    fn a_proxy_is_tested_against_the_first_upstream_that_uses_it() {
        let cfg: tw_config::Config = serde_yaml_ng::from_str(
            "version: 1\nproxies:\n  - { name: hk, type: socks5h, addr: 127.0.0.1:7890 }\nproviders:\n  - { name: a, base_url: https://api.anthropic.com, key: k }\n  - { name: b, base_url: http://relay.example:8080, key: k, proxy: hk }\n",
        )
        .unwrap();
        assert_eq!(
            proxy_target(&cfg, "hk"),
            ("relay.example".to_string(), 8080)
        );
        assert_eq!(
            proxy_target(&cfg, "unused"),
            ("api.anthropic.com".to_string(), 443)
        );
    }

    #[tokio::test]
    async fn a_dead_proxy_fails_at_the_proxy_hop_not_at_the_upstream() {
        // 说成「连不上上游」会让人去查上游 —— 而上游根本没被碰过。
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        let p = hop(tw_config::ProxyKind::Socks5h, a, None);
        let r = l1("https://api.anthropic.com", Some(&p)).await;
        assert!(!r.ok);
        assert_eq!(r.failed, Some(Stage::new(Step::Tcp, Peer::Proxy)), "{r:?}");
    }
}

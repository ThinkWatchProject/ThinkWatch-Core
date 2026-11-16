use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::providers::{Profile, Spec};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tw_types::Msg;

use super::error;

/// Pin user-selected destinations through direct connections AND proxy tunnels.
/// The fixed HTTPS search endpoints separately support the proxy's remote DNS;
/// that exception never applies to result pages or arbitrary redirect targets.
pub(super) struct ClientSettings {
    proxy: hyper_util::client::proxy::matcher::Matcher,
    timeout: std::time::Duration,
    search: reqwest::Client,
    direct: Mutex<Vec<DirectClient>>,
}

// A pooled direct connection is reusable only for the same origin, profile,
// and freshly checked DNS addresses. A DNS change cannot reuse an older pin.
struct DirectClient {
    origin: String,
    addresses: Vec<SocketAddr>,
    profile: Profile,
    used: Instant,
    client: reqwest::Client,
}

impl ClientSettings {
    pub fn new(config: &tw_config::Config) -> Result<Self, crate::GatewayError> {
        use hyper_util::client::proxy::matcher::Matcher;
        let timeout = std::time::Duration::from_secs(u64::from(config.web_tools.timeout_secs));
        let mut search = search_builder().timeout(timeout);
        let proxy = match config.web_tools.proxy.as_str() {
            tw_config::DIRECT => {
                search = search.no_proxy();
                Matcher::builder().build()
            }
            tw_config::SYSTEM => Matcher::from_system(),
            name => {
                let url = config
                    .proxies
                    .iter()
                    .find(|p| p.name == name)
                    .ok_or_else(|| {
                        crate::GatewayError::config(error("search proxy does not exist"))
                    })?
                    .url()
                    .map_err(|_| {
                        crate::GatewayError::config(error(
                            "search proxy credential could not be read",
                        ))
                    })?;
                search = search.proxy(
                    reqwest::Proxy::all(&url)
                        .map_err(|_| crate::GatewayError::config(error("invalid search proxy")))?,
                );
                Matcher::builder().all(url).build()
            }
        };
        Ok(Self {
            proxy,
            timeout,
            direct: Mutex::new(Vec::new()),
            search: search.build().map_err(|_| {
                crate::GatewayError::config(error("could not create search client"))
            })?,
        })
    }

    /// Only the built-in HTTPS search endpoint may use the proxy's DNS. Its host
    /// is chosen by Core, never by tool input. Arbitrary pages and redirects still
    /// resolve locally, reject private addresses, and pin the checked IP below.
    /// Otherwise a working remote-DNS proxy inherits poisoned local DNS answers.
    pub async fn get_search(
        &self,
        spec: &Spec,
        url: &reqwest::Url,
        headers: http::HeaderMap,
        form: Option<&str>,
    ) -> Result<reqwest::Response, Msg> {
        parse(url.as_str())?;
        if !spec.allows(url) {
            return Err(error(
                "search redirected outside its supported first-party pages",
            ));
        }
        let uri: http::Uri = url
            .as_str()
            .parse()
            .map_err(|_| error("invalid source URI"))?;
        if let Some(proxy) = self.proxy.intercept(&uri)
            // SOCKS5 explicitly selects local DNS; SOCKS5h and CONNECT use remote DNS.
            && proxy.uri().scheme_str() != Some("socks5")
        {
            if spec.profile == Profile::Basic {
                return tokio::time::timeout(
                    self.timeout,
                    self.proxied(url, None, proxy, headers, form),
                )
                .await
                .map_err(|_| error("proxy connection timed out"))?;
            }
            // Reuse the first-party browser session connection. Result pages
            // still use checked and pinned IPs.
            return request(&self.search, url, form)
                .headers(headers)
                .send()
                .await
                .map_err(|e| error(e.without_url().to_string()));
        }
        let addresses = check(url).await?;
        self.get_with_headers(url, &addresses, headers, spec.profile, form)
            .await
    }

    pub async fn get(
        &self,
        url: &reqwest::Url,
        addresses: &[SocketAddr],
    ) -> Result<reqwest::Response, Msg> {
        self.get_with_headers(url, addresses, request_headers(), Profile::Basic, None)
            .await
    }

    fn direct_client(
        &self,
        url: &reqwest::Url,
        addresses: &[SocketAddr],
        profile: Profile,
    ) -> Result<reqwest::Client, Msg> {
        let origin = url.origin().ascii_serialization();
        let mut addresses = addresses.to_vec();
        addresses.sort_unstable();
        addresses.dedup();
        let mut clients = self.direct.lock().unwrap_or_else(|e| e.into_inner());
        clients.retain(|entry| entry.used.elapsed() < Duration::from_secs(90));
        if let Some(entry) = clients.iter_mut().find(|entry| {
            entry.origin == origin && entry.addresses == addresses && entry.profile == profile
        }) {
            entry.used = Instant::now();
            return Ok(entry.client.clone());
        }
        let builder = if profile == Profile::BrowserHttp1 {
            search_builder()
        } else {
            crate::outbound::base_client_builder()
        };
        let client = builder
            .no_proxy()
            .resolve_to_addrs(
                url.host_str().ok_or_else(|| error("URL has no host"))?,
                &addresses,
            )
            .timeout(self.timeout)
            .build()
            .map_err(|_| error("could not create web client"))?;
        if clients.len() >= 32 {
            let oldest = clients
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(i, _)| i)
                .unwrap();
            clients.swap_remove(oldest);
        }
        clients.push(DirectClient {
            origin,
            addresses,
            profile,
            used: Instant::now(),
            client: client.clone(),
        });
        Ok(client)
    }

    async fn get_with_headers(
        &self,
        url: &reqwest::Url,
        addresses: &[SocketAddr],
        headers: http::HeaderMap,
        profile: Profile,
        form: Option<&str>,
    ) -> Result<reqwest::Response, Msg> {
        let uri: http::Uri = url
            .as_str()
            .parse()
            .map_err(|_| error("invalid source URI"))?;
        if let Some(proxy) = self.proxy.intercept(&uri) {
            return tokio::time::timeout(
                self.timeout,
                self.proxied(url, Some(addresses), proxy, headers, form),
            )
            .await
            .map_err(|_| error("proxy connection timed out"))?;
        }
        let client = self.direct_client(url, addresses, profile)?;
        request(&client, url, form)
            .headers(headers)
            .send()
            .await
            .map_err(|e| error(e.without_url().to_string()))
    }

    async fn proxied(
        &self,
        url: &reqwest::Url,
        addresses: Option<&[SocketAddr]>,
        proxy: hyper_util::client::proxy::matcher::Intercept,
        mut headers: http::HeaderMap,
        form: Option<&str>,
    ) -> Result<reqwest::Response, Msg> {
        use http_body_util::BodyExt;
        use tw_config::ProxyKind;
        let proxy_uri = proxy.uri();
        let host = proxy_uri
            .host()
            .ok_or_else(|| error("proxy has no hostname"))?;
        let (kind, port) = match proxy_uri.scheme_str().unwrap_or("http") {
            "http" => (ProxyKind::Http, 80),
            "https" => (ProxyKind::Https, 443),
            "socks5" | "socks5h" => (ProxyKind::Socks5, 1080),
            _ => return Err(error("unsupported web proxy scheme")),
        };
        let port = proxy_uri.port_u16().unwrap_or(port);
        let hop = crate::l1::ProxyHop {
            kind,
            addr: format!("{host}:{port}"),
            auth: proxy
                .raw_auth()
                .map(|(user, password)| (user.into(), password.into())),
        };
        let targets = match addresses {
            Some(addresses) => {
                let mut addresses = addresses.to_vec();
                addresses.sort_by_key(SocketAddr::is_ipv6);
                addresses.into_iter().take(4).map(Some).collect::<Vec<_>>()
            }
            None => vec![None],
        };
        let mut last = error("source has no public address");
        let mut connected = None;
        for target in targets {
            let target_host = target
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| url.host_str().expect("validated search URL").to_string());
            let target_port = target.map(|a| a.port()).unwrap_or(443);
            let connect = async {
                let tcp = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
                    .await
                    .map_err(|_| error("could not connect to the configured proxy"))?;
                let mut io: Box<dyn crate::l1::Io> = Box::new(tcp);
                if kind == ProxyKind::Https {
                    io = crate::l1::tls_handshake_boxed(io, host.trim_matches(['[', ']'])).await?;
                }
                match kind {
                    ProxyKind::Socks5 | ProxyKind::Socks5h => {
                        crate::l1::socks5_connect(&mut io, &hop, &target_host, target_port, target)
                            .await?
                    }
                    _ => crate::l1::http_connect(&mut io, &hop, &target_host, target_port).await?,
                }
                if url.scheme() == "https" {
                    io = crate::l1::tls_handshake_boxed(
                        io,
                        url.host_str().unwrap_or("").trim_matches(['[', ']']),
                    )
                    .await?;
                }
                Ok::<_, Msg>(io)
            }
            .await;
            match connect {
                Ok(io) => {
                    connected = Some(io);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let io = connected.ok_or(last)?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
                .await
                .map_err(|_| error("could not start HTTP over the proxy tunnel"))?;
        let connection = Connection(tokio::spawn(async move {
            let _ = connection.await;
        }));
        if form.is_some() {
            headers.insert(
                "content-type",
                http::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        let mut request = http::Request::builder()
            .method(if form.is_some() { "POST" } else { "GET" })
            .uri(format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            ))
            .header("host", url.authority())
            .body(http_body_util::Full::new(bytes::Bytes::copy_from_slice(
                form.unwrap_or_default().as_bytes(),
            )))
            .map_err(|_| error("invalid web request"))?;
        // This manually pinned HTTP/1 tunnel does not run reqwest's decoder.
        // Do not advertise compression that this transport cannot decode.
        headers.insert(
            "accept-encoding",
            http::HeaderValue::from_static("identity"),
        );
        request.headers_mut().extend(headers);
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| error("source request through the proxy failed"))?;
        let (parts, body) = response.into_parts();
        let stream = async_stream::stream! {
            // Dropping a cancelled/oversized response must also close the tunnel.
            let _connection = connection;
            let mut data = body.into_data_stream();
            use futures::StreamExt;
            while let Some(chunk) = data.next().await { yield chunk; }
        };
        Ok(http::Response::from_parts(parts, reqwest::Body::wrap_stream(stream)).into())
    }
}

fn request(
    client: &reqwest::Client,
    url: &reqwest::Url,
    form: Option<&str>,
) -> reqwest::RequestBuilder {
    match form {
        Some(body) => client
            .post(url.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.to_owned()),
        None => client.get(url.clone()),
    }
}

fn search_builder() -> reqwest::ClientBuilder {
    crate::outbound::base_client_builder()
        // Match the HTTP/1 navigation flow on both direct and tunneled paths.
        // Some search frontends return a different page when ALPN selects h2.
        .http1_only()
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true)
}

struct Connection(tokio::task::JoinHandle<()>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn request_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    for (name, value) in [
        (
            "user-agent",
            "ThinkWatch-WebTools/1.0 (+https://thinkwat.ch)",
        ),
        ("accept-encoding", "identity"),
        (
            "accept",
            "text/html,application/rss+xml,application/xml,text/plain;q=0.9",
        ),
    ] {
        headers.insert(
            http::header::HeaderName::from_static(name),
            http::HeaderValue::from_static(value),
        );
    }
    headers
}

pub(super) fn parse(value: &str) -> Result<reqwest::Url, Msg> {
    let url = reqwest::Url::parse(value).map_err(|_| error("invalid page URL"))?;
    if value.len() > 8192
        || value.contains('\\')
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.port_or_known_default(), Some(80 | 443))
        || url.host_str().is_none()
    {
        return Err(error(
            "only public HTTP/HTTPS URLs without credentials on ports 80/443 are supported",
        ));
    }
    Ok(url)
}

pub(super) async fn check(url: &reqwest::Url) -> Result<Vec<SocketAddr>, Msg> {
    parse(url.as_str())?;
    let host = url
        .host_str()
        .ok_or_else(|| error("URL has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let addresses: Vec<SocketAddr> =
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
            .await
            .map_err(|_| error("source hostname could not be resolved"))?
            .collect();
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err(error("source resolves to a non-public address"));
    }
    Ok(addresses)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}
fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}
fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = ip.segments();
    // Only global unicast, excluding transition and documentation blocks.
    (s[0] & 0xe000) == 0x2000
        && s[0] != 0x2002
        && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
        && !(s[0] == 0x3fff && s[1] <= 0x0fff)
}

pub(super) fn validate_domains(domains: &[String]) -> Result<(), Msg> {
    if domains.len() > 100
        || domains.iter().any(|d| {
            d.is_empty()
                || d.len() > 253
                || d.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        })
    {
        return Err(error("domain filters must contain valid hostnames"));
    }
    Ok(())
}

pub(super) fn domains_allow(url: &reqwest::Url, allowed: &[String], blocked: &[String]) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let matches = |domain: &String| {
        let d = domain.to_ascii_lowercase();
        host.eq_ignore_ascii_case(&d) || host.to_ascii_lowercase().ends_with(&format!(".{d}"))
    };
    (allowed.is_empty() || allowed.iter().any(matches)) && !blocked.iter().any(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_local_metadata_mapped_and_transition_addresses() {
        for s in [
            "127.0.0.1",
            "10.2.3.4",
            "169.254.169.254",
            "100.64.1.1",
            "192.168.1.1",
            "0.0.0.0",
            "224.1.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(!public_ip(s.parse().unwrap()), "{s}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn domain_rules_use_labels_and_block_rules_win() {
        let allow = vec!["example.org".into()];
        assert!(domains_allow(
            &parse("https://docs.example.org/a").unwrap(),
            &allow,
            &[]
        ));
        assert!(!domains_allow(
            &parse("https://badexample.org").unwrap(),
            &allow,
            &[]
        ));
        assert!(!domains_allow(
            &parse("https://example.org").unwrap(),
            &allow,
            &allow
        ));
        assert!(parse("http://user:password@example.org").is_err());
        assert!(parse("http://example.org:8080").is_err());
    }
}

#[cfg(test)]
mod network_tests {
    use super::super::providers;
    use super::*;
    use tw_api::SearchEngine;

    #[tokio::test]
    async fn checked_direct_connections_are_reused_but_a_changed_dns_pin_is_not() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first = v4.local_addr().unwrap();
        let v6 = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, first.port()))
            .await
            .unwrap();
        let second = v6.local_addr().unwrap();
        let server = |listener: tokio::net::TcpListener, body: &'static str| {
            tokio::spawn(async move {
                // A second request must arrive on this same socket. If the client is
                // rebuilt for each GET, the next accept is never served and times out.
                let (mut socket, _) = listener.accept().await.unwrap();
                loop {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        let Ok(byte) = socket.read_u8().await else {
                            return;
                        };
                        head.push(byte);
                    }
                    socket
                        .write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{body}")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            })
        };
        let old = server(v4, "old");
        let new = server(v6, "new");
        let mut config = tw_config::Config::default();
        config.web_tools.proxy = tw_config::DIRECT.into();
        let settings = ClientSettings::new(&config).unwrap();
        // Call the pinned transport directly with test loopbacks. Production
        // callers must pass public::check before reaching this layer.
        let url: reqwest::Url = format!("http://pool.test:{}/first", first.port())
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            for address in [first, first, second] {
                let body = settings
                    .get(&url, &[address])
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                assert_eq!(body, if address == first { "old" } else { "new" });
            }
        })
        .await
        .unwrap();
        assert_eq!(settings.direct.lock().unwrap().len(), 2);
        settings
            .direct_client(&url, &[second], Profile::BrowserHttp1)
            .unwrap();
        assert_eq!(
            settings.direct.lock().unwrap().len(),
            3,
            "profiles have distinct pools"
        );
        for i in 0..40 {
            let url = format!("http://host-{i}.test/").parse().unwrap();
            settings
                .direct_client(&url, &[first], Profile::Basic)
                .unwrap();
        }
        assert_eq!(settings.direct.lock().unwrap().len(), 32);
        old.abort();
        new.abort();
    }

    #[tokio::test]
    async fn only_search_transport_decodes_browser_compression() {
        use std::io::Write;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let plain = b"<html><body>compressed search result</body></html>";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plain).unwrap();
        let encoded = encoder.finish().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let wire = encoded.clone();
        let server = tokio::spawn(async move {
            for search in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                assert_eq!(head.contains("accept-encoding:"), search);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", wire.len()).as_bytes()).await.unwrap();
                socket.write_all(&wire).await.unwrap();
            }
        });
        let url = format!("http://{address}/");
        let decoded = search_builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(decoded.as_ref(), plain);
        let untouched = crate::outbound::base_client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(untouched.as_ref(), encoded);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn search_uses_remote_dns_through_http_and_socks5h_proxies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (scheme, engine) in [
            ("http", SearchEngine::Duckduckgo),
            ("socks5h", SearchEngine::Duckduckgo),
            ("http", SearchEngine::Bing),
            ("socks5h", SearchEngine::Bing),
        ] {
            let provider = providers::get(engine);
            let locale = tw_api::WebSearchLocale::default();
            let url = provider
                .start(&providers::Query {
                    text: "test",
                    allowed: &[],
                    locale: &locale,
                })
                .unwrap()
                .url;
            let hostname = url.host_str().unwrap().to_owned();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                if scheme == "http" {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(socket.read_u8().await.unwrap());
                    }
                    assert!(
                        String::from_utf8(head)
                            .unwrap()
                            .starts_with(&format!("CONNECT {hostname}:443 HTTP/1.1"))
                    );
                    socket
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                } else {
                    let mut hello = [0u8; 3];
                    socket.read_exact(&mut hello).await.unwrap();
                    assert_eq!(hello, [5, 1, 0]);
                    socket.write_all(&[5, 0]).await.unwrap();
                    let mut head = [0u8; 5];
                    socket.read_exact(&mut head).await.unwrap();
                    assert_eq!(&head[..4], &[5, 1, 0, 3], "proxy must receive a hostname");
                    let mut name = vec![0u8; head[4] as usize];
                    socket.read_exact(&mut name).await.unwrap();
                    assert_eq!(name, hostname.as_bytes());
                    assert_eq!(socket.read_u16().await.unwrap(), 443);
                    socket
                        .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                }
            });
            let settings = ClientSettings {
                direct: Mutex::new(Vec::new()),
                proxy: hyper_util::client::proxy::matcher::Matcher::builder()
                    .all(format!("{scheme}://127.0.0.1:{port}"))
                    .build(),
                timeout: std::time::Duration::from_secs(3),
                search: search_builder()
                    .no_proxy()
                    .proxy(reqwest::Proxy::all(format!("{scheme}://127.0.0.1:{port}")).unwrap())
                    .build()
                    .unwrap(),
            };
            assert!(
                settings
                    .get_search(provider.spec(), &url, request_headers(), None)
                    .await
                    .is_err()
            );
            server.await.unwrap();
            // A redirect or page read cannot inherit the search endpoint exception.
            let local = parse("https://127.0.0.1/search").unwrap();
            let failure = settings
                .get_search(provider.spec(), &local, request_headers(), None)
                .await
                .unwrap_err();
            assert!(failure.text.contains("supported first-party"), "{failure}");
        }
    }
    #[tokio::test]
    async fn local_targets_are_rejected_before_a_connection_is_opened() {
        for url in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://2130706433/",
        ] {
            assert!(check(&parse(url).unwrap()).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn proxy_receives_checked_ip_while_origin_receives_original_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(head)
                    .unwrap()
                    .starts_with("CONNECT 1.1.1.1:80 HTTP/1.1")
            );
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(head)
                    .unwrap()
                    .to_lowercase()
                    .contains("host: example.org")
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                )
                .await
                .unwrap();
        });
        let settings = ClientSettings {
            direct: Mutex::new(Vec::new()),
            proxy: hyper_util::client::proxy::matcher::Matcher::builder()
                // Match the IPv4 listener exactly: resolving localhost may try
                // ::1 first, whose refusal can consume the Windows CI timeout.
                .all(format!("http://127.0.0.1:{port}"))
                .build(),
            timeout: std::time::Duration::from_secs(10),
            search: search_builder().no_proxy().build().unwrap(),
        };
        let url = parse("http://example.org/").unwrap();
        assert_eq!(
            settings
                .get(&url, &["1.1.1.1:80".parse().unwrap()])
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "proxied"
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn form_queries_cross_direct_and_pinned_proxy_transports_without_redirect_replay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for proxy in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let form = "request=%5B%22Rust+%26+中文%22%5D";
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                if proxy {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(socket.read_u8().await.unwrap());
                    }
                    assert!(
                        String::from_utf8(head)
                            .unwrap()
                            .starts_with("CONNECT 1.1.1.1:80 HTTP/1.1")
                    );
                    socket
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                }
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap().to_lowercase();
                assert!(head.starts_with("post /search http/1.1"), "{head}");
                assert!(head.contains("host: origin.test"));
                assert!(head.contains("content-type: application/x-www-form-urlencoded"));
                assert!(head.contains(&format!("content-length: {}", form.len())));
                let mut body = vec![0; form.len()];
                socket.read_exact(&mut body).await.unwrap();
                assert_eq!(body, form.as_bytes());
                socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            });
            let mut config = tw_config::Config::default();
            config.web_tools.proxy = tw_config::DIRECT.into();
            let mut settings = ClientSettings::new(&config).unwrap();
            let (url, pin) = if proxy {
                settings.proxy = hyper_util::client::proxy::matcher::Matcher::builder()
                    .all(format!("http://{address}"))
                    .build();
                (
                    "http://origin.test/search".parse().unwrap(),
                    "1.1.1.1:80".parse().unwrap(),
                )
            } else {
                (
                    format!("http://origin.test:{}/search", address.port())
                        .parse()
                        .unwrap(),
                    address,
                )
            };
            // Loopback pins are injected only in this transport test. Normal
            // execution validates public addresses before reaching this layer.
            let response = settings
                .get_with_headers(
                    &url,
                    &[pin],
                    request_headers(),
                    Profile::BrowserHttp1,
                    Some(form),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::FOUND);
            server.await.unwrap();
        }
    }
}

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tw_types::Msg;

use super::error;

/// Pin the destination through direct connections AND proxy tunnels. Letting an
/// HTTP CONNECT/SOCKS proxy resolve the original hostname would reopen the DNS
/// rebinding window even though the local preflight check passed.
pub(super) struct ClientSettings {
    proxy: hyper_util::client::proxy::matcher::Matcher,
    timeout: std::time::Duration,
}

impl ClientSettings {
    pub fn new(config: &tw_config::Config) -> Result<Self, crate::GatewayError> {
        use hyper_util::client::proxy::matcher::Matcher;
        let proxy = match config.web_tools.proxy.as_str() {
            tw_config::DIRECT => Matcher::builder().build(),
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
                Matcher::builder().all(url).build()
            }
        };
        Ok(Self {
            proxy,
            timeout: std::time::Duration::from_secs(u64::from(config.web_tools.timeout_secs)),
        })
    }

    pub async fn get(
        &self,
        url: &reqwest::Url,
        addresses: &[SocketAddr],
    ) -> Result<reqwest::Response, Msg> {
        let uri: http::Uri = url
            .as_str()
            .parse()
            .map_err(|_| error("invalid source URI"))?;
        if let Some(proxy) = self.proxy.intercept(&uri) {
            return tokio::time::timeout(self.timeout, self.proxied(url, addresses, proxy))
                .await
                .map_err(|_| error("proxy connection timed out"))?;
        }
        let client = crate::outbound::base_client_builder()
            .no_proxy()
            .resolve_to_addrs(
                url.host_str().ok_or_else(|| error("URL has no host"))?,
                addresses,
            )
            .timeout(self.timeout)
            .build()
            .map_err(|_| error("could not create web client"))?;
        client
            .get(url.clone())
            .headers(request_headers())
            .send()
            .await
            .map_err(|e| error(e.without_url().to_string()))
    }

    async fn proxied(
        &self,
        url: &reqwest::Url,
        addresses: &[SocketAddr],
        proxy: hyper_util::client::proxy::matcher::Intercept,
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
        let mut addresses = addresses.to_vec();
        addresses.sort_by_key(SocketAddr::is_ipv6);
        let mut last = error("source has no public address");
        let mut connected = None;
        for target in addresses.into_iter().take(4) {
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
                        crate::l1::socks5_connect(
                            &mut io,
                            &hop,
                            &target.ip().to_string(),
                            target.port(),
                            Some(target),
                        )
                        .await?
                    }
                    _ => {
                        crate::l1::http_connect(
                            &mut io,
                            &hop,
                            &target.ip().to_string(),
                            target.port(),
                        )
                        .await?
                    }
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
        let mut request = http::Request::builder()
            .uri(format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            ))
            .header("host", url.authority())
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .map_err(|_| error("invalid web request"))?;
        request.headers_mut().extend(request_headers());
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

struct Connection(tokio::task::JoinHandle<()>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn request_headers() -> http::HeaderMap {
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
    use super::*;
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
            proxy: hyper_util::client::proxy::matcher::Matcher::builder()
                .all(format!("http://localhost:{port}"))
                .build(),
            timeout: std::time::Duration::from_secs(2),
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
}

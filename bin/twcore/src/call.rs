//! `twcore call`：往本机正在跑的 core 的控制面发一个请求。
//!
//! 控制面的每条连接都要先握手（`tw-link`），curl 敲不开它了。这个命令读
//! 同一份配置里的钥匙、走和桌面端同一条握手，于是调试和脚本（包括 CI 的
//! smoke）照样能问 core。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use tw_api::control::Address;

pub fn run(
    config: &Path,
    endpoint: &str,
    method: &str,
    data: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let dir = config
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(tw_api::data::dir);
    let key = tw_link::read_key(config)
        .with_context(|| format!("reading the control key from {}", config.display()))?;
    let method: hyper::Method = method
        .to_ascii_uppercase()
        .parse()
        .with_context(|| format!("`{method}` is not an HTTP method"))?;
    let endpoint = if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (status, body) = rt.block_on(async move {
        match Address::in_dir(&dir) {
            #[cfg(unix)]
            Address::Socket(p) => {
                let s = tokio::net::UnixStream::connect(&p)
                    .await
                    .with_context(|| format!("connecting to {}", p.display()))?;
                send(s, &key, method, &endpoint, data).await
            }
            #[cfg(not(unix))]
            Address::Socket(p) => anyhow::bail!("{} is a unix socket", p.display()),
            Address::Loopback { port_file } => {
                let port: u16 = std::fs::read_to_string(&port_file)
                    .with_context(|| format!("reading {}", port_file.display()))?
                    .trim()
                    .parse()
                    .with_context(|| format!("{} holds no port number", port_file.display()))?;
                let s = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                    .await
                    .with_context(|| format!("connecting to 127.0.0.1:{port}"))?;
                send(s, &key, method, &endpoint, data).await
            }
        }
    })?;
    match out {
        Some(file) => {
            std::fs::write(&file, &body).with_context(|| format!("writing {}", file.display()))?;
            println!("{status}");
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&body)?;
        }
    }
    Ok(())
}

async fn send<S>(
    stream: S,
    key: &tw_link::ControlKey,
    method: hyper::Method,
    endpoint: &str,
    data: Option<String>,
) -> Result<(u16, Bytes)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (link, _) = tw_link::connect(stream, key, concat!("twcore ", env!("CARGO_PKG_VERSION")))
        .await
        .context("the control-plane handshake")?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(link)).await?;
    tokio::spawn(conn);
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(endpoint)
        .header(hyper::header::HOST, "localhost");
    if data.is_some() {
        req = req.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let req = req.body(Full::new(Bytes::from(data.unwrap_or_default())))?;
    let resp = sender.send_request(req).await?;
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await?.to_bytes();
    Ok((status, body))
}

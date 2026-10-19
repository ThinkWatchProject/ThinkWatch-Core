//! 进程内的假 WebDAV、假 S3：内容放在内存里，几十行。
//!
//! **只实现同步用到的那几样**，但按规矩答：`PUT` 认 `If-None-Match: *`、`MOVE` 认
//! `Overwrite: F`、`PROPFIND` 只列一层、S3 的列目录认 `prefix` 和 `delimiter`。凭据不对回 401 /
//! 403。可以让它「坏掉」：回 503，或者不认 `MOVE`、`If-None-Match`（有的服务器就是这样）。
//!
//! 假 S3 **自己核对 SigV4 签名**（[`sigv4`]，照 AWS 的规范另写一份，不借被测的那一份；用 AWS
//! 文档里的示例核对过它自己，见 `stores.rs`）：签名算错了照样回 403。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use sha2::{Digest, Sha256};

pub const USER: &str = "tester";
pub const PASS: &str = "app-password";
pub const AK: &str = "AKIDFAKESYNCTEST";
pub const SK: &str = "fake-secret-for-tests-only";

/// 假服务端的开关
#[derive(Default)]
pub struct Knobs {
    /// 一律回 503
    pub down: AtomicBool,
    /// WebDAV 不认 MOVE（405）
    pub no_move: AtomicBool,
    /// 不认 `If-None-Match`：照写不误（WebDAV）或者回 501（S3）
    pub no_conditional: AtomicBool,
    /// 收到的请求数
    pub requests: AtomicUsize,
}

pub struct Fake {
    pub addr: SocketAddr,
    /// 路径（不带开头的 `/`，WebDAV 是去掉根之后的，S3 是桶内的键）→ 内容
    pub files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    pub knobs: Arc<Knobs>,
}

impl Fake {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn names(&self) -> Vec<String> {
        self.files.lock().unwrap().keys().cloned().collect()
    }
    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

async fn serve(
    handler: impl Fn(Request<Body>, Vec<u8>) -> Response<Body> + Send + Sync + 'static,
) -> SocketAddr {
    let handler = Arc::new(handler);
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let handler = handler.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap_or_default()
                .to_vec();
            handler(Request::from_parts(parts, Body::empty()), bytes)
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn status(s: u16) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(s).unwrap())
        .body(Body::empty())
        .unwrap()
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// WebDAV，根在 `/dav/root/`。
pub async fn webdav() -> Fake {
    let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Default::default();
    let dirs: Arc<Mutex<std::collections::BTreeSet<String>>> = Default::default();
    let knobs: Arc<Knobs> = Default::default();
    let (f, d, k) = (files.clone(), dirs.clone(), knobs.clone());
    let addr = serve(move |req, body| {
        k.requests.fetch_add(1, Ordering::Relaxed);
        if k.down.load(Ordering::Relaxed) {
            return status(503);
        }
        use base64::Engine as _;
        let want = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"))
        );
        if header(req.headers(), "authorization") != Some(want.as_str()) {
            return status(401);
        }
        let path = req.uri().path().to_string();
        let rel = match path.strip_prefix("/dav/root/") {
            Some(r) => r,
            // 根本身：用户选的那个文件夹，在
            None if path == "/dav/root" => "",
            None => return status(404),
        };
        if rel.is_empty() && req.method().as_str() == "MKCOL" {
            return status(405);
        }
        let rel = percent_encoding::percent_decode_str(rel)
            .decode_utf8_lossy()
            .to_string();
        let mut files = f.lock().unwrap();
        let mut dirs = d.lock().unwrap();
        match req.method().as_str() {
            "MKCOL" => {
                let dir = rel.trim_end_matches('/').to_string();
                if dir.contains('/') && !dirs.contains(dir.rsplit_once('/').unwrap().0) {
                    return status(409);
                }
                if dirs.insert(dir) { status(201) } else { status(405) }
            }
            "PROPFIND" => {
                let dir = rel.trim_end_matches('/').to_string();
                if !dir.is_empty() && !dirs.contains(&dir) {
                    return status(404);
                }
                let at = |p: &str| {
                    if p.is_empty() {
                        "/dav/root/".to_string()
                    } else {
                        format!("/dav/root/{p}/")
                    }
                };
                let mut xml = String::from(
                    r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#,
                );
                xml.push_str(&format!(
                    "<D:response><D:href>{}</D:href><D:propstat><D:prop>\
                     <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>",
                    at(&dir)
                ));
                if header(req.headers(), "depth") != Some("0") {
                    for (name, bytes) in files.iter() {
                        let (parent, leaf) = name.rsplit_once('/').unwrap_or(("", name));
                        if parent == dir {
                            xml.push_str(&format!(
                                "<D:response><D:href>{}{leaf}</D:href><D:propstat><D:prop>\
                                 <D:getcontentlength>{}</D:getcontentlength><D:resourcetype/></D:prop>\
                                 <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                                at(parent),
                                bytes.len()
                            ));
                        }
                    }
                }
                xml.push_str("</D:multistatus>");
                Response::builder()
                    .status(207)
                    .header("content-type", "application/xml")
                    .body(Body::from(xml))
                    .unwrap()
            }
            "GET" | "HEAD" => match files.get(&rel) {
                Some(b) if req.method() == "GET" => Response::new(Body::from(b.clone())),
                Some(_) => status(200),
                None => status(404),
            },
            "PUT" => {
                let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                if !parent.is_empty() && !dirs.contains(parent) {
                    return status(409);
                }
                if header(req.headers(), "if-none-match") == Some("*")
                    && !k.no_conditional.load(Ordering::Relaxed)
                    && files.contains_key(&rel)
                {
                    return status(412);
                }
                let existed = files.insert(rel, body).is_some();
                status(if existed { 204 } else { 201 })
            }
            "DELETE" => match files.remove(&rel) {
                Some(_) => status(204),
                None => status(404),
            },
            "MOVE" => {
                if k.no_move.load(Ordering::Relaxed) {
                    return status(405);
                }
                let Some(dest) = header(req.headers(), "destination") else {
                    return status(400);
                };
                let dest_path = url::Url::parse(dest)
                    .map(|u| u.path().to_string())
                    .unwrap_or_else(|_| dest.to_string());
                let Some(dest_rel) = dest_path.strip_prefix("/dav/root/") else {
                    return status(502);
                };
                let dest_rel = dest_rel.to_string();
                let overwrite = header(req.headers(), "overwrite") != Some("F");
                if !overwrite && files.contains_key(&dest_rel) {
                    return status(412);
                }
                let Some(b) = files.remove(&rel) else {
                    return status(404);
                };
                let existed = files.insert(dest_rel, b).is_some();
                status(if existed { 204 } else { 201 })
            }
            _ => status(405),
        }
    })
    .await;
    // 根在
    Fake { addr, files, knobs }
}

/// S3，路径方式，桶叫 `tw-bucket`。
pub async fn s3() -> Fake {
    let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Default::default();
    let knobs: Arc<Knobs> = Default::default();
    let (f, k) = (files.clone(), knobs.clone());
    let addr = serve(move |req, body| {
        k.requests.fetch_add(1, Ordering::Relaxed);
        if k.down.load(Ordering::Relaxed) {
            return status(503);
        }
        let auth = header(req.headers(), "authorization").unwrap_or_default();
        let signed = sigv4(
            req.method().as_str(),
            req.uri().path(),
            req.uri().query().unwrap_or_default(),
            req.headers(),
            &body,
            AK,
            SK,
        );
        if !auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={AK}/")) || signed.is_err() {
            return Response::builder()
                .status(403)
                .body(Body::from(format!(
                    "<Error><Code>SignatureDoesNotMatch</Code><Message>{}</Message></Error>",
                    signed.err().unwrap_or_default()
                )))
                .unwrap();
        }
        let path = req.uri().path().to_string();
        let Some(key) = path.strip_prefix("/tw-bucket") else {
            return status(404);
        };
        let key = percent_encoding::percent_decode_str(key.trim_start_matches('/'))
            .decode_utf8_lossy()
            .to_string();
        let mut files = f.lock().unwrap();
        match req.method().as_str() {
            "GET" if key.is_empty() => {
                let q: BTreeMap<String, String> = url::form_urlencoded::parse(
                    req.uri().query().unwrap_or_default().as_bytes(),
                )
                .into_owned()
                .collect();
                assert_eq!(q.get("list-type").map(String::as_str), Some("2"));
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let delim = q.get("delimiter").cloned();
                let mut xml = String::from(
                    r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated>"#,
                );
                for (name, bytes) in files.iter() {
                    let Some(rest) = name.strip_prefix(&prefix) else {
                        continue;
                    };
                    if delim.as_deref() == Some("/") && rest.contains('/') {
                        continue;
                    }
                    xml.push_str(&format!(
                        "<Contents><Key>{name}</Key><Size>{}</Size></Contents>",
                        bytes.len()
                    ));
                }
                xml.push_str("</ListBucketResult>");
                Response::new(Body::from(xml))
            }
            "GET" => match files.get(&key) {
                Some(b) => Response::new(Body::from(b.clone())),
                None => Response::builder()
                    .status(404)
                    .body(Body::from("<Error><Code>NoSuchKey</Code></Error>"))
                    .unwrap(),
            },
            "HEAD" => status(if files.contains_key(&key) { 200 } else { 404 }),
            "PUT" => {
                if header(req.headers(), "if-none-match") == Some("*") {
                    if k.no_conditional.load(Ordering::Relaxed) {
                        return Response::builder()
                            .status(501)
                            .body(Body::from("<Error><Code>NotImplemented</Code></Error>"))
                            .unwrap();
                    }
                    if files.contains_key(&key) {
                        return status(412);
                    }
                }
                files.insert(key, body);
                status(200)
            }
            "DELETE" => {
                files.remove(&key);
                status(204)
            }
            _ => status(405),
        }
    })
    .await;
    Fake { addr, files, knobs }
}

// ─────────────────────────────────────────────── SigV4（服务端核对）

/// HMAC-SHA256（RFC 2104），照定义写
fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let d: [u8; 32] = Sha256::digest(key).into();
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// AWS 的 URI 编码：只有 `A-Z a-z 0-9 - _ . ~` 不编码，别的一律 `%XX`（大写）
fn aws_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

/// 照 AWS SigV4 的规范从收到的请求重算签名，和 `Authorization` 里的比。不对是 `Err(原因)`
pub fn sigv4(
    method: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
    access_key: &str,
    secret: &str,
) -> Result<(), String> {
    let auth = header(headers, "authorization").ok_or("no Authorization")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("not AWS4-HMAC-SHA256")?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=').ok_or("bad Authorization")?;
        fields.insert(k, v);
    }
    let cred = fields.get("Credential").ok_or("no Credential")?;
    let signed = fields.get("SignedHeaders").ok_or("no SignedHeaders")?;
    let got = fields.get("Signature").ok_or("no Signature")?;
    let mut scope_parts = cred.splitn(2, '/');
    if scope_parts.next() != Some(access_key) {
        return Err("unknown access key".into());
    }
    let scope = scope_parts.next().ok_or("no scope")?;
    let s: Vec<&str> = scope.split('/').collect();
    if s.len() != 4 || s[2] != "s3" || s[3] != "aws4_request" {
        return Err(format!("bad scope {scope}"));
    }
    let amz_date = header(headers, "x-amz-date").ok_or("no x-amz-date")?;
    if !amz_date.starts_with(s[0]) {
        return Err("x-amz-date is not the scope's date".into());
    }
    let payload = header(headers, "x-amz-content-sha256").ok_or("no x-amz-content-sha256")?;
    if payload != "UNSIGNED-PAYLOAD" && payload != hex(&Sha256::digest(body)) {
        return Err("x-amz-content-sha256 does not match the body".into());
    }
    let names: Vec<&str> = signed.split(';').collect();
    if !names.contains(&"host") {
        return Err("host is not signed".into());
    }
    let mut canon_headers = String::new();
    for n in &names {
        let v = headers
            .get_all(*n)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join(",");
        if v.is_empty() && headers.get(*n).is_none() {
            return Err(format!("signed header {n} is not there"));
        }
        canon_headers.push_str(&format!("{n}:{v}\n"));
    }
    let uri: String = path
        .split('/')
        .map(|seg| aws_encode(&decode(seg), false))
        .collect::<Vec<_>>()
        .join("/");
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (aws_encode(&decode(k), false), aws_encode(&decode(v), false))
        })
        .collect();
    pairs.sort();
    let canon_query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical = format!("{method}\n{uri}\n{canon_query}\n{canon_headers}\n{signed}\n{payload}");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), s[0].as_bytes());
    let k_region = hmac(&k_date, s[1].as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let want = hex(&hmac(&k_signing, to_sign.as_bytes()));
    if want != *got {
        return Err(format!(
            "signature does not match; canonical request:\n{canonical}"
        ));
    }
    Ok(())
}

/// 一个没人听的地址：连上去就被拒绝
pub fn nowhere() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    format!("http://{a}")
}

/// 测试里的 HTTP 客户端：不走系统代理，User-Agent 用通用值
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .user_agent("sync-test")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

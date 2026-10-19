//! 进程内的假 WebDAV、假 S3：内容放在内存里，几十行。
//!
//! **只实现同步用到的那几样**，但按规矩答：`PUT` 认 `If-None-Match: *`、`MOVE` 认
//! `Overwrite: F`、`PROPFIND` 只列一层、S3 的列目录认 `prefix` 和 `delimiter`。凭据不对回 401 /
//! 403。可以让它「坏掉」：回 503，或者不认 `MOVE`、`If-None-Match`（有的服务器就是这样）。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};

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
        let Some(rel) = req.uri().path().strip_prefix("/dav/root/") else {
            // 根本身
            if req.uri().path().trim_end_matches('/') == "/dav/root" {
                return match req.method().as_str() {
                    "MKCOL" => status(405),
                    "PROPFIND" => status(207),
                    _ => status(405),
                };
            }
            return status(404);
        };
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
                if !dirs.contains(&dir) {
                    return status(404);
                }
                let mut xml = String::from(
                    r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#,
                );
                xml.push_str(&format!(
                    "<D:response><D:href>/dav/root/{dir}/</D:href><D:propstat><D:prop>\
                     <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>"
                ));
                for (name, bytes) in files.iter() {
                    if let Some((parent, leaf)) = name.rsplit_once('/')
                        && parent == dir
                    {
                        xml.push_str(&format!(
                            "<D:response><D:href>/dav/root/{parent}/{leaf}</D:href><D:propstat><D:prop>\
                             <D:getcontentlength>{}</D:getcontentlength><D:resourcetype/></D:prop>\
                             <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                            bytes.len()
                        ));
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
        if !auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={AK}/"))
            || !auth.contains("/s3/aws4_request")
            || header(req.headers(), "x-amz-content-sha256").is_none()
        {
            return Response::builder()
                .status(403)
                .body(Body::from(
                    "<Error><Code>SignatureDoesNotMatch</Code></Error>",
                ))
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

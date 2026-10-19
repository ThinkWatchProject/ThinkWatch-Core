//! WebDAV 位置：坚果云、Nextcloud、群晖 NAS、InfiniCloud。
//!
//! 只用五个方法：`PROPFIND`（列目录，Depth 1）、`GET`、`PUT`、`DELETE`、`MOVE`，外加建目录的
//! `MKCOL`。写入先 `PUT` 到一个点开头的临时名，再 `MOVE` 过去（只新增时带 `Overwrite: F`）；
//! 「仅当不存在时创建」用 `If-None-Match: *`。
//!
//! **证书一律校验**，`http://` 只给本机和内网（见 [`tw_config::sync::address_ok`]）：这条
//! 连接上传的是同步位置的密码，和可能没加密的整份配置。**不跟重定向**：跟到别的主机，
//! 密码就跟着去了。

use std::time::Duration;

use reqwest::{Method, StatusCode};
use tw_types::msg;

use super::{Entry, Store, StoreError, check_path, http_error, http_status, split, temp_name};

/// 一个请求最多等多久。版本最大 16 MiB，慢的网盘也够了
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct WebDav {
    /// 以 `/` 结尾
    base: url::Url,
    user: String,
    pass: String,
    http: reqwest::Client,
}

fn method(m: &'static str) -> Method {
    Method::from_bytes(m.as_bytes()).expect("a valid method name")
}

impl WebDav {
    pub fn new(
        url: &str,
        user: &str,
        pass: &str,
        http: reqwest::Client,
    ) -> Result<Self, StoreError> {
        if !tw_config::sync::address_ok(url) {
            return Err(StoreError::Other(
                tw_config::sync::SyncError::Address {
                    field: "url",
                    url: url.to_string(),
                }
                .msg(),
            ));
        }
        let mut s = url.trim().to_string();
        if !s.ends_with('/') {
            s.push('/');
        }
        let base = url::Url::parse(&s).map_err(|e| {
            StoreError::Other(msg!(
                "sync.webdav.bad_url", url = url, detail = e =>
                "{url} is not a WebDAV address: {detail}"
            ))
        })?;
        Ok(Self {
            base,
            user: user.to_string(),
            pass: pass.to_string(),
            http,
        })
    }

    /// 文件的地址。路径只含我们自己生成的字符，拼上去不用转义
    fn file(&self, path: &str) -> Result<url::Url, StoreError> {
        check_path(path)?;
        self.base.join(path).map_err(|e| {
            StoreError::Other(msg!(
                "sync.webdav.bad_url", url = path, detail = e =>
                "{url} is not a WebDAV address: {detail}"
            ))
        })
    }

    /// 目录的地址，以 `/` 结尾。空的是根
    fn dir(&self, dir: &str) -> Result<url::Url, StoreError> {
        if dir.is_empty() {
            return Ok(self.base.clone());
        }
        let mut u = self.file(dir)?;
        u.set_path(&format!("{}/", u.path().trim_end_matches('/')));
        Ok(u)
    }

    fn req(&self, m: Method, url: url::Url) -> reqwest::RequestBuilder {
        let r = self.http.request(m, url).timeout(TIMEOUT);
        if self.user.is_empty() && self.pass.is_empty() {
            r
        } else {
            r.basic_auth(&self.user, Some(&self.pass))
        }
    }

    async fn send(&self, r: reqwest::RequestBuilder) -> Result<reqwest::Response, StoreError> {
        r.send().await.map_err(|e| http_error(&e))
    }

    /// 建目录。已经有了（405）不算错；上一级没有（409）先建上一级（同步位置的根）
    async fn mkcol(&self, dir: &str) -> Result<(), StoreError> {
        let url = self.dir(dir)?;
        let r = self.send(self.req(method("MKCOL"), url.clone())).await?;
        match r.status().as_u16() {
            200 | 201 | 204 | 405 => Ok(()),
            409 if !dir.is_empty() => {
                Box::pin(self.mkcol("")).await?;
                let r = self.send(self.req(method("MKCOL"), url)).await?;
                match r.status().as_u16() {
                    200 | 201 | 204 | 405 => Ok(()),
                    _ => Err(http_status(r.status(), "MKCOL")),
                }
            }
            _ => Err(http_status(r.status(), "MKCOL")),
        }
    }

    async fn put_raw(
        &self,
        url: url::Url,
        bytes: &[u8],
        if_none_match: bool,
    ) -> Result<StatusCode, StoreError> {
        let mut r = self
            .req(Method::PUT, url)
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec());
        if if_none_match {
            r = r.header("if-none-match", "*");
        }
        Ok(self.send(r).await?.status())
    }

    /// 写临时名、改名过去。`overwrite` 是 MOVE 的 `Overwrite`。返回 MOVE 的状态，服务端
    /// 不支持 MOVE（405、501）时是 `None`，临时文件已经删掉
    async fn via_temp(
        &self,
        path: &str,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<Option<StatusCode>, StoreError> {
        let (dir, name) = split(path);
        let tmp_path = if dir.is_empty() {
            temp_name(name)
        } else {
            format!("{dir}/{}", temp_name(name))
        };
        let tmp = self.file(&tmp_path)?;
        let dest = self.file(path)?;
        let st = self.put_raw(tmp.clone(), bytes, false).await?;
        if !st.is_success() {
            return Err(http_status(st, "PUT"));
        }
        let r = self
            .send(
                self.req(method("MOVE"), tmp.clone())
                    .header("destination", dest.as_str())
                    .header("overwrite", if overwrite { "T" } else { "F" }),
            )
            .await;
        let st = match r {
            Ok(r) => r.status(),
            Err(e) => {
                let _ = self.send(self.req(Method::DELETE, tmp)).await;
                return Err(e);
            }
        };
        if st.is_success() {
            return Ok(Some(st));
        }
        let _ = self.send(self.req(Method::DELETE, tmp)).await;
        if matches!(st.as_u16(), 405 | 501) {
            return Ok(None);
        }
        Ok(Some(st))
    }

    async fn exists(&self, path: &str) -> Result<bool, StoreError> {
        let r = self.send(self.req(Method::HEAD, self.file(path)?)).await?;
        match r.status().as_u16() {
            404 => Ok(false),
            s if (200..300).contains(&s) => Ok(true),
            _ => Err(http_status(r.status(), "HEAD")),
        }
    }
}

/// `href` 里最后一段，解码之后。目录以 `/` 结尾，去掉再取
fn last_segment(href: &str) -> String {
    let path = match url::Url::parse(href) {
        Ok(u) => u.path().to_string(),
        Err(_) => href.to_string(),
    };
    let seg = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    percent_encoding::percent_decode_str(seg)
        .decode_utf8_lossy()
        .to_string()
}

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;

impl Store for WebDav {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        let url = self.dir(dir)?;
        let own = url.path().trim_end_matches('/').to_string();
        let r = self
            .send(
                self.req(method("PROPFIND"), url)
                    .header("depth", "1")
                    .header("content-type", "application/xml; charset=utf-8")
                    .body(PROPFIND),
            )
            .await?;
        match r.status().as_u16() {
            207 | 200 => {}
            404 => return Ok(Vec::new()),
            _ => return Err(http_status(r.status(), "PROPFIND")),
        }
        let body = r.text().await.map_err(|e| http_error(&e))?;
        let Some((records, _)) = super::xml::parse(&body, "response") else {
            return Err(StoreError::Other(msg!(
                "sync.webdav.bad_listing" =>
                "the WebDAV server answered a directory listing that could not be read"
            )));
        };
        let mut out = Vec::new();
        for r in records {
            let Some(href) = r.get("href") else { continue };
            let path = match url::Url::parse(href) {
                Ok(u) => u.path().to_string(),
                Err(_) => href.to_string(),
            };
            if r.has("collection") || path.trim_end_matches('/') == own {
                continue;
            }
            let name = last_segment(href);
            if name.is_empty() || name.starts_with('.') {
                continue;
            }
            out.push(Entry {
                name,
                size: r
                    .get("getcontentlength")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let r = self.send(self.req(Method::GET, self.file(path)?)).await?;
        match r.status().as_u16() {
            404 | 410 => Ok(None),
            s if (200..300).contains(&s) => {
                Ok(Some(r.bytes().await.map_err(|e| http_error(&e))?.to_vec()))
            }
            _ => Err(http_status(r.status(), "GET")),
        }
    }

    async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let (dir, _) = split(path);
        self.mkcol(dir).await?;
        let exists = || StoreError::Exists {
            path: path.to_string(),
        };
        match self.via_temp(path, bytes, false).await? {
            Some(st) if st.is_success() => Ok(()),
            Some(st) if st.as_u16() == 412 => Err(exists()),
            Some(st) => Err(http_status(st, "MOVE")),
            None => {
                // 不支持 MOVE 的服务器：直接写，带上「不存在才写」
                let st = self.put_raw(self.file(path)?, bytes, true).await?;
                match st.as_u16() {
                    412 => Err(exists()),
                    s if (200..300).contains(&s) => Ok(()),
                    _ => Err(http_status(st, "PUT")),
                }
            }
        }
    }

    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let (dir, _) = split(path);
        self.mkcol(dir).await?;
        match self.via_temp(path, bytes, true).await? {
            Some(st) if st.is_success() => Ok(()),
            Some(st) => Err(http_status(st, "MOVE")),
            None => {
                let st = self.put_raw(self.file(path)?, bytes, false).await?;
                if st.is_success() {
                    Ok(())
                } else {
                    Err(http_status(st, "PUT"))
                }
            }
        }
    }

    async fn delete(&self, path: &str) -> Result<(), StoreError> {
        let r = self
            .send(self.req(Method::DELETE, self.file(path)?))
            .await?;
        match r.status().as_u16() {
            404 | 410 => Ok(()),
            s if (200..300).contains(&s) => Ok(()),
            _ => Err(http_status(r.status(), "DELETE")),
        }
    }

    async fn create_exclusive(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        let (dir, _) = split(path);
        self.mkcol(dir).await?;
        // 先看一眼：有的服务器不认 If-None-Match，会直接覆盖
        if self.exists(path).await? {
            return Ok(false);
        }
        let st = self.put_raw(self.file(path)?, bytes, true).await?;
        match st.as_u16() {
            412 => Ok(false),
            s if (200..300).contains(&s) => Ok(true),
            _ => Err(http_status(st, "PUT")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_http_address_off_the_local_network_is_refused() {
        let http = reqwest::Client::new();
        assert!(WebDav::new("http://dav.example.com/x", "u", "p", http.clone()).is_err());
        assert!(WebDav::new("https://dav.example.com/x", "u", "p", http.clone()).is_ok());
        assert!(WebDav::new("http://192.168.1.2:5005/x", "u", "p", http).is_ok());
    }

    #[test]
    fn hrefs_are_decoded_whether_absolute_or_not() {
        assert_eq!(last_segment("/dav/a%20b/versions/x.twconfig"), "x.twconfig");
        assert_eq!(last_segment("https://h/dav/tw/devices/"), "devices");
        assert_eq!(last_segment("/dav/%E5%90%8C%E6%AD%A5/"), "同步");
    }

    #[test]
    fn files_and_directories_resolve_under_the_base() {
        let d = WebDav::new("https://h/dav/ThinkWatch", "u", "p", reqwest::Client::new()).unwrap();
        assert_eq!(
            d.file("versions/0000000001-aaaaaaaa.twconfig")
                .unwrap()
                .as_str(),
            "https://h/dav/ThinkWatch/versions/0000000001-aaaaaaaa.twconfig"
        );
        assert_eq!(
            d.dir("versions").unwrap().as_str(),
            "https://h/dav/ThinkWatch/versions/"
        );
        assert_eq!(d.dir("").unwrap().as_str(), "https://h/dav/ThinkWatch/");
        assert!(d.file("../x").is_err());
    }
}

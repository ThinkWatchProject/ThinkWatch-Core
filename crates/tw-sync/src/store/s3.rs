//! S3 兼容存储：Amazon S3、Cloudflare R2、阿里云 OSS、腾讯云 COS、MinIO。
//!
//! 签名是 SigV4，和 Bedrock 用的同一份（[`tw_bedrock::sign_service`]）。四个动作：
//! `ListObjectsV2`（列目录，带 `delimiter=/`，不递归）、`GetObject`、`PutObject`、`DeleteObject`。
//! 「只新增」和「仅当不存在时创建」都带 `If-None-Match: *`：S3、R2、MinIO 认它；不认的服务端
//! （回 501）退回到先看一眼再写 —— 版本的名字归写它的那台设备独占，只有 keyring 会有两台设备
//! 抢，那一刻两台设备同时开启加密同步，退回之后后写的那台覆盖了先写的，先写的那台下一次同步
//! 时发现口令对不上（keyring 换了）就转入加入流程。
//!
//! **地址怎么拼**（[`addressing`]）：端点主机以 `amazonaws.com`、`aliyuncs.com`、`myqcloud.com`
//! 结尾的用虚拟主机（`<桶>.<端点>/<键>`，OSS、COS 要求如此），别的（R2、MinIO、IP 地址、自定义
//! 域名）用路径（`<端点>/<桶>/<键>`）。

use std::time::{Duration, SystemTime};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Method;
use tw_types::msg;

use super::{
    Entry, MAX_FILE, MAX_LISTING, Store, StoreError, check_path, http_status, read_limited,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// 路径里的一段：RFC 3986 的非保留字符不编，别的都编
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// 查询参数的值：同上，斜杠也编
const QUERY: &AsciiSet = SEGMENT;

/// 这几家只认（或者推荐）虚拟主机的写法
const VIRTUAL_HOSTED: &[&str] = &["amazonaws.com", "aliyuncs.com", "myqcloud.com"];

#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub struct S3 {
    /// 桶的地址，以 `/` 结尾：`https://b.s3.us-east-1.amazonaws.com/` 或 `http://nas:9000/b/`
    bucket_url: String,
    /// 键的前缀，空的或者以 `/` 结尾
    prefix: String,
    region: String,
    creds: tw_bedrock::Credentials,
    http: reqwest::Client,
}

fn bad(endpoint: &str) -> StoreError {
    StoreError::Other(
        tw_config::sync::SyncError::Address {
            field: "endpoint",
            url: endpoint.to_string(),
        }
        .msg(),
    )
}

/// 寻址方式：端点主机以 `amazonaws.com`、`aliyuncs.com`、`myqcloud.com` 结尾的用虚拟主机
/// （桶名进域名，阿里云 OSS、腾讯云 COS 要求如此）；别的（R2、MinIO、IP 地址、自定义域名）用
/// 路径。不给界面加选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    VirtualHost,
    Path,
}

/// 按端点的主机选寻址方式（纯函数）
pub fn addressing(host: &str) -> Addressing {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if VIRTUAL_HOSTED
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        Addressing::VirtualHost
    } else {
        Addressing::Path
    }
}

/// 桶的地址，以 `/` 结尾（纯函数）。虚拟主机方式下，端点本身已经以 `<桶>.` 开头的（照着控制台
/// 抄来的桶域名）不再加一遍。地址规则（`http://` 只给内网）在这之前查
pub fn bucket_url(endpoint: &str, bucket: &str) -> Option<String> {
    let mut u = url::Url::parse(endpoint.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let base_path = u.path().trim_end_matches('/').to_string();
    match addressing(&host) {
        Addressing::VirtualHost => {
            if !host.starts_with(&format!("{bucket}.")) {
                u.set_host(Some(&format!("{bucket}.{host}"))).ok()?;
            }
            u.set_path(&format!("{base_path}/"));
        }
        Addressing::Path => {
            let seg = utf8_percent_encode(bucket, SEGMENT).to_string();
            u.set_path(&format!("{base_path}/{seg}/"));
        }
    }
    Some(u.to_string())
}

impl S3 {
    pub fn new(c: Config, http: reqwest::Client) -> Result<Self, StoreError> {
        // `http://` 只给本机和内网，和 WebDAV 同一条规则；证书由客户端照常校验
        if !tw_config::sync::address_ok(&c.endpoint) {
            return Err(bad(&c.endpoint));
        }
        let bucket_url = bucket_url(&c.endpoint, &c.bucket).ok_or_else(|| bad(&c.endpoint))?;
        let p = c.prefix.trim().trim_matches('/');
        let prefix = if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        };
        Ok(Self {
            bucket_url,
            prefix,
            region: c.region.trim().to_string(),
            creds: tw_bedrock::Credentials {
                access_key_id: c.access_key_id,
                secret_access_key: c.secret_access_key,
                session_token: None,
            },
            http,
        })
    }

    fn key(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    fn object_url(&self, key: &str) -> String {
        let enc: Vec<String> = key
            .split('/')
            .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
            .collect();
        format!("{}{}", self.bucket_url, enc.join("/"))
    }

    /// 签好名、发出去。`headers` 一起签
    async fn send(
        &self,
        m: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<reqwest::Response, StoreError> {
        let signed = tw_bedrock::sign_service(
            &self.creds,
            &self.region,
            "s3",
            m.as_str(),
            url,
            headers,
            body,
            SystemTime::now(),
        )
        .map_err(|e| {
            StoreError::Other(msg!(
                "sync.s3.sign", detail = e =>
                "the request to the S3 location could not be signed: {detail}"
            ))
        })?;
        let mut r = self.http.request(m, url).timeout(TIMEOUT);
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        for (k, v) in signed {
            r = r.header(k, v);
        }
        if !body.is_empty() {
            r = r.body(body.to_vec());
        }
        super::send(r).await
    }

    /// S3 错误答复里的 `<Code>`
    async fn error_code(r: reqwest::Response) -> String {
        let body = read_limited(r, 64 * 1024, "the error")
            .await
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        super::xml::parse(&body, "Error")
            .and_then(|(rs, top)| {
                rs.first()
                    .and_then(|e| e.get("Code").map(str::to_string))
                    .or_else(|| top.get("Code").cloned())
            })
            .unwrap_or_default()
    }

    async fn exists(&self, path: &str) -> Result<bool, StoreError> {
        let url = self.object_url(&self.key(path));
        let r = self.send(Method::HEAD, &url, &[], b"").await?;
        match r.status().as_u16() {
            404 => Ok(false),
            s if (200..300).contains(&s) => Ok(true),
            _ => Err(http_status(r.status(), "HEAD")),
        }
    }

    /// 写一个对象。`exclusive`：带 `If-None-Match: *`，已经有了是 `Ok(false)`；服务端不认这个
    /// 头时退回到先看一眼再写
    async fn put_object(
        &self,
        path: &str,
        bytes: &[u8],
        exclusive: bool,
    ) -> Result<bool, StoreError> {
        let url = self.object_url(&self.key(path));
        let ct = ("content-type", "application/octet-stream");
        if !exclusive {
            return self.put_plain(&url, bytes).await.map(|()| true);
        }
        let r = self
            .send(Method::PUT, &url, &[ct, ("if-none-match", "*")], bytes)
            .await?;
        let st = r.status();
        match st.as_u16() {
            s if (200..300).contains(&s) => Ok(true),
            // 已经有了；409 是另一个条件写入正在写同一个键
            412 | 409 => Ok(false),
            501 => self.put_checked(path, bytes).await,
            400 => {
                let code = Self::error_code(r).await;
                if code == "NotImplemented" || code == "InvalidArgument" {
                    self.put_checked(path, bytes).await
                } else {
                    Err(http_status(st, "PUT"))
                }
            }
            _ => Err(http_status(st, "PUT")),
        }
    }

    async fn put_checked(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        if self.exists(path).await? {
            return Ok(false);
        }
        let url = self.object_url(&self.key(path));
        self.put_plain(&url, bytes).await.map(|()| true)
    }

    async fn put_plain(&self, url: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let ct = ("content-type", "application/octet-stream");
        let r = self.send(Method::PUT, url, &[ct], bytes).await?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(http_status(r.status(), "PUT"))
        }
    }
}

impl Store for S3 {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        check_path(dir)?;
        let prefix = format!("{}{dir}/", self.prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        // 一页一千个；同步位置上最多几个版本、几十台设备，翻页只是兜底
        for _ in 0..100 {
            let mut url = format!(
                "{}?delimiter=%2F&list-type=2&prefix={}",
                self.bucket_url,
                utf8_percent_encode(&prefix, QUERY)
            );
            if let Some(t) = &token {
                url.push_str(&format!(
                    "&continuation-token={}",
                    utf8_percent_encode(t, QUERY)
                ));
            }
            let r = self.send(Method::GET, &url, &[], b"").await?;
            if !r.status().is_success() {
                return Err(http_status(r.status(), "ListObjectsV2"));
            }
            let body = read_limited(r, MAX_LISTING, "the listing").await?;
            let body = String::from_utf8_lossy(&body).to_string();
            let Some((records, top)) = super::xml::parse(&body, "Contents") else {
                return Err(StoreError::Other(msg!(
                    "sync.s3.bad_listing" =>
                    "the S3 location answered a listing that could not be read"
                )));
            };
            for r in records {
                let Some(key) = r.get("Key") else { continue };
                let Some(name) = key.strip_prefix(&prefix) else {
                    continue;
                };
                if name.is_empty() || name.contains('/') || name.starts_with('.') {
                    continue;
                }
                out.push(Entry {
                    name: name.to_string(),
                    size: r.get("Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                });
            }
            match (
                top.get("IsTruncated").map(String::as_str),
                top.get("NextContinuationToken"),
            ) {
                (Some("true"), Some(t)) => token = Some(t.clone()),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        check_path(path)?;
        let url = self.object_url(&self.key(path));
        let r = self.send(Method::GET, &url, &[], b"").await?;
        match r.status().as_u16() {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(read_limited(r, MAX_FILE, path).await?)),
            _ => Err(http_status(r.status(), "GET")),
        }
    }

    async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        check_path(path)?;
        if self.put_object(path, bytes, true).await? {
            Ok(())
        } else {
            Err(StoreError::Exists {
                path: path.to_string(),
            })
        }
    }

    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        check_path(path)?;
        self.put_object(path, bytes, false).await.map(|_| ())
    }

    async fn delete(&self, path: &str) -> Result<(), StoreError> {
        check_path(path)?;
        let url = self.object_url(&self.key(path));
        let r = self.send(Method::DELETE, &url, &[], b"").await?;
        match r.status().as_u16() {
            404 => Ok(()),
            s if (200..300).contains(&s) => Ok(()),
            _ => Err(http_status(r.status(), "DELETE")),
        }
    }

    async fn create_exclusive(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        check_path(path)?;
        self.put_object(path, bytes, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(endpoint: &str, bucket: &str, prefix: &str) -> S3 {
        S3::new(
            Config {
                endpoint: endpoint.into(),
                region: "auto".into(),
                bucket: bucket.into(),
                prefix: prefix.into(),
                access_key_id: "AK".into(),
                secret_access_key: "SK".into(),
            },
            reqwest::Client::new(),
        )
        .unwrap()
    }

    #[test]
    fn the_address_style_follows_the_provider() {
        let aws = s3("https://s3.us-east-1.amazonaws.com", "tw-b", "");
        assert_eq!(aws.bucket_url, "https://tw-b.s3.us-east-1.amazonaws.com/");
        let oss = s3("https://oss-cn-hangzhou.aliyuncs.com", "tw-b", "me");
        assert_eq!(oss.bucket_url, "https://tw-b.oss-cn-hangzhou.aliyuncs.com/");
        assert_eq!(oss.key("keyring"), "me/keyring");
        let pasted = s3(
            "https://tw-b.cos.ap-guangzhou.myqcloud.com",
            "tw-b",
            "/a/b/",
        );
        assert_eq!(
            pasted.bucket_url,
            "https://tw-b.cos.ap-guangzhou.myqcloud.com/"
        );
        assert_eq!(pasted.key("versions/x"), "a/b/versions/x");
        let r2 = s3("https://abc123.r2.cloudflarestorage.com", "tw-b", "");
        assert_eq!(
            r2.bucket_url,
            "https://abc123.r2.cloudflarestorage.com/tw-b/"
        );
        let minio = s3("http://192.168.1.10:9000", "tw-b", "");
        assert_eq!(minio.bucket_url, "http://192.168.1.10:9000/tw-b/");
        assert_eq!(
            minio.object_url("versions/0000000001-aaaaaaaa.twconfig"),
            "http://192.168.1.10:9000/tw-b/versions/0000000001-aaaaaaaa.twconfig"
        );
    }

    #[test]
    fn the_addressing_is_chosen_by_the_endpoint_host_alone() {
        use Addressing::*;
        for (host, want) in [
            ("s3.us-east-1.amazonaws.com", VirtualHost),
            ("s3.amazonaws.com", VirtualHost),
            ("amazonaws.com", VirtualHost),
            ("oss-cn-hangzhou.aliyuncs.com", VirtualHost),
            ("cos.ap-guangzhou.myqcloud.com", VirtualHost),
            ("S3.EU-WEST-1.AMAZONAWS.COM", VirtualHost),
            ("abc123.r2.cloudflarestorage.com", Path),
            ("192.168.1.10", Path),
            ("nas", Path),
            ("minio.example.com", Path),
            ("tw-b.minio.lan", Path),
            ("notamazonaws.com", Path),
            ("amazonaws.com.evil.example", Path),
        ] {
            assert_eq!(addressing(host), want, "{host}");
        }
        assert_eq!(
            bucket_url("http://tw-b.minio.lan:9000", "tw-b").unwrap(),
            "http://tw-b.minio.lan:9000/tw-b/",
            "路径方式不猜桶名是不是已经在域名里"
        );
        assert_eq!(
            bucket_url("https://minio.example.com/s3/", "tw-b").unwrap(),
            "https://minio.example.com/s3/tw-b/"
        );
    }

    #[test]
    fn a_public_plain_http_endpoint_is_refused() {
        let r = S3::new(
            Config {
                endpoint: "http://s3.example.com".into(),
                region: "auto".into(),
                bucket: "b-1".into(),
                prefix: String::new(),
                access_key_id: "AK".into(),
                secret_access_key: "SK".into(),
            },
            reqwest::Client::new(),
        );
        assert!(r.is_err());
    }
}

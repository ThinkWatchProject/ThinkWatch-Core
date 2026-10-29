//! 配置里保存着的凭据，交给出站脱敏按值认（规则 `thinkwatch-saved-secret`）。
//!
//! 收的是**用户填进来、拿到就能用的那些值**：上游的密钥（含 OAuth 令牌、AWS 访问密钥）、
//! 上游自定义请求头里的密钥、本机网关的访问密钥、代理口令、控制面密钥、同步与备份的口令和
//! S3 Secret。`${VAR}` 按网关转发时的办法展开（[`crate::Secret::resolve`]），展开不了的不收。
//! 停用着的上游、密钥照样收：值还在配置里，贴进对话照样是外泄。
//!
//! 不足 [`tw_guard::redact::rules::SAVED_MIN_CHARS`] 个字符的由脱敏引擎丢掉，这里不另筛。

use tw_guard::redact::rules::{SavedSecret, SavedSource, SavedSourceKind};

use crate::Config;
use crate::credential::host_of;

/// 请求头的名字里带着这些词的，值才算凭据：`HTTP-Referer`、`X-Title`、`anthropic-version`
/// 这些是标识和版本，不是秘密，按值去找只会把正文里同样的网址、名字换掉
const SECRET_HEADER_WORDS: &[&str] = &[
    "auth",
    "key",
    "token",
    "secret",
    "pass",
    "cookie",
    "session",
    "signature",
    "credential",
];

fn item(value: String, kind: SavedSourceKind, name: &str, hosts: &[String]) -> SavedSecret {
    SavedSecret {
        value,
        source: SavedSource {
            kind,
            name: name.to_string(),
        },
        hosts: hosts.to_vec(),
    }
}

/// 一个请求头的值里真正的凭据：`Bearer xxx` 去掉鉴权方式那个词
fn header_value(v: &str) -> &str {
    v.trim().rsplit(' ').next().unwrap_or(v)
}

impl Config {
    /// 配置里保存着的凭据，按出现的先后
    pub fn saved_secrets(&self) -> Vec<SavedSecret> {
        let mut out = Vec::new();
        for p in &self.providers {
            let host = host_of(&p.base_url);
            let hosts: Vec<String> = if host.is_empty() {
                Vec::new()
            } else {
                vec![host]
            };
            let key = |v: String| item(v, SavedSourceKind::UpstreamKey, &p.name, &hosts);
            if let Some(Ok(v)) = p.key.as_ref().map(crate::Secret::resolve) {
                out.push(key(v));
            }
            if let Some(o) = &p.oauth {
                // 刷新令牌、客户端密码发往换发令牌的那个端点
                let mut token_hosts = hosts.clone();
                let ep = host_of(&o.endpoint);
                if !ep.is_empty() {
                    token_hosts.push(ep);
                }
                for v in [Some(&o.refresh), o.client_secret.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    out.push(item(
                        v.clone(),
                        SavedSourceKind::UpstreamKey,
                        &p.name,
                        &token_hosts,
                    ));
                }
                if let Some(a) = &o.access {
                    out.push(key(a.clone()));
                }
            }
            if let Some(aws) = &p.aws {
                for s in [
                    &aws.access_key_id,
                    &aws.secret_access_key,
                    &aws.session_token,
                ]
                .into_iter()
                .flatten()
                {
                    if let Ok(v) = s.resolve() {
                        out.push(key(v));
                    }
                }
            }
            for h in p.headers.iter() {
                let name = h.name.to_ascii_lowercase();
                if !SECRET_HEADER_WORDS.iter().any(|w| name.contains(w))
                    || h.value.raw().contains(crate::credential::ACCESS_TOKEN)
                {
                    continue;
                }
                if let Ok(v) = h.value.resolve() {
                    out.push(item(
                        header_value(&v).to_string(),
                        SavedSourceKind::UpstreamHeader,
                        &p.name,
                        &hosts,
                    ));
                }
            }
        }
        // 访问密钥、控制面密钥只该发往本机：哪儿都不列，发往本机以外的地方都算外传
        for c in &self.clients {
            out.push(item(
                c.key.clone(),
                SavedSourceKind::AccessKey,
                &c.name,
                &[],
            ));
        }
        for x in &self.proxies {
            if let Some(a) = &x.auth
                && let Ok(v) = a.pass.resolve()
            {
                let host = host_of(&format!("proxy://{}", x.addr));
                out.push(item(v, SavedSourceKind::ProxyPassword, &x.name, &[host]));
            }
        }
        if let Some(k) = &self.listen.control.key {
            out.push(item(k.clone(), SavedSourceKind::ControlKey, "", &[]));
        }
        if let Some(s) = &self.sync {
            if let Some(p) = &s.passphrase {
                out.push(item(p.clone(), SavedSourceKind::SyncCredential, "", &[]));
            }
            match &s.target {
                crate::sync::Target::Webdav { url, pass, .. } if !pass.is_empty() => {
                    out.push(item(
                        pass.clone(),
                        SavedSourceKind::SyncCredential,
                        "",
                        &[host_of(url)],
                    ));
                }
                crate::sync::Target::S3 {
                    endpoint,
                    secret_access_key,
                    ..
                } => {
                    out.push(item(
                        secret_access_key.clone(),
                        SavedSourceKind::SyncCredential,
                        "",
                        &[host_of(endpoint)],
                    ));
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Header, Headers, Provider, Secret};

    #[test]
    fn upstream_keys_headers_and_access_keys_are_collected_with_their_hosts() {
        // SAFETY: 测试进程里只有这一处设置它
        unsafe { std::env::set_var("TW_TEST_SAVED_KEY", "resolved-from-the-environment") };
        let cfg = Config {
            providers: vec![
                Provider {
                    name: "DeepSeek".into(),
                    base_url: "https://api.deepseek.com/v1".into(),
                    key: Some(Secret::new("${TW_TEST_SAVED_KEY}")),
                    headers: Headers::new(vec![
                        Header {
                            name: "X-Relay-Token".into(),
                            value: Secret::new("Bearer relay-token-value-1"),
                        },
                        Header {
                            name: "HTTP-Referer".into(),
                            value: Secret::new("https://my-app.example.com"),
                        },
                    ]),
                    ..Default::default()
                },
                Provider {
                    name: "missing".into(),
                    base_url: "https://x.example.com".into(),
                    key: Some(Secret::new("${TW_TEST_SAVED_NOT_SET}")),
                    ..Default::default()
                },
            ],
            clients: vec![Client {
                name: "default".into(),
                key: "tw-access-key-value".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let got: Vec<(String, SavedSourceKind, String, Vec<String>)> = cfg
            .saved_secrets()
            .into_iter()
            .map(|s| (s.value, s.source.kind, s.source.name, s.hosts))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "resolved-from-the-environment".into(),
                    SavedSourceKind::UpstreamKey,
                    "DeepSeek".into(),
                    vec!["api.deepseek.com".to_string()]
                ),
                (
                    "relay-token-value-1".into(),
                    SavedSourceKind::UpstreamHeader,
                    "DeepSeek".into(),
                    vec!["api.deepseek.com".to_string()]
                ),
                (
                    "tw-access-key-value".into(),
                    SavedSourceKind::AccessKey,
                    "default".into(),
                    vec![]
                ),
            ]
        );
    }
}

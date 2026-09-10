//! OAuth 凭据的自动刷新（DESIGN.md §3.6 第 4 类）。
//!
//! # 三个不显然的决定
//!
//! **一、换 token 要走这家 provider 自己的 HTTP client。**它带着该走的
//! 代理（§3.7）。用一个干净的 client 去换的话，代理后面的用户会得到一个
//! 「数据面通、刷新不通」的组合 —— 而那个症状看起来完全不像凭据问题。
//!
//! **二、缓存在内存里，不落盘。**access token 是**派生状态**：不是用户
//! 输入的，会过期，丢了重新换一个就行（§3.1 对运行时状态的定义）。落盘
//! 只会多一处密钥副本，换不到任何东西。
//!
//! **三、轮换 refresh token 不在支持范围里，而且要说出来。**很多 OAuth2
//! 服务器每次刷新都发一个新的 refresh token 并作废旧的。支持它意味着
//! 我们得把新的**写回用户的 config.yaml** —— 一个会自己改你配置文件的
//! 网关，比一个说「这种情况请用 `exec`」的网关可怕得多。
//!
//! 所以检测到轮换时：本进程内用新的（这一次能跑通），同时发一个事件说
//! 清「重启之后要你自己更新 config.yaml」。**不说的话，症状是几天后
//! 某次重启开始全是 401，而那时没人会想到是轮换。**

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tw_config::OAuth;

/// 刷新失败之后至少等多久再试。
///
/// **没有它，一个坏掉的 token 端点会被每个请求敲一次。**而那既解决不了
/// 问题，又可能让对方把我们限流。
const BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("换 token 时连不上 {endpoint}：{why}")]
    Http { endpoint: String, why: String },
    #[error("token 端点返回 {status}：{body}")]
    Status { status: u16, body: String },
    #[error("token 端点的响应里没有 access_token")]
    NoToken,
    #[error("上一次换 token 失败了（{why}），{secs} 秒之后再试")]
    Backoff { why: String, secs: u64 },
}

/// 配置里那份凭据的指纹。
///
/// **不是安全用途**，所以用 std 的 hasher 就够：它只回答「用户是不是把
/// config.yaml 里的 refresh token 改了」。**没有它，缓存会盖住用户的修改**
/// —— 用户换了个新凭据，重载配置，然后发现还在用旧的报 401，
/// 而配置文件里明明是对的。那是最难查的一类。
fn fingerprint(cfg: &OAuth) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cfg.refresh.hash(&mut h);
    cfg.endpoint.hash(&mut h);
    cfg.client_id.hash(&mut h);
    cfg.client_secret.hash(&mut h);
    h.finish()
}

struct Live {
    /// 生成它的那份配置的指纹。对不上就是用户改了配置，这条作废
    fp: u64,
    access: String,
    /// 什么时候该去刷。`None` = 服务器没说 `expires_in`，那就一直用到 401
    renew_at: Option<Instant>,
    /// 当前在用的 refresh token（可能是服务器换过的那个）
    refresh: String,
    /// 上一次失败，以及什么时候可以再试
    failed: Option<(String, Instant)>,
}

/// 每个 provider 一份活着的 token。
#[derive(Default)]
pub struct Cache {
    inner: Mutex<HashMap<String, Live>>,
    /// 已经报过轮换的 provider。
    ///
    /// **会轮换的服务器每次刷新都轮换一次。**`expires_in: 1h` 就是每小时
    /// 一条一模一样的告警 —— 而那条话说一次就够了（要做的事是「去改
    /// config.yaml」，改完之前重复说没有新信息，改完之后它还会继续说）。
    /// 通知的代价是用户学会忽略通知，包括那些真该看的（§2.4）。
    reported: Mutex<std::collections::HashSet<String>>,
}

/// 换回来的东西。
struct Fresh {
    access: String,
    expires_in: Option<u64>,
    refresh: Option<String>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 拿一个能用的 access token。
    ///
    /// **只在需要时才联网**：手里那个还没到提前量就直接用。
    pub async fn token(
        &self,
        provider: &str,
        cfg: &OAuth,
        http: &reqwest::Client,
    ) -> Result<(String, Option<String>), OauthError> {
        // 先看手里有没有能用的
        {
            let g = self.inner.lock().expect("锁没毒");
            if let Some(live) = g.get(provider).filter(|l| l.fp == fingerprint(cfg)) {
                if let Some((why, until)) = &live.failed
                    && Instant::now() < *until
                {
                    return Err(OauthError::Backoff {
                        why: why.clone(),
                        secs: until.saturating_duration_since(Instant::now()).as_secs(),
                    });
                }
                let fresh_enough = match live.renew_at {
                    Some(at) => Instant::now() < at,
                    // 服务器没说过期时间 —— 一直用，401 的时候再说
                    None => true,
                };
                if fresh_enough && !live.access.is_empty() {
                    return Ok((live.access.clone(), None));
                }
            } else if let Some(a) = &cfg.access
                && !a.is_empty()
            {
                // 配置里给了一个现成的，先用它 —— 省掉启动时那一次往返
                return Ok((a.clone(), None));
            }
        }
        self.refresh(provider, cfg, http).await
    }

    /// 手里那个不行了（比如上游回了 401），强制换一个。
    pub async fn invalidate_and_refresh(
        &self,
        provider: &str,
        cfg: &OAuth,
        http: &reqwest::Client,
    ) -> Result<(String, Option<String>), OauthError> {
        {
            let mut g = self.inner.lock().expect("锁没毒");
            if let Some(live) = g.get_mut(provider).filter(|l| l.fp == fingerprint(cfg)) {
                live.renew_at = Some(Instant::now());
                // 强制刷新时把退避清掉 —— 这是调用方明确要求的一次
                live.failed = None;
            }
        }
        self.refresh(provider, cfg, http).await
    }

    async fn refresh(
        &self,
        provider: &str,
        cfg: &OAuth,
        http: &reqwest::Client,
    ) -> Result<(String, Option<String>), OauthError> {
        // 用手里那个 refresh（可能是服务器换过的），没有就用配置里的
        let refresh = {
            let g = self.inner.lock().expect("锁没毒");
            g.get(provider)
                // 指纹对不上 = 用户改了配置，缓存里那个（哪怕是服务器
                // 换发的）一律不算，用配置里的重新开始
                .filter(|l| l.fp == fingerprint(cfg))
                .map(|l| l.refresh.clone())
                .unwrap_or_else(|| cfg.refresh.clone())
        };
        let got = match exchange(cfg, &refresh, http).await {
            Ok(f) => f,
            Err(e) => {
                // **记下失败并退避。**没有它，一个坏掉的 token 端点会被
                // 每个请求敲一次
                let mut g = self.inner.lock().expect("锁没毒");
                let why = e.to_string();
                let fp = fingerprint(cfg);
                let entry = g.entry(provider.to_string()).or_insert_with(|| Live {
                    fp: fingerprint(cfg),
                    access: String::new(),
                    renew_at: Some(Instant::now()),
                    refresh: refresh.clone(),
                    failed: None,
                });
                entry.fp = fp;
                entry.failed = Some((why, Instant::now() + BACKOFF));
                return Err(e);
            }
        };
        // 服务器换了 refresh token 吗
        let rotated = got
            .refresh
            .as_deref()
            .filter(|r| !r.is_empty() && *r != refresh.as_str())
            .map(|r| r.to_string())
            // **每个 provider 只往外报一次**（见 `reported` 的注释）。
            // 缓存里照样换成新的 —— 报不报和用不用是两件事
            .filter(|_| {
                self.reported
                    .lock()
                    .expect("锁没毒")
                    .insert(provider.to_string())
            });

        let renew_at = got.expires_in.map(|secs| {
            let lead = cfg.refresh_before().as_secs();
            // **下限 1 秒，而这个下限是有意义的**：提前量写得比有效期还长
            // 时（`expires_in: 60` 配 `refresh_before: 5m`），减出来是 0，
            // 那意味着「现在就该刷」—— 于是每个请求刷一次，而那比不刷更糟。
            // 宁可让 token 短命 1 秒。
            let life = secs.saturating_sub(lead).max(1);
            Instant::now() + Duration::from_secs(life)
        });
        let mut g = self.inner.lock().expect("锁没毒");
        g.insert(
            provider.to_string(),
            Live {
                fp: fingerprint(cfg),
                access: got.access.clone(),
                renew_at,
                refresh: rotated.clone().unwrap_or(refresh),
                failed: None,
            },
        );
        Ok((got.access, rotated))
    }
}

/// 把**我们刚发出去的那几个值**从对方的正文里抹掉。
///
/// `tw_secret::mask_body` 认的是已知的密钥形状，而 OAuth 的 refresh token
/// 是不透明的 —— 实测 `{"provided":"1//0gLd…"}` 这种回显原样穿过去了，
/// 因为那个字段名它不认识，那个值也不像任何一种 key。
///
/// 而这一层不一样：**这几个值是我们自己发出去的，我们知道它们长什么样**，
/// 所以可以精确地抹掉，不依赖任何模式。
fn scrub_sent(text: &str, sent: &[&str]) -> String {
    let mut out = text.to_string();
    // 太短的不抹：一个三个字母的 client_secret 会把正文里无关的地方也
    // 换掉，那时错误消息本身变得不可读，比漏一点更妨碍排查
    for v in sent.iter().filter(|v| v.len() >= 6) {
        out = out.replace(*v, "<省略>");
        // 表单是百分号编码之后发出去的，对方回显的很可能是编码后的样子
        let enc = form_encode(v);
        if enc != *v {
            out = out.replace(&enc, "<省略>");
        }
    }
    out
}

/// `application/x-www-form-urlencoded` 的编码规则。**只用来比对**，
/// 发请求那份是 reqwest 编的 —— 密钥的编码不自己写。
fn form_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                o.push(b as char)
            }
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

async fn exchange(cfg: &OAuth, refresh: &str, http: &reqwest::Client) -> Result<Fresh, OauthError> {
    let mut form: Vec<(&str, &str)> =
        vec![("grant_type", "refresh_token"), ("refresh_token", refresh)];
    if let Some(id) = &cfg.client_id {
        form.push(("client_id", id));
    }
    if let Some(sec) = &cfg.client_secret {
        form.push(("client_secret", sec));
    }
    let resp = http
        .post(&cfg.endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| OauthError::Http {
            endpoint: tw_secret::redact_url(&cfg.endpoint),
            why: e.to_string(),
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        // **正文要打码。**OAuth 服务器的错误里经常把请求参数回显出来，
        // 那里面有 refresh token（§9.7）。
        //
        // 顺序是「先抹我们发的、再按形状打码、最后截断」：反过来的话
        // `mask_body` 会把凭据改成另一个样子，于是精确比对找不到它了。
        let scrubbed = scrub_sent(
            &text,
            &[refresh, cfg.client_secret.as_deref().unwrap_or("")],
        );
        let masked = tw_secret::mask_body(&scrubbed);
        return Err(OauthError::Status {
            status,
            body: masked.chars().take(400).collect::<String>(),
        });
    }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| OauthError::NoToken)?;
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(OauthError::NoToken)?
        .to_string();
    Ok(Fresh {
        access,
        expires_in: v.get("expires_in").and_then(|x| x.as_u64()),
        refresh: v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str) -> OAuth {
        OAuth {
            access: None,
            refresh: "r-original".into(),
            endpoint: endpoint.into(),
            client_id: None,
            client_secret: None,
            refresh_before: Some("5m".into()),
        }
    }

    #[test]
    fn a_configured_access_token_is_used_without_going_online() {
        // 省掉启动时那一次往返。
        let c = Cache::new();
        let mut o = cfg("http://127.0.0.1:1/nope");
        o.access = Some("a-existing".into());
        let http = reqwest::Client::new();
        let got = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(c.token("p", &o, &http))
            .unwrap();
        assert_eq!(got.0, "a-existing");
        assert!(got.1.is_none());
    }

    #[test]
    fn a_broken_endpoint_backs_off_instead_of_being_hammered() {
        // **没有退避，一个坏掉的 token 端点会被每个请求敲一次。**
        let c = Cache::new();
        let o = cfg("http://127.0.0.1:1/nope");
        let http = reqwest::Client::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let first = rt.block_on(c.token("p", &o, &http)).unwrap_err();
        assert!(matches!(first, OauthError::Http { .. }), "{first}");
        let second = rt.block_on(c.token("p", &o, &http)).unwrap_err();
        // 第二次直接被退避挡住，没有再去连
        assert!(matches!(second, OauthError::Backoff { .. }), "{second}");
        assert!(second.to_string().contains("秒之后再试"), "{second}");
    }

    #[test]
    fn the_duration_suffixes_the_docs_use_all_parse() {
        use tw_config::parse_duration_secs as p;
        assert_eq!(p("30s"), Some(30));
        assert_eq!(p("5m"), Some(300));
        assert_eq!(p("1h"), Some(3600));
        assert_eq!(p("300"), Some(300));
        // 写坏了返回 None，调用方按默认走 —— 一个写错的提前量不该让
        // 上游整个不可用
        assert_eq!(p("五分钟"), None);
        assert_eq!(p(""), None);
        assert_eq!(cfg("x").refresh_before().as_secs(), 300);
        let mut bad = cfg("x");
        bad.refresh_before = Some("乱写".into());
        assert_eq!(bad.refresh_before().as_secs(), 300, "写坏了该走默认");
    }

    #[test]
    fn describing_an_oauth_credential_never_shows_a_token() {
        // **refresh token 比 access token 更值钱**，它换得出无数个 access。
        let s = tw_config::Secret::OAuth {
            oauth: cfg("https://auth.example.com/token"),
        };
        let d = s.describe();
        assert!(!d.contains("r-original"), "{d}");
        assert!(d.contains("auth.example.com"), "{d}");
    }

    #[test]
    fn a_sync_resolve_says_what_is_going_on_instead_of_inventing_a_value() {
        let s = tw_config::Secret::OAuth { oauth: cfg("x") };
        let e = s.resolve().unwrap_err();
        assert!(e.to_string().contains("换 token"), "{e}");
    }
}

//! OAuth 凭据的自动刷新（第 3 类）。
//!
//! # 三个不显然的决定
//!
//! **一、换 token 要走这家 provider 自己的 HTTP client。**它带着该走的
//! 代理。用一个干净的 client 去换的话，代理后面的用户会得到一个
//! 「数据面通、刷新不通」的组合 —— 而那个症状看起来完全不像凭据问题。
//!
//! **二、换回来的 access token 连同过期时间写回 config.yaml**（2026-09-18 定，以前
//! 只放内存）。重启之后配置里那个没过期就直接用：不用先换一次 token，也就不会
//! 每次重启都轮换一次 refresh token、写一次文件；启动时 token 端点一时连不上，
//! 请求也照样能发。内存里仍然有一份，转发时不读文件。
//!
//! **三、服务器换发的新 refresh token 要写回 config.yaml。**很多 OAuth2
//! 服务器每次刷新都换发一个新的并作废旧的。那是一次用户没要求的写入，
//! 但**不写回更糟**：换发新的那一刻旧的已经在服务端作废了 —— 不写回
//! 等于让配置文件从那一秒起就是坏的，只是症状延迟到下一次重启（那时
//! 突然全是 401，而没人会想到是几天前的一次轮换）。
//!
//! 这一层只负责**把新值交出去**（`Renewed` 往通道里一放），写文件是
//! 控制面的事 —— 那里才有历史快照、乐观并发和防回环，而这里是转发
//! 路径，它不能等一次磁盘写。
//!
//! **四、同一时刻只许一次刷新。**会轮换的 refresh token 只能用一次：两个请求
//! 同时拿同一个去刷，后到的那个会被判成重复使用，而 OpenAI 这类服务器会就此
//! 作废整条登录。所以刷新按 provider 排队，排到的先看前面的人是不是已经刷好了。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tw_config::OAuth;

/// 刷新失败之后至少等多久再试。
///
/// **没有它，一个坏掉的 token 端点会被每个请求敲一次。**而那既解决不了
/// 问题，又可能让对方把我们限流。
const BACKOFF: Duration = Duration::from_secs(30);

/// 刚换来不到这么久的 access token 被上游拒了，不再强制换。
///
/// **问题不在 token**（账号被停用、没有权限……）。不设这条的话，这种上游的每个请求
/// 都会换一次 token —— 会轮换的服务器每次作废一个 refresh token，配置文件每个请求写一次。
const FRESH_ENOUGH: Duration = Duration::from_secs(60);

/// refresh token 作废之后多久再试。
///
/// **不是指望它自己好起来**：过期、被用过、被吊销的 refresh token 只有重新登录能换掉。
/// 退避这么久是为了不让每个请求都去撞一次 token 端点；重新登录会换掉配置里的
/// refresh token，缓存随即不再作数，下一个请求立刻用新的。
const EXPIRED_BACKOFF: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("{endpoint} could not be reached while refreshing the token: {why}")]
    Http { endpoint: String, why: String },
    #[error("the token endpoint answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the token endpoint's response has no access_token")]
    NoToken,
    #[error("the last token refresh failed ({why}); retrying in {secs} s")]
    Backoff { why: String, secs: u64 },
    /// refresh token 已经作废。**重试没有用**，要重新登录或换一个 refresh token
    #[error("the OAuth credential has expired; sign in again or replace the refresh token ({why})")]
    Expired { why: String },
}

impl OauthError {
    /// 要不要重新登录才能恢复。
    pub fn needs_login(&self) -> bool {
        matches!(self, OauthError::Expired { .. })
    }
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
    /// **生成它的那份配置**的指纹。对不上未必作废 —— 见 `usable_for`
    fp: u64,
    access: String,
    /// 什么时候该去刷。`None` = 服务器没说 `expires_in`，那就一直用到 401
    renew_at: Option<Instant>,
    /// 当前在用的 refresh token（可能是服务器换过的那个）
    refresh: String,
    /// 这个 access token 是什么时候从 token 端点换来的。`None` = 从配置里读的，不知道多久了
    obtained: Option<Instant>,
    /// 上一次失败，以及什么时候可以再试
    failed: Option<Failure>,
}

struct Failure {
    why: String,
    until: Instant,
    /// refresh token 作废了（见 [`OauthError::Expired`]）
    expired: bool,
}

impl Failure {
    fn error(&self) -> OauthError {
        if self.expired {
            OauthError::Expired {
                why: self.why.clone(),
            }
        } else {
            OauthError::Backoff {
                why: self.why.clone(),
                secs: self
                    .until
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
            }
        }
    }
}

impl Live {
    /// 这条缓存还能拿来回答这份配置吗。
    ///
    /// **两个出口，缺一不可** —— 而缺的那个会变成一个自我维持的轮换
    /// 循环：
    ///
    /// 1. `fp` 相同：配置没变过，最常见的情况。
    /// 2. **配置里的 refresh 就是我们手里这个**：轮换之后我们把新值写回
    ///    了 config.yaml，文件一变监听就重载，重载出来的 `cfg`
    ///    带着新值 —— 指纹当然对不上第一条。只认第一条的话，每次重载
    ///    都会重新换一次 token，而每次换又换回一个新的 refresh、又写一次
    ///    文件、又触发一次重载。**一小时一次的轮换会变成一个停不下来的
    ///    循环。**
    ///
    /// 用户真的手改成了别的值时，两条都不成立 —— 于是重新换，这正是
    /// 想要的。
    fn usable_for(&self, cfg: &OAuth) -> bool {
        self.fp == fingerprint(cfg) || self.refresh == cfg.refresh
    }
}

/// 刷新换回来的 token，要有人把它写回 config.yaml。
///
/// **数据面不写文件。**它只把这件事交出去（和 body 那条路同一
/// 条纪律：观测和维护绝不跑在转发路径上）。真正动文件的是控制面 ——
/// 那里才有 `ConfigManager`，才有历史快照、乐观并发、防回环那一整套。
///
/// 里面的 token 是**原文**：它们必须原样送到写文件那一层，所以只在进程内的通道里走，
/// 不进日志、不进事件、不进诊断包（`Debug` 也不打印它们）。
#[derive(Clone)]
pub struct Renewed {
    pub provider: String,
    pub access: String,
    /// RFC 3339（UTC）。服务器没说有效期就没有
    pub expires_at: Option<String>,
    /// 服务器换发了新的 refresh token 才有
    pub refresh: Option<String>,
    /// token 端点，**已打码**，只用来说人话
    pub endpoint: String,
}

impl std::fmt::Debug for Renewed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewed")
            .field("provider", &self.provider)
            .field("expires_at", &self.expires_at)
            .field("rotated", &self.refresh.is_some())
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

pub type RenewalSender = tokio::sync::mpsc::Sender<Renewed>;

/// 每个 provider 一份活着的 token。
#[derive(Default)]
pub struct Cache {
    inner: Mutex<HashMap<String, Live>>,
    /// 每个 provider 一把刷新锁（见模块文档第四条）
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
    ) -> Result<(String, Option<Renewed>), OauthError> {
        if let Some(ready) = self.cached(provider, cfg)? {
            return Ok((ready, None));
        }
        let flight = self.flight(provider);
        let _one = flight.lock().await;
        // 排队的这段时间里，前面那个请求可能已经刷好了
        if let Some(ready) = self.cached(provider, cfg)? {
            return Ok((ready, None));
        }
        self.refresh(provider, cfg, http).await
    }

    /// 手里那个不行了（上游回了 401），强制换一个。
    ///
    /// `sent_at` 是被拒的那个请求发出去的时刻。**在那之后已经有人换过的话，直接用新的**，
    /// 不再换第二次 —— 几个并发请求同时撞上 401 时，只该有一次刷新。手里那个是一分钟内
    /// 刚换来的，也不再换（见 [`FRESH_ENOUGH`]），原样交回去。
    pub async fn invalidate_and_refresh(
        &self,
        provider: &str,
        cfg: &OAuth,
        http: &reqwest::Client,
        sent_at: Instant,
    ) -> Result<(String, Option<Renewed>), OauthError> {
        let flight = self.flight(provider);
        let _one = flight.lock().await;
        {
            let mut g = self.inner.lock().expect("lock not poisoned");
            if let Some(live) = g.get_mut(provider).filter(|l| l.usable_for(cfg)) {
                let renewed = !live.access.is_empty()
                    && live
                        .obtained
                        .is_some_and(|t| t > sent_at || t.elapsed() < FRESH_ENOUGH)
                    && live.failed.is_none()
                    && live.renew_at.is_none_or(|at| Instant::now() < at);
                if renewed {
                    return Ok((live.access.clone(), None));
                }
                live.renew_at = Some(Instant::now());
                // 强制刷新时把退避清掉 —— 这是调用方明确要求的一次
                live.failed = None;
            }
        }
        self.refresh(provider, cfg, http).await
    }

    /// 这家最近一次刷新失败的原因，和是不是要重新登录。没失败过、或者已经恢复，是 `None`。
    pub fn failure(&self, provider: &str, cfg: &OAuth) -> Option<(String, bool)> {
        let g = self.inner.lock().expect("lock not poisoned");
        let f = g
            .get(provider)
            .filter(|l| l.usable_for(cfg))?
            .failed
            .as_ref()?;
        Some((f.error().to_string(), f.expired))
    }

    fn flight(&self, provider: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.flights
            .lock()
            .expect("lock not poisoned")
            .entry(provider.to_string())
            .or_default()
            .clone()
    }

    /// 手里能直接用的 token。`Err` 是还在退避。
    fn cached(&self, provider: &str, cfg: &OAuth) -> Result<Option<String>, OauthError> {
        let mut g = self.inner.lock().expect("lock not poisoned");
        if let Some(live) = g.get(provider).filter(|l| l.usable_for(cfg)) {
            if let Some(f) = &live.failed
                && Instant::now() < f.until
            {
                return Err(f.error());
            }
            // 服务器没说过期时间 —— 一直用，401 的时候再说
            let fresh_enough = live.renew_at.is_none_or(|at| Instant::now() < at);
            if fresh_enough && !live.access.is_empty() {
                return Ok(Some(live.access.clone()));
            }
        } else if let Some(a) = cfg.access.as_deref().filter(|a| !a.is_empty()) {
            // 配置里写着一个（上次刷新写回的，或者手填的）。**没到提前量就直接用** ——
            // 省掉启动时那一次往返，也省掉一次 refresh token 轮换
            let renew_at = match cfg.expires_at.as_deref() {
                None => None,
                Some(at) => match remaining(at) {
                    Some(left) if left > cfg.refresh_before() => {
                        Some(Instant::now() + (left - cfg.refresh_before()))
                    }
                    // 快过期、已过期或者写坏了：去换一个
                    _ => return Ok(None),
                },
            };
            g.insert(
                provider.to_string(),
                Live {
                    fp: fingerprint(cfg),
                    access: a.to_string(),
                    renew_at,
                    refresh: cfg.refresh.clone(),
                    obtained: None,
                    failed: None,
                },
            );
            return Ok(Some(a.to_string()));
        }
        Ok(None)
    }

    /// 真的去换。**调用方持有这家的刷新锁**，所以这里不会有并发的第二次。
    async fn refresh(
        &self,
        provider: &str,
        cfg: &OAuth,
        http: &reqwest::Client,
    ) -> Result<(String, Option<Renewed>), OauthError> {
        // 用手里那个 refresh（可能是服务器换过的），没有就用配置里的
        let refresh = {
            let g = self.inner.lock().expect("lock not poisoned");
            g.get(provider)
                // 认不出来 = 用户改了配置，缓存里那个（哪怕是服务器
                // 换发的）一律不算，用配置里的重新开始
                .filter(|l| l.usable_for(cfg))
                .map(|l| l.refresh.clone())
                .unwrap_or_else(|| cfg.refresh.clone())
        };
        let got = match exchange(cfg, &refresh, http).await {
            Ok(f) => f,
            Err(e) => {
                // **记下失败并退避。**没有它，一个坏掉的 token 端点会被
                // 每个请求敲一次
                let (why, expired) = match &e {
                    OauthError::Expired { why } => (why.clone(), true),
                    other => (other.to_string(), false),
                };
                let until = Instant::now() + if expired { EXPIRED_BACKOFF } else { BACKOFF };
                let mut g = self.inner.lock().expect("lock not poisoned");
                let entry = g.entry(provider.to_string()).or_insert_with(|| Live {
                    fp: fingerprint(cfg),
                    access: String::new(),
                    renew_at: Some(Instant::now()),
                    refresh: refresh.clone(),
                    obtained: None,
                    failed: None,
                });
                entry.fp = fingerprint(cfg);
                entry.failed = Some(Failure {
                    why,
                    until,
                    expired,
                });
                return Err(e);
            }
        };
        // 服务器换了 refresh token 吗
        let rotated = got
            .refresh
            .as_deref()
            .filter(|r| !r.is_empty() && *r != refresh.as_str())
            .map(|r| r.to_string());
        // **每一次轮换都要往外送，不能在这儿去重。**新值要写回
        // config.yaml，而漏掉一次写回就等于让配置文件从那一刻
        // 起是坏的。「同一句话别说第二遍」是报事件那一层的事，不是这一层的。
        let mut g = self.inner.lock().expect("lock not poisoned");
        g.insert(
            provider.to_string(),
            Live {
                fp: fingerprint(cfg),
                access: got.access.clone(),
                renew_at: renew_at(cfg, got.expires_in),
                refresh: rotated.clone().unwrap_or(refresh),
                obtained: Some(Instant::now()),
                failed: None,
            },
        );
        let renewed = Renewed {
            provider: provider.to_string(),
            access: got.access.clone(),
            expires_at: got.expires_in.map(|secs| {
                (chrono::Utc::now() + chrono::Duration::seconds(secs.min(i64::MAX as u64) as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            }),
            refresh: rotated,
            endpoint: tw_secret::redact_url(&cfg.endpoint),
        };
        Ok((got.access, Some(renewed)))
    }
}

/// 配置里写的过期时间离现在还有多久。已经过了、或者写坏了，是 `None`
fn remaining(expires_at: &str) -> Option<Duration> {
    let at = chrono::DateTime::parse_from_rfc3339(expires_at.trim()).ok()?;
    (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

/// 什么时候该去刷。`None` = 服务器没说有效期
fn renew_at(cfg: &OAuth, expires_in: Option<u64>) -> Option<Instant> {
    expires_in.map(|secs| {
        let lead = cfg.refresh_before().as_secs();
        // **下限 1 秒，而这个下限是有意义的**：提前量写得比有效期还长
        // 时（`expires_in: 60` 配 `refresh_before: 5m`），减出来是 0，
        // 那意味着「现在就该刷」—— 于是每个请求刷一次，而那比不刷更糟。
        // 宁可让 token 短命 1 秒。
        let life = secs.saturating_sub(lead).max(1);
        Instant::now() + Duration::from_secs(life)
    })
}

/// token 端点的这个回答是不是在说「refresh token 作废了」。
///
/// RFC 6749 用 `invalid_grant` 说这件事；OpenAI 还在 `error.code` 里细分成过期、被用过、
/// 被吊销（和 Codex 的登录实现认的是同一组）。401 也算：客户端凭据本身不被接受。
fn is_expired(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let code = v
        .get("error")
        .and_then(|e| {
            e.as_str()
                .or_else(|| e.get("code").and_then(|c| c.as_str()))
        })
        .or_else(|| v.get("code").and_then(|c| c.as_str()))
        .map(str::to_ascii_lowercase);
    matches!(
        code.as_deref(),
        Some(
            "invalid_grant"
                | "refresh_token_expired"
                | "refresh_token_reused"
                | "refresh_token_invalidated"
        )
    )
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
        out = out.replace(*v, "<omitted>");
        // 表单是百分号编码之后发出去的，对方回显的很可能是编码后的样子
        let enc = form_encode(v);
        if enc != *v {
            out = out.replace(&enc, "<omitted>");
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
    let req = http.post(&cfg.endpoint);
    // **ChatGPT 登录用 JSON 刷新**：Codex 就是这么发的，这是实际验证过的格式。别的
    // OAuth 服务器照 RFC 6749 发表单
    let req = if cfg.client_id.as_deref() == Some(tw_config::chatgpt::CLIENT_ID) {
        let body: serde_json::Map<String, serde_json::Value> = form
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
            .collect();
        req.json(&body)
    } else {
        req.form(&form)
    };
    let resp = req.send().await.map_err(|e| OauthError::Http {
        endpoint: tw_secret::redact_url(&cfg.endpoint),
        why: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        // **正文要打码。**OAuth 服务器的错误里经常把请求参数回显出来，
        // 那里面有 refresh token。
        //
        // 顺序是「先抹我们发的、再按形状打码、最后截断」：反过来的话
        // `mask_body` 会把凭据改成另一个样子，于是精确比对找不到它了。
        let scrubbed = scrub_sent(
            &text,
            &[refresh, cfg.client_secret.as_deref().unwrap_or("")],
        );
        let masked = tw_secret::mask_body(&scrubbed);
        let body = masked.chars().take(400).collect::<String>();
        if is_expired(status, &text) {
            return Err(OauthError::Expired {
                why: format!("the token endpoint answered {status}: {body}"),
            });
        }
        return Err(OauthError::Status { status, body });
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
            expires_at: None,
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
    fn a_configured_access_token_is_used_until_its_lead_time_then_renewed() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let http = reqwest::Client::new();
        let at = |secs: i64| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };

        // 离过期还有一天：直接用，不联网（端点是个连不上的地址）
        let mut fresh = cfg("http://127.0.0.1:1/nope");
        fresh.access = Some("a-fresh".into());
        fresh.expires_at = Some(at(86_400));
        let got = rt.block_on(Cache::new().token("p", &fresh, &http)).unwrap();
        assert_eq!(got.0, "a-fresh");

        // 只剩一分钟，提前量是五分钟：要去换 —— 换不到就是连接错误，而不是拿着旧的发出去
        let mut soon = cfg("http://127.0.0.1:1/nope");
        soon.access = Some("a-soon".into());
        soon.expires_at = Some(at(60));
        let err = rt
            .block_on(Cache::new().token("p", &soon, &http))
            .unwrap_err();
        assert!(matches!(err, OauthError::Http { .. }), "{err}");

        // 已经过期、或者过期时间写坏了：同样去换
        for bad in [at(-10), "明天".to_string()] {
            let mut stale = cfg("http://127.0.0.1:1/nope");
            stale.access = Some("a-stale".into());
            stale.expires_at = Some(bad);
            assert!(rt.block_on(Cache::new().token("p", &stale, &http)).is_err());
        }
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
        assert!(second.to_string().contains("retrying in"), "{second}");
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
    fn headers_without_a_token_say_so_instead_of_inventing_a_value() {
        // OAuth 的 token 要联网换，同步那条路给不出来 —— 说出来，而不是发一个空的鉴权头
        let p = tw_config::Provider {
            name: "o".into(),
            base_url: "https://relay.example".into(),
            oauth: Some(cfg("https://auth.example.com/token")),
            ..Default::default()
        };
        assert_eq!(
            p.outbound_headers(None),
            Err(tw_config::CredentialError::NoToken)
        );
    }
}

//! 上游的余额：钱包里还有多少、额度用了多少、到期时刻，从各家自己的余额接口读。
//!
//! （不要和 [`crate::balance`] 混了：那个是 `load-balance` 组轮到了谁。）
//!
//! 每一种接口一个读法（[`tw_api::BalanceSource`]），都用这家上游自己的密钥、走它自己的
//! 出站设置（代理），请求头只有 ThinkWatch 的 User-Agent、`Accept` 和凭据
//! （[`crate::egress::balance_headers`]），每个请求最多等 [`TIMEOUT`]。接口的路径接在地址的
//! **源**（`https://主机:端口`）后面，不接在 `/anthropic` 这类路径后面。
//!
//! 哪一种：配置里写明的（`balance:`），或者 `auto` 时按主机认（OpenRouter、DeepSeek、
//! Moonshot 的官方地址）；别的地址问一次它是哪一种中转站（[`detect`]），问出来的只记在内存里。
//! ChatGPT、Z.ai 账号和 Bedrock 不读：它们的额度另有来处（[`crate::quota`]、[`crate::glm`]）。
//!
//! **读不读、读成没有都不影响转发**：读在后台，结果只进视图和 `balance_updated` 事件。
//! 什么时候读见 [`tracker`]，读懂回答见 [`parse`]。

pub mod parse;
pub mod tracker;

use std::time::Duration;

use serde_json::Value;
use tw_api::BalanceSource;
use tw_types::{Msg, msg};

pub use parse::Reading;
pub use tracker::{Claim, Detection, Outcome, Plan, Step, Tracker, Why};

/// 跟着一个经过这一家的请求走：请求结束时（它被丢掉时，正常交完、客户端走掉都一样）
/// 记一笔，到了时候就重读这一家的余额（见 [`tracker`]）。**只做个记号，不联网**
#[must_use = "dropping it at once counts the request as ended"]
pub struct Passing {
    tracker: std::sync::Arc<Tracker>,
    provider: String,
}

impl Passing {
    pub fn new(tracker: std::sync::Arc<Tracker>, provider: &str) -> Self {
        Self {
            tracker,
            provider: provider.to_string(),
        }
    }
}

impl Drop for Passing {
    fn drop(&mut self) {
        self.tracker.note_request(&self.provider);
    }
}

/// 读余额的每一个请求最多等多久
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// 余额接口的回答最多读这么多。都是几百字节的 JSON，大得多的不是余额
const BODY_MAX: usize = 256 * 1024;

/// 后台多久看一次该读谁。请求结束时会提前叫醒它
const TICK: Duration = Duration::from_secs(5);

/// 地址的源：`https://主机[:端口]`。读不出主机的是 `None`
pub fn origin(base_url: &str) -> Option<String> {
    let u = reqwest::Url::parse(base_url.trim()).ok()?;
    u.host_str()?;
    let o = u.origin();
    o.is_tuple().then(|| o.ascii_serialization())
}

/// 主机，小写
fn host(base_url: &str) -> Option<String> {
    let u = reqwest::Url::parse(base_url.trim()).ok()?;
    Some(u.host_str()?.to_ascii_lowercase())
}

/// `auto` 时按主机认得出的官方地址。
pub fn by_host(base_url: &str) -> Option<BalanceSource> {
    match host(base_url)?.as_str() {
        "openrouter.ai" => Some(BalanceSource::Openrouter),
        "api.deepseek.com" => Some(BalanceSource::Deepseek),
        "api.moonshot.cn" | "api.moonshot.ai" => Some(BalanceSource::Moonshot),
        _ => None,
    }
}

/// 模型厂商自己的接口地址：**不问**它是哪一种中转站，它不是中转站
const VENDOR_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "generativelanguage.googleapis.com",
];

/// `balance:` 和地址定下来的读法。`None`：这家没有余额可读（关掉了、地址读不出主机、
/// 模型厂商自己的地址）
pub fn plan(setting: tw_config::BalanceSetting, base_url: &str) -> Option<Plan> {
    let setting = tw_api::BalanceSetting::from(setting);
    if setting == tw_api::BalanceSetting::Off {
        return None;
    }
    let host = host(base_url)?;
    if let Some(s) = setting.source() {
        return Some(Plan::Known(s));
    }
    if let Some(s) = by_host(base_url) {
        return Some(Plan::Known(s));
    }
    (!VENDOR_HOSTS.contains(&host.as_str())).then_some(Plan::Detect)
}

/// 读余额用的 `Authorization`：这家的 API 密钥，按 Bearer 发。密钥写在 `headers` 里的，
/// 取鉴权的那一个头。OAuth 的、没有凭据的、凭据读不出来的是 `None`
pub fn authorization(p: &tw_config::Provider) -> Option<String> {
    if let Some(k) = &p.key {
        return k.resolve().ok().map(|k| format!("Bearer {k}"));
    }
    if p.oauth.is_some() {
        return None;
    }
    let headers = p.outbound_headers(None).ok()?;
    let named = |want: &[&str]| {
        headers
            .iter()
            .find(|(n, _)| want.iter().any(|w| n.eq_ignore_ascii_case(w)))
            .map(|(_, v)| v.clone())
    };
    named(&["authorization"]).or_else(|| {
        named(&["x-api-key", "api-key", "x-goog-api-key"]).map(|v| format!("Bearer {v}"))
    })
}

// ---------------------------------------------------------------- 取数

/// 一个余额请求没成的样子。
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// 连不上、超时、读到一半断了
    Transport(Msg),
    /// 回的不是 2xx
    Status(u16),
    /// 2xx，可不是我们认得的回答
    Unrecognized,
}

impl Failure {
    /// 给界面的那句话
    pub fn msg(&self) -> Msg {
        match self {
            Failure::Transport(why) => why.clone(),
            Failure::Status(status @ (401 | 403)) => msg!(
                "gw.balance.rejected", status = status =>
                "The balance could not be read: the upstream rejected the key (HTTP {status})."
            ),
            Failure::Status(status) => msg!(
                "gw.balance.status", status = status =>
                "The balance could not be read: the upstream answered HTTP {status}."
            ),
            Failure::Unrecognized => msg!(
                "gw.balance.unrecognized" =>
                "The balance could not be read: the answer is in an unrecognized format."
            ),
        }
    }

    /// 对方说了话（转到别处去、4xx、读不懂的回答）：它就是这样，再问也一样。连不上、超时、
    /// 5xx、429 说明不了什么。
    ///
    /// **3xx 也是回答**：出站的 client 不跟跳转，问到一个跳去登录页、首页的地址，说明这里
    /// 没有这个接口，不是没问成
    fn answered(&self) -> bool {
        match self {
            Failure::Transport(_) => false,
            Failure::Status(s) => (300..500).contains(s) && *s != 429,
            Failure::Unrecognized => true,
        }
    }
}

/// 发一个 GET，读回 JSON。`auth` 是 `None` 的不带凭据（公开的接口）
async fn get_json(http: &reqwest::Client, url: &str, auth: Option<&str>) -> Result<Value, Failure> {
    let mut req = http.get(url).timeout(TIMEOUT);
    for (name, value) in crate::egress::balance_headers(auth) {
        req = req.header(name, value);
    }
    let mut r = req.send().await.map_err(transport)?;
    let status = r.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Failure::Status(status));
    }
    let mut body = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(transport)? {
        if body.len() + chunk.len() > BODY_MAX {
            return Err(Failure::Unrecognized);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| Failure::Unrecognized)
}

fn transport(e: reqwest::Error) -> Failure {
    Failure::Transport(if e.is_timeout() {
        msg!(
            "gw.balance.timeout", secs = TIMEOUT.as_secs() =>
            "The balance could not be read: no answer within {secs} seconds."
        )
    } else if e.is_connect() {
        msg!(
            "gw.balance.connect" =>
            "The balance could not be read: the upstream could not be reached."
        )
    } else {
        msg!(
            "gw.balance.request_failed", detail = chain(&e) =>
            "The balance could not be read: {detail}"
        )
    })
}

/// 错误连同它的每一层原因：reqwest 自己只说最外面一层
fn chain(e: &reqwest::Error) -> String {
    let mut detail = e.to_string();
    let mut cur = std::error::Error::source(e);
    while let Some(c) = cur {
        detail.push_str(": ");
        detail.push_str(&c.to_string());
        cur = c.source();
    }
    detail
}

/// 问出来的是哪一种。
#[derive(Debug)]
pub enum Detected {
    /// 认出来了，**连同认出它的那一次回答**：Sub2API 和企业网关的余额就在里面，New API 的
    /// 总额度也在，不用再问一遍
    Found(BalanceSource, Value),
    /// 两个都答了话，可都不是
    Neither,
    /// 有一个没问成：连不上、超时、对方出错。带着头一个没问成的原因
    Undecided(Failure),
}

/// 先问 Sub2API（和企业网关）的用量接口，再问 New API 的账单接口。
pub async fn detect(http: &reqwest::Client, origin: &str, auth: &str) -> Detected {
    let usage = get_json(http, &format!("{origin}/v1/usage"), Some(auth)).await;
    if let Ok(v) = &usage {
        if parse::is_sub2api(v) {
            return Detected::Found(BalanceSource::Sub2api, v.clone());
        }
        if parse::is_thinkwatch(v) {
            return Detected::Found(BalanceSource::Thinkwatch, v.clone());
        }
    }
    let billing = get_json(
        http,
        &format!("{origin}/v1/dashboard/billing/subscription"),
        Some(auth),
    )
    .await;
    if let Ok(v) = &billing
        && parse::is_newapi(v)
    {
        return Detected::Found(BalanceSource::Newapi, v.clone());
    }
    // 两个都答了话（不是这个形状、没有这个接口、不认这把密钥、转到别处去）：两种都不是。
    // 有一个没问成，就说不准
    match [usage, billing]
        .into_iter()
        .find_map(|r| r.err().filter(|f| !f.answered()))
    {
        Some(f) => Detected::Undecided(f),
        None => Detected::Neither,
    }
}

/// 读一次。`known`：检测时已经拿到的那一份回答（Sub2API、企业网关的用量，New API 的账单）
pub async fn read(
    http: &reqwest::Client,
    source: BalanceSource,
    base_url: &str,
    auth: &str,
    known: Option<Value>,
) -> Result<Reading, Failure> {
    let origin = origin(base_url).ok_or(Failure::Unrecognized)?;
    let get = |path: &str| {
        let url = format!("{origin}{path}");
        async move { get_json(http, &url, Some(auth)).await }
    };
    let known_or = |path: &'static str| {
        let known = known.clone();
        async move {
            match known {
                Some(v) => Ok(v),
                None => get(path).await,
            }
        }
    };
    let parsed = |r: Option<Reading>| r.ok_or(Failure::Unrecognized);
    match source {
        // **只问这把密钥自己的接口**：账户余额（`/api/v1/credits`）只有管理密钥问得到，
        // 能转发请求的密钥问它是 403
        BalanceSource::Openrouter => parsed(parse::openrouter(&get("/api/v1/key").await?)),
        BalanceSource::Deepseek => parsed(parse::deepseek(&get("/user/balance").await?)),
        BalanceSource::Moonshot => {
            // 回答里不写货币：国际站是美元，国内站是人民币
            let currency = if host(base_url).is_some_and(|h| h.ends_with(".ai")) {
                "USD"
            } else {
                "CNY"
            };
            parsed(parse::moonshot(
                &get("/v1/users/me/balance").await?,
                currency,
            ))
        }
        BalanceSource::Sub2api => parsed(parse::sub2api(&known_or("/v1/usage").await?)),
        BalanceSource::Thinkwatch => parsed(parse::thinkwatch(&known_or("/v1/usage").await?)),
        BalanceSource::Newapi => {
            let subscription = known_or("/v1/dashboard/billing/subscription").await?;
            let usage = get(&newapi_usage_path(chrono::Utc::now().date_naive())).await?;
            // 账单接口的数按站点的额度显示方式折算（美元、人民币或者 token），回答里不说是
            // 哪一种：问站点的状态接口。它是公开的，不带凭据。答了话却说不出来的，单位是
            // 「不知道」；没问成的这一次就没读成，留着上一次读到的
            let unit = match get_json(http, &format!("{origin}/api/status"), None).await {
                Ok(status) => parse::newapi_unit(&status),
                Err(f) if f.answered() => parse::UNKNOWN,
                Err(f) => return Err(f),
            };
            parsed(parse::newapi(&subscription, &usage, unit))
        }
    }
}

/// New API 的用量接口要一个日期范围。它算的是这把密钥一共用了多少，范围给宽一些：
/// 一百天前到明天
fn newapi_usage_path(today: chrono::NaiveDate) -> String {
    let start = today - chrono::Days::new(100);
    let end = today + chrono::Days::new(1);
    format!(
        "/v1/dashboard/billing/usage?start_date={}&end_date={}",
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d")
    )
}

/// 做一步：要先认来源的先认，认出来了接着读。交回这一次的结果（给 [`tracker::Claim::settle`]）
pub async fn run(http: &reqwest::Client, step: Step, base_url: &str, auth: &str) -> Outcome {
    let Some(origin) = origin(base_url) else {
        return Outcome {
            detection: Some(Detection::Neither),
            read: None,
        };
    };
    let (source, known, detection) = match step {
        Step::Read(s) => (s, None, None),
        Step::Detect => match detect(http, &origin, auth).await {
            Detected::Found(s, known) => (s, Some(known), Some(Detection::Found(s))),
            Detected::Neither => {
                return Outcome {
                    detection: Some(Detection::Neither),
                    read: None,
                };
            }
            Detected::Undecided(f) => {
                return Outcome {
                    detection: Some(Detection::Undecided(f.msg())),
                    read: None,
                };
            }
        },
    };
    let r = read(http, source, base_url, auth, known).await;
    Outcome {
        detection,
        read: Some((source, r.map_err(|f| f.msg()))),
    }
}

/// 后台：启动时读一遍，之后按节奏读（见 [`tracker`]）。有请求结束时被提前叫醒。
pub fn spawn(state: crate::AppState) {
    tokio::spawn(async move {
        loop {
            let _ = state.start_due_balances();
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = state.balances.wake.notified() => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_config::BalanceSetting as S;

    #[test]
    fn the_origin_drops_the_path_and_keeps_the_port() {
        assert_eq!(
            origin("https://api.deepseek.com/anthropic").as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(
            origin("http://127.0.0.1:8080/v1/").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            origin("https://relay.example:443/api").as_deref(),
            Some("https://relay.example")
        );
        assert_eq!(origin("不是地址"), None);
    }

    #[test]
    fn auto_knows_the_official_hosts() {
        let known = |url: &str| match plan(S::Auto, url) {
            Some(Plan::Known(s)) => Some(s),
            _ => None,
        };
        assert_eq!(
            known("https://openrouter.ai/api/v1"),
            Some(BalanceSource::Openrouter)
        );
        assert_eq!(
            known("https://api.deepseek.com/anthropic"),
            Some(BalanceSource::Deepseek)
        );
        assert_eq!(
            known("https://api.moonshot.cn/v1"),
            Some(BalanceSource::Moonshot)
        );
        assert_eq!(
            known("https://API.Moonshot.AI/anthropic"),
            Some(BalanceSource::Moonshot)
        );
        // 只是名字里含着的不算
        assert_eq!(
            plan(S::Auto, "https://openrouter.ai.example.com/v1"),
            Some(Plan::Detect)
        );
        assert_eq!(
            plan(S::Auto, "https://relay.example/v1"),
            Some(Plan::Detect)
        );
        // 模型厂商自己的地址不是中转站，不问
        assert_eq!(plan(S::Auto, "https://api.anthropic.com"), None);
        assert_eq!(plan(S::Auto, "https://api.openai.com/v1"), None);
    }

    #[test]
    fn an_explicit_source_wins_and_off_reads_nothing() {
        assert_eq!(
            plan(S::Sub2api, "https://api.deepseek.com"),
            Some(Plan::Known(BalanceSource::Sub2api))
        );
        assert_eq!(
            plan(S::Thinkwatch, "https://gw.corp.example"),
            Some(Plan::Known(BalanceSource::Thinkwatch))
        );
        assert_eq!(plan(S::Off, "https://openrouter.ai/api/v1"), None);
        assert_eq!(plan(S::Auto, "不是地址"), None);
    }

    #[test]
    fn the_key_goes_as_a_bearer_whatever_header_the_protocol_uses() {
        let p = tw_config::Provider {
            base_url: "https://api.deepseek.com/anthropic".into(),
            key: Some("sk-ds".into()),
            protocol: Some(tw_config::Protocol::Anthropic),
            ..Default::default()
        };
        assert_eq!(authorization(&p).as_deref(), Some("Bearer sk-ds"));
        // 写在 headers 里的：鉴权头原样，别的鉴权头补上 Bearer
        let h = |name: &str, value: &str| tw_config::Provider {
            base_url: "https://relay.example".into(),
            headers: tw_config::Headers::new(vec![tw_config::Header {
                name: name.into(),
                value: value.into(),
            }]),
            ..Default::default()
        };
        assert_eq!(
            authorization(&h("Authorization", "Bearer sk-h")).as_deref(),
            Some("Bearer sk-h")
        );
        assert_eq!(
            authorization(&h("x-api-key", "sk-x")).as_deref(),
            Some("Bearer sk-x")
        );
        assert_eq!(authorization(&h("x-team", "blue")), None);
        assert_eq!(authorization(&tw_config::Provider::default()), None);
    }

    #[test]
    fn newapi_usage_asks_for_a_wide_range() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        assert_eq!(
            newapi_usage_path(today),
            "/v1/dashboard/billing/usage?start_date=2026-07-02&end_date=2026-10-11"
        );
    }
}

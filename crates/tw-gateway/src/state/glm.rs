//! GLM Coding Plan 的额度：什么时候去问、问来的和 429 说的怎么记（见 [`crate::glm`]）。

use std::time::Duration;

use super::AppState;
use crate::glm::{self, Answer, Why};
use crate::server::now_ms;

/// 问一次额度接口最多等多久
const ASK_TIMEOUT: Duration = Duration::from_secs(15);

/// 429 的 body 最多读多少、等多久。**这一跳已经失败了**，读它只为认出额度用完，
/// 不能因此拖住下一家
const BODY_LIMIT: usize = 16 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(2);

impl AppState {
    /// 额度接口换成别的地址。**给测试用**：平时就是 Z.ai 和 BigModel 自己的
    pub fn set_glm_sites(&self, sites: glm::Sites) {
        self.glm.set_sites(sites);
    }

    /// 这一家是 GLM Coding Plan 的话，额度接口的地址和凭据的指纹。
    ///
    /// 没有 key 的不算：额度跟着 key 走，没有 key 就没有可问的
    fn glm_target(&self, p: &tw_config::Provider) -> Option<(String, String)> {
        let key = p.key.as_ref()?;
        let url = self.glm.sites().quota_url(&p.base_url)?;
        // 指纹，不是 key 本身：它只用来认出「换了 key」
        let ident = blake3::hash(key.raw().as_bytes()).to_hex().to_string();
        Some((url, ident))
    }

    /// 界面来要额度：该问的 GLM 上游都问一遍，**最多等 `wait`**。
    ///
    /// 等不到的照样在后台问完，结果进 `/quota` 和 `QuotaSeen`。60 秒内问过的、
    /// 退避中的、没有套餐的，这一次都不问
    pub async fn refresh_glm_quotas(&self, wait: Duration) {
        let cfg = self.config();
        let asks: Vec<_> = cfg
            .providers
            .iter()
            .filter_map(|p| self.spawn_glm_ask(p, Why::Demand))
            .collect();
        if asks.is_empty() {
            return;
        }
        let _ = tokio::time::timeout(wait, futures::future::join_all(asks)).await;
    }

    /// 有请求去了这一家：到时候了就在后台问一次额度。**不挡转发**
    pub(crate) fn glm_traffic(&self, p: &tw_config::Provider) {
        let _ = self.spawn_glm_ask(p, Why::Traffic);
    }

    /// 该问就起一个任务去问。**任务自己跑完**：等的一方放弃了，节奏也照样记上
    fn spawn_glm_ask(
        &self,
        p: &tw_config::Provider,
        why: Why,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let (url, ident) = self.glm_target(p)?;
        if !self.glm.claim(&p.name, &ident, why, now_ms()) {
            return None;
        }
        let state = self.clone();
        let p = p.clone();
        Some(tokio::spawn(async move {
            let answer = state.ask_glm(&p, &url).await;
            match &answer {
                Answer::Quota(q) => state.record_quota(state.bus.next_id(), &p.name, q.clone()),
                // 没有套餐就没有额度数据：之前记着的也不再作数
                Answer::NoPlan => state.forget_quota(&p.name),
                Answer::Rejected => {
                    tracing::warn!(provider = %p.name, "the GLM quota endpoint rejected the key")
                }
                Answer::Failed => {
                    tracing::debug!(provider = %p.name, "the GLM quota could not be read")
                }
            }
            state.glm.settle(&p.name, &ident, &answer, now_ms());
        }))
    }

    /// 问一次额度接口。**key 原样放在 `Authorization` 里，不加 `Bearer`**：那个接口就是
    /// 这么认的
    async fn ask_glm(&self, p: &tw_config::Provider, url: &str) -> Answer {
        let Some(Ok(key)) = p.key.as_ref().map(|k| k.resolve()) else {
            return Answer::Failed;
        };
        // 这一家自己的 client：走它该走的代理
        let http = self.client_for(&p.name);
        let sent = http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, key)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(ASK_TIMEOUT)
            .send()
            .await;
        let Ok(r) = sent else {
            return Answer::Failed;
        };
        let status = r.status().as_u16();
        match r.bytes().await {
            Ok(body) => glm::parse(status, &body, now_ms()),
            Err(_) => Answer::Failed,
        }
    }

    /// 一个 GLM 上游回了 429：body 里说额度用完了的话，**那一刻就记成用完**，报一次
    /// `QuotaExhausted`。
    ///
    /// 重置时刻以额度接口问来的为准；还没问到过，才用消息里说的
    pub(crate) async fn note_glm_429(
        &self,
        id: u64,
        p: &tw_config::Provider,
        r: reqwest::Response,
    ) {
        if self.glm_target(p).is_none() {
            return;
        }
        let Some(body) = read_capped(r).await else {
            return;
        };
        self.note_glm_exhausted(id, &p.name, &body);
    }

    /// 读出来的 429 body 说额度用完了的话，记下来。
    pub fn note_glm_exhausted(&self, id: u64, provider: &str, body: &[u8]) {
        let Some(hit) = glm::exhausted(body) else {
            return;
        };
        let now = now_ms();
        let mut quota = self.quotas().remove(provider).unwrap_or_default();
        let window = hit
            .window
            .map(str::to_string)
            .unwrap_or_else(|| glm::guess_window(hit.code, &quota));
        let known = quota
            .windows
            .iter()
            .find(|w| w.window == window)
            .and_then(|w| w.resets_at_ms)
            .filter(|t| *t > now);
        let resets_at_ms = known.or(hit.resets_at_ms.filter(|t| *t > now));
        tracing::info!(provider, code = hit.code, %window, "GLM says the quota is used up");
        // 用完就是用满 —— 上游说的，不是推断
        match quota.windows.iter_mut().find(|w| w.window == window) {
            Some(w) => {
                w.used_percent = 100.0;
                w.status = Some("rejected".to_string());
                w.resets_at_ms = resets_at_ms;
            }
            None => quota.windows.push(crate::quota::Window {
                window,
                used_percent: 100.0,
                resets_at_ms,
                status: Some("rejected".to_string()),
                credits: None,
            }),
        }
        self.record_quota(id, provider, quota);
    }
}

/// 读 body，最多 [`BODY_LIMIT`] 字节、[`BODY_TIMEOUT`]。读不完就算了
async fn read_capped(mut r: reqwest::Response) -> Option<Vec<u8>> {
    let read = async {
        let mut out = Vec::new();
        while let Some(chunk) = r.chunk().await.ok()? {
            out.extend_from_slice(&chunk);
            if out.len() > BODY_LIMIT {
                return None;
            }
        }
        Some(out)
    };
    tokio::time::timeout(BODY_TIMEOUT, read).await.ok()?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_config::{Client, Config, Provider};

    fn state() -> AppState {
        AppState::new(Config {
            version: 1,
            clients: vec![Client {
                name: "default".into(),
                key: "tw-good".into(),
                ..Default::default()
            }],
            providers: vec![Provider {
                name: "glm".into(),
                base_url: "https://api.z.ai/api/anthropic".into(),
                key: Some("sk-test".into()),
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap()
    }

    const FIVE_HOURS: &[u8] = br#"{"type":"error","error":{"type":"1308","message":"Usage limit reached for 5 hour. Your limit will reset at 2099-10-03 08:23:14"}}"#;

    #[tokio::test]
    async fn a_used_up_window_is_reported_once_with_the_message_reset_time() {
        let s = state();
        let mut rx = s.bus.subscribe();
        s.note_glm_exhausted(7, "glm", FIVE_HOURS);
        let q = &s.quotas()["glm"];
        assert_eq!(q.windows.len(), 1);
        let w = &q.windows[0];
        assert_eq!(w.window, "5h");
        assert!(w.rejected());
        let utc = chrono::DateTime::parse_from_rfc3339("2099-10-03T00:23:14Z").unwrap();
        assert_eq!(w.resets_at_ms, Some(utc.timestamp_millis() as u64));

        let mut exhausted = 0;
        s.note_glm_exhausted(8, "glm", FIVE_HOURS);
        while let Ok(e) = rx.try_recv() {
            if let tw_api::Event::QuotaExhausted { window, .. } = e {
                assert_eq!(window, "5h");
                exhausted += 1;
            }
        }
        assert_eq!(exhausted, 1, "同一个窗口用完只报一次");
    }

    #[tokio::test]
    async fn the_quota_endpoint_reset_time_wins_over_the_message() {
        let s = state();
        let at = now_ms() + 3_600_000;
        s.record_quota(
            1,
            "glm",
            crate::quota::Quota {
                windows: vec![crate::quota::Window {
                    window: "5h".into(),
                    used_percent: 97.0,
                    resets_at_ms: Some(at),
                    status: None,
                    credits: Some(tw_api::QuotaCredits {
                        total: 2000.0,
                        used: 1940.0,
                        remaining: 60.0,
                    }),
                }],
            },
        );
        s.note_glm_exhausted(2, "glm", FIVE_HOURS);
        let w = &s.quotas()["glm"].windows[0];
        assert_eq!(w.resets_at_ms, Some(at));
        assert_eq!(w.used_percent, 100.0);
        assert!(w.rejected());
    }

    #[tokio::test]
    async fn a_rate_limit_that_is_not_the_quota_changes_nothing() {
        let s = state();
        s.note_glm_exhausted(
            1,
            "glm",
            br#"{"type":"error","error":{"type":"1313","message":"fair use"}}"#,
        );
        assert!(s.quotas().is_empty());
    }

    #[test]
    fn only_a_glm_upstream_with_a_key_is_asked() {
        let s = state();
        let cfg = s.config();
        assert!(s.glm_target(&cfg.providers[0]).is_some());
        let mut other = cfg.providers[0].clone();
        other.base_url = "https://api.anthropic.com".into();
        assert!(s.glm_target(&other).is_none());
        let mut keyless = cfg.providers[0].clone();
        keyless.key = None;
        assert!(s.glm_target(&keyless).is_none());
    }
}

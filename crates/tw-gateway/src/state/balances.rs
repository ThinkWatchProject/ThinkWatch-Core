//! 上游的余额：读谁、什么时候读、读到的怎么告诉界面（见 [`crate::balances`]）。

use super::AppState;
use crate::balances::{self, Claim, Detection, Plan, Step, Why};
use crate::server::now_ms;

/// 一家要读余额的上游：怎么读、凭什么读。
struct Target {
    plan: Plan,
    /// 地址、凭据和 `balance:` 的指纹，**不是凭据本身**：它只用来认出「换了」
    ident: String,
    /// `Authorization` 的值
    auth: String,
}

impl AppState {
    /// 这一家读不读余额、怎么读。
    ///
    /// **不读的**：ChatGPT 账号、Z.ai / BigModel 的地址（额度另有来处，见 [`crate::quota`]、
    /// [`crate::glm`]）、Bedrock、OAuth 凭据的、没有凭据可发的、`balance: off` 的
    fn balance_target(&self, p: &tw_config::Provider) -> Option<Target> {
        if p.is_bedrock() || p.effective_protocol() == Some(tw_config::Protocol::Chatgpt) {
            return None;
        }
        if self.glm.sites().quota_url(&p.base_url).is_some() {
            return None;
        }
        let plan = balances::plan(p.balance, &p.base_url)?;
        let auth = balances::authorization(p)?;
        let ident = blake3::hash(
            format!(
                "{}\n{}\n{}",
                p.base_url.trim(),
                p.credential_identity(),
                tw_api::BalanceSetting::from(p.balance)
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        Some(Target { plan, ident, auth })
    }

    /// 配置里的这一家读不读。**停用的不读**：用户不想用它了，不该再拿它的密钥去问
    fn saved_target(&self, p: &tw_config::Provider) -> Option<Target> {
        if p.disabled {
            return None;
        }
        self.balance_target(p)
    }

    /// 这一家现在的余额（[`tw_api::ProviderView::balance`]）。没有余额可读、还没读完第一次
    /// 都是 `None`
    pub fn balance_of(&self, p: &tw_config::Provider) -> Option<tw_api::Balance> {
        let t = self.saved_target(p)?;
        self.balances.get(&p.name, &t.ident)
    }

    /// 到了时候的都去读，每家一个任务（见 [`crate::balances::tracker`]）。后台不等它们；
    /// 测试等。
    ///
    /// **配置和 client 取自同一份运行时**：分两次取，中间换了配置，就可能拿着这一份的密钥
    /// 配上另一份的 client（没有这一家的 client 时退回的默认 client 不走它的代理）
    pub fn start_due_balances(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let rt = self.runtime();
        self.balances
            .retain(|name| rt.config.providers.iter().any(|p| p.name == name));
        rt.config
            .providers
            .iter()
            .filter_map(|p| {
                let t = self.saved_target(p)?;
                let http = rt.clients.get(&p.name)?.clone();
                let claim = self
                    .balances
                    .claim(&p.name, &t.ident, t.plan, Why::Due, now_ms())?;
                let state = self.clone();
                let base_url = p.base_url.clone();
                Some(tokio::spawn(async move {
                    let _ = state.run_balance(http, base_url, t.auth, claim).await;
                }))
            })
            .collect()
    }

    /// 界面要：马上读这一家，`auto` 而没认出来的再问一次。
    ///
    /// - `Ok(Some)`：读到的（没读成的话带着原因，留着上一次读到的）；
    /// - `Ok(None)`：没有这一家，或者它没有余额可读（两种中转站都不是）；
    /// - `Err`：`auto` 的这一家**没问成**是哪一种（连不上、超时、对方出错），没问成的原因。
    ///
    /// **读在自己的任务里**：界面那头断开了（这个 future 被丢掉），这一次照样读完、记下、
    /// 报 `balance_updated`，这一家也不会一直算作「正在读」
    pub async fn refresh_balance(
        &self,
        name: &str,
    ) -> Result<Option<tw_api::Balance>, tw_types::Msg> {
        let rt = self.runtime();
        let Some(p) = rt.config.providers.iter().find(|p| p.name == name) else {
            return Ok(None);
        };
        let Some(t) = self.saved_target(p) else {
            return Ok(None);
        };
        let Some(http) = rt.clients.get(name).cloned() else {
            return Ok(None);
        };
        let Some(claim) = self
            .balances
            .claim(name, &t.ident, t.plan, Why::Demand, now_ms())
        else {
            return Ok(None);
        };
        let state = self.clone();
        let base_url = p.base_url.clone();
        let read =
            tokio::spawn(async move { state.run_balance(http, base_url, t.auth, claim).await });
        match read.await {
            Ok(r) => r,
            // 读的代码 panic 了：照原样抛出去，和在这里直接读一样
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Ok(None),
        }
    }

    /// 做一步、记下来；读过了就报一条 `balance_updated`。交回同 [`Self::refresh_balance`]；
    /// 凭据在这中间换了的话是 `Ok(None)`
    async fn run_balance(
        &self,
        http: reqwest::Client,
        base_url: String,
        auth: String,
        claim: Claim,
    ) -> Result<Option<tw_api::Balance>, tw_types::Msg> {
        let name = claim.provider().to_string();
        let outcome = balances::run(&http, claim.step(), &base_url, &auth).await;
        let read = outcome.read.is_some();
        if let Some((_, Err(why))) = &outcome.read {
            tracing::debug!(provider = name, "the balance could not be read: {why}");
        }
        let undecided = match &outcome.detection {
            Some(Detection::Undecided(why)) => {
                tracing::debug!(provider = name, "the balance source is undecided: {why}");
                Some(why.clone())
            }
            _ => None,
        };
        let now = now_ms();
        let Some(left) = claim.settle(outcome, now) else {
            return Ok(None);
        };
        if read {
            self.bus.emit(tw_api::Event::BalanceUpdated {
                id: self.bus.next_id(),
                provider: name,
                at_ms: now,
            });
        }
        match undecided {
            Some(why) => Err(why),
            None => Ok(left),
        }
    }

    /// 检测一家（可能还没保存的）上游时顺带读一次余额，`auto` 的先认是哪一种。**不记**：
    /// 表单里的还不是配置。`http` 是检测用的那个 client，和转发走同一条出站路径
    pub async fn check_balance(
        &self,
        http: &reqwest::Client,
        p: &tw_config::Provider,
    ) -> Option<tw_api::Balance> {
        let t = self.balance_target(p)?;
        let step = match t.plan {
            Plan::Known(s) => Step::Read(s),
            Plan::Detect => Step::Detect,
        };
        let (source, read) = balances::run(http, step, &p.base_url, &t.auth).await.read?;
        Some(balances::tracker::fresh(source, read, now_ms()))
    }

    /// 一个请求开始经过这一家：交回跟着它走的记号（见 [`balances::Passing`]）
    pub(crate) fn balance_passing(&self, provider: &str) -> balances::Passing {
        balances::Passing::new(self.balances.clone(), provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_config::{BalanceSetting, Config, Provider};

    fn state(providers: Vec<Provider>) -> AppState {
        AppState::new(Config {
            version: 1,
            clients: vec![tw_config::Client {
                name: "default".into(),
                key: "tw-good".into(),
                ..Default::default()
            }],
            providers,
            ..Default::default()
        })
        .unwrap()
    }

    fn up(name: &str, base_url: &str) -> Provider {
        Provider {
            name: name.into(),
            base_url: base_url.into(),
            key: Some("sk-test".into()),
            ..Default::default()
        }
    }

    /// 账号上游、Bedrock、Z.ai 的地址、OAuth、没有凭据的、停用的、关掉的：都不读
    #[test]
    fn who_has_a_balance_to_read() {
        let s = state(vec![]);
        let reads = |p: Provider| s.saved_target(&p).is_some();
        assert!(reads(up("or", "https://openrouter.ai/api/v1")));
        assert!(reads(up("relay", "https://relay.example/v1")));
        assert!(!reads(up(
            "chatgpt",
            "https://chatgpt.com/backend-api/codex"
        )));
        assert!(!reads(up(
            "bedrock",
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        )));
        assert!(!reads(up("glm", "https://api.z.ai/api/anthropic")));
        assert!(!reads(up(
            "bigmodel",
            "https://open.bigmodel.cn/api/anthropic"
        )));
        // 写明了也不读：它们的额度另有来处
        assert!(!reads(Provider {
            balance: BalanceSetting::Sub2api,
            ..up("glm", "https://api.z.ai/api/anthropic")
        }));
        assert!(!reads(Provider {
            key: None,
            ..up("ollama", "http://127.0.0.1:11434")
        }));
        assert!(!reads(Provider {
            disabled: true,
            ..up("or", "https://openrouter.ai/api/v1")
        }));
        assert!(!reads(Provider {
            balance: BalanceSetting::Off,
            ..up("or", "https://openrouter.ai/api/v1")
        }));
        assert!(!reads(Provider {
            key: None,
            oauth: Some(tw_config::OAuth {
                access: None,
                expires_at: None,
                refresh: "rt".into(),
                endpoint: "https://auth.example/token".into(),
                client_id: None,
                client_secret: None,
                refresh_before: None,
            }),
            ..up("corp", "https://llm.corp.example")
        }));
    }

    #[test]
    fn a_new_key_or_setting_is_a_new_ident() {
        let s = state(vec![]);
        let ident = |p: &Provider| s.balance_target(p).unwrap().ident;
        let a = up("relay", "https://relay.example/v1");
        let b = Provider {
            key: Some("sk-other".into()),
            ..a.clone()
        };
        let c = Provider {
            balance: BalanceSetting::Newapi,
            ..a.clone()
        };
        assert_ne!(ident(&a), ident(&b));
        assert_ne!(ident(&a), ident(&c));
        assert_eq!(ident(&a), ident(&a.clone()));
        assert!(!ident(&a).contains("sk-test"), "指纹不是凭据本身");
    }

    /// 请求结束时只做记号：从没读过的上游什么都不发生，读过的到了时候才读
    #[tokio::test]
    async fn a_request_ending_only_marks_the_upstream() {
        let s = state(vec![up("relay", "https://relay.example/v1")]);
        drop(s.balance_passing("relay"));
        assert!(s.balances.get("relay", "").is_none());
    }
}

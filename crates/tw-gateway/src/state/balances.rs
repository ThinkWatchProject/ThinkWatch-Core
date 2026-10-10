//! 上游的余额：读谁、什么时候读、读到的怎么告诉界面（见 [`crate::balances`]）。

use super::AppState;
use crate::balances::{self, Plan, Step, Why};
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
    /// 测试等
    pub fn start_due_balances(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let cfg = self.config();
        self.balances
            .retain(|name| cfg.providers.iter().any(|p| p.name == name));
        cfg.providers
            .iter()
            .filter_map(|p| {
                let t = self.saved_target(p)?;
                let step = self
                    .balances
                    .claim(&p.name, &t.ident, t.plan, Why::Due, now_ms())?;
                let state = self.clone();
                let (name, base_url) = (p.name.clone(), p.base_url.clone());
                Some(tokio::spawn(async move {
                    state.run_balance(&name, &base_url, t, step).await;
                }))
            })
            .collect()
    }

    /// 界面要：马上读这一家，`auto` 而没认出来的再问一次。`None`：没有这一家，或者它
    /// 没有余额可读
    pub async fn refresh_balance(&self, name: &str) -> Option<tw_api::Balance> {
        let cfg = self.config();
        let p = cfg.providers.iter().find(|p| p.name == name)?;
        let t = self.saved_target(p)?;
        let step = self
            .balances
            .claim(name, &t.ident, t.plan, Why::Demand, now_ms())?;
        self.run_balance(name, &p.base_url, t, step).await.flatten()
    }

    /// 做一步、记下来；读过了就报一条 `balance_updated`。交回这一家现在的余额；凭据在这
    /// 中间换了的话是 `None`
    async fn run_balance(
        &self,
        name: &str,
        base_url: &str,
        t: Target,
        step: Step,
    ) -> Option<Option<tw_api::Balance>> {
        // 这一家自己的 client：走它该走的代理
        let http = self.client_for(name);
        let outcome = balances::run(&http, step, base_url, &t.auth).await;
        let read = outcome.read.is_some();
        if let Some((_, Err(why))) = &outcome.read {
            tracing::debug!(provider = name, "the balance could not be read: {why}");
        }
        let now = now_ms();
        let left = self.balances.settle(name, &t.ident, outcome, now)?;
        if read {
            self.bus.emit(tw_api::Event::BalanceUpdated {
                id: self.bus.next_id(),
                provider: name.to_string(),
                at_ms: now,
            });
        }
        Some(left)
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

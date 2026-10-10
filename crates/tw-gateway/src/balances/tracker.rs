//! 什么时候去读余额，和读到的记在哪儿。**只算时间，不联网**：测试把时刻传进来。
//!
//! 读的节奏：
//! - **启动时读一次**（还没读过的就该读）；
//! - **有请求经过这一家、结束之后**，离上一次读满 [`AFTER_REQUEST_MS`] 就再读：余额是
//!   请求结束时扣的，一阵连着的请求只在最后多读一次，不是每个请求读一次；
//! - 没有请求时每 [`EVERY_MS`] 读一次：别处（网页上、别的设备）花掉的也要看得见；
//! - 界面要的时候马上读（[`Why::Demand`]），不看节奏。
//!
//! `auto` 而地址不认识的，先问它是哪一种中转站（见 [`super::detect`]）。**只问一次**：
//! 认出来了就记住，两种都不是就不再问（换了地址、凭据或者 `balance:` 才从头来）；
//! 连不上、对方出错算没问成，隔 [`EVERY_MS`] 再问，**不跟着请求问**。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use tw_api::{Balance, BalanceSource, Msg};

use super::parse::Reading;

/// 请求结束之后，离上一次读至少隔这么久才再读
pub const AFTER_REQUEST_MS: u64 = 60_000;
/// 没有请求时隔这么久读一次；没问成的检测也隔这么久再问
pub const EVERY_MS: u64 = 10 * 60_000;

/// 这一家的余额从哪儿来：配置和地址就定了的，还是要先问一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Known(BalanceSource),
    Detect,
}

/// 问「是哪一种中转站」的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    Found(BalanceSource),
    /// 两种都不是：这一家没有余额可读
    Neither,
    /// 没问成：连不上、超时、对方出错
    Undecided,
}

/// 为什么去读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// 后台的节奏：启动、请求结束之后、定时
    Due,
    /// 界面要：马上读，不看节奏
    Demand,
}

/// 这一次要做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Read(BalanceSource),
    /// 先问是哪一种中转站，认出来了接着读
    Detect,
}

/// 一次读完的结果，交给 [`Tracker::settle`]。
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// 问过「是哪一种」的话，问出来的
    pub detection: Option<Detection>,
    /// 读过的话：从哪儿读的、读到的或者没读成的原因
    pub read: Option<(BalanceSource, Result<Reading, Msg>)>,
}

/// 一次读的结果写成 [`Balance`]：读成了是读到的，没读成是空的加上原因。
pub fn fresh(source: BalanceSource, read: Result<Reading, Msg>, now_ms: u64) -> Balance {
    let (r, error) = match read {
        Ok(r) => (r, None),
        Err(why) => (Reading::default(), Some(why)),
    };
    Balance {
        source,
        read_at_ms: now_ms,
        wallet: r.wallet,
        quota: r.quota,
        windows: r.windows,
        expires_at_ms: r.expires_at_ms,
        spent: r.spent,
        error,
    }
}

/// 一家上游。
#[derive(Debug, Default)]
struct Slot {
    /// 地址、凭据和 `balance:` 的指纹。**变了就从头来**：之前读到的、问出来的说的是旧的
    ident: String,
    detection: Option<Detection>,
    /// 最近一次读或者问完的时刻，不论结果
    tried_at: Option<u64>,
    /// 上一次开始读之后，有请求经过这一家结束了
    dirty: bool,
    /// 正在读的有几个。后台的节奏只在一个都没有时才读
    running: u32,
    balance: Option<Balance>,
}

impl Slot {
    /// 这一家读的是哪儿：配置定了的，或者问出来的
    fn source(&self, plan: Plan) -> Option<BalanceSource> {
        match (plan, self.detection) {
            (Plan::Known(s), _) | (Plan::Detect, Some(Detection::Found(s))) => Some(s),
            _ => None,
        }
    }

    /// 按后台的节奏，现在该读（或者该问）了吗
    fn due(&self, plan: Plan, now_ms: u64) -> bool {
        if self.running > 0 {
            return false;
        }
        let Some(at) = self.tried_at else {
            return true;
        };
        let since = now_ms.saturating_sub(at);
        if self.source(plan).is_some() {
            return (self.dirty && since >= AFTER_REQUEST_MS) || since >= EVERY_MS;
        }
        match self.detection {
            None => true,
            // 没问成的隔一阵再问；**不看请求**，问一次就是两个请求
            Some(Detection::Undecided) => since >= EVERY_MS,
            Some(Detection::Neither | Detection::Found(_)) => false,
        }
    }
}

/// 每家上游的余额和读的节奏。**跨重载存活**：改一条路由规则不该让所有余额重读一遍。
#[derive(Default)]
pub struct Tracker {
    slots: Mutex<HashMap<String, Slot>>,
    /// 有请求经过的上游结束了：叫醒后台（见 [`super::spawn`]），到了时候就读
    pub wake: tokio::sync::Notify,
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        // 锁中毒了照样用里面的：余额晚一点更新，好过不再更新
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 这一家现在要读吗、读什么。要读就占上，读完必须 [`Tracker::settle`]。
    ///
    /// `Demand` 不看节奏：认出过的直接读，没认出来的（两种都不是、没问成）再问一次
    pub fn claim(
        &self,
        provider: &str,
        ident: &str,
        plan: Plan,
        why: Why,
        now_ms: u64,
    ) -> Option<Step> {
        let mut g = self.lock();
        let slot = g.entry(provider.to_string()).or_default();
        if slot.ident != ident {
            *slot = Slot {
                ident: ident.to_string(),
                ..Default::default()
            };
        }
        if why == Why::Due && !slot.due(plan, now_ms) {
            return None;
        }
        slot.running += 1;
        // 这一次读到的已经算上了到此刻为止结束的请求
        slot.dirty = false;
        Some(match slot.source(plan) {
            Some(s) => Step::Read(s),
            None => Step::Detect,
        })
    }

    /// 读完了：记下节奏和读到的，交回这一家现在的余额。**凭据在这中间换了的话**，这个结果
    /// 说的是旧的那一份，不记，是 `None`。
    ///
    /// 读成了的整份换上；没读成的**留着上一次读到的**，只记下原因 —— 一次超时不该让界面上
    /// 的余额消失。之前读的是别的来源（问出来的变了）就不留
    pub fn settle(
        &self,
        provider: &str,
        ident: &str,
        outcome: Outcome,
        now_ms: u64,
    ) -> Option<Option<Balance>> {
        let mut g = self.lock();
        let slot = g.get_mut(provider).filter(|s| s.ident == ident)?;
        slot.running = slot.running.saturating_sub(1);
        slot.tried_at = Some(now_ms);
        if let Some(d) = outcome.detection {
            // 认出来过的不因为一次没问成就忘掉
            if !matches!(slot.detection, Some(Detection::Found(_))) || d != Detection::Undecided {
                slot.detection = Some(d);
            }
            if !matches!(d, Detection::Found(_)) && outcome.read.is_none() {
                slot.balance = None;
            }
        }
        if let Some((source, read)) = outcome.read {
            let before = slot.balance.take().filter(|b| b.source == source);
            slot.balance = Some(match (read, before) {
                (Err(why), Some(b)) => Balance {
                    error: Some(why),
                    ..b
                },
                (read, _) => fresh(source, read, now_ms),
            });
        }
        Some(slot.balance.clone())
    }

    /// 有请求经过这一家结束了。**只做个记号**：读不读、什么时候读由节奏定
    pub fn note_request(&self, provider: &str) {
        let known = match self.lock().get_mut(provider) {
            Some(slot) => {
                slot.dirty = true;
                true
            }
            None => false,
        };
        if known {
            self.wake.notify_one();
        }
    }

    /// 这一家现在的余额。凭据、地址或者 `balance:` 变过的话，记着的那份不作数
    pub fn get(&self, provider: &str, ident: &str) -> Option<Balance> {
        self.lock()
            .get(provider)
            .filter(|s| s.ident == ident)
            .and_then(|s| s.balance.clone())
    }

    /// 配置里没有了的上游：忘掉
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.lock().retain(|name, _| keep(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_api::Money;
    use tw_types::msg;

    const NOW: u64 = 1_790_000_000_000;
    const DS: Plan = Plan::Known(BalanceSource::Deepseek);

    fn wallet(amount: f64) -> Reading {
        Reading {
            wallet: Some(Money {
                amount,
                currency: "CNY".into(),
            }),
            ..Default::default()
        }
    }

    fn read(r: Result<Reading, Msg>) -> Outcome {
        Outcome {
            detection: None,
            read: Some((BalanceSource::Deepseek, r)),
        }
    }

    fn failed() -> Msg {
        msg!("gw.balance.status", status = 502 => "x")
    }

    /// 读一次：占上、读到 `amount`、记下
    fn read_at(t: &Tracker, now: u64, amount: f64) {
        assert_eq!(
            t.claim("ds", "k", DS, Why::Due, now),
            Some(Step::Read(BalanceSource::Deepseek))
        );
        t.settle("ds", "k", read(Ok(wallet(amount))), now);
    }

    #[test]
    fn it_reads_at_start_and_then_every_ten_minutes() {
        let t = Tracker::default();
        read_at(&t, NOW, 10.0);
        assert_eq!(t.claim("ds", "k", DS, Why::Due, NOW + EVERY_MS - 1), None);
        assert!(t.claim("ds", "k", DS, Why::Due, NOW + EVERY_MS).is_some());
    }

    /// 请求结束之后最多每 60 秒读一次：一阵连着的请求，在最后补读一次
    #[test]
    fn requests_ending_trigger_a_read_at_most_every_minute() {
        let t = Tracker::default();
        read_at(&t, NOW, 10.0);
        t.note_request("ds");
        // 刚读过：一分钟之内不读
        assert_eq!(t.claim("ds", "k", DS, Why::Due, NOW + 10_000), None);
        assert_eq!(t.claim("ds", "k", DS, Why::Due, NOW + 59_999), None);
        read_at(&t, NOW + AFTER_REQUEST_MS, 9.0);
        // 这之后没有请求：等满十分钟
        assert_eq!(
            t.claim("ds", "k", DS, Why::Due, NOW + 3 * AFTER_REQUEST_MS),
            None
        );
        // 又有请求结束了：离上一次读已经过了一分钟，马上读
        t.note_request("ds");
        assert!(
            t.claim("ds", "k", DS, Why::Due, NOW + 3 * AFTER_REQUEST_MS)
                .is_some()
        );
    }

    #[test]
    fn a_request_ending_while_reading_counts_for_the_next_read() {
        let t = Tracker::default();
        assert!(t.claim("ds", "k", DS, Why::Due, NOW).is_some());
        // 正在读：不另读
        assert_eq!(t.claim("ds", "k", DS, Why::Due, NOW + 1), None);
        t.note_request("ds");
        t.settle("ds", "k", read(Ok(wallet(1.0))), NOW + 500);
        assert!(
            t.claim("ds", "k", DS, Why::Due, NOW + 500 + AFTER_REQUEST_MS)
                .is_some()
        );
    }

    #[test]
    fn a_demand_reads_now_whatever_the_rhythm() {
        let t = Tracker::default();
        read_at(&t, NOW, 10.0);
        assert!(t.claim("ds", "k", DS, Why::Demand, NOW + 1).is_some());
    }

    /// 读失败了：**留着上一次读到的**，只记下原因；之后读成了，原因清掉
    #[test]
    fn a_failed_read_keeps_the_last_good_reading_and_says_why() {
        let t = Tracker::default();
        read_at(&t, NOW, 10.0);
        assert!(t.claim("ds", "k", DS, Why::Demand, NOW + 5).is_some());
        let b = t
            .settle("ds", "k", read(Err(failed())), NOW + 5)
            .unwrap()
            .unwrap();
        assert_eq!(b.wallet.as_ref().unwrap().amount, 10.0);
        assert_eq!(b.read_at_ms, NOW, "是那一次读到的时刻");
        assert_eq!(b.error.as_ref().unwrap().code, "gw.balance.status");

        assert!(t.claim("ds", "k", DS, Why::Demand, NOW + 9).is_some());
        let b = t
            .settle("ds", "k", read(Ok(wallet(8.0))), NOW + 9)
            .unwrap()
            .unwrap();
        assert_eq!(b.wallet.unwrap().amount, 8.0);
        assert_eq!(b.error, None);
    }

    #[test]
    fn a_first_read_that_fails_is_an_empty_balance_with_the_reason() {
        let t = Tracker::default();
        assert!(t.claim("ds", "k", DS, Why::Due, NOW).is_some());
        let b = t
            .settle("ds", "k", read(Err(failed())), NOW)
            .unwrap()
            .unwrap();
        assert_eq!(b.wallet, None);
        assert_eq!(b.read_at_ms, NOW);
        assert!(b.error.is_some());
        // 失败了也按节奏来，不重试个不停
        assert_eq!(t.claim("ds", "k", DS, Why::Due, NOW + 30_000), None);
    }

    #[test]
    fn a_new_key_starts_over() {
        let t = Tracker::default();
        read_at(&t, NOW, 10.0);
        assert!(t.get("ds", "k").is_some());
        assert_eq!(t.get("ds", "k2"), None, "换了凭据，旧的余额不作数");
        assert!(t.claim("ds", "k2", DS, Why::Due, NOW + 1).is_some());
        // 旧 key 的那一次晚到了：不记
        assert_eq!(t.settle("ds", "k", read(Ok(wallet(1.0))), NOW + 2), None);
    }

    #[test]
    fn detection_is_asked_once_and_remembered() {
        let t = Tracker::default();
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Due, NOW),
            Some(Step::Detect)
        );
        t.settle(
            "r",
            "k",
            Outcome {
                detection: Some(Detection::Found(BalanceSource::Sub2api)),
                read: Some((BalanceSource::Sub2api, Ok(Reading::default()))),
            },
            NOW,
        );
        // 认出来了：之后直接读
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Due, NOW + EVERY_MS),
            Some(Step::Read(BalanceSource::Sub2api))
        );
    }

    /// 两种都不是：**不再问**，请求再多也不问；只有界面要的时候再问一次
    #[test]
    fn a_host_that_is_neither_is_not_asked_again() {
        let t = Tracker::default();
        assert!(t.claim("r", "k", Plan::Detect, Why::Due, NOW).is_some());
        let left = t.settle(
            "r",
            "k",
            Outcome {
                detection: Some(Detection::Neither),
                read: None,
            },
            NOW,
        );
        assert_eq!(left, Some(None), "没有余额");
        for i in 1..20 {
            t.note_request("r");
            assert_eq!(
                t.claim("r", "k", Plan::Detect, Why::Due, NOW + i * EVERY_MS),
                None
            );
        }
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Demand, NOW + 1),
            Some(Step::Detect)
        );
    }

    /// 没问成：隔十分钟再问，**不跟着请求问**
    #[test]
    fn an_undecided_detection_waits_ten_minutes_whatever_the_traffic() {
        let t = Tracker::default();
        assert!(t.claim("r", "k", Plan::Detect, Why::Due, NOW).is_some());
        t.settle(
            "r",
            "k",
            Outcome {
                detection: Some(Detection::Undecided),
                read: None,
            },
            NOW,
        );
        t.note_request("r");
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Due, NOW + AFTER_REQUEST_MS),
            None
        );
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Due, NOW + EVERY_MS),
            Some(Step::Detect)
        );
    }

    #[test]
    fn a_found_source_is_not_forgotten_because_one_detection_failed() {
        let t = Tracker::default();
        assert!(t.claim("r", "k", Plan::Detect, Why::Due, NOW).is_some());
        t.settle(
            "r",
            "k",
            Outcome {
                detection: Some(Detection::Found(BalanceSource::Newapi)),
                read: Some((BalanceSource::Newapi, Ok(Reading::default()))),
            },
            NOW,
        );
        assert!(
            t.claim("r", "k", Plan::Detect, Why::Demand, NOW + 1)
                .is_some()
        );
        t.settle(
            "r",
            "k",
            Outcome {
                detection: Some(Detection::Undecided),
                read: None,
            },
            NOW + 2,
        );
        assert_eq!(
            t.claim("r", "k", Plan::Detect, Why::Demand, NOW + 3),
            Some(Step::Read(BalanceSource::Newapi))
        );
    }

    #[test]
    fn a_request_on_an_upstream_never_read_changes_nothing() {
        let t = Tracker::default();
        t.note_request("nobody");
        assert_eq!(t.get("nobody", ""), None);
    }

    #[test]
    fn upstreams_no_longer_configured_are_forgotten() {
        let t = Tracker::default();
        read_at(&t, NOW, 1.0);
        t.retain(|name| name != "ds");
        assert_eq!(t.get("ds", "k"), None);
    }
}

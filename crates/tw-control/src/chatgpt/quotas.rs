//! Account quotas are shared Core state, independent of which desktop page is open.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tw_config::{Protocol, Provider};

use crate::{ControlState, Fail};

// Real traffic supplies quota headers. Query only to fill a cold/stale cache or after a reset.
const REFRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_secs(5);

type Answer = Result<tw_api::ChatgptUsage, Fail>;

#[derive(Default)]
struct Reading {
    answer: Option<Answer>,
    finished: Option<Instant>,
    next: Option<Instant>,
    failures: u32,
}

impl Reading {
    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|at| now >= at)
    }

    fn finish(&mut self, answer: Answer, now: Instant) {
        self.failures = if answer.is_ok() {
            0
        } else {
            self.failures.saturating_add(1)
        };
        let delay = match &answer {
            Ok(usage) => refresh_after(&usage.windows, super::now_ms()),
            Err(_) => retry_after(self.failures),
        };
        self.next = Some(now + delay);
        self.finished = Some(now);
        self.answer = Some(answer);
    }

    fn observe(&mut self, windows: &[tw_api::QuotaWindow], now: Instant) {
        self.failures = 0;
        self.next = Some(now + refresh_after(windows, super::now_ms()));
    }
}

fn retry_after(failures: u32) -> Duration {
    RETRY
        .saturating_mul(1 << failures.saturating_sub(1).min(4))
        .min(REFRESH)
}

fn refresh_after(windows: &[tw_api::QuotaWindow], now_ms: u64) -> Duration {
    windows
        .iter()
        .filter_map(|w| w.resets_at_ms)
        .filter(|&at| at > now_ms)
        // Let the upstream finish resetting; never automatically query twice within a minute.
        .map(|at| Duration::from_millis(at - now_ms).saturating_add(TICK))
        .min()
        .unwrap_or(REFRESH)
        .clamp(RETRY, REFRESH)
}

struct Slot {
    identity: (String, String),
    reading: Arc<tokio::sync::Mutex<Reading>>,
}

pub(super) struct Cache {
    slots: Mutex<HashMap<String, Slot>>,
    limit: Semaphore,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            limit: Semaphore::new(4),
        }
    }
}

fn identity(p: &Provider) -> (String, String) {
    (p.base_url.clone(), p.credential_identity())
}

impl Cache {
    fn slot(&self, p: &Provider) -> Arc<tokio::sync::Mutex<Reading>> {
        let mut slots = self.slots.lock().expect("quota cache lock poisoned");
        let id = identity(p);
        let slot = slots.entry(p.name.clone()).or_insert_with(|| Slot {
            identity: id.clone(),
            reading: Default::default(),
        });
        if slot.identity != id {
            *slot = Slot {
                identity: id,
                reading: Default::default(),
            };
        }
        slot.reading.clone()
    }

    fn observe(&self, p: &Provider, windows: &[tw_api::QuotaWindow]) {
        // Do not assign queued events from a deleted/replaced account to its replacement.
        let reading = self
            .slots
            .lock()
            .expect("quota cache lock poisoned")
            .get(&p.name)
            .filter(|slot| slot.identity == identity(p))
            .map(|slot| slot.reading.clone());
        if let Some(reading) = reading
            && let Ok(mut reading) = reading.try_lock()
        {
            reading.observe(windows, Instant::now());
        }
        // A read holding the slot will set its own deadline on completion.
    }
}

fn accounts(s: &ControlState) -> Vec<Provider> {
    s.config()
        .providers
        .iter()
        .filter(|p| {
            !p.disabled
                && p.effective_protocol() == Some(Protocol::Chatgpt)
                && !p.oauth.as_ref().is_some_and(|oauth| {
                    s.gateway
                        .oauth
                        .failure(&p.name, oauth)
                        .is_some_and(|(_, relogin)| relogin)
                })
        })
        .cloned()
        .collect()
}

/// Poll without a desktop window. Dropping/aborting this task also cancels its reads.
/// It must run on the gateway runtime, whose HTTP connection pools it shares.
pub fn spawn(s: ControlState) -> tokio::task::JoinHandle<()> {
    let mut events = s.bus().subscribe();
    tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            for (p, reading) in due(&s) {
                let state = s.clone();
                tasks.spawn(async move { read(&state, &p, reading).await });
            }
            tokio::select! {
                _ = tick.tick() => {},
                _ = tasks.join_next(), if !tasks.is_empty() => {},
                event = async {
                    loop {
                        match events.recv().await {
                            Ok(event @ (tw_api::Event::QuotaSeen { .. } | tw_api::Event::ConfigReloaded { .. })) => break Some(event),
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break None,
                            _ => {},
                        }
                    }
                } => {
                    let Some(event) = event else { return };
                    if let tw_api::Event::QuotaSeen { provider, windows, .. } = event
                        && let Some(p) = s.config().providers.iter().find(|p| p.name == provider)
                    {
                        s.chatgpt.quotas.observe(p, &windows);
                    }
                },
            }
        }
    })
}

fn due(s: &ControlState) -> Vec<(Provider, OwnedMutexGuard<Reading>)> {
    let providers = accounts(s);
    s.chatgpt
        .quotas
        .slots
        .lock()
        .expect("quota cache lock poisoned")
        .retain(|name, _| providers.iter().any(|p| &p.name == name));
    providers
        .into_iter()
        .filter_map(|p| {
            let reading = s.chatgpt.quotas.slot(&p).try_lock_owned().ok()?;
            reading.due(Instant::now()).then_some((p, reading))
        })
        .collect()
}

/// All quota readers, including overview and the menu bar, can warm a cold cache.
/// A caller timing out leaves the bounded reads running; QuotaSeen delivers the result.
pub async fn refresh(s: &ControlState, wait: Duration) {
    let tasks: Vec<_> = due(s)
        .into_iter()
        .map(|(p, reading)| {
            let state = s.clone();
            tokio::spawn(async move { read(&state, &p, reading).await })
        })
        .collect();
    let _ = tokio::time::timeout(wait, futures::future::join_all(tasks)).await;
}

/// Explicit account details share the same in-flight read, including failures.
pub(super) async fn usage(s: &ControlState, name: &str) -> Answer {
    let p = s
        .config()
        .providers
        .iter()
        .find(|p| p.name == name)
        .cloned()
        .ok_or_else(|| crate::no_such_upstream(name))?;
    let started = Instant::now();
    let reading = s.chatgpt.quotas.slot(&p).lock_owned().await;
    if reading.finished.is_some_and(|at| at >= started) {
        return reading
            .answer
            .clone()
            .expect("finished quota read has an answer");
    }
    read(s, &p, reading).await
}

async fn read(s: &ControlState, p: &Provider, mut reading: OwnedMutexGuard<Reading>) -> Answer {
    let _permit = s
        .chatgpt
        .quotas
        .limit
        .acquire()
        .await
        .expect("quota semaphore closed");
    let answer = super::read_usage(s, &p.name).await;
    // A disabled, deleted or replaced account must not be overwritten by an old answer.
    let current = s.config().providers.iter().any(|now| {
        now.name == p.name
            && now.disabled == p.disabled
            && now.effective_protocol() == p.effective_protocol()
            && identity(now) == identity(p)
    });
    if !current {
        return Err(crate::no_such_upstream(&p.name));
    }
    if let Ok(usage) = &answer {
        super::record_usage(s, &p.name, usage);
    }
    reading.finish(answer.clone(), Instant::now());
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshes_stale_quotas_and_backs_off_failures() {
        let now = Instant::now();
        let mut r = Reading::default();
        assert!(r.due(now));
        let ok = tw_api::ChatgptUsage {
            windows: vec![],
            reset_credits: None,
        };
        r.finish(Ok(ok.clone()), now);
        assert!(!r.due(now + REFRESH - Duration::from_millis(1)));
        assert!(r.due(now + REFRESH));
        assert_eq!(retry_after(1), RETRY);
        assert_eq!(retry_after(2), Duration::from_secs(120));
        assert_eq!(retry_after(100), Duration::from_secs(900));
        r.failures = 4;
        r.finish(Ok(ok), now);
        assert_eq!(r.failures, 0);
        assert_eq!(r.next, Some(now + REFRESH));
    }

    #[test]
    fn fresh_traffic_postpones_queries_and_resets_schedule_one_earlier_read() {
        let now = Instant::now();
        let mut r = Reading {
            failures: 3,
            ..Default::default()
        };
        r.observe(&[], now);
        assert_eq!(r.failures, 0);
        assert_eq!(r.next, Some(now + REFRESH));
        r.observe(&[], now + Duration::from_secs(800));
        assert!(!r.due(now + REFRESH));

        let wall = 1_000_000;
        let window = |at| tw_api::QuotaWindow {
            window: "5h".into(),
            used_percent: 99.0,
            resets_at_ms: Some(at),
            status: None,
            credits: None,
        };
        assert_eq!(
            refresh_after(&[window(wall + 120_000)], wall),
            Duration::from_secs(125)
        );
        assert_eq!(refresh_after(&[window(wall + 1_000)], wall), RETRY);
        assert_eq!(refresh_after(&[window(wall - 1)], wall), REFRESH);
        assert_eq!(refresh_after(&[window(wall + 3_600_000)], wall), REFRESH);
    }
}

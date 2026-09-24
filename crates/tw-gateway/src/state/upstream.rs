//! 上游此刻的样子：凭据被不被拒、代理通不通、订阅额度还剩多少。
//!
//! **概览要给现状**：这些都只在变化时发事件，界面晚打开就错过了，所以同时
//! 记一份在内存里给查询用。

use super::AppState;
use crate::server::now_ms;

/// 这个出站设置是配置里定义的代理吗。`direct` 和 `system` 不是：
/// 前者没有代理，后者的地址在 reqwest 建连时才去环境里查，我们没有它可以握手
fn named_proxy(proxy: &str) -> bool {
    !proxy.is_empty() && proxy != tw_config::DIRECT && proxy != tw_config::SYSTEM
}

/// 一个代理最近一次检查的结果。
///
/// **只在转发失败之后才检查**，而且同一个代理隔一会儿才检一次 —— 一条打不通的
/// 链路上每个请求都去检一遍，等于把一次故障放大成一串握手。
pub(super) struct ProxyState {
    reachable: bool,
    checked_at: std::time::Instant,
    /// 正在检查。并发的失败只触发一次
    checking: bool,
    /// 不通时检出来的样子。**概览要给现状**：只在变化时报的事件，界面晚打开
    /// 就错过了
    fault: Option<tw_api::ProxyFault>,
}

/// 同一个代理两次检查之间至少隔多久
const PROXY_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

impl AppState {
    /// 上游对凭据的态度变了没有。**进入被拒和恢复各报一次**。
    ///
    /// 熔断器看不见这件事：4xx 不算失败（换一家也一样被拒），所以一个凭据坏掉的
    /// 上游永远不会被熔断，也就永远不会有 `HealthChanged`。而这件事要用户去改配置。
    pub(crate) fn note_auth(&self, provider: &str, status: u16) {
        let rejected = status == 401 || status == 403;
        // 别的失败（500、429、超时）什么都不说明：凭据可能好好的
        if !rejected && !(200..300).contains(&status) {
            return;
        }
        let changed = self
            .rejected
            .lock()
            .map(|mut g| {
                if rejected {
                    g.insert(provider.to_string(), status).is_none()
                } else {
                    g.remove(provider).is_some()
                }
            })
            .unwrap_or(false);
        if !changed {
            return;
        }
        if rejected {
            tracing::warn!(provider, status, "the upstream rejected the credential");
        }
        self.bus.emit(tw_api::Event::AuthChanged {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            state: if rejected { "rejected" } else { "accepted" }.into(),
            status: rejected.then_some(status),
            at_ms: now_ms(),
        });
    }

    /// 经这个代理的请求成功了：它之前要是被判成不通，现在说一声通了。
    pub(crate) fn note_proxy_ok(&self, proxy: &str) {
        if !named_proxy(proxy) {
            return;
        }
        let recovered = self
            .proxies
            .lock()
            .map(|mut g| match g.get_mut(proxy) {
                Some(st) if !st.reachable => {
                    st.reachable = true;
                    st.checked_at = std::time::Instant::now();
                    st.fault = None;
                    true
                }
                _ => false,
            })
            .unwrap_or(false);
        if recovered {
            self.bus.emit(tw_api::Event::ProxyChanged {
                id: self.bus.next_id(),
                proxy: proxy.to_string(),
                state: "reachable".into(),
                failed: None,
                detail: None,
                at_ms: now_ms(),
            });
        }
    }

    /// 经这个代理的请求连不上了：检一次代理本身。
    ///
    /// **不做定时探测**，只在转发失败之后顺手检一次 —— 没有它，代理挂掉在界面上
    /// 看起来是「好几家上游同时不通」，而那两件事要做的处理完全不同。
    pub(crate) fn check_proxy(&self, proxy: &str) {
        if !named_proxy(proxy) {
            return;
        }
        let due = self
            .proxies
            .lock()
            .map(|mut g| {
                let st = g.entry(proxy.to_string()).or_insert(ProxyState {
                    reachable: true,
                    // 第一次就该检：把时间放到足够早
                    checked_at: std::time::Instant::now() - PROXY_RECHECK,
                    checking: false,
                    fault: None,
                });
                let due = !st.checking && st.checked_at.elapsed() >= PROXY_RECHECK;
                if due {
                    st.checking = true;
                }
                due
            })
            .unwrap_or(false);
        if !due {
            return;
        }
        let state = self.clone();
        let name = proxy.to_string();
        tokio::spawn(async move {
            let cfg = state.config();
            // 卡在哪一步 + 为什么。**两个都要**：一句「TCP 握手失败」说不出
            // 是地址错了还是代理没起来。两样分开发，界面自己组句
            let result: Option<(Option<crate::l1::Stage>, tw_types::Msg)> =
                match cfg.proxies.iter().find(|p| p.name == name) {
                    Some(px) => match crate::l1::hop_of(px) {
                        Ok(hop) => {
                            let (host, port) = crate::l1::proxy_target(&cfg, &name);
                            let r = crate::l1::l1_proxy(&hop, &host, port).await;
                            if r.ok {
                                None
                            } else {
                                Some((
                                    r.failed,
                                    r.error.unwrap_or_else(|| {
                                        tw_types::msg!(
                                            "l1.unreachable" => "The proxy could not be reached."
                                        )
                                    }),
                                ))
                            }
                        }
                        Err(e) => Some((
                            Some(crate::l1::Stage {
                                step: crate::l1::Step::Config,
                                peer: crate::l1::Peer::Proxy,
                            }),
                            e,
                        )),
                    },
                    // 配置刚好在这中间改了，代理没了：不报
                    None => None,
                };
            let fault = result.as_ref().map(|(stage, why)| tw_api::ProxyFault {
                failed: stage.as_ref().map(|s| tw_api::L1Stage {
                    step: s.step,
                    peer: s.peer,
                }),
                detail: why.clone(),
                at_ms: now_ms(),
            });
            let changed = state
                .proxies
                .lock()
                .map(|mut g| match g.get_mut(&name) {
                    Some(st) => {
                        st.checking = false;
                        st.checked_at = std::time::Instant::now();
                        let reachable = fault.is_none();
                        let changed = st.reachable != reachable;
                        st.reachable = reachable;
                        // 一直不通、原因换了也记下最新的这一次
                        st.fault = fault.clone();
                        changed
                    }
                    None => false,
                })
                .unwrap_or(false);
            if !changed {
                return;
            }
            match &fault {
                Some(f) => tracing::warn!(proxy = %name, "proxy is unreachable: {}", f.detail),
                None => tracing::info!(proxy = %name, "proxy is reachable again"),
            }
            let (failed, detail) = match fault {
                Some(f) => (f.failed, Some(f.detail)),
                None => (None, None),
            };
            state.bus.emit(tw_api::Event::ProxyChanged {
                id: state.bus.next_id(),
                proxy: name,
                state: if detail.is_some() {
                    "unreachable"
                } else {
                    "reachable"
                }
                .into(),
                failed,
                detail,
                at_ms: now_ms(),
            });
        });
    }

    /// 读响应头里的订阅额度：存下来，报给界面，用完的窗口单独报一次。
    ///
    /// **429 的那一跳也要读**：额度用完时上游回的正是 429，只读成功那一跳的话，
    /// 「用完了」这件事永远看不到。
    pub(crate) fn note_quota(&self, id: u64, provider: &str, headers: &reqwest::header::HeaderMap) {
        self.record_quota(id, provider, crate::quota::from_headers(headers, now_ms()));
    }

    /// 记下一份额度。**账号接口问来的也走这里**：额度只在内存里，冷启动之后要等第一次
    /// 请求才有，而界面一打开就该看得见
    pub fn record_quota(&self, id: u64, provider: &str, quota: crate::quota::Quota) {
        if quota.is_empty() {
            return;
        }
        if let Ok(mut g) = self.quotas.lock() {
            g.insert(provider.to_string(), quota.clone());
        }
        self.bus.emit(tw_api::Event::QuotaSeen {
            id,
            provider: provider.to_string(),
            windows: quota
                .windows
                .iter()
                .map(|w| tw_api::QuotaWindow {
                    window: w.window.clone(),
                    used_percent: w.used_percent,
                    resets_at_ms: w.resets_at_ms,
                    status: w.status.clone(),
                })
                .collect(),
            at_ms: now_ms(),
        });
        for w in &quota.windows {
            let key = (provider.to_string(), w.window.clone());
            let changed = self
                .exhausted
                .lock()
                .map(|mut g| {
                    if w.rejected() {
                        g.insert(key)
                    } else {
                        g.remove(&key);
                        false
                    }
                })
                .unwrap_or(false);
            if changed {
                tracing::warn!(provider, window = %w.window, "the subscription quota is used up");
                self.bus.emit(tw_api::Event::QuotaExhausted {
                    id: self.bus.next_id(),
                    provider: provider.to_string(),
                    window: w.window.clone(),
                    resets_at_ms: w.resets_at_ms,
                    at_ms: now_ms(),
                });
            }
        }
    }

    /// 每个上游最近一次报的订阅额度。
    pub fn quotas(&self) -> std::collections::HashMap<String, crate::quota::Quota> {
        self.quotas.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 这一家的凭据正被拒绝的话，它回的状态码（见 [`AppState::note_auth`]）。
    pub fn auth_rejected(&self, provider: &str) -> Option<u16> {
        self.rejected
            .lock()
            .ok()
            .and_then(|g| g.get(provider).copied())
    }

    /// 这个代理被发现不通的话，检出来的样子（见 [`AppState::check_proxy`]）。
    pub fn proxy_fault(&self, proxy: &str) -> Option<tw_api::ProxyFault> {
        self.proxies
            .lock()
            .ok()
            .and_then(|g| g.get(proxy).and_then(|st| st.fault.clone()))
    }
}

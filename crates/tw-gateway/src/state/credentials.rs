//! 发给上游的凭据：取、换、失效了报、轮换了写回。

use super::AppState;
use crate::server::now_ms;

impl AppState {
    /// 要发给这一家的请求头，凭据在里面。**OAuth 那一类要联网换 token，所以这条路是
    /// async 的**；密钥和 `${ENV}` 走同步那条，零额外成本。
    ///
    /// `http` 必须是**这一家自己的** client：换 token 要走它该走的代理。
    /// 用一个干净的 client 去换，代理后面的用户会得到一个
    /// 「数据面通、刷新不通」的组合 —— 而那个症状看起来完全不像凭据问题。
    pub async fn headers_for(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
    ) -> Result<Vec<(String, String)>, String> {
        let token = match &p.oauth {
            Some(o) => Some(self.oauth_token(p, o, http).await?),
            None => None,
        };
        p.outbound_headers(token.as_deref())
            .map_err(|e| e.to_string())
    }

    /// 这一家的 OAuth access token。没配 oauth 是错误。
    ///
    /// 检测一个沿用原凭据的上游时用：**按原来那一家的名字换**，缓存和轮换写回都认名字。
    pub async fn oauth_token_for(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
    ) -> Result<String, String> {
        let o = p
            .oauth
            .as_ref()
            .ok_or_else(|| format!("upstream `{}` has no OAuth configured", p.name))?;
        self.oauth_token(p, o, http).await
    }

    /// 上游回了 401 之后换一个 access token，重新生成请求头。
    ///
    /// **只该调一次**（由调用方保证）：换回来的 token 还是 401，说明问题不在 token。
    /// `sent_at` 是被拒的那个请求发出去的时刻 —— 那之后已经有人换过的话，直接用新的。
    pub async fn headers_after_401(
        &self,
        p: &tw_config::Provider,
        http: &reqwest::Client,
        sent_at: std::time::Instant,
    ) -> Result<Vec<(String, String)>, String> {
        let o = p
            .oauth
            .as_ref()
            .ok_or_else(|| format!("upstream `{}` has no OAuth configured", p.name))?;
        let got = self
            .oauth
            .invalidate_and_refresh(&p.name, o, http, sent_at)
            .await;
        let token = self.settle_token(p, got)?;
        p.outbound_headers(Some(&token)).map_err(|e| e.to_string())
    }

    /// 换一个 access token，服务器换发了新的 refresh token 就交给控制面写回。
    async fn oauth_token(
        &self,
        p: &tw_config::Provider,
        o: &tw_config::OAuth,
        http: &reqwest::Client,
    ) -> Result<String, String> {
        let got = self.oauth.token(&p.name, o, http).await;
        self.settle_token(p, got)
    }

    /// 换 token 的结果：失效了要说，轮换了要写回。
    fn settle_token(
        &self,
        p: &tw_config::Provider,
        got: Result<(String, Option<crate::oauth::Renewed>), crate::oauth::OauthError>,
    ) -> Result<String, String> {
        let (token, renewed) = match got {
            Ok(t) => {
                if let Ok(mut told) = self.expired_told.lock() {
                    told.remove(&p.name);
                }
                t
            }
            Err(e) => {
                if e.needs_login() {
                    self.report_expired(&p.name, &e);
                }
                return Err(e.to_string());
            }
        };
        if let Some(r) = renewed {
            let rotated = r.refresh.is_some();
            let sink = self.renewal_sink.lock().ok().and_then(|g| g.clone());
            let lost = match sink {
                // **交出去就不管了。**写文件、存历史、防回环都在控制面，
                // 而这里是转发路径 —— 它不能等一次磁盘写。
                // 通道满 = 前一次还没写完
                Some(tx) => tx
                    .try_send(r)
                    .err()
                    .map(|_| "the write-back queue is full, so this rotation was not written back"),
                // 控制面没起来：**只报不写**，而且要说清没写
                None => Some(
                    "the gateway is running on its own, with no configuration manager, so this \
                     rotation was not written back",
                ),
            };
            // **换发的 refresh token 没写回才要说** —— 丢掉的是一份还没落盘、旧的已经作废的
            // 凭据。access token 没写回不要紧：下次启动拿 refresh token 再换一个就是
            if let (Some(why), true) = (lost, rotated) {
                self.report_rotation(&p.name, false, why);
            }
        }
        Ok(token)
    }

    /// 凭据失效报给界面。**同一家只报一次**，直到它恢复。
    fn report_expired(&self, provider: &str, e: &crate::oauth::OauthError) {
        let first = self
            .expired_told
            .lock()
            .map(|mut g| g.insert(provider.to_string()))
            .unwrap_or(false);
        if !first {
            return;
        }
        tracing::warn!(
            provider,
            "the OAuth credential has expired and needs a new sign-in: {e}"
        );
        self.bus.emit(tw_api::Event::CredentialExpired {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            detail: e.to_string(),
            at_ms: now_ms(),
        });
    }

    /// 凭据轮换的结果报给界面。
    ///
    /// **写成功也要报一次。**用户的 config.yaml 被我们改了 —— 哪怕改得
    /// 完全正确，不说一声也是不对的：他的编辑器会弹「文件已在磁盘上更改」，
    /// 而那时他应该已经知道原因。
    pub fn report_rotation(&self, provider: &str, persisted: bool, detail: &str) {
        {
            let mut told = self.rotation_told.lock().expect("lock not poisoned");
            if persisted {
                if !told.insert(provider.to_string()) {
                    // 这家的「已经帮你写回去了」说过了
                    tracing::debug!(
                        provider,
                        "the credential rotated again and was written back"
                    );
                    return;
                }
            } else {
                // 从「写得进去」变成「写不进去」是状态变了 —— 下次写成功
                // 的时候要重新说一句，否则用户不知道问题已经解决
                told.remove(provider);
            }
        }
        if persisted {
            tracing::info!(
                provider,
                "the token endpoint issued a new refresh token; it was written back to config.yaml"
            );
        } else {
            tracing::warn!(
                provider,
                detail,
                "the token endpoint issued a new refresh token and it could not be written back to \
                 config.yaml; this has to be dealt with before a restart, or every request to this \
                 upstream will come back 401"
            );
        }
        self.bus.emit(tw_api::Event::CredentialRotated {
            id: self.bus.next_id(),
            provider: provider.to_string(),
            persisted,
            detail: detail.to_string(),
            at_ms: now_ms(),
        });
    }
}

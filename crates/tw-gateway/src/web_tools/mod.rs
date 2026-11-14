//! Search and public-page tools, shared by the gateway and the control-plane tester.

mod collect;
mod history;
mod locale;
pub(crate) mod protocol;
mod public;
pub(crate) mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tw_api::{SearchEngine, WebToolRequest, WebToolResult, WebToolsConfig};
use tw_types::{Msg, msg};

type PageCached = Arc<AsyncMutex<Option<(Instant, (String, String))>>>;

type Cached = Arc<AsyncMutex<Option<(Instant, WebToolResult)>>>;

pub struct WebTools {
    pub(crate) history: Arc<history::History>,
    config: WebToolsConfig,
    client: public::ClientSettings,
    slots: Semaphore,
    cache: Mutex<HashMap<String, Cached>>,
    pages: Mutex<HashMap<String, PageCached>>,
    engines: AsyncMutex<HashMap<SearchEngine, EngineState>>,
}

#[derive(Default)]
struct EngineState {
    next: Option<Instant>,
    failed_until: Option<Instant>,
}

pub(crate) fn error(detail: impl Into<String>) -> Msg {
    msg!("gw.web_tools.failed", detail = detail.into() => "Web tools: {detail}")
}

/// Both API interception and the control tester use the same outbound protections.
pub async fn execute(
    rt: &crate::state::Runtime,
    identity: &str,
    request: &WebToolRequest,
) -> Result<WebToolResult, Msg> {
    execute_scoped(rt, identity, request, &[], &[]).await
}

pub(crate) async fn execute_scoped(
    rt: &crate::state::Runtime,
    identity: &str,
    request: &WebToolRequest,
    allowed: &[String],
    blocked: &[String],
) -> Result<WebToolResult, Msg> {
    let text = serde_json::to_string(request).map_err(|_| error("invalid tool request"))?;
    let body = serde_json::json!({"messages":[{"role":"user","content":text}]});
    let screened = crate::guard::screen_value(
        &crate::guard::Screen::of(rt),
        tw_dialect::ir::Dialect::Anthropic,
        &body,
    );
    if screened.refusal().is_some() {
        return Err(error("request was blocked by the configured content rules"));
    }
    let bytes = screened
        .body
        .unwrap_or_else(|| bytes::Bytes::from(body.to_string()));
    let shape = Some(crate::guard::Shape::Generate(
        tw_dialect::ir::Dialect::Anthropic,
    ));
    let ledger = crate::guard::ledger_for(&bytes);
    let (bytes, _) = crate::guard::replace(
        rt.config.security.redact.mode,
        &rt.redact,
        bytes,
        &ledger,
        shape,
    );
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| error("protected tool request is invalid"))?;
    let request = serde_json::from_str(
        body["messages"][0]["content"]
            .as_str()
            .ok_or_else(|| error("protected tool input is missing"))?,
    )
    .map_err(|_| error("protected tool input is invalid"))?;
    rt.web_tools
        .execute_scoped(identity, &request, allowed, blocked)
        .await
}

impl WebTools {
    fn locale(&self) -> tw_api::WebSearchLocale {
        tw_config::web_tools::resolve_locale(&self.config, &tw_config::web_tools::system_locale())
    }

    fn cache_key(
        identity: &str,
        request: &WebToolRequest,
        locale: &tw_api::WebSearchLocale,
    ) -> Result<String, Msg> {
        serde_json::to_string(&(identity, request, locale)).map_err(|_| error("invalid request"))
    }

    #[cfg(test)]
    pub(crate) async fn seed(
        &self,
        identity: &str,
        request: &WebToolRequest,
        result: WebToolResult,
    ) {
        let key = Self::cache_key(identity, request, &self.locale()).unwrap();
        self.cache.lock().unwrap().insert(
            key,
            Arc::new(AsyncMutex::new(Some((Instant::now(), result)))),
        );
    }
    pub fn new(config: &tw_config::Config) -> Result<Self, crate::GatewayError> {
        Ok(Self {
            history: Arc::new(history::History::default()),
            config: config.web_tools.clone(),
            client: public::ClientSettings::new(config)?,
            slots: Semaphore::new(config.web_tools.max_concurrent as usize),
            cache: Mutex::new(HashMap::new()),
            pages: Mutex::new(HashMap::new()),
            engines: AsyncMutex::new(HashMap::new()),
        })
    }

    /// Caller identity is part of the cache key. Only successful results are cached.
    pub async fn execute(
        &self,
        identity: &str,
        request: &WebToolRequest,
    ) -> Result<WebToolResult, Msg> {
        self.execute_scoped(identity, request, &[], &[]).await
    }

    async fn execute_scoped(
        &self,
        identity: &str,
        request: &WebToolRequest,
        allowed: &[String],
        blocked: &[String],
    ) -> Result<WebToolResult, Msg> {
        let started = Instant::now();
        let run = async {
            let locale = self.locale();
            let key = Self::cache_key(identity, request, &locale)?;
            let key = if allowed.is_empty() && blocked.is_empty() {
                key
            } else {
                format!("{key}\n{}", serde_json::json!([allowed, blocked]))
            };
            let entry = {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                if cache.len() >= 256 {
                    cache.retain(|_, e| Arc::strong_count(e) > 1);
                }
                if cache.len() >= 256 && !cache.contains_key(&key) {
                    return Err(error("too many pending web operations"));
                }
                cache
                    .entry(key)
                    .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
                    .clone()
            };
            // Holding this lock coalesces concurrent requests without leaving a background
            // operation behind when the last caller is cancelled.
            let mut cached = entry.lock().await;
            if let Some((at, result)) = &*cached
                && at.elapsed() < Duration::from_secs(u64::from(self.config.cache_secs))
            {
                let mut result = result.clone();
                result.cached = true;
                return Ok(result);
            }
            let _slot = self
                .slots
                .acquire()
                .await
                .map_err(|_| error("executor stopped"))?;
            let deadline = started + Duration::from_secs(u64::from(self.config.timeout_secs));
            let result = self
                .run(identity, request, (allowed, blocked), deadline, &locale)
                .await?;
            if self.config.cache_secs > 0 {
                *cached = Some((Instant::now(), result.clone()));
            }
            Ok(result)
        };
        let mut result = tokio::time::timeout(
            Duration::from_secs(u64::from(self.config.timeout_secs)),
            run,
        )
        .await
        .map_err(|_| error("operation timed out"))??;
        result.elapsed_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
        Ok(result)
    }

    async fn run(
        &self,
        identity: &str,
        request: &WebToolRequest,
        (scope_allowed, scope_blocked): (&[String], &[String]),
        deadline: Instant,
        locale: &tw_api::WebSearchLocale,
    ) -> Result<WebToolResult, Msg> {
        let mut result = WebToolResult {
            sources: Vec::new(),
            cached: false,
            elapsed_ms: 0,
            warnings: Vec::new(),
            truncated: false,
        };
        match request {
            WebToolRequest::Search {
                query,
                allowed_domains,
                blocked_domains,
            } => {
                if query.trim().is_empty() || query.len() > 4096 {
                    return Err(error("query must contain between 1 and 4096 bytes"));
                }
                public::validate_domains(allowed_domains)?;
                public::validate_domains(blocked_domains)?;
                let engines = if self.config.engines.is_empty() {
                    vec![SearchEngine::Bing, SearchEngine::Duckduckgo]
                } else {
                    self.config.engines.clone()
                };
                let mut succeeded = false;
                let engine_count = engines.len();
                for (index, engine) in engines.into_iter().enumerate() {
                    // Leave time for the next engine. A stalled engine must not
                    // consume the entire operation's timeout and prevent failover.
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .saturating_sub(Duration::from_millis(20));
                    let budget = remaining
                        / (engine_count - index + usize::from(self.config.read_pages > 0)) as u32;
                    let attempt = tokio::time::timeout(budget, async {
                        if !self.reserve(engine).await {
                            return Ok(None);
                        }
                        self.search(engine, query, allowed_domains, blocked_domains, locale)
                            .await
                            .map(Some)
                    })
                    .await
                    .unwrap_or_else(|_| Err(error("search engine timed out")));
                    match attempt {
                        Ok(None) => continue,
                        Ok(Some(sources)) => {
                            succeeded = true;
                            for source in sources {
                                if !result.sources.iter().any(|s| s.url == source.url) {
                                    result.sources.push(source);
                                }
                            }
                            if result.sources.len() >= self.config.max_results as usize {
                                break;
                            }
                        }
                        Err(e) => {
                            self.engines
                                .lock()
                                .await
                                .entry(engine)
                                .or_default()
                                .failed_until = Some(Instant::now() + Duration::from_secs(60));
                            result.warnings.push(e);
                        }
                    }
                }
                if !succeeded {
                    return Err(error(
                        "all configured search engines failed or are cooling down",
                    ));
                }
                result.sources.truncate(self.config.max_results as usize);
                let mut read = 0;
                for source in &mut result.sources {
                    if read >= self.config.read_pages || source.text.chars().count() >= 300 {
                        continue;
                    }
                    read += 1;
                    let budget = deadline
                        .saturating_duration_since(Instant::now())
                        .saturating_sub(Duration::from_millis(20))
                        / (self.config.read_pages - read + 1);
                    if budget < Duration::from_millis(100) {
                        break;
                    }
                    match tokio::time::timeout(
                        budget.min(Duration::from_secs(5)),
                        self.page(identity, &source.url, allowed_domains, blocked_domains),
                    )
                    .await
                    .unwrap_or_else(|_| Err(error("optional page reading timed out")))
                    {
                        Ok((_, text)) => source.text = collect::excerpt(&text, query, 3000),
                        Err(e) => result.warnings.push(e),
                    }
                }
            }
            WebToolRequest::OpenPage { url, offset } => {
                let (title, text) = self
                    .page(identity, url, scope_allowed, scope_blocked)
                    .await?;
                let chars = text.chars().count();
                let limit = self.config.max_page_chars as usize;
                result.truncated = chars > *offset as usize + limit;
                result.sources.push(tw_api::WebSource {
                    url: url.clone(),
                    title,
                    text: text.chars().skip(*offset as usize).take(limit).collect(),
                    engine: None,
                    published_at: None,
                });
            }
            WebToolRequest::FindInPage { url, pattern } => {
                if pattern.trim().is_empty() || pattern.len() > 512 {
                    return Err(error("find pattern must contain between 1 and 512 bytes"));
                }
                let (title, text) = self
                    .page(identity, url, scope_allowed, scope_blocked)
                    .await?;
                let found = collect::find(&text, pattern, self.config.max_page_chars as usize);
                result.sources.push(tw_api::WebSource {
                    url: url.clone(),
                    title,
                    text: found,
                    engine: None,
                    published_at: None,
                });
            }
        }
        Ok(result)
    }

    async fn reserve(&self, engine: SearchEngine) -> bool {
        let delay = {
            let mut engines = self.engines.lock().await;
            let state = engines.entry(engine).or_default();
            let now = Instant::now();
            if state.failed_until.is_some_and(|until| until > now) {
                return false;
            }
            let at = state.next.unwrap_or(now).max(now);
            state.next = Some(at + Duration::from_millis(u64::from(self.config.interval_ms)));
            at.saturating_duration_since(now)
        };
        tokio::time::sleep(delay).await;
        true
    }

    async fn search(
        &self,
        engine: SearchEngine,
        query: &str,
        allowed: &[String],
        blocked: &[String],
        locale: &tw_api::WebSearchLocale,
    ) -> Result<Vec<tw_api::WebSource>, Msg> {
        let mut q = query.to_string();
        if !allowed.is_empty() {
            q.push_str(&format!(
                " ({})",
                allowed
                    .iter()
                    .map(|d| format!("site:{d}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ));
        } else if !self.config.allowed_domains.is_empty() {
            q.push_str(&format!(
                " ({})",
                self.config
                    .allowed_domains
                    .iter()
                    .map(|d| format!("site:{d}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ));
        }
        let mut url = reqwest::Url::parse(match engine {
            SearchEngine::Bing => "https://www.bing.com/search",
            SearchEngine::Duckduckgo => "https://html.duckduckgo.com/html/",
        })
        .expect("built-in URL");
        url.query_pairs_mut().append_pair("q", &q);
        if engine == SearchEngine::Bing {
            url.query_pairs_mut().append_pair("format", "rss");
        }
        locale::apply(&mut url, engine, locale);
        let (_, body) = self.fetch(url, None).await?;
        let mut sources = collect::search(engine, &body)?;
        sources.retain(|s| {
            reqwest::Url::parse(&s.url).ok().is_some_and(|url| {
                self.allowed(&url) && public::domains_allow(&url, allowed, blocked)
            })
        });
        Ok(sources)
    }

    fn allowed(&self, url: &reqwest::Url) -> bool {
        public::domains_allow(
            url,
            &self.config.allowed_domains,
            &self.config.blocked_domains,
        )
    }

    async fn page(
        &self,
        identity: &str,
        url: &str,
        allowed: &[String],
        blocked: &[String],
    ) -> Result<(String, String), Msg> {
        let key = format!(
            "{identity}\n{url}\n{}",
            serde_json::json!([allowed, blocked])
        );
        let entry = {
            let mut pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
            if pages.len() >= 32 {
                pages.retain(|_, p| Arc::strong_count(p) > 1);
            }
            if pages.len() >= 32 && !pages.contains_key(&key) {
                return Err(error("too many pending page reads"));
            }
            pages
                .entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
                .clone()
        };
        let mut cache = entry.lock().await;
        if let Some((at, page)) = &*cache
            && at.elapsed() < Duration::from_secs(u64::from(self.config.cache_secs))
        {
            return Ok(page.clone());
        }
        let page = self.fetch_page(url, allowed, blocked).await?;
        if self.config.cache_secs > 0 {
            *cache = Some((Instant::now(), page.clone()));
        }
        Ok(page)
    }

    async fn fetch_page(
        &self,
        url: &str,
        allowed: &[String],
        blocked: &[String],
    ) -> Result<(String, String), Msg> {
        let url = public::parse(url)?;
        if !self.allowed(&url) {
            return Err(error("page is excluded by the configured domain rules"));
        }
        let (kind, body) = self.fetch(url, Some((allowed, blocked))).await?;
        if kind.contains("html") {
            Ok(collect::page(&body))
        } else if kind.starts_with("text/") {
            Ok((String::new(), body))
        } else {
            Err(error("page must be HTML or plain text"))
        }
    }

    async fn fetch(
        &self,
        mut url: reqwest::Url,
        filters: Option<(&[String], &[String])>,
    ) -> Result<(String, String), Msg> {
        use futures::StreamExt;
        for _ in 0..6 {
            let addresses = public::check(&url).await?;
            if let Some((allowed, blocked)) = filters
                && (!self.allowed(&url) || !public::domains_allow(&url, allowed, blocked))
            {
                return Err(error("redirect is excluded by the configured domain rules"));
            }
            let response = self.client.get(&url, &addresses).await?;
            if response.status().is_redirection() {
                let to = response
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or_else(|| error("redirect has no location"))?;
                let next = url.join(to).map_err(|_| error("invalid redirect"))?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err(error("refused an insecure redirect"));
                }
                url = next;
                continue;
            }
            if !response.status().is_success() {
                return Err(error(format!(
                    "source returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            let kind = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_lowercase();
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| error(e.without_url().to_string()))?;
                if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                    return Err(error("source exceeded the 2 MiB download limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            return String::from_utf8(bytes)
                .map(|body| (kind, body))
                .map_err(|_| error("source is not UTF-8 text"));
        }
        Err(error("too many redirects"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "uses public search engines and public webpages"]
    async fn live_search_read_and_find() {
        let mut config = tw_config::Config::default();
        config.web_tools.engines = vec![SearchEngine::Bing];
        config.web_tools.read_pages = 0;
        let tools = WebTools::new(&config).unwrap();
        let search = tools
            .execute(
                "live-test",
                &WebToolRequest::Search {
                    query: "SQLite release notes".into(),
                    allowed_domains: vec![],
                    blocked_domains: vec![],
                },
            )
            .await
            .unwrap();
        assert!(!search.sources.is_empty());
        let page = tools
            .execute(
                "live-test",
                &WebToolRequest::OpenPage {
                    url: "https://www.sqlite.org/changes.html".into(),
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert!(page.sources[0].text.to_lowercase().contains("sqlite"));
        let found = tools
            .execute(
                "live-test",
                &WebToolRequest::FindInPage {
                    url: "https://www.sqlite.org/changes.html".into(),
                    pattern: "release".into(),
                },
            )
            .await
            .unwrap();
        assert!(found.sources[0].text.to_lowercase().contains("release"));
    }
}

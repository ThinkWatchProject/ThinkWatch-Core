//! Search and public-page tools, shared by the gateway and the control-plane tester.

mod cache;
mod collect;
mod history;
pub(crate) mod protocol;
mod providers;
mod public;
mod quality;
mod session;
pub(crate) mod wire;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tw_api::{SearchEngine, WebToolRequest, WebToolResult, WebToolsConfig};
use tw_types::{Msg, msg};

pub struct WebTools {
    pub(crate) history: Arc<history::History>,
    config: WebToolsConfig,
    client: public::ClientSettings,
    slots: Semaphore,
    cache: cache::Cache<WebToolResult>,
    pages: cache::Cache<(String, String)>,
    engines: AsyncMutex<HashMap<SearchEngine, EngineState>>,
    sessions: session::Pool,
}

#[derive(Default)]
struct EngineState {
    next: Option<Instant>,
    failed_until: Option<Instant>,
    last_failure: Option<Msg>,
}

/// Capabilities come from the same registry that executes requests. Desktop
/// clients can offer new engines without hard-coding an exhaustive enum list.
pub fn available_engines() -> Vec<tw_api::WebSearchEngine> {
    providers::AUTOMATIC
        .iter()
        .map(|&engine| {
            let spec = providers::get(engine).spec();
            tw_api::WebSearchEngine {
                id: engine,
                name: spec.name.into(),
                min_interval_ms: spec.min_interval_ms,
            }
        })
        .collect()
}

fn engine_name(engine: SearchEngine) -> &'static str {
    providers::get(engine).spec().name
}

fn failure_detail(e: &Msg) -> &str {
    if e.code == "gw.web_tools.failed" {
        e.args.get("detail").map(String::as_str).unwrap_or(&e.text)
    } else {
        &e.text
    }
}

fn http_failure(status: reqwest::StatusCode, headers: &http::HeaderMap) -> Msg {
    let mut failure = error(format!("source returned HTTP {}", status.as_u16()));
    failure
        .args
        .insert("http_status".into(), status.as_u16().to_string());
    if matches!(status.as_u16(), 429 | 503)
        && let Some(value) = headers.get("retry-after").and_then(|v| v.to_str().ok())
    {
        let seconds = value.trim().parse::<u64>().ok().or_else(|| {
            chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|at| (at.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
        });
        if let Some(seconds) = seconds {
            failure
                .args
                .insert("retry_after_secs".into(), seconds.to_string());
        }
    }
    failure
}

impl EngineState {
    fn cooling_down(&self, engine: SearchEngine) -> Msg {
        let seconds = self
            .failed_until
            .map(|until| until.saturating_duration_since(Instant::now()).as_secs() + 1)
            .unwrap_or(0);
        let cause = self
            .last_failure
            .as_ref()
            .map(failure_detail)
            .unwrap_or("previous search failed");
        error(format!(
            "{} is cooling down; retry in {seconds}s. Last failure: {cause}",
            engine_name(engine)
        ))
    }
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
        *self.cache.get(key).unwrap().lock().await = Some((Instant::now(), result));
    }
    pub fn new(config: &tw_config::Config) -> Result<Self, crate::GatewayError> {
        Ok(Self {
            history: Arc::new(history::History::default()),
            config: config.web_tools.clone(),
            client: public::ClientSettings::new(config)?,
            slots: Semaphore::new(config.web_tools.max_concurrent as usize),
            cache: cache::Cache::new(256),
            pages: cache::Cache::new(32),
            engines: AsyncMutex::new(HashMap::new()),
            sessions: session::Pool::default(),
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
            let entry = self.cache.get(key)?;
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
                    providers::AUTOMATIC.to_vec()
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
                    let mut searching = false;
                    let attempt = tokio::time::timeout(budget, async {
                        if !self.reserve(engine).await {
                            return Ok(None);
                        }
                        searching = true;
                        self.search(
                            identity,
                            engine,
                            query,
                            allowed_domains,
                            blocked_domains,
                            locale,
                        )
                        .await
                        .map(Some)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err(error(if searching {
                            "search engine timed out"
                        } else {
                            "search rate-limit wait timed out"
                        }))
                    });
                    match attempt {
                        Ok(None) => {
                            let engines = self.engines.lock().await;
                            if let Some(state) = engines.get(&engine) {
                                result.warnings.push(state.cooling_down(engine));
                            }
                        }
                        Ok(Some(sources)) => {
                            if let Some(state) = self.engines.lock().await.get_mut(&engine) {
                                state.failed_until = None;
                                state.last_failure = None;
                            }
                            succeeded = true;
                            for source in sources {
                                if !result.sources.iter().any(|s| s.url == source.url) {
                                    result.sources.push(source);
                                }
                            }
                            // Engine order is failover order. Filling a healthy
                            // engine's shorter result set from a worse fallback
                            // can reintroduce unrelated pages.
                            if !result.sources.is_empty() {
                                break;
                            }
                        }
                        Err(e) => {
                            let query_mismatch = e.code == "gw.web_tools.query_mismatch";
                            let cooldown = e
                                .args
                                .get("retry_after_secs")
                                .and_then(|v| v.parse::<u64>().ok())
                                .and_then(|seconds| {
                                    Instant::now().checked_add(Duration::from_secs(seconds.max(1)))
                                })
                                .unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
                            let e =
                                error(format!("{}: {}", engine_name(engine), failure_detail(&e)));
                            // A poor match for one query does not make the engine
                            // unavailable for every other caller for a minute.
                            if searching && !query_mismatch {
                                let mut engines = self.engines.lock().await;
                                let state = engines.entry(engine).or_default();
                                state.failed_until = Some(cooldown);
                                state.last_failure = Some(e.clone());
                            }
                            result.warnings.push(e);
                        }
                    }
                }
                if !succeeded {
                    // Do not discard the engine errors: a challenge, a broken proxy,
                    // and a timeout need different remedies. Retries during cooldown
                    // retain the original cause and say when another attempt is due.
                    let reasons = result
                        .warnings
                        .iter()
                        .map(failure_detail)
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(error(format!(
                        "all configured search engines failed or are cooling down. {reasons}"
                    )));
                }
                result.sources.truncate(self.config.max_results as usize);
                let mut read = 0;
                for source in &mut result.sources {
                    if read >= self.config.read_pages
                        || source.text.chars().count() >= 300
                        || reqwest::Url::parse(&source.url)
                            .is_ok_and(|url| url.path().to_ascii_lowercase().ends_with(".pdf"))
                    {
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
                        Ok((_, text)) => {
                            let excerpt = collect::excerpt(&text, query, 3000);
                            if !excerpt.is_empty() && !source.text.contains(&excerpt) {
                                // The engine snippet may be more focused than the
                                // landing page. Keep it instead of replacing it
                                // with navigation text or unrelated paragraphs.
                                source.text = format!("{}\n\n{excerpt}", source.text)
                                    .trim()
                                    .chars()
                                    .take(4000)
                                    .collect();
                            }
                        }
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
        loop {
            let delay = {
                let mut engines = self.engines.lock().await;
                let state = engines.entry(engine).or_default();
                let now = Instant::now();
                if state.failed_until.is_some_and(|until| until > now) {
                    return false;
                }
                let at = state.next.unwrap_or(now);
                if at <= now {
                    state.next = Some(
                        now + Duration::from_millis(u64::from(
                            self.config
                                .interval_ms
                                .max(providers::get(engine).spec().min_interval_ms),
                        )),
                    );
                    return true;
                }
                at.saturating_duration_since(now)
            };
            // Reserve only when a request can actually start. Cancelled waiters
            // must not accumulate future slots and delay unrelated searches.
            tokio::time::sleep(delay).await;
        }
    }

    async fn search(
        &self,
        identity: &str,
        engine: SearchEngine,
        query: &str,
        allowed: &[String],
        blocked: &[String],
        locale: &tw_api::WebSearchLocale,
    ) -> Result<Vec<tw_api::WebSource>, Msg> {
        let provider = providers::get(engine);
        let spec = provider.spec();
        let session = self.sessions.get(identity, spec, locale)?;
        let context = session::Request {
            spec,
            kind: providers::Kind::Document,
            session: session.as_ref().map(|s| &s.session),
            locale,
        };
        if let Some(session) = &session {
            session
                .session
                .ready
                .get_or_try_init(|| async {
                    let home = spec
                        .home
                        .expect("session requires a homepage")
                        .parse()
                        .unwrap();
                    let (_, body) = self.fetch(home, None, Some(context), None).await?;
                    provider.validate_home(&body)
                })
                .await?;
        }
        let query_context = providers::Query {
            text: query.trim(),
            allowed: if allowed.is_empty() {
                &self.config.allowed_domains
            } else {
                allowed
            },
            locale,
        };
        let mut requests = 0;
        let mut sources = providers::run(provider, &query_context, |request| {
            let retry = requests > 0;
            requests += 1;
            async move {
                // The first reservation is made by the scheduler. Further exchange
                // steps with a source minimum must obey that same engine-wide pace.
                if retry && spec.min_interval_ms > 0 && !self.reserve(engine).await {
                    return Err(error("search engine entered cooldown"));
                }
                let context = session::Request {
                    kind: request.kind,
                    ..context
                };
                self.fetch(request.url, None, Some(context), request.form.as_deref())
                    .await
                    .map(|(_, body)| body)
            }
        })
        .await?;
        let had_results = !sources.is_empty();
        let usable = quality::filter(query, &mut sources);
        if had_results && !usable {
            return Err(msg!("gw.web_tools.query_mismatch" =>
                "The search engine returned results that did not match the query."));
        }
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
        let entry = self.pages.get(key)?;
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
        let (kind, body) = self
            .fetch(url, Some((allowed, blocked)), None, None)
            .await?;
        if kind.contains("html") {
            collect::page(&body)
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
        search: Option<session::Request<'_>>,
        form: Option<&str>,
    ) -> Result<(String, String), Msg> {
        use futures::StreamExt;
        for _ in 0..6 {
            if let Some((allowed, blocked)) = filters
                && (!self.allowed(&url) || !public::domains_allow(&url, allowed, blocked))
            {
                return Err(error("redirect is excluded by the configured domain rules"));
            }
            let response = match search {
                Some(context) => {
                    let response = self
                        .client
                        .get_search(context.spec, &url, context.headers(&url)?, form)
                        .await?;
                    context.remember(&url, response.headers())?;
                    response
                }
                None => {
                    let addresses = public::check(&url).await?;
                    self.client.get(&url, &addresses).await?
                }
            };
            if response.status().is_redirection() {
                // Form queries never follow redirects: do not replay their body
                // into a login, verification, or different document endpoint.
                if form.is_some() {
                    return Err(http_failure(response.status(), response.headers()));
                }
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
                return Err(http_failure(response.status(), response.headers()));
            }
            let kind = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_lowercase();
            // Reject binary documents before trying to decode them as UTF-8.
            if search.is_none() && !kind.contains("html") && !kind.starts_with("text/") {
                return Err(error("page must be HTML or plain text"));
            }
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

    #[test]
    fn rate_limits_keep_server_retry_after_seconds_and_dates() {
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after", "120".parse().unwrap());
        assert_eq!(
            http_failure(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers).arg("retry_after_secs"),
            "120"
        );
        let future = chrono::Utc::now() + chrono::Duration::minutes(2);
        headers.insert("retry-after", future.to_rfc2822().parse().unwrap());
        let seconds: u64 = http_failure(reqwest::StatusCode::SERVICE_UNAVAILABLE, &headers)
            .arg("retry_after_secs")
            .parse()
            .unwrap();
        assert!((118..=120).contains(&seconds));
        headers.insert("retry-after", "invalid".parse().unwrap());
        assert!(
            http_failure(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers)
                .arg("retry_after_secs")
                .is_empty()
        );
    }

    #[tokio::test]
    #[ignore = "live traditional-engine relevance, filters, and concurrent cache checks"]
    async fn live_direct_search_matrix() {
        live_engine_matrix(&[SearchEngine::Duckduckgo]).await;
    }

    #[tokio::test]
    #[ignore = "live Bing relevance with a configured Chinese locale, filters, and cache"]
    async fn live_bing_search_matrix() {
        live_engine_matrix(&[SearchEngine::Bing]).await;
    }

    #[tokio::test]
    #[ignore = "live Bing-first search with DuckDuckGo failover, filters, and cache"]
    async fn live_bing_failover_matrix() {
        live_engine_matrix(&[SearchEngine::Bing, SearchEngine::Duckduckgo]).await;
    }

    #[tokio::test]
    #[ignore = "live Google Docs relevance, domain filters, and concurrent cache checks"]
    async fn live_google_docs_search_matrix() {
        live_engine_matrix(&[SearchEngine::GoogleDocs]).await;
    }

    #[tokio::test]
    #[ignore = "live automatic search with Google Docs and traditional-engine failover"]
    async fn live_automatic_search_matrix() {
        live_engine_matrix(providers::AUTOMATIC).await;
    }

    async fn live_engine_matrix(engines: &[SearchEngine]) {
        let mut config = tw_config::Config::default();
        config.web_tools.engines = engines.to_vec();
        config.web_tools.language = "zh-Hans".into();
        config.web_tools.region = "CN".into();
        config.web_tools.read_pages = 0;
        let tools = WebTools::new(&config).unwrap();
        let request = |query: &str| WebToolRequest::Search {
            query: query.into(),
            allowed_domains: vec![],
            blocked_domains: vec![],
        };
        let mut failures = Vec::new();
        for (query, domain, path) in [
            (
                "Python asyncio TaskGroup documentation",
                "python.org",
                "asyncio-task",
            ),
            ("Rust OnceLock documentation", "rust-lang.org", "OnceLock"),
            ("SQLite release notes", "sqlite.org", "changes"),
            ("劳动合同法 第三十八条", "gov.cn", ""),
            (
                "2025 Nobel physics prize winners",
                "nobelprize.org",
                "physics/2025",
            ),
            (
                "Apple Mac mini M4 technical specifications",
                "apple.com",
                "121555",
            ),
            ("中华人民共和国民法典 第一千零六十二条", "gov.cn", ""),
            ("OpenAI Responses API reference", "openai.com", "responses"),
        ] {
            let result = match tools.execute("matrix", &request(query)).await {
                Ok(result) => result,
                Err(error) => {
                    failures.push(format!("{query}: {error}"));
                    continue;
                }
            };
            eprintln!(
                "{query}: {} sources, {} ms, {:?}, {:?}",
                result.sources.len(),
                result.elapsed_ms,
                result.sources.first().and_then(|s| s.engine),
                result
                    .sources
                    .iter()
                    .take(3)
                    .map(|s| &s.url)
                    .collect::<Vec<_>>()
            );
            if engines.len() == 1 {
                assert!(result.warnings.is_empty(), "{query}: {:?}", result.warnings);
            }
            if !result.sources.iter().any(|source| {
                reqwest::Url::parse(&source.url).is_ok_and(|url| {
                    public::domains_allow(&url, &[domain.into()], &[]) && url.path().contains(path)
                })
            }) {
                failures.push(format!("{query}: missing {domain}/{path}"));
            }
            assert!(
                result
                    .sources
                    .iter()
                    .all(|s| s.engine.is_some_and(|e| engines.contains(&e)))
            );
        }
        assert!(
            failures.is_empty(),
            "{engines:?} failed relevance checks: {failures:?}"
        );
        let req = request("PostgreSQL transaction isolation documentation");
        let results =
            futures::future::join_all((0..4).map(|_| tools.execute("matrix", &req))).await;
        let results: Vec<_> = results.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(results.iter().filter(|r| r.cached).count(), 3);
        assert!(results.iter().all(|r| !r.sources.is_empty()));
        for (allowed_domains, blocked_domains) in [
            (vec!["sqlite.org".into()], vec![]),
            (vec![], vec!["sqlite.org".into()]),
        ] {
            let result = tools
                .execute(
                    "matrix",
                    &WebToolRequest::Search {
                        query: "SQLite release notes".into(),
                        allowed_domains: allowed_domains.clone(),
                        blocked_domains: blocked_domains.clone(),
                    },
                )
                .await
                .unwrap();
            assert!(!result.sources.is_empty());
            assert!(result.sources.iter().all(|s| public::domains_allow(
                &reqwest::Url::parse(&s.url).unwrap(),
                &allowed_domains,
                &blocked_domains
            )));
        }
    }

    #[tokio::test]
    async fn source_minimum_interval_and_capabilities_share_the_registry() {
        let mut config = tw_config::Config::default();
        config.web_tools.interval_ms = 250;
        let tools = WebTools::new(&config).unwrap();
        assert!(tools.reserve(SearchEngine::GoogleDocs).await);
        let next = tools.engines.lock().await[&SearchEngine::GoogleDocs]
            .next
            .unwrap();
        assert!(next.saturating_duration_since(Instant::now()) > Duration::from_millis(5900));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                tools.reserve(SearchEngine::GoogleDocs)
            )
            .await
            .is_err()
        );
        assert_eq!(
            tools.engines.lock().await[&SearchEngine::GoogleDocs].next,
            Some(next)
        );
        let available = available_engines();
        assert_eq!(available[0].id, SearchEngine::GoogleDocs);
        assert_eq!(available[0].min_interval_ms, 6000);
    }

    #[tokio::test]
    async fn cancelled_waiters_do_not_reserve_more_engine_slots() {
        let tools = WebTools::new(&tw_config::Config::default()).unwrap();
        assert!(tools.reserve(SearchEngine::Bing).await);
        let next = tools.engines.lock().await[&SearchEngine::Bing].next;
        for _ in 0..3 {
            assert!(
                tokio::time::timeout(Duration::from_millis(1), tools.reserve(SearchEngine::Bing))
                    .await
                    .is_err()
            );
            assert_eq!(tools.engines.lock().await[&SearchEngine::Bing].next, next);
        }
    }

    #[tokio::test]
    async fn rate_limit_wait_timeout_does_not_mark_the_engine_unavailable() {
        let mut config = tw_config::Config::default();
        config.web_tools.timeout_secs = 1;
        config.web_tools.read_pages = 0;
        config.web_tools.engines = vec![SearchEngine::Bing];
        let tools = WebTools::new(&config).unwrap();
        tools.engines.lock().await.insert(
            SearchEngine::Bing,
            EngineState {
                next: Some(Instant::now() + Duration::from_secs(60)),
                ..Default::default()
            },
        );
        let failure = tools
            .execute(
                "test",
                &WebToolRequest::Search {
                    query: "SQLite release notes".into(),
                    allowed_domains: vec![],
                    blocked_domains: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(failure.text.contains("rate-limit wait"), "{failure}");
        assert!(
            tools.engines.lock().await[&SearchEngine::Bing]
                .failed_until
                .is_none()
        );
    }

    #[tokio::test]
    async fn unavailable_engines_keep_their_causes_and_retry_deadline() {
        let mut config = tw_config::Config::default();
        config.web_tools.engines = vec![SearchEngine::Bing, SearchEngine::Duckduckgo];
        let tools = WebTools::new(&config).unwrap();
        let until = Instant::now() + Duration::from_secs(60);
        for (engine, reason) in [
            (SearchEngine::Bing, "source returned HTTP 429"),
            (
                SearchEngine::Duckduckgo,
                "search engine requires a verification challenge",
            ),
        ] {
            tools.engines.lock().await.insert(
                engine,
                EngineState {
                    next: None,
                    failed_until: Some(until),
                    last_failure: Some(error(format!("{}: {reason}", engine_name(engine)))),
                },
            );
        }
        for query in ["SQLite release notes", "Rust documentation"] {
            let failure = tools
                .execute(
                    "test",
                    &WebToolRequest::Search {
                        query: query.into(),
                        allowed_domains: vec![],
                        blocked_domains: vec![],
                    },
                )
                .await
                .unwrap_err();
            assert!(failure.text.contains("Bing"), "{failure}");
            assert!(failure.text.contains("429"), "{failure}");
            assert!(failure.text.contains("DuckDuckGo"), "{failure}");
            assert!(failure.text.contains("verification challenge"), "{failure}");
            assert!(failure.text.contains("retry in"), "{failure}");
            assert_eq!(failure.text.matches("Web tools:").count(), 1, "{failure}");
            for state in tools.engines.lock().await.values() {
                assert_eq!(
                    state.failed_until,
                    Some(until),
                    "retry must not extend cooldown"
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "uses the public DuckDuckGo search engine"]
    async fn live_search_duckduckgo() {
        live_duckduckgo_after_cooldown(false).await;
    }

    #[tokio::test]
    #[ignore = "live DuckDuckGo fallback while Google Docs is cooling down"]
    async fn live_search_google_cooldown_falls_back() {
        live_duckduckgo_after_cooldown(true).await;
    }

    async fn live_duckduckgo_after_cooldown(google_failed: bool) {
        let mut config = tw_config::Config::default();
        config.web_tools.engines = vec![SearchEngine::Duckduckgo];
        if google_failed {
            config.web_tools.engines.insert(0, SearchEngine::GoogleDocs);
        }
        config.web_tools.read_pages = 0;
        let tools = WebTools::new(&config).unwrap();
        if google_failed {
            tools.engines.lock().await.insert(
                SearchEngine::GoogleDocs,
                EngineState {
                    failed_until: Some(Instant::now() + Duration::from_secs(60)),
                    last_failure: Some(error("Google (Docs): source returned HTTP 500")),
                    ..Default::default()
                },
            );
        }
        let result = tools
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
        if google_failed {
            assert!(result.warnings.iter().any(|w| w.text.contains("500")));
        }
        assert!(
            result.sources.iter().any(|s| {
                reqwest::Url::parse(&s.url).is_ok_and(|url| {
                    url.host_str()
                        .is_some_and(|host| host == "sqlite.org" || host.ends_with(".sqlite.org"))
                        && url.path().contains("changes")
                })
            }),
            "release notes query must find SQLite's release history: {:?}",
            result.sources
        );
        assert!(
            result
                .sources
                .iter()
                .all(|s| s.engine == Some(SearchEngine::Duckduckgo))
        );
    }

    #[tokio::test]
    #[ignore = "uses public search engines and public webpages"]
    async fn live_search_read_and_find() {
        let mut config = tw_config::Config::default();
        config.web_tools.engines = vec![SearchEngine::Duckduckgo];
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
        assert!(
            search.sources.iter().any(|s| {
                reqwest::Url::parse(&s.url).is_ok_and(|url| {
                    url.host_str()
                        .is_some_and(|host| host == "sqlite.org" || host.ends_with(".sqlite.org"))
                        && url.path().contains("changes")
                })
            }),
            "release notes query must find SQLite's release history: {:?}",
            search.sources
        );
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

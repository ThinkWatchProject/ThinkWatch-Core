//! Source adapters describe requests and parse responses; only the executor does
//! I/O. New sources share the same URL checks, budgets, sessions, and failover.

mod bing;
mod duckduckgo;
mod google_docs;
mod locale;

use reqwest::Url;
use tw_api::{SearchEngine, WebSearchLocale, WebSource};
use tw_types::Msg;

pub(super) const AUTOMATIC: &[SearchEngine] = &[
    SearchEngine::GoogleDocs,
    SearchEngine::Duckduckgo,
    SearchEngine::Bing,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Profile {
    /// The existing uncompressed HTTP/1 tunnel, also used for page reads.
    Basic,
    /// Browser navigation headers and decoded, pooled HTTP/1 connections.
    BrowserHttp1,
}

pub(super) struct Endpoint {
    pub host: &'static str,
    pub paths: &'static [&'static str],
}

pub(super) struct Spec {
    pub id: SearchEngine,
    pub name: &'static str,
    pub profile: Profile,
    pub min_interval_ms: u32,
    pub home: Option<&'static str>,
    pub endpoints: &'static [Endpoint],
}

impl Spec {
    pub fn allows(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && self
                .endpoints
                .iter()
                .any(|e| url.host_str() == Some(e.host) && e.paths.contains(&url.path()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Document,
    Data,
}

#[derive(Clone)]
pub(super) struct Request {
    pub url: Url,
    pub kind: Kind,
    /// Encoded form body selects POST. None preserves the existing GET flow.
    pub form: Option<String>,
}

pub(super) struct Query<'a> {
    pub text: &'a str,
    pub allowed: &'a [String],
    pub locale: &'a WebSearchLocale,
}

pub(super) enum Step {
    Fetch(Request),
    Done(Vec<WebSource>),
}

pub(super) trait Provider: Sync {
    fn spec(&self) -> &'static Spec;
    fn start(&self, query: &Query<'_>) -> Result<Request, Msg>;
    fn recover(&self, _request: &Request, _failure: &Msg, _query: &Query<'_>) -> Option<Request> {
        None
    }
    fn parse(&self, request: &Request, body: &str, query: &Query<'_>) -> Result<Step, Msg>;
    fn validate_home(&self, _body: &str) -> Result<(), Msg> {
        Ok(())
    }
}

pub(super) fn get(engine: SearchEngine) -> &'static dyn Provider {
    match engine {
        SearchEngine::GoogleDocs => &*google_docs::GOOGLE_DOCS,
        SearchEngine::Bing => &bing::Bing,
        SearchEngine::Duckduckgo => &duckduckgo::Duckduckgo,
    }
}

/// A bounded multi-step exchange shared by all adapters. No adapter can fetch a
/// discovered URL outside its declared first-party endpoints, even in tests.
pub(super) async fn run<F, Fut>(
    provider: &dyn Provider,
    query: &Query<'_>,
    mut fetch: F,
) -> Result<Vec<WebSource>, Msg>
where
    F: FnMut(Request) -> Fut,
    Fut: std::future::Future<Output = Result<String, Msg>>,
{
    let mut request = provider.start(query)?;
    for _ in 0..4 {
        if !provider.spec().allows(&request.url) {
            return Err(super::error(
                "search requested an unsupported first-party endpoint",
            ));
        }
        let body = match fetch(request.clone()).await {
            Ok(body) => body,
            Err(failure) => match provider.recover(&request, &failure, query) {
                Some(next) => {
                    request = next;
                    continue;
                }
                None => return Err(failure),
            },
        };
        match provider.parse(&request, &body, query)? {
            Step::Fetch(next) => request = next,
            Step::Done(sources) => return Ok(sources),
        }
    }
    Err(super::error("search exceeded its request-step limit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AdditionalSource;
    static ADDITIONAL: Spec = Spec {
        id: SearchEngine::Bing,
        name: "Test adapter",
        profile: Profile::Basic,
        min_interval_ms: 0,
        home: None,
        endpoints: &[Endpoint {
            host: "source.test",
            paths: &["/search", "/data"],
        }],
    };
    impl Provider for AdditionalSource {
        fn spec(&self) -> &'static Spec {
            &ADDITIONAL
        }
        fn start(&self, _: &Query<'_>) -> Result<Request, Msg> {
            Ok(Request {
                url: "https://source.test/search".parse().unwrap(),
                kind: Kind::Document,
                form: None,
            })
        }
        fn parse(&self, _: &Request, body: &str, _: &Query<'_>) -> Result<Step, Msg> {
            if body == "done" {
                Ok(Step::Done(vec![]))
            } else {
                Ok(Step::Fetch(Request {
                    url: body.parse().unwrap(),
                    kind: Kind::Data,
                    form: None,
                }))
            }
        }
    }

    #[tokio::test]
    async fn an_additional_adapter_runs_without_engine_branches_and_cannot_escape_or_loop() {
        let locale = WebSearchLocale::default();
        let query = Query {
            text: "sample",
            allowed: &[],
            locale: &locale,
        };
        let mut calls = Vec::new();
        let found = run(&AdditionalSource, &query, |request| {
            calls.push(request.kind);
            std::future::ready(Ok(if request.kind == Kind::Document {
                "https://source.test/data".into()
            } else {
                "done".into()
            }))
        })
        .await
        .unwrap();
        assert!(found.is_empty());
        assert_eq!(calls, [Kind::Document, Kind::Data]);

        let mut calls = 0;
        let result = run(&AdditionalSource, &query, |_| {
            calls += 1;
            std::future::ready(Ok("https://127.0.0.1/data".into()))
        })
        .await;
        assert!(result.unwrap_err().text.contains("unsupported"));
        assert_eq!(
            calls, 1,
            "untrusted discovered URLs must fail before transport"
        );

        let mut calls = 0;
        let result = run(&AdditionalSource, &query, |_| {
            calls += 1;
            std::future::ready(Ok(format!("https://source.test/data?page={calls}")))
        })
        .await;
        assert!(result.unwrap_err().text.contains("request-step limit"));
        assert_eq!(calls, 4);
    }

    #[test]
    fn adapters_keep_their_query_and_locale_semantics() {
        let locale = WebSearchLocale {
            language: "zh-Hans".into(),
            region: "CN".into(),
        };
        let allowed = vec!["sqlite.org".into()];
        let query = Query {
            text: "SQLite release notes",
            allowed: &allowed,
            locale: &locale,
        };
        for &engine in AUTOMATIC {
            let provider = get(engine);
            assert_eq!(provider.spec().id, engine);
            let request = provider.start(&query).unwrap();
            assert!(provider.spec().allows(&request.url));
            if let Some(home) = provider.spec().home {
                assert!(provider.spec().allows(&home.parse().unwrap()));
            }
        }
        let bing = get(SearchEngine::Bing).start(&query).unwrap().url;
        assert_eq!(
            bing.query_pairs().find(|(k, _)| k == "q").unwrap().1,
            "SQLite release notes (site:sqlite.org)"
        );
        assert_eq!(bing.query_pairs().count(), 1);
        let ddg = get(SearchEngine::Duckduckgo).start(&query).unwrap().url;
        assert_eq!(
            ddg.query_pairs().find(|(k, _)| k == "q").unwrap().1,
            query.text
        );
        assert_eq!(
            ddg.query_pairs().find(|(k, _)| k == "kl").unwrap().1,
            "cn-zh"
        );
    }
    #[test]
    fn remote_dns_exception_only_covers_fixed_https_search_endpoints() {
        assert!(
            get(SearchEngine::Bing).spec().allows(
                &super::super::public::parse("https://www.bing.com/search?q=Rust").unwrap()
            )
        );
        assert!(
            get(SearchEngine::Duckduckgo)
                .spec()
                .allows(&super::super::public::parse("https://duckduckgo.com/?q=Rust").unwrap())
        );
        for value in [
            "http://www.bing.com/search?q=Rust",
            "https://www.bing.com/redirect",
            "https://www.bing.com.example.org/search",
            "https://example.org/search",
            "https://127.0.0.1/search",
            "https://www.bing.com:8443/search",
            "https://user:password@www.bing.com/search",
        ] {
            assert!(
                !get(SearchEngine::Bing)
                    .spec()
                    .allows(&reqwest::Url::parse(value).unwrap()),
                "{value}"
            );
        }
    }
}

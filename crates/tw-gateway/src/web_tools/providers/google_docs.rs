//! Google's legacy Docs research endpoint returns web results as JSON. Document
//! IDs are public request contexts, not credentials or result filters. Keep the
//! source identity distinct from the ordinary Google SERP.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use scraper::Html;
use serde_json::{Value, json};
use tw_api::{SearchEngine, WebSource};
use tw_types::Msg;

use super::super::{collect, error};
use super::{Endpoint, Kind, Profile, Provider, Query, Request, Spec, Step};

// Public Kubernetes community agendas, linked from:
// https://github.com/kubernetes/community/tree/master/sig-autoscaling
// https://github.com/kubernetes/community/tree/master/sig-architecture
// Search reads the web; it does not read or modify these documents. Eligibility
// can change independently of public readability, so failed contexts cool down.
const PATHS: &[&str] = &[
    "/document/d/1RvhQAEIrVLHbyNnuaT99-6u9ZUMp7BfkPupT2LAZK7w/explore/search",
    "/document/d/10QkXwiZfBL7wBKHHFslooXvGDKHA75jkkgiIXuKFryM/explore/search",
    "/document/d/1HtTI0rJEGP_MSf6eO87aCmx_tzpovPAAg7U2Zxwm8FE/explore/search",
];
static SPEC: Spec = Spec {
    id: SearchEngine::GoogleDocs,
    name: "Google (Docs)",
    profile: Profile::BrowserHttp1,
    min_interval_ms: 6000,
    home: None,
    endpoints: &[Endpoint {
        host: "docs.google.com",
        paths: PATHS,
    }],
};

#[derive(Default)]
struct State {
    preferred: usize,
    unavailable: [Option<Instant>; 3],
}

#[derive(Default)]
pub(super) struct GoogleDocs {
    state: Mutex<State>,
}

pub(super) static GOOGLE_DOCS: LazyLock<GoogleDocs> = LazyLock::new(GoogleDocs::default);

impl GoogleDocs {
    fn choose(&self, query: &Query<'_>) -> Option<Request> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        (0..PATHS.len())
            .map(|offset| (state.preferred + offset) % PATHS.len())
            .find(|&index| state.unavailable[index].is_none_or(|until| until <= now))
            .map(|index| request(index, query))
    }
}

impl Provider for GoogleDocs {
    fn spec(&self) -> &'static Spec {
        &SPEC
    }

    fn start(&self, query: &Query<'_>) -> Result<Request, Msg> {
        self.choose(query)
            .ok_or_else(|| error("all Google Docs search contexts are cooling down"))
    }

    fn recover(&self, request: &Request, failure: &Msg, query: &Query<'_>) -> Option<Request> {
        let status = failure.arg("http_status").parse::<u16>().ok()?;
        // Never switch documents to retry an engine-wide throttle, server error,
        // network failure, or unknown response. Those go to normal failover.
        if !matches!(status, 300..=399 | 401 | 403 | 404) {
            return None;
        }
        let index = PATHS.iter().position(|path| *path == request.url.path())?;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.unavailable[index] = Some(Instant::now() + Duration::from_secs(900));
        }
        self.choose(query)
    }

    fn parse(&self, request: &Request, body: &str, _: &Query<'_>) -> Result<Step, Msg> {
        let sources = results(body)?;
        if let Some(index) = PATHS.iter().position(|path| *path == request.url.path()) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.preferred = index;
            state.unavailable[index] = None;
        }
        Ok(Step::Done(sources))
    }
}

fn request(index: usize, query: &Query<'_>) -> Request {
    let language = match query.locale.language.as_str() {
        "" => "en",
        lang if lang.starts_with("zh-Hans") => "zh-CN",
        lang if lang.starts_with("zh-Hant") => "zh-TW",
        lang => lang,
    };
    // Twenty is the UI's maximum. One page avoids multiplying requests when
    // domain/relevance filtering removes some entries. Region is not supported
    // by this legacy protocol; do not silently add a country restriction.
    let payload = json!([
        "documentsuggest.search.search_request",
        query.text,
        [20, "CAA="],
        [language, language],
        1
    ]);
    Request {
        url: format!("https://docs.google.com{}", PATHS[index])
            .parse()
            .unwrap(),
        kind: Kind::Data,
        form: Some(
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("request", &payload.to_string())
                .finish(),
        ),
    }
}

fn text(value: &str) -> String {
    Html::parse_fragment(value)
        .root_element()
        .text()
        .collect::<String>()
}

fn results(body: &str) -> Result<Vec<WebSource>, Msg> {
    let invalid = || error("Google Docs returned an unrecognized search response");
    let body = body.trim_start();
    let body = if body.starts_with(")]}'") {
        body.split_once('\n').ok_or_else(invalid)?.1
    } else {
        body
    };
    let root: Value = serde_json::from_str(body).map_err(|_| invalid())?;
    let message = root.get(0).and_then(Value::as_array).ok_or_else(invalid)?;
    if message.first().and_then(Value::as_str) != Some("documentsuggest.search.search_response") {
        return Err(invalid());
    }
    let records = match message.get(1) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(records)) => records,
        _ => return Err(invalid()),
    };
    let mut sources = Vec::new();
    for record in records {
        let row = record.as_array().ok_or_else(invalid)?;
        let organic = match row.first() {
            Some(Value::Null) => continue, // Knowledge card, not an organic result.
            Some(Value::Array(organic)) => organic,
            _ => return Err(invalid()),
        };
        let url = organic
            .first()
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let title = organic.get(1).and_then(Value::as_str).ok_or_else(invalid)?;
        let snippet = organic.get(2).and_then(Value::as_str).ok_or_else(invalid)?;
        let title = text(title);
        if title.trim().is_empty() {
            continue;
        }
        if let Some(source) = collect::source(SearchEngine::GoogleDocs, url, &title, &text(snippet))
            && !sources.iter().any(|s: &WebSource| s.url == source.url)
        {
            sources.push(source);
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fixed_google_contexts_are_allowed() {
        for path in PATHS {
            assert!(SPEC.allows(&format!("https://docs.google.com{path}").parse().unwrap()));
        }
        for url in [
            "https://docs.google.com/document/d/arbitrary/explore/search",
            "https://docs.google.com/document/d/private/edit",
            "https://accounts.google.com/ServiceLogin",
            "https://127.0.0.1/",
            "http://docs.google.com/",
        ] {
            assert!(!SPEC.allows(&url.parse().unwrap()), "{url}");
        }
    }

    #[test]
    fn parses_organic_json_without_turning_invalid_responses_into_empty_success() {
        let body = json!([[
            "documentsuggest.search.search_response",
            [
                [
                    [
                        "https://docs.example.org/task?x=1&utm_source=google#part",
                        "Task<b>Group</b> &amp; API",
                        "Use <b>TaskGroup</b> &amp; await."
                    ],
                    null
                ],
                [null, "knowledge card"],
                [[
                    "https://docs.example.org/task?x=1#other",
                    "duplicate",
                    "same URL"
                ]],
                [["javascript:alert(1)", "invalid URL", "ignore"]]
            ],
            "CAo="
        ]]);
        let found = results(&format!(")]}}'\n{body}")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "TaskGroup & API");
        assert_eq!(found[0].text, "Use TaskGroup & await.");
        assert_eq!(found[0].url, "https://docs.example.org/task?x=1");
        assert_eq!(found[0].engine, Some(SearchEngine::GoogleDocs));
        for empty in [
            r#"[["documentsuggest.search.search_response", null]]"#,
            r#"[["documentsuggest.search.search_response", []]]"#,
        ] {
            assert!(results(empty).unwrap().is_empty());
        }
        for invalid in [
            "<html>Sign in</html>",
            "[]",
            "{}",
            ")]}'",
            r#"[["error", null]]"#,
            r#"[["documentsuggest.search.search_response", {}]]"#,
            r#"[["documentsuggest.search.search_response", [[]]]]"#,
            r#"[["documentsuggest.search.search_response", [[[123,"title","text"]]]]]"#,
        ] {
            assert!(results(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn context_failures_are_bounded_and_remembered_but_throttles_never_rotate() {
        let provider = GoogleDocs::default();
        let locale = tw_api::WebSearchLocale {
            language: "zh-Hans".into(),
            region: "CN".into(),
        };
        let query = Query {
            text: "Rust & 中文 + ?",
            allowed: &[],
            locale: &locale,
        };
        let first = provider.start(&query).unwrap();
        let fields: Vec<_> =
            url::form_urlencoded::parse(first.form.as_ref().unwrap().as_bytes()).collect();
        let body: Value = serde_json::from_str(&fields[0].1).unwrap();
        assert_eq!(body[1], query.text);
        assert_eq!(body[3], json!(["zh-CN", "zh-CN"]));
        assert!(
            first.url.query().is_none(),
            "query belongs only in the POST body"
        );
        let mut calls = Vec::new();
        super::super::run(&provider, &query, |request| {
            calls.push(request.url.path().to_string());
            std::future::ready(if calls.len() == 1 {
                Err(super::super::super::http_failure(
                    reqwest::StatusCode::FORBIDDEN,
                    &Default::default(),
                ))
            } else {
                Ok(r#"[["documentsuggest.search.search_response", []]]"#.into())
            })
        })
        .await
        .unwrap();
        assert_eq!(calls, [PATHS[0], PATHS[1]]);
        assert_eq!(provider.start(&query).unwrap().url.path(), PATHS[1]);
        for status in [429, 500, 503] {
            let mut calls = 0;
            let result = super::super::run(&provider, &query, |_| {
                calls += 1;
                std::future::ready(Err(super::super::super::http_failure(
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    &Default::default(),
                )))
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
        let mut calls = 0;
        assert!(
            super::super::run(&provider, &query, |_| {
                calls += 1;
                std::future::ready(Err(super::super::super::http_failure(
                    reqwest::StatusCode::NOT_FOUND,
                    &Default::default(),
                )))
            })
            .await
            .is_err()
        );
        assert_eq!(calls, 2, "first document is already cooling down");
        assert!(provider.start(&query).is_err());
    }
}

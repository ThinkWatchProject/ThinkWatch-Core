//! Read the same first-party organic results used by DuckDuckGo's regular site.
//! The HTML endpoint frequently challenges even when this normal flow works.
//! Never execute returned JavaScript or consume ads / generated answer text.

use scraper::{Html, Selector};
use serde::Deserialize;
use std::sync::LazyLock;
use tw_api::{SearchEngine, WebSource};
use tw_types::Msg;

use super::super::{collect, error};
use super::{Endpoint, Kind, Profile, Provider, Query, Request, Spec, Step};

static CHALLENGE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("#challenge-form, #anomaly-form, script[src*='anomaly.js']").unwrap()
});
static PRELOAD: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("script#deep_preload_script[src], link#deep_preload_link[href]").unwrap()
});

pub(super) fn is_data_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("links.duckduckgo.com" | "duckduckgo.com")
        )
        && url.port_or_known_default() == Some(443)
        && url.path() == "/d.js"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

pub(super) fn data_url(body: &str, query: &str) -> Result<reqwest::Url, Msg> {
    let doc = Html::parse_document(body);
    if doc.select(&CHALLENGE).next().is_some() {
        return Err(error("search engine requires a verification challenge"));
    }
    let link = doc
        .select(&PRELOAD)
        .next()
        .and_then(|node| node.attr("src").or_else(|| node.attr("href")))
        .ok_or_else(|| error("DuckDuckGo search page layout was not recognized"))?;
    let base = reqwest::Url::parse(URL).unwrap();
    let url = base
        .join(link)
        .map_err(|_| error("invalid search data URL"))?;
    // This URL comes from untrusted HTML. Do not grant its host proxy remote DNS
    // unless it is exactly a known first-party data endpoint for this query.
    let queries: Vec<_> = url.query_pairs().filter(|(key, _)| key == "q").collect();
    if !is_data_url(&url) || queries.len() != 1 || queries[0].1 != query {
        return Err(error("DuckDuckGo returned an unexpected search data URL"));
    }
    Ok(url)
}

#[derive(Deserialize)]
struct Row {
    #[serde(default)]
    u: Option<String>,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    a: Option<String>,
    #[serde(default)]
    n: Option<String>,
}

fn text(html: &str) -> String {
    Html::parse_fragment(html).root_element().text().collect()
}

pub(super) fn results(body: &str) -> Result<Vec<WebSource>, Msg> {
    // Only the 'd' (organic) array. The 'a' array and deepPayload can contain
    // advertisements and AI answers. Parse JSON as data, never evaluate code.
    let data = ["DDG.pageLayout.load('d',", "DDG.pageLayout.load(\"d\","]
        .iter()
        .find_map(|marker| body.split_once(marker).map(|(_, tail)| tail))
        .ok_or_else(|| {
            error(
                if body.contains("isJsaChallenge") || body.contains("challenge-form") {
                    "search engine requires browser verification"
                } else {
                    "DuckDuckGo did not return organic search results"
                },
            )
        })?;
    let mut stream = serde_json::Deserializer::from_str(data).into_iter::<Vec<Row>>();
    let rows = stream
        .next()
        .ok_or_else(|| error("DuckDuckGo search results are missing"))?
        .map_err(|_| error("invalid DuckDuckGo search results"))?;
    if !data[stream.byte_offset()..].trim_start().starts_with(')') {
        return Err(error("invalid DuckDuckGo search result wrapper"));
    }
    let mut sources = Vec::new();
    let mut malformed = false;
    for row in rows {
        match (row.u, row.t) {
            (Some(url), Some(title)) if !title.trim().is_empty() => {
                if let Some(source) = collect::source(
                    SearchEngine::Duckduckgo,
                    &url,
                    &text(&title),
                    &text(&row.a.unwrap_or_default()),
                ) {
                    sources.push(source);
                } else {
                    malformed = true;
                }
            }
            // A final pagination object is not a result; never follow it here.
            (None, None) if row.n.is_some() => {}
            _ => malformed = true,
        }
    }
    if malformed && sources.is_empty() {
        return Err(error("DuckDuckGo search result layout was not recognized"));
    }
    Ok(sources)
}

const URL: &str = "https://duckduckgo.com/";
static SPEC: Spec = Spec {
    id: SearchEngine::Duckduckgo,
    name: "DuckDuckGo",
    profile: Profile::Basic,
    min_interval_ms: 0,
    home: None,
    endpoints: &[
        Endpoint {
            host: "duckduckgo.com",
            paths: &["/", "/d.js"],
        },
        Endpoint {
            host: "links.duckduckgo.com",
            paths: &["/d.js"],
        },
    ],
};
pub(super) struct Duckduckgo;
impl Provider for Duckduckgo {
    fn spec(&self) -> &'static Spec {
        &SPEC
    }
    fn start(&self, query: &Query<'_>) -> Result<Request, Msg> {
        let mut url = reqwest::Url::parse(URL).unwrap();
        url.query_pairs_mut()
            .append_pair("q", query.text)
            .append_pair("ia", "web");
        if let Some(market) = super::locale::duckduckgo_market(query.locale) {
            url.query_pairs_mut().append_pair("kl", market);
        }
        Ok(Request {
            url,
            kind: Kind::Document,
            form: None,
        })
    }
    fn parse(&self, request: &Request, body: &str, query: &Query<'_>) -> Result<Step, Msg> {
        match request.kind {
            Kind::Document => data_url(body, query.text).map(|url| {
                Step::Fetch(Request {
                    url,
                    kind: Kind::Data,
                    form: None,
                })
            }),
            Kind::Data => results(body).map(Step::Done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_first_party_data_without_executing_scripts() {
        let url = data_url(
            r#"<script>throw 'do not execute'</script><script id="deep_preload_script" src="https://links.duckduckgo.com/d.js?q=Rust%20OnceLock&amp;vqd=example"></script>"#,
            "Rust OnceLock",
        ).unwrap();
        assert_eq!(url.query_pairs().count(), 2);
        assert!(is_data_url(&url));
        assert!(
            data_url(
                r#"<link id="deep_preload_link" href="/d.js?q=中文">"#,
                "中文"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_changed_queries_and_untrusted_data_destinations() {
        for url in [
            "https://evil.example/d.js?q=Rust",
            "https://links.duckduckgo.com.evil.example/d.js?q=Rust",
            "https://127.0.0.1/d.js?q=Rust",
            "http://links.duckduckgo.com/d.js?q=Rust",
            "https://links.duckduckgo.com:8443/d.js?q=Rust",
            "https://user@links.duckduckgo.com/d.js?q=Rust",
            "https://links.duckduckgo.com/redirect?q=Rust",
            "https://links.duckduckgo.com/d.js?q=Python",
            "https://links.duckduckgo.com/d.js?q=Rust&q=Python",
        ] {
            assert!(
                data_url(
                    &format!(r#"<script id="deep_preload_script" src="{url}"></script>"#),
                    "Rust"
                )
                .is_err(),
                "{url}"
            );
        }
        assert!(
            data_url("<form id='challenge-form'></form>", "Rust")
                .unwrap_err()
                .text
                .contains("verification")
        );
    }

    #[test]
    fn reads_only_organic_json_and_preserves_markup_text_and_escaped_punctuation() {
        let body = r#"window.execDeep = function() { DDG.pageLayout.load('a',[{"u":"https://ads.example/","t":"Ad"}]); DDG.deep.deepPayload={"answer":"invented answer"}; DDG.pageLayout.load('d',[{"u":"https://docs.example/a?x=1&y=2&utm_source=search#top","t":"Rust &amp; 中文","a":"Use <b>OnceLock</b> with \"quoted ], text\"."},{"n":"/d.js?next=1"}]); }"#;
        let found = results(body).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, "https://docs.example/a?x=1&y=2");
        assert_eq!(found[0].title, "Rust & 中文");
        assert_eq!(found[0].text, "Use OnceLock with \"quoted ], text\".");
        assert!(found[0].published_at.is_none());
        assert!(results("DDG.pageLayout.load('d',[]);").unwrap().is_empty());
    }

    #[test]
    fn malformed_data_and_challenges_are_failures_not_empty_successes() {
        for body in [
            "DDG.pageLayout.load('a',[]);",
            "DDG.pageLayout.load('d',[{bad}]);",
            "DDG.pageLayout.load('d',[{\"new_field\":1}]);",
            "DDG.pageLayout.load('d',[] + injected());",
            "<form id='challenge-form'>",
        ] {
            assert!(results(body).is_err(), "{body}");
        }
        assert!(
            results("return {isJsaChallenge: true};")
                .unwrap_err()
                .text
                .contains("browser verification")
        );
        // A normal result may discuss a challenge; it is still a valid result.
        assert!(results(r#"DDG.pageLayout.load('d',[{"u":"https://example.org/","t":"isJsaChallenge and challenge-form"}]);"#).is_ok());
    }
}

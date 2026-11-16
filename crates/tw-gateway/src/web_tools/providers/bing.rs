use super::super::{collect::source, error};
use scraper::{Html, Selector};
use std::sync::LazyLock;
use tw_api::{SearchEngine, WebSource};
use tw_types::Msg;

static CHALLENGE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("#challenge-form, #anomaly-form, #b_captcha, .b_captcha, #turnstile-widget")
        .unwrap()
});
static ROWS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#b_results .b_algo").unwrap());
static LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h2 a[href]").unwrap());
static SNIPPET: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".b_caption p, .b_snippet").unwrap());
static EMPTY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#b_results .b_no").unwrap());
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn verification(doc: &Html) -> Result<(), Msg> {
    if doc.select(&CHALLENGE).next().is_some() {
        Err(error("search engine requires a verification challenge"))
    } else {
        Ok(())
    }
}

fn html_results(body: &str) -> Result<Vec<WebSource>, Msg> {
    use base64::Engine;
    let doc = Html::parse_document(body);
    verification(&doc)?;
    let mut sources = Vec::new();
    let base = reqwest::Url::parse(URL).unwrap();
    for row in doc.select(&ROWS) {
        let Some(link) = row.select(&LINK).next() else {
            continue;
        };
        let Some(href) = link.attr("href") else {
            continue;
        };
        let Ok(url) = base.join(href) else { continue };
        let destination = if matches!(
            url.host_str(),
            Some("www.bing.com" | "cn.bing.com" | "bing.com")
        ) {
            if !matches!(url.path(), "/ck/a" | "/ck/a/") {
                continue;
            }
            let Some(encoded) = url
                .query_pairs()
                .find_map(|(key, value)| (key == "u").then(|| value.into_owned()))
            else {
                continue;
            };
            let Some(encoded) = encoded.strip_prefix("a1") else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.trim_end_matches('='))
            else {
                continue;
            };
            let Ok(destination) = String::from_utf8(bytes) else {
                continue;
            };
            destination
        } else {
            url.to_string()
        };
        let title = link.text().collect::<String>();
        let snippet = row
            .select(&SNIPPET)
            .next()
            .map(|node| node.text().collect::<String>())
            .unwrap_or_default();
        if let Some(source) = source(SearchEngine::Bing, &destination, &title, &snippet) {
            sources.push(source);
        }
    }
    if sources.is_empty() && doc.select(&EMPTY).next().is_none() {
        return Err(error("Bing search page layout was not recognized"));
    }
    Ok(sources)
}

fn rss(body: &str) -> Result<Vec<WebSource>, Msg> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(false);
    let mut items = Vec::new();
    let mut in_item = false;
    let mut rss = false;
    let (mut tag, mut title, mut url, mut text) =
        (Vec::new(), String::new(), String::new(), String::new());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                tag = e.name().as_ref().as_bytes().to_vec();
                if tag == b"rss" {
                    rss = true;
                }
                if tag == b"item" {
                    in_item = true;
                    title.clear();
                    url.clear();
                    text.clear();
                }
            }
            Ok(event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) if in_item => {
                let value = match &event {
                    Event::Text(e) => e.as_ref().to_string(),
                    Event::CData(e) => e.as_ref().to_string(),
                    Event::GeneralRef(e) => {
                        quick_xml::escape::unescape(&format!("&{};", e.as_ref()))
                            .map_err(|_| error("invalid search XML entity"))?
                            .into_owned()
                    }
                    _ => unreachable!(),
                };
                match tag.as_slice() {
                    b"title" => title.push_str(&value),
                    b"link" => url.push_str(&value),
                    b"description" => text.push_str(&value),
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == "item" => {
                if let Some(s) = source(
                    SearchEngine::Bing,
                    &url,
                    &title,
                    &clean(
                        &Html::parse_fragment(&text)
                            .root_element()
                            .text()
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                ) {
                    // Bing RSS dates describe the search item, not a verified
                    // publication date of the linked page. Do not invent recency.
                    items.push(s);
                }
                in_item = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(error("invalid search XML")),
            _ => {}
        }
    }
    if !rss {
        return Err(error("search engine did not return RSS results"));
    }
    Ok(items)
}

fn search(body: &str) -> Result<Vec<WebSource>, Msg> {
    if body.trim_start().starts_with("<?xml") || body.trim_start().starts_with("<rss") {
        rss(body)
    } else {
        html_results(body)
    }
}

use super::{Endpoint, Kind, Profile, Provider, Query, Request, Spec, Step};
const URL: &str = "https://www.bing.com/search";
static SPEC: Spec = Spec {
    id: SearchEngine::Bing,
    name: "Bing",
    profile: Profile::BrowserHttp1,
    min_interval_ms: 0,
    home: Some("https://www.bing.com/"),
    endpoints: &[
        Endpoint {
            host: "www.bing.com",
            paths: &["/", "/search"],
        },
        Endpoint {
            host: "cn.bing.com",
            paths: &["/", "/search"],
        },
    ],
};
pub(super) struct Bing;
impl Provider for Bing {
    fn spec(&self) -> &'static Spec {
        &SPEC
    }
    fn start(&self, query: &Query<'_>) -> Result<Request, Msg> {
        let mut q = query.text.to_owned();
        if !query.allowed.is_empty() {
            q.push_str(&format!(
                " ({})",
                query
                    .allowed
                    .iter()
                    .map(|d| format!("site:{d}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ));
        }
        // Language is supplied by the browser headers. Forced cc/setlang URL
        // parameters can select a regional backend that broadens the query.
        let mut url = reqwest::Url::parse(URL).unwrap();
        url.query_pairs_mut().append_pair("q", &q);
        Ok(Request {
            url,
            kind: Kind::Document,
            form: None,
        })
    }
    fn parse(&self, _: &Request, body: &str, _: &Query<'_>) -> Result<Step, Msg> {
        search(body).map(Step::Done)
    }
    fn validate_home(&self, body: &str) -> Result<(), Msg> {
        verification(&Html::parse_document(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bing_web_results_exclude_ads_and_decode_only_known_tracking_links() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://docs.example.org/guide?a=1&b=2#section");
        let body = format!(
            r#"<ol id="b_results">
            <li class="b_ad"><h2><a href="https://ads.example.org/">Advertisement</a></h2></li>
            <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?u=a1{encoded}">The <strong>official</strong> guide</a></h2><div class="b_caption"><p>Use <b>TaskGroup</b> &amp; await tasks.</p></div></li>
            <li class="b_algo"><h2><a href="https://example.org/direct">Direct source</a></h2><div class="b_snippet">Plain snippet</div></li>
            <li class="b_algo"><h2><a href="/ck/a?u=a1invalid">Broken tracker</a></h2></li>
            <li class="b_algo"><h2><a href="/search?q=other">Related search</a></h2></li>
            </ol>"#
        );
        let rows = search(&body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, "https://docs.example.org/guide?a=1&b=2");
        assert_eq!(rows[0].title, "The official guide");
        assert_eq!(rows[0].text, "Use TaskGroup & await tasks.");
        assert!(rows[0].published_at.is_none());
        assert_eq!(rows[1].text, "Plain snippet");
        assert!(
            search(r#"<ol id="b_results"><li class="b_no">No results</li></ol>"#)
                .unwrap()
                .is_empty()
        );
        assert!(search(r#"<div id="b_captcha">Verify</div>"#).is_err());
        assert!(
            search(r#"<div id="turnstile-widget"></div>"#)
                .unwrap_err()
                .text
                .contains("verification")
        );
        // Merely mentioning a challenge in scripts or an article is not one.
        assert!(
            verification(&Html::parse_document(
                r#"<script>const id = 'turnstile-widget';</script>"#
            ))
            .is_ok()
        );
        assert!(search("<main>Unexpected layout</main>").is_err());
    }
}
#[cfg(test)]
mod entity_tests {
    use super::*;
    #[test]
    fn rss_preserves_entities_cdata_and_query_parameters() {
        let result = search( r#"<rss><channel><item><title>Rust &amp; Web &#x4E2D;&#25991;</title><link>https://example.org/?a=1&amp;b=2</link><description><![CDATA[<p>A &amp; B</p>]]></description></item></channel></rss>"#).unwrap();
        assert_eq!(result[0].title, "Rust & Web 中文");
        assert_eq!(result[0].url, "https://example.org/?a=1&b=2");
        assert_eq!(result[0].text, "A & B");
    }

    #[test]
    fn search_item_dates_are_not_page_publication_dates() {
        let result = search( "<rss><channel><item><title>Old page</title><link>https://example.org/</link><pubDate>Mon, 16 Nov 2026 00:00:00 GMT</pubDate></item></channel></rss>").unwrap();
        assert!(result[0].published_at.is_none());
    }
}

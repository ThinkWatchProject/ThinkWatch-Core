use scraper::{Html, Selector};
use tw_api::{SearchEngine, WebSource};
use tw_types::Msg;

use super::error;

fn selector(s: &str) -> Selector {
    Selector::parse(s).expect("static CSS selector")
}
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn search(engine: SearchEngine, body: &str) -> Result<Vec<WebSource>, Msg> {
    if body.contains("anomaly.js") || body.contains("challenge-form") || body.contains("b_captcha")
    {
        return Err(error("search engine requires a verification challenge"));
    }
    match engine {
        SearchEngine::Bing => rss(body),
        SearchEngine::Duckduckgo => duckduckgo(body),
    }
}

fn source(engine: SearchEngine, url: &str, title: &str, text: &str) -> Option<WebSource> {
    let mut url = super::public::parse(url).ok()?;
    url.set_fragment(None);
    let params: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !matches!(k.as_ref(), "gclid" | "fbclid"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if params.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(params);
    }
    Some(WebSource {
        url: url.to_string(),
        title: clean(title).chars().take(500).collect(),
        text: clean(text).chars().take(2000).collect(),
        engine: Some(engine),
        published_at: None,
    })
}

fn rss(body: &str) -> Result<Vec<WebSource>, Msg> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(false);
    let mut items = Vec::new();
    let mut in_item = false;
    let mut rss = false;
    let (mut tag, mut title, mut url, mut text, mut date) = (
        Vec::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    );
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
                    date.clear();
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
                    b"pubDate" => date.push_str(&value),
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == "item" => {
                if let Some(mut s) = source(
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
                    if !date.is_empty() {
                        s.published_at = Some(date.clone());
                    }
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

fn duckduckgo(body: &str) -> Result<Vec<WebSource>, Msg> {
    let doc = Html::parse_document(body);
    let mut result = Vec::new();
    for row in doc.select(&selector(".result")) {
        let Some(link) = row.select(&selector(".result__a")).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let absolute = if href.starts_with("//") {
            format!("https:{href}")
        } else {
            href.to_string()
        };
        let Ok(url) = reqwest::Url::parse(&absolute) else {
            continue;
        };
        let destination = url
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned())
            .unwrap_or(absolute);
        let title = link.text().collect::<Vec<_>>().join(" ");
        let text = row
            .select(&selector(".result__snippet"))
            .next()
            .map(|n| n.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        if let Some(s) = source(SearchEngine::Duckduckgo, &destination, &title, &text) {
            result.push(s);
        }
    }
    if result.is_empty() && !body.contains("no-results") {
        return Err(error("search page layout was not recognized"));
    }
    Ok(result)
}

pub(super) fn page(body: &str) -> (String, String) {
    let mut doc = Html::parse_document(body);
    let remove: Vec<_> = doc
        .select(&selector(
            "script,style,noscript,nav,footer,header,aside,form,svg,template",
        ))
        .map(|node| node.id())
        .collect();
    for id in remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
    let title = doc
        .select(&selector("title"))
        .next()
        .map(|n| clean(&n.text().collect::<Vec<_>>().join(" ")))
        .unwrap_or_default();
    let root = doc
        .select(&selector("article,main,[role=main]"))
        .next()
        .unwrap_or_else(|| doc.root_element());
    let text = root
        .text()
        .map(clean)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (title.chars().take(500).collect(), text)
}

pub(super) fn excerpt(text: &str, query: &str, limit: usize) -> String {
    let words: Vec<_> = query
        .split_whitespace()
        .filter(|s| s.chars().count() > 2)
        .map(str::to_lowercase)
        .collect();
    let mut lines: Vec<_> = text.lines().enumerate().collect();
    lines.sort_by_key(|(i, line)| {
        let l = line.to_lowercase();
        (
            std::cmp::Reverse(words.iter().filter(|w| l.contains(w.as_str())).count()),
            *i,
        )
    });
    lines
        .iter()
        .take(12)
        .map(|(_, s)| *s)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(limit)
        .collect()
}

pub(super) fn find(text: &str, pattern: &str, limit: usize) -> String {
    let lines: Vec<_> = text.lines().collect();
    let pattern = pattern.to_lowercase();
    let mut selected = std::collections::BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        if line.to_lowercase().contains(&pattern) {
            selected.extend(i.saturating_sub(1)..=(i + 1).min(lines.len().saturating_sub(1)));
        }
    }
    if selected.is_empty() {
        return "No matching text was found.".into();
    }
    selected
        .into_iter()
        .map(|i| format!("{}: {}", i + 1, lines[i]))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_search_sources_and_rejects_challenges() {
        let r = search(SearchEngine::Bing, "<rss><channel><item><title>Release</title><link>https://example.org/?utm_source=x</link><description>Version 3</description></item></channel></rss>").unwrap();
        assert_eq!(r[0].url, "https://example.org/");
        let html = r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2F">Release</a><a class="result__snippet">Version 3</a></div>"#;
        assert_eq!(
            search(SearchEngine::Duckduckgo, html).unwrap()[0].text,
            "Version 3"
        );
        assert!(search(SearchEngine::Duckduckgo, "<form id='challenge-form'>").is_err());
        assert!(search(SearchEngine::Bing, "<html>Access denied</html>").is_err());
    }
    #[test]
    fn extracts_readable_text_and_finds_unicode_context() {
        let (_, text) = page(
            "<title>Doc</title><nav>menu</nav><main><p>你好</p><p>Version 3</p><script>secret</script><p>Next</p></main>",
        );
        assert!(!text.contains("secret"));
        assert!(!text.contains("menu"));
        let found = find(&text, "VERSION", 1000);
        assert!(found.contains("你好"));
        assert!(found.contains("Version 3"));
    }
}

#[cfg(test)]
mod entity_tests {
    use super::*;
    #[test]
    fn rss_preserves_entities_cdata_and_query_parameters() {
        let result = search(SearchEngine::Bing, r#"<rss><channel><item><title>Rust &amp; Web &#x4E2D;&#25991;</title><link>https://example.org/?a=1&amp;b=2</link><description><![CDATA[<p>A &amp; B</p>]]></description></item></channel></rss>"#).unwrap();
        assert_eq!(result[0].title, "Rust & Web 中文");
        assert_eq!(result[0].url, "https://example.org/?a=1&b=2");
        assert_eq!(result[0].text, "A & B");
    }
}

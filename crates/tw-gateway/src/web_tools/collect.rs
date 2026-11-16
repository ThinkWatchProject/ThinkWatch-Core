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

pub(super) fn source(
    engine: SearchEngine,
    url: &str,
    title: &str,
    text: &str,
) -> Option<WebSource> {
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

pub(super) fn page(body: &str) -> Result<(String, String), Msg> {
    let mut doc = Html::parse_document(body);
    if doc
        .select(&selector(
            "#challenge-form,#cf-challenge-running,#challenge-running",
        ))
        .next()
        .is_some()
    {
        return Err(error("page requires a verification challenge"));
    }
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
        .select(&selector("main,[role=main]"))
        .next()
        .or_else(|| doc.select(&selector("article")).next())
        .or_else(|| doc.select(&selector("body")).next())
        .unwrap_or_else(|| doc.root_element());
    // Text nodes are not paragraphs: inline <a>, <code> and <em> elements must
    // not split a sentence into unrelated lines. Use an explicit stack so a
    // deeply nested public page cannot overflow the call stack.
    let mut text = String::new();
    let mut stack: Vec<_> = root.children().rev().map(|node| (node, false)).collect();
    while let Some((node, closing)) = stack.pop() {
        if let Some(element) = node.value().as_element() {
            let block = matches!(
                element.name(),
                "p" | "div"
                    | "section"
                    | "article"
                    | "main"
                    | "li"
                    | "ul"
                    | "ol"
                    | "pre"
                    | "blockquote"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "br"
                    | "tr"
                    | "dt"
                    | "dd"
            );
            if block && !text.ends_with('\n') {
                text.push('\n');
            }
            if closing && matches!(element.name(), "td" | "th") {
                text.push(' ');
            }
            if !closing {
                stack.push((node, true));
                stack.extend(node.children().rev().map(|child| (child, false)));
            }
        } else if let Some(value) = node.value().as_text() {
            text.push_str(value);
        }
    }
    let text = text
        .lines()
        .map(clean)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return Err(error(
            "page contained no readable text; it may require JavaScript",
        ));
    }
    Ok((title.chars().take(500).collect(), text))
}

pub(super) fn excerpt(text: &str, query: &str, limit: usize) -> String {
    let words: Vec<_> = query
        .split_whitespace()
        .filter(|s| s.chars().count() > 2)
        .map(str::to_lowercase)
        .collect();
    let lines: Vec<_> = text.lines().collect();
    let score = |line: &str| {
        let l = line.to_lowercase();
        words.iter().filter(|w| l.contains(w.as_str())).count()
    };
    let best = lines
        .iter()
        .enumerate()
        .max_by_key(|(i, line)| (score(line), std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
        .unwrap_or(0);
    // Preserve source order and surrounding context instead of sorting sentences
    // by keyword count, which can change what the page actually says.
    lines[best.saturating_sub(1)..lines.len().min(best + 11)]
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
    fn extracts_readable_text_and_finds_unicode_context() {
        let (_, text) = page(
            "<title>Doc</title><nav>menu</nav><main><p>你好</p><p>Version 3</p><script>secret</script><p>Next</p></main>",
        ).unwrap();
        assert!(!text.contains("secret"));
        assert!(!text.contains("menu"));
        let found = find(&text, "VERSION", 1000);
        assert!(found.contains("你好"));
        assert!(found.contains("Version 3"));
    }

    #[test]
    fn inline_markup_does_not_break_sentences_or_phrase_searches() {
        let (_, text) = page(
            "<title>Doc</title><main><p>Use <code>asyncio.TaskGroup</code> to await <em>all</em> tasks.</p><p>Errors cancel the remaining tasks.</p></main>",
        ).unwrap();
        assert_eq!(
            text,
            "Use asyncio.TaskGroup to await all tasks.\nErrors cancel the remaining tasks."
        );
        assert!(find(&text, "await all tasks", 1000).contains("Use asyncio.TaskGroup"));
    }

    #[test]
    fn excerpts_keep_context_in_the_original_order() {
        let text = "Introduction\nDo not use this on older versions.\nTaskGroup first appeared in Python 3.11.\nIn earlier versions, use gather.\nTaskGroup handles child tasks together.";
        let extracted = excerpt(text, "TaskGroup Python", 1000);
        assert!(extracted.starts_with("Do not use this on older versions."));
        assert!(
            extracted.find("first appeared").unwrap()
                < extracted.find("In earlier versions").unwrap()
        );
        assert!(
            extracted.find("In earlier versions").unwrap()
                < extracted.find("handles child").unwrap()
        );
        assert_eq!(excerpt("", "anything", 100), "");
    }

    #[test]
    fn empty_pages_and_verification_are_not_returned_as_content() {
        assert!(page("<html><body><script>render()</script></body></html>").is_err());
        assert!(page("<form id='challenge-form'>Verify you are human</form>").is_err());
        // Cloudflare also injects scripts into successfully loaded normal pages.
        assert!(page("<main>Article text</main><script src='/cdn-cgi/challenge-platform/scripts/jsd/main.js'></script>").is_ok());
        let (_, text) = page("<article>Advertisement</article><main><table><tr><td>Version</td><td>3.11</td></tr></table></main>").unwrap();
        assert_eq!(text, "Version 3.11");
    }
}

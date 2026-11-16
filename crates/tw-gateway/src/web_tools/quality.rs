//! A relevance floor for public search pages. HTTP 200 is not evidence that an
//! engine honored the query: some pages silently fall back to a broader search.

use std::collections::BTreeSet;

use tw_api::WebSource;

fn cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9fff | 0x20000..=0x323af | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

fn word(s: &str) -> String {
    // Common singular/plural differences should not hide otherwise relevant pages.
    if s.len() > 4 && s.ends_with('s') && !s.ends_with("ss") {
        s[..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn terms(s: &str, query: bool) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for part in s.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        let mut ascii = String::new();
        let mut previous = None;
        for c in part.chars() {
            if cjk(c) {
                if !ascii.is_empty() {
                    terms.insert(word(&ascii));
                    ascii.clear();
                }
                if let Some(p) = previous {
                    terms.insert(format!("{p}{c}"));
                }
                previous = Some(c);
            } else {
                previous = None;
                ascii.push(c);
            }
        }
        if !ascii.is_empty() {
            terms.insert(word(&ascii));
        }
    }
    if query {
        terms.retain(|s| {
            s.chars().count() > 1
                && !matches!(
                    s.as_str(),
                    "the"
                        | "an"
                        | "and"
                        | "or"
                        | "for"
                        | "of"
                        | "to"
                        | "in"
                        | "on"
                        | "is"
                        | "are"
                        | "with"
                        | "how"
                        | "what"
                        | "where"
                        | "when"
                        | "who"
                        | "why"
                        | "do"
                        | "does"
                        | "can"
                        | "please"
                        | "find"
                        | "search"
                        | "about"
                        | "tell"
                        | "me"
                        | "use"
                        | "using"
                        | "documentation"
                        | "official"
                        | "tutorial"
                        | "guide"
                )
        });
    }
    terms
}

/// Reject clearly unrelated results while preserving the engine's ranking.
/// Keyword coverage must not promote a secondary source over the primary source
/// merely because a longer snippet repeats more query terms.
/// This is a lexical sanity check, not a claim to judge factual correctness.
/// Return whether the remaining set is usable. A lone partial match in a mostly
/// unrelated response must not stop failover; a real single-result response or
/// an exact lexical match still can. This does not establish factual accuracy.
pub(super) fn filter(query: &str, sources: &mut Vec<WebSource>) -> bool {
    // Operators constrain the engine, not the words that must appear in a page.
    let query = query
        .split_whitespace()
        .filter(|part| {
            !part.starts_with('-')
                && !part.to_ascii_lowercase().starts_with("site:")
                && !part.to_ascii_lowercase().starts_with("filetype:")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let wanted = terms(&query, true);
    if wanted.is_empty() {
        return true;
    }
    let score = |source: &WebSource| {
        // A search/redirect URL can echo the entire query even when its contents
        // are unrelated. Only the origin and path contribute URL evidence.
        let address = reqwest::Url::parse(&source.url)
            .map(|url| format!("{} {}", url.host_str().unwrap_or_default(), url.path()))
            .unwrap_or_default();
        let visible = terms(
            &format!("{} {} {}", source.title, address, source.text),
            false,
        );
        wanted.intersection(&visible).count()
    };
    // One matching word out of a multi-word query is usually a broadened result.
    // Keep this conservative: natural-language questions and cross-language
    // results may share only a few terms, even when they answer the question.
    let minimum = if wanted.len() <= 2 { 1 } else { 2 };
    let original = sources.len();
    let mut best = 0;
    sources.retain(|s| {
        let matched = score(s);
        best = best.max(matched);
        matched >= minimum
    });
    !sources.is_empty() && (original < 4 || sources.len() * 4 > original || best == wanted.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(title: &str, url: &str, text: &str) -> WebSource {
        WebSource {
            title: title.into(),
            url: url.into(),
            text: text.into(),
            engine: None,
            published_at: None,
        }
    }

    #[test]
    fn sparse_partial_matches_do_not_stop_failover() {
        for (query, partial) in [
            (
                "SQLite release notes",
                source(
                    "SQLite downloads",
                    "https://sqlite.org/download.html",
                    "Download the current release",
                ),
            ),
            (
                "OpenAI Responses API reference",
                source(
                    "OpenAI Platform",
                    "https://platform.openai.com/",
                    "Build with the API",
                ),
            ),
        ] {
            let mut single = vec![partial.clone()];
            assert!(
                filter(query, &mut single),
                "a genuine single result is allowed"
            );
            let mut rows = vec![partial];
            rows.extend(
                (0..7).map(|_| source("Unrelated", "https://example.org/", "Other topics")),
            );
            assert!(!filter(query, &mut rows));
            assert_eq!(
                rows.len(),
                1,
                "the remaining partial match cannot establish usefulness"
            );
        }
        let mut rows = vec![source(
            "Widget lock timeout",
            "https://docs.example.org/widget",
            "Reference",
        )];
        rows.extend((0..7).map(|_| source("Unrelated", "https://example.org/", "Other topics")));
        assert!(
            filter("Widget lock timeout guide", &mut rows),
            "an exact match remains usable"
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn broad_and_unrelated_results_do_not_satisfy_a_specific_query() {
        let mut rows = vec![
            source(
                "Welcome to Python.org",
                "https://python.org/",
                "The Python programming language",
            ),
            source(
                "Python documentation",
                "https://docs.python.org/",
                "Official Python documentation and tutorials",
            ),
            source("搜狗输入法", "https://sogou.com/", "输入法下载"),
            source(
                "Coroutines and Tasks",
                "https://docs.python.org/3/library/asyncio-task.html",
                "TaskGroup documentation for structured concurrency",
            ),
        ];
        filter("Python asyncio TaskGroup documentation", &mut rows);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].url.contains("asyncio-task"));
    }

    #[test]
    fn broad_product_homepages_do_not_displace_specific_results() {
        let mut rows = vec![
            source(
                "SQLite Home Page",
                "https://sqlite.org/",
                "A small database engine",
            ),
            source(
                "Release History",
                "https://sqlite.org/changes.html",
                "SQLite release notes",
            ),
            source(
                "SQLite releases",
                "https://example.org/sqlite",
                "Release details",
            ),
        ];
        filter("SQLite release notes", &mut rows);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].url.ends_with("changes.html"));
    }

    #[test]
    fn chinese_queries_and_inline_identifiers_have_meaningful_terms() {
        let mut rows = vec![
            source(
                "中华人民共和国劳动合同法",
                "https://example.org/law",
                "第三十八条 劳动者可以解除劳动合同",
            ),
            source("中华人民共和国", "https://example.org/about", "国家介绍"),
        ];
        filter("劳动合同法 第三十八条", &mut rows);
        assert_eq!(rows.len(), 1);
        let mut rows = vec![source(
            "OnceLock",
            "https://doc.rust-lang.org/std/sync/struct.OnceLock.html",
            "OnceLock documentation",
        )];
        filter("Rust std::sync::OnceLock documentation", &mut rows);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn operators_and_question_wording_do_not_hide_relevant_results() {
        let mut rows = vec![source(
            "TaskGroup",
            "https://docs.python.org/3/library/asyncio-task.html",
            "TaskGroup is an asynchronous context manager",
        )];
        filter(
            "How does Python use TaskGroup to wait for several tasks to finish? site:docs.python.org -tutorial",
            &mut rows,
        );
        assert_eq!(rows.len(), 1);
        filter("如何使用Python的TaskGroup等待所有任务结束", &mut rows);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn query_echo_in_a_url_is_not_relevance_evidence() {
        let mut rows = vec![source(
            "Other topics",
            "https://example.org/search?q=Python+asyncio+TaskGroup+documentation",
            "No results",
        )];
        filter("Python asyncio TaskGroup documentation", &mut rows);
        assert!(rows.is_empty());
    }

    #[test]
    fn filtering_keeps_engine_order_when_an_extra_keyword_matches_a_later_result() {
        let mut rows = vec![
            source(
                "Release history",
                "https://sqlite.org/changes.html",
                "SQLite releases",
            ),
            source(
                "SQLite release notes",
                "https://example.org/",
                "SQLite release notes",
            ),
            source("Unrelated", "https://example.org/other", "Other topics"),
        ];
        filter("SQLite release notes", &mut rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, "https://sqlite.org/changes.html");
    }
}

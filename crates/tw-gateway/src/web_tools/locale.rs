use tw_api::{SearchEngine, WebSearchLocale};

pub(super) fn apply(url: &mut reqwest::Url, engine: SearchEngine, locale: &WebSearchLocale) {
    let mut params = url.query_pairs_mut();
    match engine {
        SearchEngine::Bing => {
            if !locale.language.is_empty() {
                params.append_pair("setlang", &locale.language);
            }
            if !locale.region.is_empty() {
                params.append_pair("cc", &locale.region);
            }
        }
        SearchEngine::Duckduckgo => {
            if let Some(market) = duckduckgo_market(locale) {
                params.append_pair("kl", market);
            }
        }
    }
}

// DuckDuckGo has a fixed set of regional markets, not arbitrary BCP 47 pairs.
// https://duckduckgo.com/duckduckgo-help-pages/settings/params
// Prefer the requested language within a country; otherwise use that country's
// supported default. Unsupported regions keep the engine's default.
const DDG_MARKETS: &str = "ar-es au-en at-de be-fr be-nl br-pt bg-bg ca-en ca-fr ct-ca cl-es cn-zh co-es hr-hr cz-cs dk-da ee-et fi-fi fr-fr de-de gr-el hk-tzh hu-hu is-is in-en id-en ie-en il-he it-it jp-jp kr-kr lv-lv lt-lt my-en mx-es nl-nl nz-en no-no pk-en pe-es ph-en pl-pl pt-pt ro-ro ru-ru sa-ar sg-en sk-sk sl-sl za-en es-es se-sv ch-de ch-fr ch-it tw-tzh th-en tr-tr us-en us-es ua-uk uk-en vn-en";

fn duckduckgo_market(locale: &WebSearchLocale) -> Option<&'static str> {
    let region = match locale.region.to_ascii_lowercase().as_str() {
        "gb" => "uk".into(),
        "si" => "sl".into(),
        other => other.to_owned(),
    };
    let language = locale.language.to_ascii_lowercase();
    let language = match language.split('-').next().unwrap_or_default() {
        "ja" => "jp",
        "ko" => "kr",
        "zh" if language.contains("hant") || matches!(region.as_str(), "tw" | "hk") => "tzh",
        other => other,
    };
    let exact = format!("{region}-{language}");
    DDG_MARKETS
        .split_whitespace()
        .find(|&m| m == exact)
        .or_else(|| {
            DDG_MARKETS
                .split_whitespace()
                .find(|m| m.split('-').next() == Some(region.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_parameters_use_supported_markets_without_duplicating_bcp47_regions() {
        for (language, region, market) in [
            ("en-US", "US", Some("us-en")),
            ("zh-Hant", "TW", Some("tw-tzh")),
            ("zh-Hans", "CN", Some("cn-zh")),
            ("ja", "JP", Some("jp-jp")),
            ("ko", "KR", Some("kr-kr")),
            ("en", "GB", Some("uk-en")),
            ("sl", "SI", Some("sl-sl")),
            ("fr", "CA", Some("ca-fr")),
            ("zh-Hans", "US", Some("us-en")),
            ("en", "AQ", None),
            ("en", "", None),
        ] {
            let locale = WebSearchLocale {
                language: language.into(),
                region: region.into(),
            };
            assert_eq!(duckduckgo_market(&locale), market);
            let mut url = reqwest::Url::parse("https://www.bing.com/search?q=test").unwrap();
            apply(&mut url, SearchEngine::Bing, &locale);
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "setlang" && v == language)
            );
            assert_eq!(
                url.query_pairs()
                    .find(|(k, _)| k == "cc")
                    .map(|(_, v)| v.into_owned()),
                (!region.is_empty()).then(|| region.to_string())
            );
        }
    }
}

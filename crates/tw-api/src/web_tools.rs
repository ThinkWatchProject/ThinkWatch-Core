//! The gateway's web tools. Configuration belongs to the connected Core.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WebToolsMode {
    #[default]
    Upstream,
    Core,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SearchEngine {
    Bing,
    Duckduckgo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default, deny_unknown_fields)]
pub struct WebToolsConfig {
    pub mode: WebToolsMode,
    /// Empty chooses healthy engines automatically; otherwise tried in this order.
    pub engines: Vec<SearchEngine>,
    /// An existing proxy name, "system", or "direct".
    pub proxy: String,
    pub language: String,
    pub region: String,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
    pub max_results: u32,
    /// Number of search result pages to read when snippets are insufficient. Zero disables it.
    pub read_pages: u32,
    pub max_page_chars: u32,
    pub timeout_secs: u32,
    pub max_concurrent: u32,
    pub interval_ms: u32,
    pub cache_secs: u32,
    pub max_tool_calls: u32,
    /// Only before any search result is used; never silently switches paid services on.
    pub fallback_to_upstream: bool,
}

impl Default for WebToolsConfig {
    fn default() -> Self {
        Self {
            mode: WebToolsMode::Upstream,
            engines: Vec::new(),
            proxy: "system".into(),
            language: String::new(),
            region: String::new(),
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            max_results: 8,
            read_pages: 2,
            max_page_chars: 12_000,
            timeout_secs: 30,
            max_concurrent: 4,
            interval_ms: 1000,
            cache_secs: 600,
            max_tool_calls: 8,
            fallback_to_upstream: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WebToolsView {
    pub config: WebToolsConfig,
    pub config_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WebToolsSave {
    pub base_version: String,
    pub config: WebToolsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebToolRequest {
    Search {
        query: String,
        #[serde(default)]
        allowed_domains: Vec<String>,
        #[serde(default)]
        blocked_domains: Vec<String>,
    },
    OpenPage {
        url: String,
        #[serde(default)]
        offset: u32,
    },
    FindInPage {
        url: String,
        pattern: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WebSource {
    pub url: String,
    pub title: String,
    pub text: String,
    pub engine: Option<SearchEngine>,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WebToolResult {
    pub sources: Vec<WebSource>,
    pub cached: bool,
    pub elapsed_ms: u32,
    /// Partial-source failures, distinct from a successful search with zero results.
    pub warnings: Vec<tw_types::Msg>,
    pub truncated: bool,
}

/// Compact, non-content diagnostics carried in the recorded response headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WebToolTrace {
    pub action: String,
    pub engines: Vec<SearchEngine>,
    pub cached: bool,
    pub elapsed_ms: u32,
    pub sources: u32,
    pub warnings: u32,
}

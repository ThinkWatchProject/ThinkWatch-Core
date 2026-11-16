//! Browser-compatible HTTP sessions for first-party search pages only.
//! Cookies stay in memory and are isolated by caller and engine by WebTools.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue};
use reqwest::cookie::{CookieStore, Jar};
use tw_api::{SearchEngine, WebSearchLocale};
use tw_types::Msg;

use super::{
    error,
    providers::{Kind, Profile, Spec},
    public,
};

#[derive(Default)]
pub(super) struct Session {
    cookies: Jar,
    cookie_bytes: AtomicUsize,
    pub ready: tokio::sync::OnceCell<()>,
}

pub(super) struct TimedSession {
    pub created: Instant,
    pub session: Session,
}

impl Default for TimedSession {
    fn default() -> Self {
        Self {
            created: Instant::now(),
            session: Session::default(),
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    identity: String,
    engine: SearchEngine,
    language: String,
    region: String,
}

pub(super) struct Pool {
    entries: Mutex<HashMap<Key, (Instant, Arc<TimedSession>)>>,
    capacity: usize,
    ttl: Duration,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: 64,
            ttl: Duration::from_secs(600),
        }
    }
}

impl Pool {
    pub fn get(
        &self,
        identity: &str,
        spec: &Spec,
        locale: &WebSearchLocale,
    ) -> Result<Option<Arc<TimedSession>>, Msg> {
        if spec.home.is_none() {
            return Ok(None);
        }
        let key = Key {
            identity: identity.into(),
            engine: spec.id,
            language: locale.language.clone(),
            region: locale.region.clone(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, (_, value)| {
            Arc::strong_count(value) > 1 || value.created.elapsed() < self.ttl
        });
        if let Some((used, value)) = entries.get_mut(&key) {
            *used = Instant::now();
            return Ok(Some(value.clone()));
        }
        if entries.len() >= self.capacity {
            // Evict one idle session rather than reject a new caller while stale
            // idle cookies occupy every slot. In-flight sessions are never split.
            let oldest = entries
                .iter()
                .filter(|(_, (_, value))| Arc::strong_count(value) == 1)
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                return Err(error("too many active search sessions"));
            }
        }
        let value = Arc::new(TimedSession::default());
        entries.insert(key, (Instant::now(), value.clone()));
        Ok(Some(value))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Request<'a> {
    pub spec: &'static Spec,
    pub kind: Kind,
    pub session: Option<&'a Session>,
    pub locale: &'a WebSearchLocale,
}

impl Request<'_> {
    pub fn headers(&self, url: &reqwest::Url) -> Result<HeaderMap, Msg> {
        if !self.spec.allows(url) {
            return Err(error(
                "search redirected outside its supported first-party pages",
            ));
        }
        let mut headers = match self.spec.profile {
            Profile::Basic => public::request_headers(),
            Profile::BrowserHttp1 => browser_headers(self.kind, self.locale)?,
        };
        if (self.session.is_some_and(|s| s.ready.get().is_some()) || self.kind == Kind::Data)
            && let Some(home) = self.spec.home
        {
            // Only the origin is sent, never another caller's query or result URL.
            headers.insert("referer", HeaderValue::from_static(home));
        }
        if let Some(cookie) = self.session.and_then(|s| s.cookies.cookies(url)) {
            headers.insert("cookie", cookie);
        }
        Ok(headers)
    }

    pub fn remember(&self, url: &reqwest::Url, headers: &HeaderMap) -> Result<(), Msg> {
        if !self.spec.allows(url) {
            return Err(error(
                "refused cookies from an unsupported search destination",
            ));
        }
        let Some(session) = self.session else {
            return Ok(());
        };
        let cookies: Vec<_> = headers.get_all("set-cookie").iter().collect();
        let bytes = cookies
            .iter()
            .map(|cookie| cookie.as_bytes().len())
            .sum::<usize>();
        if cookies.len() > 32
            || cookies.iter().any(|cookie| cookie.as_bytes().len() > 4096)
            || session
                .cookie_bytes
                .fetch_add(bytes, Ordering::Relaxed)
                .saturating_add(bytes)
                > 1024 * 1024
        {
            return Err(error("search session exceeded the cookie limit"));
        }
        session.cookies.set_cookies(&mut cookies.into_iter(), url);
        Ok(())
    }
}

fn browser_headers(kind: Kind, locale: &WebSearchLocale) -> Result<HeaderMap, Msg> {
    let script = kind == Kind::Data;
    let mut headers = HeaderMap::new();
    // The compatibility tokens request the normal web layout. Keep our own
    // product name; do not rotate identities or use cookies from a browser.
    headers.insert("user-agent", HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 ThinkWatch/1.0"
        ));
    headers.insert(
        "accept",
        HeaderValue::from_static(if script {
            "*/*"
        } else {
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        }),
    );
    let language = match locale.language.as_str() {
        "zh-Hans" => "zh-CN",
        "zh-Hant" => "zh-TW",
        "" => "en-US",
        other => other,
    };
    let base = language.split('-').next().unwrap_or(language);
    let mut language = if language == base {
        language.to_string()
    } else {
        format!("{language},{base};q=0.9")
    };
    if base != "en" {
        language.push_str(",en;q=0.8");
    }
    headers.insert(
        "accept-language",
        HeaderValue::from_str(&language).map_err(|_| error("invalid search language"))?,
    );
    if !script {
        headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::super::providers;
    use super::*;

    #[test]
    fn cookie_sessions_are_independent_of_the_request_profile() {
        static BASIC: Spec = Spec {
            id: SearchEngine::Bing,
            name: "Basic session",
            min_interval_ms: 0,
            profile: Profile::Basic,
            home: Some("https://www.bing.com/"),
            endpoints: &[providers::Endpoint {
                host: "www.bing.com",
                paths: &["/", "/search"],
            }],
        };
        let session = Session::default();
        let locale = WebSearchLocale::default();
        let request = Request {
            spec: &BASIC,
            kind: Kind::Document,
            session: Some(&session),
            locale: &locale,
        };
        let url = "https://www.bing.com/".parse().unwrap();
        let mut response = HeaderMap::new();
        response.insert(
            "set-cookie",
            "session=test; Path=/; Secure".parse().unwrap(),
        );
        request.remember(&url, &response).unwrap();
        session.ready.set(()).unwrap();
        let headers = request.headers(&url).unwrap();
        assert_eq!(headers["cookie"], "session=test");
        assert_eq!(headers["referer"], "https://www.bing.com/");
        assert_eq!(headers["accept-encoding"], "identity");
    }

    #[tokio::test]
    async fn sessions_coalesce_warmup_isolate_locales_and_evict_only_idle_entries() {
        let pool = Pool {
            capacity: 2,
            ..Pool::default()
        };
        let spec = providers::get(SearchEngine::Bing).spec();
        let locale = WebSearchLocale::default();
        let first = pool.get("caller", spec, &locale).unwrap().unwrap();
        let same = pool.get("caller", spec, &locale).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let calls = AtomicUsize::new(0);
        futures::future::join_all((0..4).map(|_| {
            first.session.ready.get_or_init(|| async {
                calls.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            })
        }))
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let english = WebSearchLocale {
            language: "en".into(),
            region: "US".into(),
        };
        let second = pool.get("caller", spec, &english).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(pool.get("third", spec, &locale).is_err());
        drop(second);
        assert!(pool.get("third", spec, &locale).is_ok());
        assert!(Arc::ptr_eq(
            &first,
            &pool.get("caller", spec, &locale).unwrap().unwrap()
        ));
        assert_eq!(pool.entries.lock().unwrap().len(), 2);
    }

    #[test]
    fn expired_sessions_are_replaced_after_their_last_user_releases_them() {
        let pool = Pool {
            ttl: Duration::ZERO,
            ..Pool::default()
        };
        let spec = providers::get(SearchEngine::Bing).spec();
        let locale = WebSearchLocale::default();
        let first = pool.get("caller", spec, &locale).unwrap().unwrap();
        first.session.ready.set(()).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &pool.get("caller", spec, &locale).unwrap().unwrap()
        ));
        drop(first);
        assert!(
            pool.get("caller", spec, &locale)
                .unwrap()
                .unwrap()
                .session
                .ready
                .get()
                .is_none()
        );
    }

    #[test]
    fn search_cookies_follow_scope_and_do_not_cross_callers_or_engines() {
        let session = Session::default();
        let locale = WebSearchLocale {
            language: "zh-Hans".into(),
            region: "CN".into(),
        };
        let request = Request {
            spec: providers::get(SearchEngine::Bing).spec(),
            kind: Kind::Document,
            session: Some(&session),
            locale: &locale,
        };
        let home_url = "https://www.bing.com/".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.append(
            "set-cookie",
            "session=test; Path=/search; Secure; HttpOnly"
                .parse()
                .unwrap(),
        );
        headers.append(
            "set-cookie",
            "foreign=test; Domain=example.org; Path=/".parse().unwrap(),
        );
        request.remember(&home_url, &headers).unwrap();
        assert!(!request.headers(&home_url).unwrap().contains_key("cookie"));
        let search = "https://www.bing.com/search".parse().unwrap();
        let headers = request.headers(&search).unwrap();
        assert_eq!(headers["cookie"], "session=test");
        assert_eq!(headers["accept-language"], "zh-CN,zh;q=0.9,en;q=0.8");
        assert!(headers["accept"].to_str().unwrap().starts_with("text/html"));
        assert!(!headers.contains_key("referer"));
        session.ready.set(()).unwrap();
        assert_eq!(
            request.headers(&search).unwrap()["referer"],
            "https://www.bing.com/"
        );
        assert!(
            request
                .headers(&"https://example.org/search".parse().unwrap())
                .is_err()
        );
        assert!(
            request
                .headers(&"https://duckduckgo.com/".parse().unwrap())
                .is_err()
        );
        let other = Session::default();
        let other = Request {
            session: Some(&other),
            ..request
        };
        assert!(!other.headers(&search).unwrap().contains_key("cookie"));
    }

    #[test]
    fn data_flow_without_session_keeps_its_existing_request_profile() {
        let locale = WebSearchLocale::default();
        let request = Request {
            spec: providers::get(SearchEngine::Duckduckgo).spec(),
            kind: Kind::Data,
            session: None,
            locale: &locale,
        };
        let data = "https://links.duckduckgo.com/d.js?q=test".parse().unwrap();
        let headers = request.headers(&data).unwrap();
        assert_eq!(headers, public::request_headers());
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("referer"));
    }

    #[test]
    fn script_requests_have_origin_referer_and_cookie_budget_is_bounded() {
        let session = Session::default();
        let locale = WebSearchLocale::default();
        let request = Request {
            spec: providers::get(SearchEngine::Bing).spec(),
            kind: Kind::Data,
            session: Some(&session),
            locale: &locale,
        };
        let data = "https://www.bing.com/search?q=test".parse().unwrap();
        let headers = request.headers(&data).unwrap();
        assert_eq!(headers["accept"], "*/*");
        assert_eq!(headers["referer"], "https://www.bing.com/");
        assert!(!headers.contains_key("upgrade-insecure-requests"));
        let mut response = HeaderMap::new();
        response.insert(
            "set-cookie",
            format!("a={}", "x".repeat(4096)).parse().unwrap(),
        );
        assert!(request.remember(&data, &response).is_err());
    }
}

//! 上游不认的推理强度，记下来。
//!
//! 各家认的档位不一样：Claude 的有的模型没有 `xhigh`，关着思考的不收 `high` 以上，GLM 这类
//! 始终思考的模型不收 `medium`。客户端（或者路由规则）写的那一档被上游拒了（400，原话说的是
//! 推理强度，见 [`tw_dialect::think::effort_rejection`]），同一家改一档、或者去掉强度再发一次。
//! **重发成了才记**：（上游，发给它的模型名）不认哪一档、改用什么。往后发给它的请求要是还写着
//! 那一档，发之前直接改，不用每次都先被拒一次。重发又被拒了的不记：原话里碰巧有一个档位名的
//! 400（「max context length exceeded」）不该让这个模型此后 7 天都换一档。
//!
//! **7 天后忘掉**：上游可能已经认了，那时再试原来那一档。存在数据目录的 `effort-memory.json`
//! 里，重启之后还在；一家上游的地址、密钥、协议变了，或者删掉了，它的这几条随之作废 —— 那已经
//! 是另一个上游了。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tw_dialect::ir::Effort;
use tw_dialect::think::{self, EffortFix};

/// 存在数据目录（配置文件所在的目录）里的文件名
pub const FILE: &str = "effort-memory.json";

/// 记多久，秒
pub const KEEP_SECS: u64 = 7 * 24 * 3600;

/// 最多记多少条。满了丢最早记下的那一条：丢了也只是下次再被拒一次、再重发一次
const MAX: usize = 1024;

/// 一条：这一家的这个模型不认 `rejected` 这一档，改用 `use`（`None` 是去掉强度）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    provider: String,
    model: String,
    /// 记下时这家长什么样（地址、凭据、协议的指纹）。对不上就作废
    identity: String,
    rejected: String,
    #[serde(rename = "use")]
    instead: Option<String>,
    /// 记下的时刻，Unix 秒
    at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    entries: Vec<Entry>,
}

type Key = (String, String, Effort);

#[derive(Default)]
struct Inner {
    /// 文件在哪儿。没告诉过数据目录（测试里直接建的网关）时只在内存里记
    path: Option<PathBuf>,
    by: HashMap<Key, (String, Option<Effort>, u64)>,
}

/// 见模块说明。**跨重载存活**：配置换了只作废变了的那几家
#[derive(Default)]
pub struct EffortMemory {
    inner: Mutex<Inner>,
    /// 写文件排着队：后写的那一次写的一定是最新的那一份
    writing: Mutex<()>,
}

/// 决定这几条还作不作数的那几样：地址、凭据、协议。**存指纹，不存原文** —— 这个文件不该多出
/// 一份密钥
fn identity(p: &tw_config::Provider) -> String {
    let raw = format!(
        "{}|{}|{:?}",
        p.base_url,
        p.credential_identity(),
        p.effective_protocol()
    );
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

fn fresh(at: u64, now: u64) -> bool {
    now.saturating_sub(at) < KEEP_SECS
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl EffortMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 告诉它数据目录在哪儿：读进那里的文件（读不了、读不成的当没有：丢了只是再被拒一次），
    /// 再按 `cfg` 对一遍。往后记下的写回那里
    pub fn set_dir(self: &Arc<Self>, dir: PathBuf, cfg: &tw_config::Config) {
        let path = dir.join(FILE);
        let file: File = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        {
            let mut g = self.lock();
            g.path = Some(path);
            g.by.clear();
            for e in file.entries {
                let Some(rejected) = parse(&e.rejected) else {
                    continue;
                };
                let instead = match e.instead.as_deref() {
                    None => None,
                    Some(s) => match parse(s) {
                        Some(x) => Some(x),
                        None => continue,
                    },
                };
                g.by.insert((e.provider, e.model, rejected), (e.identity, instead, e.at));
            }
        }
        self.reconcile(cfg);
    }

    /// 配置换了之后对一遍：删掉的上游、地址密钥协议变了的上游，它们的几条作废
    pub fn reconcile(self: &Arc<Self>, cfg: &tw_config::Config) {
        let ids: HashMap<&str, String> = cfg
            .providers
            .iter()
            .map(|p| (p.name.as_str(), identity(p)))
            .collect();
        let now = now_secs();
        let changed = {
            let mut g = self.lock();
            let before = g.by.len();
            g.by.retain(|(provider, _, _), (id, _, at)| {
                ids.get(provider.as_str()) == Some(id) && fresh(*at, now)
            });
            g.by.len() != before
        };
        if changed {
            self.save();
        }
    }

    /// 这一家的这个模型有没有记着什么。发之前先问这一句，没有的话不用解请求体
    pub fn has(&self, provider: &str, model: &str) -> bool {
        let g = self.lock();
        !g.by.is_empty() && g.by.keys().any(|(p, m, _)| p == provider && m == model)
    }

    /// 发给这一家的这个模型的请求写着 `rejected` 这一档：记着该改成什么的话交回来
    pub fn lookup(
        &self,
        p: &tw_config::Provider,
        model: &str,
        rejected: Effort,
        now: u64,
    ) -> Option<EffortFix> {
        let g = self.lock();
        let (id, instead, at) = g.by.get(&(p.name.clone(), model.to_string(), rejected))?;
        (fresh(*at, now) && *id == identity(p)).then_some(match instead {
            Some(e) => EffortFix::Use(*e),
            None => EffortFix::Drop,
        })
    }

    /// 记下：这一家的这个模型不认 `rejected`，改用 `fix` 重发成了。改用的就是它自己的不记
    pub fn note(
        self: &Arc<Self>,
        p: &tw_config::Provider,
        model: &str,
        rejected: Effort,
        fix: EffortFix,
        now: u64,
    ) {
        let instead = match fix {
            EffortFix::Use(e) if e == rejected => return,
            EffortFix::Use(e) => Some(e),
            EffortFix::Drop => None,
        };
        {
            let mut g = self.lock();
            let key = (p.name.clone(), model.to_string(), rejected);
            if g.by.len() >= MAX
                && !g.by.contains_key(&key)
                && let Some(oldest) =
                    g.by.iter()
                        .min_by_key(|(_, (_, _, at))| *at)
                        .map(|(k, _)| k.clone())
            {
                g.by.remove(&oldest);
            }
            g.by.insert(key, (identity(p), instead, now));
        }
        tracing::info!(
            provider = %p.name,
            model = %model,
            rejected = think::openai(rejected),
            instead = instead.map_or("none", think::openai),
            "remembered a reasoning effort this model does not take"
        );
        self.save();
    }

    /// 写回文件。**不在转发那条路上写**：有异步运行时就挪到阻塞线程上
    fn save(self: &Arc<Self>) {
        let me = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(move || me.save_now());
            }
            Err(_) => me.save_now(),
        }
    }

    /// 写回文件（原子写）。排着队写，写的时候现取最新的那一份
    pub(crate) fn save_now(&self) {
        let _w = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let (path, text) = {
            let g = self.lock();
            let Some(path) = g.path.clone() else {
                return;
            };
            let now = now_secs();
            let mut entries: Vec<Entry> =
                g.by.iter()
                    .filter(|(_, (_, _, at))| fresh(*at, now))
                    .map(|((provider, model, rejected), (id, instead, at))| Entry {
                        provider: provider.clone(),
                        model: model.clone(),
                        identity: id.clone(),
                        rejected: think::openai(*rejected).to_string(),
                        instead: instead.map(|e| think::openai(e).to_string()),
                        at: *at,
                    })
                    .collect();
            entries
                .sort_by(|a, b| (&a.provider, &a.model, a.at).cmp(&(&b.provider, &b.model, b.at)));
            match serde_json::to_string_pretty(&File { entries }) {
                Ok(t) => (path, t),
                Err(_) => return,
            }
        };
        if let Err(e) = tw_config::store::write_atomic(&path, &text) {
            tracing::warn!("the reasoning effort memory could not be written: {e}");
        }
    }
}

/// 存着的档位名。`none` 之类不是一档的不认
fn parse(s: &str) -> Option<Effort> {
    think::parse_openai(s).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, key: &str) -> tw_config::Provider {
        tw_config::Provider {
            name: name.into(),
            base_url: "https://relay.example.com".into(),
            key: Some(key.into()),
            ..Default::default()
        }
    }

    fn config(providers: Vec<tw_config::Provider>) -> tw_config::Config {
        tw_config::Config {
            version: 1,
            providers,
            ..Default::default()
        }
    }

    #[test]
    fn a_remembered_level_is_replaced_until_it_expires() {
        let m = Arc::new(EffortMemory::default());
        let p = provider("relay", "sk-1");
        let now = 1_000_000;
        assert!(!m.has("relay", "glm-5"));
        m.note(
            &p,
            "glm-5",
            Effort::Medium,
            EffortFix::Use(Effort::High),
            now,
        );
        assert!(m.has("relay", "glm-5"));
        assert!(!m.has("relay", "glm-4"));
        assert_eq!(
            m.lookup(&p, "glm-5", Effort::Medium, now + 60),
            Some(EffortFix::Use(Effort::High))
        );
        assert_eq!(m.lookup(&p, "glm-5", Effort::Low, now), None);
        // 7 天之后再试原来那一档
        assert_eq!(m.lookup(&p, "glm-5", Effort::Medium, now + KEEP_SECS), None);
        // 去掉强度也记得住；改用的就是它自己的不记
        m.note(&p, "glm-5", Effort::Max, EffortFix::Drop, now);
        assert_eq!(
            m.lookup(&p, "glm-5", Effort::Max, now),
            Some(EffortFix::Drop)
        );
        m.note(&p, "glm-5", Effort::Low, EffortFix::Use(Effort::Low), now);
        assert_eq!(m.lookup(&p, "glm-5", Effort::Low, now), None);
    }

    #[test]
    fn a_changed_or_removed_upstream_forgets_what_it_refused() {
        let m = Arc::new(EffortMemory::default());
        let now = now_secs();
        let (a, b) = (provider("a", "sk-1"), provider("b", "sk-2"));
        m.note(&a, "m", Effort::XHigh, EffortFix::Use(Effort::Max), now);
        m.note(&b, "m", Effort::XHigh, EffortFix::Use(Effort::Max), now);
        // 换了密钥：已经是另一个上游了，查的时候就不作数
        let rekeyed = provider("a", "sk-9");
        assert_eq!(m.lookup(&rekeyed, "m", Effort::XHigh, now), None);
        m.reconcile(&config(vec![rekeyed.clone()]));
        assert!(!m.has("a", "m"), "密钥变了");
        assert!(!m.has("b", "m"), "删掉了");
        // 地址、协议变了也一样
        m.note(&rekeyed, "m", Effort::XHigh, EffortFix::Drop, now);
        let moved = tw_config::Provider {
            base_url: "https://other.example.com".into(),
            ..rekeyed.clone()
        };
        m.reconcile(&config(vec![moved]));
        assert!(!m.has("a", "m"));
        m.note(&rekeyed, "m", Effort::XHigh, EffortFix::Drop, now);
        let reprotocoled = tw_config::Provider {
            protocol: Some(tw_config::Protocol::OpenaiChat),
            ..rekeyed.clone()
        };
        m.reconcile(&config(vec![reprotocoled]));
        assert!(!m.has("a", "m"));
        // 没变的留着
        m.note(&rekeyed, "m", Effort::XHigh, EffortFix::Drop, now);
        m.reconcile(&config(vec![rekeyed.clone()]));
        assert!(m.has("a", "m"));
    }

    #[test]
    fn what_is_remembered_survives_a_restart_in_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = provider("relay", "sk-1");
        let cfg = config(vec![p.clone()]);
        let now = now_secs();
        let m = Arc::new(EffortMemory::default());
        m.set_dir(dir.path().to_path_buf(), &cfg);
        m.note(
            &p,
            "claude-opus-4-6",
            Effort::XHigh,
            EffortFix::Use(Effort::Max),
            now,
        );
        m.note(&p, "glm-5", Effort::Medium, EffortFix::Drop, now);
        m.save_now();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains("\"rejected\": \"xhigh\""), "{text}");
        assert!(text.contains("\"use\": \"max\""), "{text}");
        assert!(!text.contains("sk-1"), "不存密钥：{text}");

        let again = Arc::new(EffortMemory::default());
        again.set_dir(dir.path().to_path_buf(), &cfg);
        assert_eq!(
            again.lookup(&p, "claude-opus-4-6", Effort::XHigh, now),
            Some(EffortFix::Use(Effort::Max))
        );
        assert_eq!(
            again.lookup(&p, "glm-5", Effort::Medium, now),
            Some(EffortFix::Drop)
        );
        // 读进来时这家已经换了密钥：作废，文件跟着改
        let again = Arc::new(EffortMemory::default());
        again.set_dir(
            dir.path().to_path_buf(),
            &config(vec![provider("relay", "sk-2")]),
        );
        assert!(!again.has("relay", "glm-5"));
        again.save_now();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(!text.contains("glm-5"), "{text}");
        // 读不成的文件当没有
        std::fs::write(dir.path().join(FILE), "not json").unwrap();
        let broken = Arc::new(EffortMemory::default());
        broken.set_dir(dir.path().to_path_buf(), &cfg);
        assert!(!broken.has("relay", "glm-5"));
    }
}

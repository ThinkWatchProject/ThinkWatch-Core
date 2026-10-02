//! 默认插件（随 core 发的那几个，清单在 [`tw_gateway::plugin::defaults`]）：第一次见到时
//! 装上，**停用着**；出了新版、而用户没动过它时，换成新版。
//!
//! # 给过什么记在哪儿
//!
//! 插件目录里的 `.defaults.json`：`{ "offered": { "<id>": "<给出去的那一版的 SHA-256>" } }`。
//! 每一次（启动时、每换入一份配置之后）对着它和配置走一遍：
//!
//! - **没给过的**：配置里已经有这个 id（用户自己的插件）就只记一笔「给过了」；否则写
//!   插件文件和底稿，配置里加一条 —— 停用、出错时拒绝、范围照 manifest、设置都是默认值、
//!   哈希是发出去的那份字节的 —— 再记下来；
//! - **给过、配置里还在、文件和批准的都还是给出去的那一份，而 core 带的已经是新版**：
//!   换文件、底稿和配置里的哈希；开关、出错时怎么办、范围和还声明着的设置照旧，新声明的
//!   设置取默认值；**新版要了旧版没要的权限就停用**；记下新版；
//! - **给过、配置里没有了**：用户删的。**不再加回去**；
//! - **给过、文件被用户改过**（或者批准的已经是别的一份）：不动。
//!
//! 文件和配置都已经是新版、只是上次没来得及记下来的（写记录那一步失败了），补记一笔。
//!
//! # 和别的写入怎么排
//!
//! 整个过程攥着 `Plugins::edits`，和控制面写插件文件、目录监听是同一把锁；配置照别的
//! 按资源写入一样走 `ConfigManager::transform`（核版本、先校验再写、保留注释、存历史，
//! 来源记成 `defaults`），**这一次要加、要换的一次写进去**：一版配置、一条历史。配置在
//! 这中间被别处改了（版本对不上），刚写的文件还原，等那一次换入之后再走一遍。
//!
//! # 不挡启动、不挡换配置
//!
//! 哪一步不成只落在那一个插件上：记一行日志、发一条 `plugin_failed`（同一个问题只说
//! 一次），这次不记「给过了」，下一次换入配置再试。换配置本身在这之前已经成了 ——
//! 这一路是换完之后才走的（[`spawn`]）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use serde::{Deserialize, Serialize};
use tw_config::edit;
use tw_config::history::Origin;
use tw_gateway::plugin::Manifest;
use tw_gateway::plugin::load::{read_capped, sha256_hex};
use tw_types::Msg;

use crate::{ApplyError, ConfigManager};

/// 记着给过哪些默认插件的文件，在插件目录里。点开头：它不是插件
pub const RECORD: &str = ".defaults.json";

/// `.defaults.json` 在哪儿。`dir` 是配置文件所在的目录
pub fn record_path(dir: &Path) -> PathBuf {
    tw_config::plugins::dir_in(dir).join(RECORD)
}

/// `.defaults.json` 的内容
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    /// id → 给出去的那一版的 SHA-256
    offered: BTreeMap<String, String>,
}

/// 随 core 发的一个插件
struct Shipped {
    id: String,
    source: String,
    /// 源码字节的 SHA-256：给出去的就是这一版
    sha256: String,
}

/// 一次走下来做了什么。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Seeded {
    /// 加进配置的（停用着）
    pub added: Vec<String>,
    /// 换成了新版的
    pub updated: Vec<String>,
    /// 换成新版时停用了的：开着，而新版要了旧版没要的权限。也在 `updated` 里
    pub disabled: Vec<String>,
    /// 只记了一笔「给过了」的：用户自己的插件占着这个 id，或者新版已经装上了
    pub marked: Vec<String>,
    /// 这次没办成的，和原因。下一次换入配置再试
    pub failed: Vec<(String, Msg)>,
}

impl Seeded {
    fn did_something(&self) -> bool {
        !(self.added.is_empty() && self.updated.is_empty() && self.marked.is_empty())
    }
}

/// 补齐默认插件的那一路。**跨多次走存活**：同一个问题只说一次。
pub struct Seeder {
    shipped: Vec<Shipped>,
    told: Mutex<Told>,
}

/// 说过的问题
#[derive(Default)]
struct Told {
    /// 按插件 id
    plugins: HashMap<String, Msg>,
    /// 记录文件读不出来的原因
    record: Option<String>,
}

/// 这一次要写进配置的一项
struct Change {
    /// 在 `Seeder::shipped` 里的位置
    at: usize,
    /// 配置里的那一条
    item: serde_yaml_ng::Mapping,
    /// 新加的是 None；换新版的是配置里批准的那个哈希（换之前的那一版）
    replaces: Option<String>,
    /// 开着、换成新版时停用了
    disabled: bool,
}

impl Seeder {
    /// 一组 (id, 源码)。**测试拿自己的插件走这一条**，生产用 [`Seeder::shipped`]
    pub fn new<'a>(shipped: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            shipped: shipped
                .into_iter()
                .map(|(id, source)| Shipped {
                    id: id.to_string(),
                    sha256: sha256_hex(source.as_bytes()),
                    source: source.to_string(),
                })
                .collect(),
            told: Mutex::default(),
        }
    }

    /// 随 core 发的那几个
    pub fn shipped() -> Self {
        Self::new(tw_gateway::plugin::defaults::ALL.iter().copied())
    }

    /// 走一遍（见模块说明）。**不会失败**：没办成的落在那一个插件上，下一次再试。
    pub async fn seed(&self, mgr: &ConfigManager) -> Seeded {
        let gw = mgr.gateway();
        let _edit = gw.plugins.edits.lock().await;
        let mut out = Seeded::default();
        let mut names: HashMap<String, String> = HashMap::new();
        let dir = super::dir_of(mgr.path());
        let record_file = record_path(&dir);
        let mut record = match read_record(&record_file) {
            Ok(r) => r,
            // 记录读不出来：分不清哪些是用户删掉的，宁可一个都不加
            Err(why) => {
                let mut told = self.told.lock().unwrap_or_else(PoisonError::into_inner);
                if told.record.as_ref() != Some(&why) {
                    tracing::warn!(
                        file = %record_file.display(),
                        "the record of the default plugins cannot be read, so none are added or \
                         updated until it is fixed or removed: {why}"
                    );
                    told.record = Some(why);
                }
                return out;
            }
        };
        let Ok(cur) = mgr.current() else {
            return out;
        };
        // 磁盘上那份此刻读不了（正在被人改）：等它下一次换入成功
        let Ok(cfg) = tw_config::try_parse(&cur.text) else {
            return out;
        };
        let before = record.clone();

        let mut plan: Vec<Change> = Vec::new();
        for (at, s) in self.shipped.iter().enumerate() {
            let entry = cfg.plugins.iter().find(|p| p.id == s.id);
            let offered = record.offered.get(&s.id).cloned();
            match (offered, entry) {
                // 用户自己的插件占着这个 id：不动它，记下给过了
                (None, Some(_)) => {
                    record.offered.insert(s.id.clone(), s.sha256.clone());
                    out.marked.push(s.id.clone());
                }
                (None, None) => match compile(mgr, s.source.as_bytes(), true).await {
                    Ok(m) => {
                        names.insert(s.id.clone(), m.name.clone());
                        plan.push(Change {
                            at,
                            item: super::entry(
                                &s.id,
                                &s.sha256,
                                false,
                                tw_api::OnError::Reject,
                                &scope_of(&m),
                                &super::reconcile(&m, &BTreeMap::new()),
                            ),
                            replaces: None,
                            disabled: false,
                        });
                    }
                    Err(why) => out.failed.push((s.id.clone(), why)),
                },
                // 用户删掉的：不再加回去
                (Some(_), None) => {}
                (Some(o), Some(_)) if o == s.sha256 => {}
                (Some(o), Some(p)) => {
                    let file = tw_config::plugins::file_path(&dir, &s.id);
                    let bytes = match read_capped(&file) {
                        Ok(b) => Some(b),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => {
                            out.failed
                                .push((s.id.clone(), unreadable(&file.display().to_string(), e)));
                            continue;
                        }
                    };
                    let on_disk = bytes.as_deref().map(sha256_hex);
                    if on_disk.as_deref() == Some(o.as_str()) && p.sha256 == o {
                        // 没动过：换成新版
                        let new = match compile(mgr, s.source.as_bytes(), true).await {
                            Ok(m) => m,
                            Err(why) => {
                                out.failed.push((s.id.clone(), why));
                                continue;
                            }
                        };
                        // 旧版要过哪些权限。读不出来就当新版多要了 —— 宁可停用
                        let old = match bytes {
                            Some(b) => compile(mgr, &b, false).await.ok(),
                            None => None,
                        };
                        let more = old.as_ref().is_none_or(|old| {
                            new.permissions.iter().any(|x| !old.permissions.contains(x))
                        });
                        names.insert(s.id.clone(), new.name.clone());
                        plan.push(Change {
                            at,
                            item: super::entry(
                                &s.id,
                                &s.sha256,
                                p.enabled && !more,
                                p.on_error.into(),
                                &super::scope_view(&p.scope),
                                &super::reconcile(&new, &p.settings),
                            ),
                            replaces: Some(o),
                            disabled: p.enabled && more,
                        });
                    } else if on_disk.as_deref() == Some(s.sha256.as_str()) && p.sha256 == s.sha256
                    {
                        // 新版已经装上了，只是上次没记下来
                        record.offered.insert(s.id.clone(), s.sha256.clone());
                        out.marked.push(s.id.clone());
                    }
                    // 否则是用户改过的：不动
                }
            }
        }

        // 一项一项先在这份原文上试写一遍：写不进去的（值写不成 YAML、配置校验不过）
        // 只去掉那一项，不连累别的
        plan.retain(|c| {
            let current = c.replaces.as_ref().map(|_| self.shipped[c.at].id.as_str());
            let tried = edit::upsert(&cur.text, edit::PLUGINS, current, &c.item)
                .map_err(|e| e.msg())
                .and_then(|t| tw_config::try_parse(&t).map(|_| ()).map_err(|r| r.msg()));
            match tried {
                Ok(()) => true,
                Err(why) => {
                    out.failed.push((self.shipped[c.at].id.clone(), why));
                    false
                }
            }
        });

        // 先写文件（插件文件和底稿），写不成的那一项去掉
        let mut undo = Vec::new();
        plan.retain(|c| {
            let s = &self.shipped[c.at];
            let src = s.source.as_bytes();
            let files = [
                (tw_config::plugins::file_path(&dir, &s.id), src),
                (tw_config::plugins::approved_path(&dir, &s.id), src),
            ];
            match super::write_files(&dir, &files) {
                Ok(u) => {
                    undo.push(u);
                    true
                }
                Err((_, Json(why))) => {
                    out.failed.push((s.id.clone(), why));
                    false
                }
            }
        });

        // 再写配置。**照走这一遍时读到的那一版写**：中间被别处改了就整个作罢、文件还原，
        // 那一次换入之后会再走一遍
        if !plan.is_empty() {
            let base = cur.version();
            let written = mgr
                .transform(Some(base.as_str()), Origin::Defaults, |text, _| {
                    let mut text = text.to_string();
                    for c in &plan {
                        let current = c.replaces.as_ref().map(|_| self.shipped[c.at].id.as_str());
                        text = edit::upsert(&text, edit::PLUGINS, current, &c.item)?;
                    }
                    Ok(text)
                })
                .await;
            match written {
                Ok(_) => {
                    for c in &plan {
                        let s = &self.shipped[c.at];
                        record.offered.insert(s.id.clone(), s.sha256.clone());
                        if c.replaces.is_some() {
                            out.updated.push(s.id.clone());
                        } else {
                            out.added.push(s.id.clone());
                        }
                        if c.disabled {
                            out.disabled.push(s.id.clone());
                        }
                    }
                }
                Err(e) => {
                    for u in undo {
                        u.restore();
                    }
                    match e {
                        // 别处刚写了配置：那一次换入会再叫这一路
                        ApplyError::Stale { .. }
                        | ApplyError::Store(tw_config::StoreError::Conflict { .. }) => {
                            tracing::debug!(
                                "the configuration changed while default plugins were being added; trying again after it"
                            );
                        }
                        e => {
                            let why = e.msg();
                            for c in &plan {
                                out.failed
                                    .push((self.shipped[c.at].id.clone(), why.clone()));
                            }
                        }
                    }
                }
            }
        }

        if record != before
            && let Err(e) = write_record(&dir, &record)
        {
            // 下一次走的时候按配置和文件补记得上：这里只说一声
            tracing::warn!(file = %record_file.display(), "the record of the default plugins could not be written: {e}");
        }
        self.tell(mgr, &out, &names);
        out
    }

    /// 把这一次的结果说出去：做了什么记一行日志；没办成的每个插件发一条 `plugin_failed`，
    /// **同一个问题只说一次**，办成了就忘掉它
    fn tell(&self, mgr: &ConfigManager, out: &Seeded, names: &HashMap<String, String>) {
        if out.did_something() {
            tracing::info!(
                added = ?out.added,
                updated = ?out.updated,
                disabled = ?out.disabled,
                marked = ?out.marked,
                "default plugins offered"
            );
        }
        let mut told = self.told.lock().unwrap_or_else(PoisonError::into_inner);
        for id in out.added.iter().chain(&out.updated).chain(&out.marked) {
            told.plugins.remove(id);
        }
        // 记录文件读得出来了
        told.record = None;
        let bus = &mgr.gateway().bus;
        for (id, why) in &out.failed {
            if told.plugins.get(id) == Some(why) {
                continue;
            }
            told.plugins.insert(id.clone(), why.clone());
            let name = names.get(id).cloned().unwrap_or_else(|| id.clone());
            tracing::warn!(plugin = %id, "a default plugin could not be set up: {why}");
            bus.emit(tw_api::Event::PluginFailed {
                id: bus.next_id(),
                plugin_id: id.clone(),
                plugin_name: name,
                request_id: None,
                message: why.clone(),
                at_ms: crate::config::now_ms(),
            });
        }
    }
}

/// 启动时走过一遍之后，**每换入一份配置再走一遍**（哪一条路进来的都算，它自己写的那一次
/// 也算 —— 再走一遍什么都不做）。返回的任务不用留着：跟着进程走
pub fn spawn(seeder: Seeder, mgr: Arc<ConfigManager>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            mgr.applied().await;
            seeder.seed(&mgr).await;
        }
    })
}

fn scope_of(m: &Manifest) -> tw_api::PluginScope {
    tw_api::PluginScope {
        clients: m.scope.clients.clone(),
        models: m.scope.models.clone(),
        upstreams: m.scope.upstreams.clone(),
    }
}

/// 编一遍读出 manifest。**放到阻塞线程上**。`keep`：结果留进缓存（马上要装上的那一份，
/// 紧接着的重载不再编），否则什么都不留
async fn compile(mgr: &ConfigManager, source: &[u8], keep: bool) -> Result<Manifest, Msg> {
    let plugins = mgr.gateway().plugins.clone();
    let source = source.to_vec();
    tokio::task::spawn_blocking(move || {
        let compiled = if keep {
            plugins.prepare(&source)
        } else {
            plugins.inspect(&source)
        };
        compiled.map(|h| h.manifest().clone()).map_err(|e| e.msg())
    })
    .await
    .unwrap_or_else(|e| Err(crate::internal(e).1.0))
}

fn unreadable(file: &str, e: std::io::Error) -> Msg {
    super::unreadable(file, e).1.0
}

fn read_record(path: &Path) -> Result<Record, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(e.to_string()),
    }
}

/// 写记录：和插件文件一样只给自己（目录 0700、文件 0600），原子替换
fn write_record(dir: &Path, record: &Record) -> std::io::Result<()> {
    tw_config::private_dir::create(&tw_config::plugins::dir_in(dir))?;
    let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    super::write_private(&record_path(dir), &bytes)
}

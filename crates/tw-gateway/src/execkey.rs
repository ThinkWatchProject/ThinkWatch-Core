//! `exec` 凭据的缓存（DESIGN.md §3.6 第 3 类）。
//!
//! # 为什么要有这一层
//!
//! `resolved_key()` 在故障转移循环里，**每个请求跑一次**。而 `exec` 的
//! 一次「跑」是 fork 一个进程 —— `gcloud auth print-access-token` 那类
//! 要几百毫秒，`op read` 要走一次本地 agent。全都挂在转发路径上。
//!
//! §3.6 的配置示例里本来就写着 `ttl: 55m  # 别每个请求都 fork 一次进程`，
//! 只是一直没实现。
//!
//! # 三个和 OAuth 那一层不一样的地方
//!
//! **一、不写 `ttl` 就不缓存。**默认缓存会造出一个新问题：「我明明换了
//! 凭据，怎么没生效」—— 而那比多 fork 几次难查得多。要缓存是用户说了算。
//!
//! **二、失败不退避。**OAuth 那边退避是因为对面是别人的网络服务，敲多了
//! 会被限流；而 `exec` 是本机的一条命令，敲不坏任何东西，**而用户很可能
//! 正在旁边改它**。退避只会让「改好了怎么还报错」变成新的困惑。
//!
//! **三、要在 `spawn_blocking` 里跑。**`run_exec` 是同步的，里面还有
//! `thread::sleep` 轮询。直接在异步任务里调它会**占住一个 tokio 工作
//! 线程**：一条卡住的命令按默认超时能占 10 秒，几条并发就能把整个运行时
//! 饿死 —— 那时挂掉的不是这一家上游，是整个网关。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// 一份跑出来的凭据。
struct Live {
    key: String,
    /// 什么时候作废
    until: Instant,
    /// 跑出它的那条命令的指纹 —— 用户改了命令，这条就不算数
    fp: u64,
}

/// 每个 provider 一份。
#[derive(Default)]
pub struct Cache {
    /// **一个 provider 一把异步锁**，而不是整张表一把。
    ///
    /// 它同时是缓存锁和**单飞**：客户端一上来常常并发发好几个请求
    /// （Claude Code 就是这样），冷缓存下没有单飞的话，那几个请求会
    /// 各 fork 一次进程 —— 而这一层存在的全部理由就是别那么干。
    inner: std::sync::Mutex<HashMap<String, Arc<Mutex<Option<Live>>>>>,
}

fn fingerprint(argv: &[String], ttl: Option<Duration>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    argv.hash(&mut h);
    ttl.hash(&mut h);
    h.finish()
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, provider: &str) -> Arc<Mutex<Option<Live>>> {
        let mut g = self.inner.lock().expect("锁没毒");
        g.entry(provider.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    /// 拿这一家的 `exec` 凭据，能用缓存就用缓存。
    ///
    /// `ttl` 是 `None` 时**完全不碰缓存**：直接跑，跑完就扔。
    pub async fn key(
        &self,
        provider: &str,
        argv: &[String],
        timeout: Duration,
        ttl: Option<Duration>,
    ) -> Result<String, tw_secret::ExecError> {
        let Some(ttl) = ttl else {
            return run(argv.to_vec(), timeout).await;
        };
        let slot = self.slot(provider);
        // **锁在整个「查—跑—写」上，这正是单飞。**排在后面的那些醒来时
        // 缓存已经是热的，于是一个进程都不用再 fork
        let mut g = slot.lock().await;
        let fp = fingerprint(argv, Some(ttl));
        if let Some(live) = g.as_ref()
            && live.fp == fp
            && Instant::now() < live.until
        {
            return Ok(live.key.clone());
        }
        let key = run(argv.to_vec(), timeout).await?;
        // **失败不写缓存**（上面的 `?` 已经返回了）：下一个请求重新跑，
        // 因为用户很可能正在旁边改那条命令
        *g = Some(Live {
            key: key.clone(),
            until: Instant::now() + ttl,
            fp,
        });
        Ok(key)
    }
}

/// 跑那条命令 —— **在阻塞线程池里**，见模块头第三条。
async fn run(argv: Vec<String>, timeout: Duration) -> Result<String, tw_secret::ExecError> {
    match tokio::task::spawn_blocking(move || tw_secret::run_exec(&argv, timeout)).await {
        Ok(r) => r,
        // 阻塞任务自己 panic 了。**说清楚是我们这边的问题**，
        // 别让它看起来像用户的命令写错了
        Err(e) => Err(tw_secret::ExecError::Spawn {
            cmd: "（凭据命令）".into(),
            source: std::io::Error::other(format!("跑命令的任务没能完成：{e}")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条会数自己被跑了几次的命令。
    fn counter(dir: &std::path::Path) -> Vec<String> {
        let f = dir.join("n");
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo x >> {0}; wc -l < {0} | tr -d ' '", f.display()),
        ]
    }

    fn ran(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("n"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn without_a_ttl_every_call_runs_the_command() {
        // 这是加 ttl 之前的行为，**不写 ttl 就要保持它** —— 默认缓存会
        // 造出「我明明换了凭据怎么没生效」那个更难查的问题
        let d = tempfile::tempdir().unwrap();
        let c = Cache::new();
        for _ in 0..3 {
            c.key("p", &counter(d.path()), Duration::from_secs(5), None)
                .await
                .unwrap();
        }
        assert_eq!(ran(d.path()), 3);
    }

    #[tokio::test]
    async fn with_a_ttl_the_command_runs_once() {
        let d = tempfile::tempdir().unwrap();
        let c = Cache::new();
        let mut keys = Vec::new();
        for _ in 0..5 {
            keys.push(
                c.key(
                    "p",
                    &counter(d.path()),
                    Duration::from_secs(5),
                    Some(Duration::from_secs(60)),
                )
                .await
                .unwrap(),
            );
        }
        assert_eq!(ran(d.path()), 1, "缓存没起作用");
        assert!(keys.iter().all(|k| k == &keys[0]), "{keys:?}");
    }

    #[tokio::test]
    async fn a_burst_of_concurrent_requests_still_forks_only_once() {
        // **冷缓存下的并发才是真实场景**：客户端一上来就并发好几个请求。
        // 没有单飞的话，这一层等于没做
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(Cache::new());
        let argv = counter(d.path());
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let c = c.clone();
            let argv = argv.clone();
            set.spawn(async move {
                c.key(
                    "p",
                    &argv,
                    Duration::from_secs(5),
                    Some(Duration::from_secs(60)),
                )
                .await
            });
        }
        let mut keys = Vec::new();
        while let Some(r) = set.join_next().await {
            keys.push(r.unwrap().unwrap());
        }
        assert_eq!(ran(d.path()), 1, "八个并发请求各跑了一次");
        assert!(keys.iter().all(|k| k == &keys[0]));
    }

    #[tokio::test]
    async fn the_cache_expires_when_the_ttl_says_so() {
        let d = tempfile::tempdir().unwrap();
        let c = Cache::new();
        let argv = counter(d.path());
        let t = Duration::from_secs(5);
        c.key("p", &argv, t, Some(Duration::from_millis(120)))
            .await
            .unwrap();
        c.key("p", &argv, t, Some(Duration::from_millis(120)))
            .await
            .unwrap();
        assert_eq!(ran(d.path()), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.key("p", &argv, t, Some(Duration::from_millis(120)))
            .await
            .unwrap();
        assert_eq!(ran(d.path()), 2, "到点了还在用旧的");
    }

    #[tokio::test]
    async fn changing_the_command_invalidates_the_cache() {
        // **缓存盖住用户的修改是最难查的一类**（和 OAuth 那边同一条）
        let c = Cache::new();
        let ttl = Some(Duration::from_secs(60));
        let t = Duration::from_secs(5);
        let a = c
            .key("p", &["/bin/echo".into(), "one".into()], t, ttl)
            .await
            .unwrap();
        let b = c
            .key("p", &["/bin/echo".into(), "two".into()], t, ttl)
            .await
            .unwrap();
        assert_eq!(a, "one");
        assert_eq!(b, "two", "改了命令还在用旧的");
    }

    #[tokio::test]
    async fn a_failure_is_not_cached_because_the_user_is_probably_fixing_it() {
        let d = tempfile::tempdir().unwrap();
        let c = Cache::new();
        let ttl = Some(Duration::from_secs(60));
        let t = Duration::from_secs(5);
        let bad = vec!["/nonexistent/definitely-not-here".to_string()];
        assert!(c.key("p", &bad, t, ttl).await.is_err());
        // 修好了就该立刻生效，不用等 ttl
        let ok = c.key("p", &counter(d.path()), t, ttl).await.unwrap();
        assert_eq!(ok.trim(), "1");
    }

    #[tokio::test]
    async fn two_providers_do_not_share_one_credential() {
        let c = Cache::new();
        let ttl = Some(Duration::from_secs(60));
        let t = Duration::from_secs(5);
        let a = c
            .key("甲", &["/bin/echo".into(), "key-a".into()], t, ttl)
            .await
            .unwrap();
        let b = c
            .key("乙", &["/bin/echo".into(), "key-b".into()], t, ttl)
            .await
            .unwrap();
        assert_eq!((a.trim(), b.trim()), ("key-a", "key-b"));
    }
}

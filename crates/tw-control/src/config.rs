//! 配置的生命周期：谁改的、怎么进来的、进不来时怎么办（DESIGN.md §3.8）。
//!
//! 这是「两个写入方，一份文件」那道题的答案所在。四个问题里，最小替换
//! 归 `tw-yaml`，指纹和冲突归 `tw_config::store`，三道校验归
//! `tw_config::reload` —— 这里负责**把它们串成一条能跑的路**，并且保证
//! 无论从哪个方向进来，走的都是同一条。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tw_config::history::Origin;
use tw_config::store::{self, Fingerprint};

/// 配置的唯一入口。**UI、CLI、文件监听都走它** —— 三条路各写一遍，
/// 就会有两条忘了存历史、一条忘了防回环。
pub struct ConfigManager {
    path: PathBuf,
    gateway: tw_gateway::AppState,
    bus: tw_observe::EventBus,
    /// 上一次**我们自己**写下去的样子。文件事件来了先和它比 ——
    /// 一样就是自己写的，直接忽略，否则会形成回环（§3.8）。
    seen: Mutex<Option<Fingerprint>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Store(#[from] store::StoreError),
    /// 三道校验里的任意一道没过。**旧配置还在服务。**
    #[error("{0}")]
    Rejected(tw_config::Rejected),
    /// 校验过了但运行时对象建不起来 —— 同样保持旧的。
    #[error("配置能读，但用不起来：{0}")]
    Build(String),
    #[error("版本对不上：你基于 {base}，而现在是 {current}。刷新一下再改。")]
    Stale { base: String, current: String },
}

impl ConfigManager {
    pub fn new(path: PathBuf, gateway: tw_gateway::AppState, bus: tw_observe::EventBus) -> Self {
        let seen = store::read(&path).ok().map(|l| l.fingerprint);
        Self {
            path,
            gateway,
            bus,
            seen: Mutex::new(seen),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 磁盘上现在这一份，连带它的版本号。
    pub fn current(&self) -> Result<tw_config::Loaded, store::StoreError> {
        store::read(&self.path)
    }

    /// 从文件重新读一遍并试着换入。文件监听走这条。
    ///
    /// 返回 `Ok(None)` 表示「这就是我们自己刚写的，什么都没做」。
    pub async fn reload_from_disk(&self) -> Result<Option<String>, ApplyError> {
        let loaded = self.current()?;
        {
            let seen = self.seen.lock().await;
            if let Some(prev) = seen.as_ref()
                && prev.same_content(&loaded.fingerprint)
            {
                // **这是回环的唯一出口。**没有它，我们写一次文件，监听
                // 响一次，我们再重载一次，严重时是个循环。
                return Ok(None);
            }
        }
        let version = self.apply_text(&loaded.text, Origin::External).await?;
        *self.seen.lock().await = Some(loaded.fingerprint);
        Ok(Some(version))
    }

    /// 校验 + 换入 + 记历史，**但不写盘**。
    ///
    /// 文件已经是那个样子了（外部改动），或者调用方接下来自己写。
    async fn apply_text(&self, text: &str, origin: Origin) -> Result<String, ApplyError> {
        let cfg = tw_config::try_parse(text).map_err(|r| {
            // **先告诉界面，再返回错误。**这条路径最常见的调用方是文件
            // 监听，而它的错误没有人接 —— 不发事件的话，用户在编辑器里
            // 写错一个字，界面上什么都不会发生。
            self.bus.emit(tw_api::Event::ConfigRejected {
                id: self.bus.next_id(),
                stage: r.stage.label().to_string(),
                message: r.message.clone(),
                line: r.line,
                excerpt: r.excerpt.clone(),
                at_ms: now_ms(),
            });
            tracing::warn!("配置没通过校验，继续用旧的：{r}");
            ApplyError::Rejected(r)
        })?;
        self.gateway
            .reload(cfg)
            .map_err(|e| ApplyError::Build(e.message))?;
        // 历史存的是**刚刚生效的这一版**。存改之前那一版是另一件事，
        // 由写入路径在写之前做。
        let _ = tw_config::history::snapshot(&self.path, text, origin);
        let version = store::version_of(text);
        tracing::info!(%version, origin = origin.label(), "配置已生效");
        self.bus.emit(tw_api::Event::ConfigReloaded {
            id: self.bus.next_id(),
            version: version.clone(),
            origin: origin.label().to_string(),
            at_ms: now_ms(),
        });
        Ok(version)
    }

    /// 写一份新配置进去。UI 和 CLI 都走这条。
    ///
    /// `base_version` 是乐观并发的凭据（§3.8）：**对不上就是 409**，
    /// 而不是覆盖。`None` 表示调用方明确要覆盖（比如首次生成）。
    pub async fn write(
        &self,
        new_text: &str,
        base_version: Option<&str>,
        origin: Origin,
    ) -> Result<String, ApplyError> {
        let cur = self.current()?;
        if let Some(base) = base_version
            && cur.version() != base
        {
            return Err(ApplyError::Stale {
                base: base.to_string(),
                current: cur.version(),
            });
        }
        // **先校验再写。**写完才发现读不回来，那份坏配置已经在盘上了 ——
        // 而用户下一次启动会撞上它。
        tw_config::try_parse(new_text).map_err(ApplyError::Rejected)?;
        // 改**之前**那一版进历史。「回滚」这个动作要的是「回到我动它
        // 之前」（§3.8 的回滚语义）。
        let _ = tw_config::history::snapshot(&self.path, &cur.text, origin);
        // 写盘之前再确认一次磁盘还是我们读到的那份 —— 绝不静默覆盖手改
        let fp = store::write_if_unchanged(&self.path, &cur.fingerprint, new_text)?;
        // **先记指纹再换入。**顺序反了的话，文件事件可能在记指纹之前就
        // 到了，于是我们把自己刚写的当成外部改动又重载一遍。
        *self.seen.lock().await = Some(fp);
        self.apply_text(new_text, origin).await
    }

    /// 回到某一版。
    pub async fn rollback(&self, version: &str) -> Result<String, ApplyError> {
        let all = tw_config::history::list(&self.path)?;
        let target = all
            .iter()
            .rev()
            .find(|v| v.version == version || v.version.ends_with(version))
            .ok_or_else(|| ApplyError::Build(format!("历史里没有 {version} 这一版")))?;
        let text = tw_config::history::read(target)?;
        let cur = self.current().ok();
        // 回滚也走同一条写入路径，所以它同样会：校验、存历史、防回环。
        self.write(
            &text,
            cur.as_ref().map(|c| c.version()).as_deref(),
            Origin::Rollback,
        )
        .await
    }

    /// 记下一个不是我们写的指纹。**首次运行生成配置之后要调它** ——
    /// 不然那次写会被自己的监听当成外部改动。
    pub async fn mark_written(&self, fp: Fingerprint) {
        *self.seen.lock().await = Some(fp);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 起一个盯着配置文件的后台任务。
///
/// 返回的 `Watch` **必须留着** —— 扔掉它就停止监听，而那个失效是静默的。
pub fn spawn_watcher(
    mgr: Arc<ConfigManager>,
) -> Result<tw_config::watch::Watch, tw_config::watch::WatchError> {
    let (w, mut rx) = tw_config::watch::watch(mgr.path())?;
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            match mgr.reload_from_disk().await {
                Ok(Some(v)) => tracing::debug!(version = %v, "文件被改过，已重载"),
                Ok(None) => tracing::trace!("文件事件是我们自己写的，忽略"),
                // 错误已经在 apply_text 里发过事件了，这里只记一行
                Err(e) => tracing::debug!("重载没成功：{e}"),
            }
        }
    });
    Ok(w)
}

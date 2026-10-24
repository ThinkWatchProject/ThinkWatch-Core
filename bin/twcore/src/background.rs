//! 后台事务：OAuth 写回、同步、价目表刷新、默认插件、模型目录、余额、日志回收。
//!
//! # 为什么放在一个单独的运行时上
//!
//! 交接提交之后，旧 core 只做完手上的请求，**后台事务一律交给新 core**（方案 6.1）：两边
//! 各自续期会让对方的 refresh token 作废，两边各自同步、写配置会互相覆盖。可这些任务大多
//! 是起了就不管的循环，没有一个能叫停的把手。把它们都起在这个运行时上，提交时
//! [`Background::stop`] 一次关掉整个运行时，任务连同它们派生的任务一起停下，转发那一边
//! （主运行时）不受影响。
//!
//! **模型目录、余额例外**，见 [`Background::start`] 里的说明：它们和转发共用连接池，
//! 不能跟着这个运行时一起关。
//!
//! **OAuth 写回例外地先收尾**：它手上那份新 refresh token 要是丢了，新 core 读到的就是一张
//! 已经作废的（见 [`tw_control::rotation::Rotation::settle`]）。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::record::Store;

/// 收尾 OAuth 写回最多等多久
const ROTATION_SETTLE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Background {
    rt: Option<tokio::runtime::Runtime>,
    rotation: Option<Arc<tw_control::rotation::Rotation>>,
}

/// 起后台事务要的东西。
pub struct Start<'a> {
    pub control: &'a tw_control::ControlState,
    /// 安全模式：只起写回、同步（暂停着）、价目表；不起默认插件、模型目录、余额
    pub safe: bool,
    pub seeder: Option<tw_control::plugins::defaults::Seeder>,
    pub store: Option<Store>,
    /// 请求库打开了没有（接替者要等旧 core 退出才打开，见 `record::open_later`）。
    /// 日志回收等它
    pub store_opened: watch::Receiver<bool>,
}

impl Background {
    /// 起来。返回同步服务（退出前要 `flush`）。
    pub fn start(&mut self, s: Start<'_>) -> std::io::Result<Arc<tw_control::sync::Sync>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tw-background")
            .enable_all()
            .build()?;
        let sync = {
            // 下面这些函数各自 `tokio::spawn`：在这个运行时里调，任务就落在这里
            let _here = rt.enter();
            if let Some(store) = s.store.clone() {
                spawn_gc(s.control.gateway.clone(), store, s.store_opened.clone());
            }
            // 凭据轮换要写回 config.yaml。**不是人发起的配置写入只有两种**，
            // 这是一种（另一种是默认插件）—— 理由是服务器换发新 refresh
            // token 的那一刻旧的就作废了，不写回等于让配置文件从那一秒起就是坏的。
            self.rotation = Some(Arc::new(tw_control::rotation::spawn(s.control.clone())));
            // 多设备同步：启动时一轮，之后按本机改动、定时、文件夹变化、唤醒。安全模式下不自动同步
            let sync = tw_control::sync::start(s.control, s.safe);
            // 定期刷新默认价目表（`pricing.auto_update`，默认开）
            tw_control::pricing::spawn(s.control.clone());
            if !s.safe
                && let Some(seeder) = s.seeder
            {
                tw_control::plugins::defaults::spawn(seeder, s.control.cfg.clone());
            }
            sync
        };
        if !s.safe {
            // **这两个不在这个运行时上**，起在调用方的（主）运行时：它们和转发共用上游的
            // 连接池，连接的驱动任务落在哪个运行时，就随那个运行时一起没 —— 关掉这个运行时
            // 会掐断在途请求正在复用的连接（交接的端到端测试里，长流就断在提交那一刻）。
            // 所以交接之后它们在旧 core 上不停，只是多几次探测；要停得网关给一个开关
            //
            // 模型目录：后台去问每个上游有哪些模型。**不挡启动** ——
            // 探测要打网络，而网关不该因为一次探测慢而起不来。
            tw_gateway::models::spawn(s.control.gateway.clone());
            // 余额：启动时读一遍，之后按节奏读（见 `tw_gateway::balances`）。**不挡启动，不挡转发**
            tw_gateway::balances::spawn(s.control.gateway.clone());
        }
        self.rt = Some(rt);
        Ok(sync)
    }

    /// OAuth 写回的把手（交接开始前收尾用）
    pub fn rotation(&self) -> Option<Arc<tw_control::rotation::Rotation>> {
        self.rotation.clone()
    }

    /// 全部停下（交接提交之后）。**先在锁外收尾 OAuth 写回**（见模块文档），再调这个。
    pub fn stop(&mut self) {
        self.rotation = None;
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
            tracing::info!("background tasks stopped; the successor runs them from now on");
        }
    }

    /// 进程退出时交出运行时，在异步上下文之外关掉（在里面丢掉一个运行时会 panic）
    pub fn take_runtime(&mut self) -> Option<tokio::runtime::Runtime> {
        self.rt.take()
    }
}

/// 收尾 OAuth 写回。
pub async fn settle_rotation(bg: &std::sync::Mutex<Background>) {
    let r = bg.lock().ok().and_then(|g| g.rotation());
    if let Some(r) = r {
        r.settle(ROTATION_SETTLE).await;
    }
}

/*
  日志回收。**启动先跑一次，之后每小时一次。**

  只在启动时跑的话，一台一直开着的机器永远轮不到它；只定时跑的
  话，关机过夜的那台永远等不到那一小时。两个都要。

  每一轮都重新读配置，所以改完期限不用重启 —— 下一轮就按新的算。
  扫的是目录和一条 DELETE，空跑一次的代价可以忽略。

  **在阻塞线程上跑，删正文时不拿记录库的锁**（见 `tw_store::task::gc`）：一次删几个
  GB 可以要好几秒，以前那期间这把锁一直攥着，记录和查询都在等。

  **接替者等请求库真的打开了才跑**：之前那是个占位的内存库，正文目录却是真的，旧 core
  还在用它。
*/
fn spawn_gc(gw: tw_gateway::AppState, rec: Store, mut opened: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let _ = opened.wait_for(|o| *o).await;
        loop {
            let r = gw.config().retention.clone();
            let freed = {
                let rec = rec.clone();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let (body_days, row_days, max) = (r.body_days, r.row_days, r.body_max_bytes);
                tokio::task::spawn_blocking(move || {
                    tw_store::task::gc(&rec, now, body_days, row_days, max)
                })
                .await
                .unwrap_or(0)
            };
            if freed > 0 {
                tracing::info!(
                    freed_bytes = freed,
                    body_days = r.body_days,
                    row_days = r.row_days,
                    "reclaimed expired request logs"
                );
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

//! twcore —— 本地 AI 网关引擎。
//!
//! 它和 UI 在用户眼里是同一个程序（DESIGN.md §2.2.1）：UI 起它、UI 关它、
//! 它自己不注册开机自启。这里做的是引擎侧那一半 —— 单实例、父进程守望、
//! 干净退出。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod lockfile;

use lockfile::{LockFile, LockOutcome};

#[derive(Parser)]
#[command(name = "twcore", version, about = "ThinkWatch Lite 的本地 AI 网关引擎")]
struct Cli {
    /// 配置文件路径。默认 ~/.thinkwatch/config.yaml
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 起网关
    Serve {
        /// 覆盖配置里的端口。写 0 让系统挑一个（测试用）
        #[arg(long)]
        port: Option<u16>,
        /// 只起控制面，不起数据面。守护连续失败后进这个模式 ——
        /// 网关挂了的时候，用户最需要的恰恰是能改配置
        #[arg(long)]
        safe: bool,
        /// 父进程 pid。它没了我们跟着退 —— 「让用户认为他俩就是一个程序」
        #[arg(long)]
        parent: Option<u32>,
    },
    /// 生成一份初始配置
    Init {
        /// 已存在时也覆盖
        #[arg(long)]
        force: bool,
    },
    /// 校验配置并把结论说清楚
    Check,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TWCORE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(tw_config::default_path);

    match cli.command {
        Command::Init { force } => cmd_init(&path, force),
        Command::Check => cmd_check(&path),
        Command::Serve { port, safe, parent } => cmd_serve(&path, port, safe, parent),
    }
}

fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} 已经存在。要重新生成请加 --force —— 但那会覆盖你现在的配置，\n\
             里面的密钥也会一起没。",
            path.display()
        );
    }
    let cfg = tw_config::generate_initial();
    let key = cfg.clients[0].key.clone();
    write_config(path, &cfg)?;
    println!("已生成 {}", path.display());
    println!();
    println!("网关密钥：{key}");
    println!("（把它配到客户端上。tw- 前缀是刻意的 —— 一眼看出这不是上游的真 key）");
    println!();
    println!("下一步：往 providers 段里加第一个上游，然后 twcore serve。");
    Ok(())
}

/// `0600`。这个文件里有明文密钥（§3.2），权限不能靠 umask 的运气。
fn write_config(path: &Path, cfg: &tw_config::Config) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("建目录 {} 失败", dir.display()))?;
    }
    let text = serde_yaml_ng::to_string(cfg)?;
    std::fs::write(path, &text).with_context(|| format!("写 {} 失败", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn cmd_check(path: &Path) -> Result<()> {
    match tw_config::load(path) {
        Ok(cfg) => {
            println!("✅ {} 没问题", path.display());
            println!(
                "   {} 个客户端，{} 个上游",
                cfg.clients.len(),
                cfg.providers.len()
            );
            for p in &cfg.providers {
                let proto = match p.effective_protocol() {
                    Some(x) => format!("{x:?}"),
                    // 猜不出来不是错误，但值得说一句 —— M0 按 Anthropic 走。
                    None => "未知（按 Anthropic 转发）".to_string(),
                };
                println!(
                    "   · {} → {} [{}]",
                    p.name,
                    tw_secret::redact_url(&p.base_url),
                    proto
                );
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_serve(path: &Path, port: Option<u16>, safe: bool, parent: Option<u32>) -> Result<()> {
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(tw_config::default_dir);
    let _lock = match LockFile::acquire(&dir)? {
        LockOutcome::Acquired(l) => l,
        LockOutcome::AlreadyRunning { pid } => {
            anyhow::bail!(
                "已经有一个 twcore 在跑（pid {pid}）。\n\
                 网关端口是固定的，两个实例抢不了同一个 —— 先停掉那个再起。\n\
                 如果那个进程其实已经不在了，删掉 {} 再试。",
                dir.join("twcore.lock").display()
            );
        }
    };

    let cfg = tw_config::load(path).with_context(|| {
        format!(
            "加载 {} 失败。没有配置的话跑一次 `twcore init`。",
            path.display()
        )
    })?;
    let bind = cfg.listen.gateway.bind.addr().to_string();
    let listen_port = port.unwrap_or(cfg.listen.gateway.port);
    let addr: std::net::SocketAddr = format!("{bind}:{listen_port}")
        .parse()
        .with_context(|| format!("监听地址不合法：{bind}:{listen_port}"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let socket = dir.join("twcore.sock");
    let config_path = path.to_path_buf();
    rt.block_on(async move {
        let state = tw_gateway::AppState::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;

        // 控制面无论如何都要起来 —— **网关挂了的时候，用户最需要的恰恰
        // 是能改配置**（§2.2.1）。安全模式就是「只有这一半」。
        let control = tw_control::ControlState {
            started: std::time::Instant::now(),
            config: std::sync::Arc::new(cfg),
            config_path,
            gateway_addr: if safe { None } else { Some(addr.to_string()) },
            bus: state.bus.clone(),
        };
        let sock = socket.clone();
        tokio::spawn(async move {
            if let Err(e) = tw_control::serve_unix(control, &sock).await {
                tracing::error!("控制面起不来：{e}");
            }
        });

        if let Some(ppid) = parent {
            // 父进程守望：GUI 没了我们跟着退。轮询而不是用 kqueue，是因为
            // 这段代码要能在被 launchd 重新 parent 之后仍然正确 —— 那时
            // getppid() 会变成 1，而我们要看的是原来那个 pid 还在不在。
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if !parent_alive(ppid) {
                        tracing::info!(ppid, "父进程没了，跟着退");
                        std::process::exit(0);
                    }
                }
            });
        }

        if safe {
            // 安全模式：只起控制面。数据面不动，让用户还能改配置、回滚。
            tracing::warn!("安全模式：只起控制面，数据面不启动");
            shutdown_signal().await;
            return Ok(());
        }

        tracing::info!(%addr, "启动");
        tokio::select! {
            r = tw_gateway::serve(state, addr) => {
                r.with_context(|| format!("监听 {addr} 失败。端口被占用的话，先看看是不是上一个实例没退干净。"))
            }
            _ = shutdown_signal() => {
                tracing::info!("收到退出信号");
                Ok(())
            }
        }
    })
}

fn parent_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

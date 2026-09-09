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
    /// 改配置。**和界面走同一套代码** —— 两套实现就是两套行为
    Config {
        #[command(subcommand)]
        what: ConfigCmd,
    },
    /// 量一条线通不通、每一段花了多久。**零成本**，不发任何业务请求
    Speed {
        /// 只测这一家。不写就全测
        provider: Option<String>,
        /// 只测某个代理本身
        #[arg(long)]
        proxy: Option<String>,
    },
    /// 看看这台机器上有哪些 AI 客户端，以及它们指向哪儿
    Clients {
        #[command(subcommand)]
        what: ClientsCmd,
    },
}

#[derive(Subcommand)]
enum ClientsCmd {
    /// 扫一遍。**只读**
    List,
    /// 「我明明配了，为什么没生效」—— 走一遍优先级链
    Why {
        /// 客户端 id，比如 claude-code
        client: String,
        /// 当前项目目录，用来查项目级配置是不是盖住了用户级
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// 算一份接管改动**并打印出来**，不落盘
    Plan { client: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 打印当前配置的原文和版本号
    Show,
    /// 改一个字段。路径写成 `/providers/官方/base_url`
    Set {
        /// **按名字定位，不是下标** —— 下标会在重排之后指向另一个东西
        path: String,
        value: String,
        /// 明确写成数字而不是字符串。`--int 8788` 和 `8788` 是两回事
        #[arg(long, conflicts_with_all = ["bool_value", "null"])]
        int: bool,
        #[arg(long = "bool", conflicts_with_all = ["int", "null"])]
        bool_value: bool,
        #[arg(long, conflicts_with_all = ["int", "bool_value"])]
        null: bool,
    },
    /// 历史版本
    History,
    /// 回到某一版
    Rollback { version: String },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TWCORE_LOG")
                // 平台证书验证器会自己 error! 一行原始的英文报错，和我们
                // 翻译过的那句重复，而且长得像是我们没处理这个错误。
                // 它没有被吞掉 —— L1 的 error 字段里说的就是它。
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("info,rustls_platform_verifier=off")
                }),
        )
        .init();

    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(tw_config::default_path);

    match cli.command {
        Command::Init { force } => cmd_init(&path, force),
        Command::Check => cmd_check(&path),
        Command::Serve { port, safe, parent } => cmd_serve(&path, port, safe, parent),
        Command::Speed { provider, proxy } => cmd_speed(&path, provider, proxy),
        Command::Config { what } => cmd_config(&path, what),
        Command::Clients { what } => cmd_clients(&path, what),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn cmd_clients(path: &Path, what: ClientsCmd) -> Result<()> {
    use tw_adopt::clients::{Gateway, adoptable, manual_only};
    use tw_adopt::detect;

    match what {
        ClientsCmd::List => {
            for d in detect::detect(&home()) {
                let state = match (d.installed, d.adopted_at_ms, &d.endpoint) {
                    (false, _, _) => "没装".to_string(),
                    (true, Some(at), Some(ep)) => format!("已接管 {} → {ep}", fmt_time(at)),
                    // **「我们写过」和「现在还是那样」是两回事。**
                    (true, Some(at), None) => {
                        format!("接管过（{}），但配置里已经没有我们写的字段了", fmt_time(at))
                    }
                    (true, None, Some(ep)) => format!("没接管，自己指向 {ep}"),
                    (true, None, None) => "装了，没接管".to_string(),
                };
                println!("{:<14} {:<12} {state}", d.id, d.name);
                println!("               {}", d.real.display());
                if d.real != d.path {
                    println!("               （{} 是个符号链接）", d.path.display());
                }
                for s in &d.shadows {
                    println!("               ⚠ {} 优先级更高", s.display());
                }
                if d.verified == tw_adopt::clients::Verified::FieldsOnly {
                    println!("               ⓘ {}", d.verified.note());
                }
            }
            println!();
            println!("接管不了、只能给指引的：");
            for m in manual_only() {
                println!("  {:<12} {}", m.name, m.how);
                println!("               {}", m.caveat);
            }
            Ok(())
        }
        ClientsCmd::Why { client, project } => {
            let c = adoptable()
                .into_iter()
                .find(|c| c.id == client)
                .ok_or_else(|| anyhow::anyhow!("没有叫 `{client}` 的客户端"))?;
            for f in detect::diagnose(&c, &home(), project.as_deref()) {
                let mark = match f.level {
                    detect::Level::Blocking => "✗",
                    detect::Level::Suspect => "?",
                    detect::Level::Clear => "✓",
                };
                println!("{mark} {}", f.title);
                println!("  {}", f.detail);
                if let Some(fix) = &f.fix {
                    println!("  → {fix}");
                }
            }
            Ok(())
        }
        ClientsCmd::Plan { client } => {
            let c = adoptable()
                .into_iter()
                .find(|c| c.id == client)
                .ok_or_else(|| anyhow::anyhow!("没有叫 `{client}` 的客户端"))?;
            let cfg = tw_config::load(path)?;
            // 0.0.0.0 是监听地址，不是能填进客户端配置的地址 —— 客户端
            // 得知道往哪儿连，那永远是 127.0.0.1
            let gw = Gateway {
                base: format!("http://127.0.0.1:{}", cfg.listen.gateway.port),
                key: None,
            };
            let plan = tw_adopt::plan::plan_adopt(&c, &home(), &gw)?;
            println!("要改：{}", plan.path.display());
            if plan.is_noop() {
                println!("（已经是这样了，什么都不用改）");
                return Ok(());
            }
            for n in &plan.notes {
                println!("  · {n}");
            }
            println!("\n--- 改完之后 ---");
            println!("{}", plan.after);
            println!("--- 以上只是算出来的，没有写盘 ---");
            Ok(())
        }
    }
}

/// 改配置。
///
/// **不连控制面，直接操作文件。**理由是这个命令必须在 core 没跑的时候
/// 也能用 —— 「配置写坏了导致 core 起不来」正是最需要 `config rollback`
/// 的时刻，而那时控制面根本不存在。
///
/// 代价是 core 正在跑时，改动要等它的文件监听发现（几百毫秒）。那条路
/// 本来就要打通，这里搭个便车而不是再造一套。
fn cmd_config(path: &Path, what: ConfigCmd) -> Result<()> {
    match what {
        ConfigCmd::Show => {
            let c = tw_config::store::read(path)?;
            println!("{}", c.text);
            eprintln!("── {} · {}", c.path.display(), c.version());
            Ok(())
        }
        ConfigCmd::History => {
            let all = tw_config::history::list(path)?;
            if all.is_empty() {
                println!("还没有历史版本。第一次改配置之后就有了。");
                return Ok(());
            }
            let now = tw_config::store::read(path).map(|c| c.version()).ok();
            // 新的在前 —— 要找的几乎总是最近那几版
            for v in all.iter().rev() {
                let mark = if Some(&v.version) == now.as_ref() {
                    "← 现在"
                } else {
                    "      "
                };
                println!(
                    "{mark}  {}  {:<10}  {} 字节  {}",
                    v.version,
                    v.origin.label(),
                    v.bytes,
                    fmt_time(v.at_ms)
                );
            }
            println!();
            println!("回到某一版：twcore config rollback <版本号>（前几位就够）");
            Ok(())
        }
        ConfigCmd::Rollback { version } => {
            let text = tw_config::history::rollback(path, &version)?;
            println!("已回到 {}", tw_config::store::version_of(&text));
            eprintln!("（core 在跑的话，它会在一秒内自己发现）");
            Ok(())
        }
        ConfigCmd::Set {
            path: pointer,
            value,
            int,
            bool_value,
            null,
        } => {
            let cur = tw_config::store::read(path)?;
            let scalar = if null {
                tw_yaml::Scalar::Null
            } else if int {
                tw_yaml::Scalar::Int(
                    value
                        .parse()
                        .with_context(|| format!("`{value}` 不是一个整数"))?,
                )
            } else if bool_value {
                tw_yaml::Scalar::Bool(
                    value
                        .parse()
                        .with_context(|| format!("`{value}` 不是 true 或 false"))?,
                )
            } else {
                tw_yaml::Scalar::Str(value)
            };
            let steps = tw_control::resolve_path(&cur.text, &pointer)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let next = tw_yaml::set(&cur.text, &steps, &scalar)?;
            // **先校验再写。**写完才发现读不回来，那份坏配置已经在盘上了。
            tw_config::try_parse(&next).map_err(|r| anyhow::anyhow!("{r}"))?;
            // 改之前那一版进历史，这样这条命令也能被 rollback 撤销
            let _ = tw_config::history::snapshot(path, &cur.text, tw_config::history::Origin::Cli);
            tw_config::store::write_if_unchanged(path, &cur.fingerprint, &next)?;
            let _ = tw_config::history::snapshot(path, &next, tw_config::history::Origin::Cli);
            println!("已改。新版本 {}", tw_config::store::version_of(&next));
            eprintln!("（core 在跑的话，它会在一秒内自己发现）");
            Ok(())
        }
    }
}

/// 给人看的时间。**本地时区** —— UTC 时间戳在一个桌面工具里没有意义。
fn fmt_time(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(t) => t
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string(),
        None => "?".to_string(),
    }
}

/// L1 测速（§4.6）。**逐个测，不并发** —— 六条线一起抢带宽测出来的
/// 握手时间不是任何一条线的真实值，而这一层存在的全部意义就是那几个
/// 数字准不准。
fn cmd_speed(path: &Path, provider: Option<String>, proxy: Option<String>) -> Result<()> {
    let cfg = tw_config::load(path).with_context(|| format!("读 {}", path.display()))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        if let Some(name) = proxy {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .with_context(|| format!("没有叫 `{name}` 的代理"))?;
            let r = tw_gateway::l1_tcp(&px.addr).await;
            print_l1(&format!("代理 {}", px.name), None, &r);
            println!("  只测到代理这一跳。代理影响的是网络层，再往上就该测上游了。");
            return Ok(());
        }
        let targets: Vec<&tw_config::Provider> = match &provider {
            Some(n) => vec![
                cfg.providers
                    .iter()
                    .find(|p| p.name == *n)
                    .with_context(|| format!("没有叫 `{n}` 的上游"))?,
            ],
            None => cfg.providers.iter().collect(),
        };
        if targets.is_empty() {
            println!("还没有配置任何上游 —— 先往 providers 段里加一个。");
            return Ok(());
        }
        for p in targets {
            match hop_for(&cfg, p) {
                Ok(hop) => {
                    let via = hop.as_ref().map(|_| p.proxy.as_str());
                    let r = tw_gateway::l1(&p.base_url, hop.as_ref()).await;
                    print_l1(&p.name, via, &r);
                }
                Err(e) => println!("{}  ⚠️  {e}", p.name),
            }
        }
        Ok(())
    })
}

fn hop_for(
    cfg: &tw_config::Config,
    p: &tw_config::Provider,
) -> Result<Option<tw_gateway::ProxyHop>> {
    match p.proxy.as_str() {
        tw_config::DIRECT => Ok(None),
        // 跟随系统代理的地址要到建连时才由环境决定，我们没有那份地址
        // 可以去握手。说出来，而不是假装直连测一遍给个漂亮数字。
        tw_config::SYSTEM => anyhow::bail!(
            "走的是系统代理，地址要到建连时才由环境决定 —— L1 测不到它。把代理显式配成一个命名条目就能测。"
        ),
        name => {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .with_context(|| format!("proxies 段里没有 `{name}`"))?;
            let auth = match &px.auth {
                None => None,
                Some(a) => Some((a.user.clone(), a.pass.resolve()?)),
            };
            Ok(Some(tw_gateway::ProxyHop {
                kind: px.kind,
                addr: px.addr.clone(),
                auth,
            }))
        }
    }
}

fn print_l1(target: &str, via: Option<&str>, r: &tw_gateway::L1Result) {
    let head = match via {
        Some(v) => format!("{target}（经 {v}）"),
        None => target.to_string(),
    };
    println!("{}  {}", if r.ok { "✅" } else { "❌" }, head);
    for seg in &r.segments {
        println!("     {:<18} {:>6} ms", seg.name, seg.ms);
    }
    if r.ok {
        println!("     {:<18} {:>6} ms", "建连总计", r.total_ms);
    }
    if let Some(e) = &r.error {
        println!("     {e}");
    }
    for n in &r.notes {
        println!("     · {n}");
    }
    println!();
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

fn write_config(path: &Path, cfg: &tw_config::Config) -> Result<()> {
    // 权限、原子写、目录创建都在 tw-config::write 里。两处各写一遍就是
    // 两处会漂移 —— 而漂移的那一处大概率是漏了 0600 的那处。
    Ok(tw_config::write(path, cfg)?)
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
                    "   · {} → {} [{}]  密钥 {}",
                    p.name,
                    tw_secret::redact_url(&p.base_url),
                    proto,
                    // 说来源而不是值。`exec` 那种要能一眼看出跑的是什么，
                    // 因为它是这个文件里唯一会执行东西的字段。
                    p.key.describe()
                );
                // exec 在 check 时**真跑一次**。这正是 check 存在的意义 ——
                // 「密钥命令能不能跑通」最容易到用第一次才发现，而那时的
                // 表现是一个莫名其妙的 401，或者网关整个卡住。
                if let Err(e) = p.resolved_key() {
                    println!("     ⚠ 密钥取不到：{e}");
                }
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

    // **配置不存在就地生成**（§7.6 第 1 步）。
    //
    // 这里曾经是「加载失败，去跑一次 twcore init」——而那是错的：真正的
    // 首次运行里根本没有人会去跑 init。UI 直接 spawn 的是 serve，于是
    // core 起不来、被守护反复重启，用户看到的是一个空界面加一串重启，
    // 而不是「加第一个上游」。
    //
    // 生成的配置没有 provider，那是合法的（见 tw-config::validate）：
    // 控制面起得来，引导流程有地方跑。
    if !path.exists() {
        let cfg = tw_config::generate_initial();
        tw_config::write(path, &cfg)
            .with_context(|| format!("生成初始配置 {} 失败", path.display()))?;
        tracing::info!(path = %path.display(), "首次运行，已生成初始配置");
    }
    let cfg = tw_config::load(path).with_context(|| format!("加载 {} 失败", path.display()))?;
    // `--port` 是一个**显式的覆盖**，配置文件不该推翻它。所以给了它
    // 之后就不再跟着配置里的监听地址走（§3.8 的「温」那一级）。
    let overridden = port.is_some();
    let mut listen = cfg.listen.gateway.clone();
    if let Some(p) = port {
        listen.port = p;
    }
    let addr = listen.socket_addr();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let socket = dir.join("twcore.sock");
    // **在起任何东西之前问**。等到 bind 失败时，网关已经在监听、客户端
    // 可能已经连上来了，而这条错误当时只会进日志。
    tw_control::socket_path_fits(&socket)?;
    let config_path = path.to_path_buf();
    rt.block_on(async move {
        let state = tw_gateway::AppState::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;

        // 观测这一层。**起不来不是致命的** —— 历史记录看不见，而网关
        // 照常转发（§4.7）。所以这里所有的失败都只记一行日志。
        //
        // body 的通道在这里建：**它是唯一同时看得见网关和存储的地方**，
        // 而两边各有各的同形结构，是为了不让「观测」挂到「转发」下面
        // （§9.0.1）。
        let (body_tx, body_rx) = tokio::sync::mpsc::channel(tw_gateway::bodies::CHANNEL_CAP);
        let store = build_store(&dir, state.bus.subscribe(), body_rx);
        if store.is_some() {
            state.set_body_sink(body_tx);
        }

        // 配置的唯一入口。UI、CLI、文件监听都从这里进（§3.8）。
        let manager = std::sync::Arc::new(tw_control::ConfigManager::new(
            config_path,
            state.clone(),
            state.bus.clone(),
        ));
        // **监听要留着** —— 扔掉它就停止监听，而那个失效是静默的。
        // 起不来不是致命的：手改文件不会自动生效，但界面和 CLI 照常能用，
        // 所以说一句就继续。
        let _watch = match tw_control::spawn_watcher(manager.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("盯不住配置文件，手改文件不会自动生效：{e}");
                None
            }
        };

        // 控制面无论如何都要起来 —— **网关挂了的时候，用户最需要的恰恰
        // 是能改配置**（§2.2.1）。安全模式就是「只有这一半」。
        let control = tw_control::ControlState {
            started: std::time::Instant::now(),
            gateway: state.clone(),
            cfg: manager,
            gateway_addr: if safe { None } else { Some(addr.to_string()) },
            store,
        };
        let sock = socket.clone();
        // **控制面没了就得退，不能只记一行日志。**
        //
        // 一个没有控制面的 core 是 UI 完全够不着的：连不上、改不了配置、
        // 也关不掉。守护那边的心跳会因此失败，然后按重启阶梯反复拉起来，
        // 而用户看到的是「core 连续失败」——真正的原因（比如 socket 路径
        // 太长）只在日志里躺着。退出让那句话有机会走到人眼前。
        let (control_died, control_dead) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let r = tw_control::serve_unix(control, &sock).await;
            let msg = match r {
                Err(e) => format!("{e}"),
                // serve_unix 正常返回意味着 accept 循环结束了，同样是没了
                Ok(()) => "控制面意外结束".to_string(),
            };
            let _ = control_died.send(msg);
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
            // **这时候控制面就是全部** —— 它没了，这个进程一件事都干不了。
            tracing::warn!("安全模式：只起控制面，数据面不启动");
            tokio::select! {
                msg = control_dead => {
                    anyhow::bail!("{}", msg.unwrap_or_else(|_| "控制面没了".into()))
                }
                _ = shutdown_signal() => {}
            }
            return Ok(());
        }

        // 模型目录：后台去问每个上游有哪些模型（§3.9）。**不挡启动** ——
        // 探测要打网络，而网关不该因为一次探测慢而起不来。
        tw_gateway::spawn_catalog_refresh(state.clone());

        tracing::info!(%addr, "启动");
        tokio::select! {
            r = async {
                if overridden {
                    tw_gateway::serve(state, addr).await
                } else {
                    tw_gateway::serve_following_config(state, addr).await
                }
            } => {
                r.with_context(|| format!("监听 {addr} 失败。端口被占用的话，先看看是不是上一个实例没退干净。"))
            }
            msg = control_dead => {
                anyhow::bail!("{}", msg.unwrap_or_else(|_| "控制面没了".into()))
            }
            _ = shutdown_signal() => {
                tracing::info!("收到退出信号");
                Ok(())
            }
        }
    })
}

/// 建观测层。**每一步失败都只是「没有历史记录」，不是「起不来」。**
///
/// 这条边界值得写死在代码形状里：这个函数返回 `Option`，而不是
/// `Result` —— 调用方连处理错误的机会都不该有，因为没有任何一种
/// 处理方式是「不转发了」（§4.7）。
fn build_store(
    dir: &Path,
    events: tokio::sync::broadcast::Receiver<tw_api::Event>,
    bodies: tokio::sync::mpsc::Receiver<tw_gateway::BodyRecord>,
) -> Option<std::sync::Arc<tokio::sync::Mutex<tw_store::Recorder>>> {
    let db = match tw_store::Db::open(&dir.join("data.db")) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("请求历史起不来，这次不记录（转发不受影响）：{e}");
            return None;
        }
    };
    // 价目表解不开是个打包错误，但同样不该挡住转发 —— 那时成本一栏
    // 是空的，而请求照常。
    let prices = match tw_pricing::Prices::builtin()
        .and_then(|p| p.with_overrides(&dir.join("pricing.yaml")))
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("价目表读不了，成本一栏会是空的：{e}");
            return None;
        }
    };
    let blobs = tw_store::Blobs::new(dir.join("blobs"));
    // 两边的 body 结构在这里对接。**一次移动，不复制** —— `Bytes` 的
    // 克隆是引用计数。
    let (tx, rx) = tokio::sync::mpsc::channel(tw_gateway::bodies::CHANNEL_CAP);
    let mut bodies = bodies;
    tokio::spawn(async move {
        while let Some(b) = bodies.recv().await {
            let mapped = tw_store::StoredBody {
                id: b.id,
                at_ms: b.at_ms,
                which: match b.kind {
                    tw_gateway::BodyKind::Request => tw_store::Which::Request,
                    tw_gateway::BodyKind::Response => tw_store::Which::Response,
                },
                body: b.body,
                original_len: b.original_len,
            };
            if tx.send(mapped).await.is_err() {
                return;
            }
        }
    });
    Some(tw_store::task::spawn(
        tw_store::Recorder::new(db, blobs, prices),
        events,
        rx,
    ))
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

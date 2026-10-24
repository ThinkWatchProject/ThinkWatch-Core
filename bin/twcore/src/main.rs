//! twcore —— 本地 AI 网关引擎。
//!
//! 它和 UI 在用户眼里是同一个程序：UI 起它、UI 关它、
//! 它自己不注册开机自启。这里做的是引擎侧那一半 —— 单实例、父进程守望、
//! 干净退出。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::watch;

mod background;
mod call;
mod handover;
mod lockfile;
mod pack_cli;
mod proc;
mod record;
mod sync_cmd;
mod upgrade;

use background::Background;
use lockfile::{LockFile, LockOutcome};

#[derive(Parser)]
// `about` 不写值就是 Cargo.toml 里的 description：一句话只写一处，两边不会各说各的
#[command(name = "twcore", version, about)]
struct Cli {
    /// Path to the configuration file; ~/.thinkwatch/config.yaml by default
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the gateway
    Serve {
        /// Override the port from the configuration; 0 lets the system pick one (for tests)
        #[arg(long)]
        port: Option<u16>,
        /// Start only the control plane, not the data plane
        //
        // 守护连续失败后进这个模式 —— 网关挂了的时候，用户最需要的恰恰是
        // 能改配置
        #[arg(long)]
        safe: bool,
        /// The parent process id; this process exits when that one does
        //
        // 「让用户认为他俩就是一个程序」
        #[arg(long)]
        parent: Option<u32>,
    },
    /// Write an initial configuration
    Init {
        /// Overwrite the configuration file even if it exists
        #[arg(long)]
        force: bool,
    },
    /// Validate the configuration and print the result
    Check,
    /// Change the configuration
    //
    // **和界面走同一套代码** —— 两套实现就是两套行为
    Config {
        #[command(subcommand)]
        what: ConfigCmd,
    },
    /// Test the link and time each stage. Sends no request of its own and costs nothing
    Speed {
        /// Test only this upstream; without it, every upstream is tested
        provider: Option<String>,
        /// Test only this proxy
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Print the control key, which the desktop app connects with
    //
    // 远程连接时用户在服务器上跑它，把钥匙抄进应用
    ControlKey {
        /// Replace the key with a new one. A running core takes the new key within a second
        /// and closes the connections made with the previous one
        #[arg(long)]
        rotate: bool,
    },
    /// Show, open or close the remote control port, which a desktop app on another machine
    /// connects to
    //
    // 只能在 core 这台机器上开关：经控制面进来的一方改不了自己进来的那扇门
    Remote {
        #[command(subcommand)]
        what: Option<RemoteCmd>,
    },
    /// Send one request to the running core's control plane and print the response
    //
    // **curl 敲不开控制面了**：每条连接先握手。调试、脚本、smoke 用这个 ——
    // 它读的是同一份配置里的钥匙，走的是和桌面端同一条握手
    Call {
        /// The endpoint, such as /status or /history?limit=1
        path: String,
        /// The HTTP method
        #[arg(short = 'X', long, default_value = "GET")]
        method: String,
        /// A JSON request body
        #[arg(short, long)]
        data: Option<String>,
        /// Write the response body to this file and print only the status code
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Export the configuration as a configuration pack (.twconfig)
    Export {
        /// Export only these upstreams (comma-separated), with what they refer to
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
        /// Leave the keys out: upstream keys, header values, AWS keys, proxy passwords and
        /// gateway keys are left empty
        #[arg(long)]
        no_secrets: bool,
        /// Encrypt the file with a passphrase, asked for or taken from TWCORE_PASSPHRASE
        #[arg(long)]
        encrypt: bool,
        /// The file to write
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Import a configuration pack: what is not here is added, and what differs is chosen item
    /// by item
    Import {
        /// The configuration pack (.twconfig)
        file: PathBuf,
        /// Show what would happen and change nothing
        #[arg(long)]
        dry_run: bool,
        /// What to do with every item that differs from the one here; asked item by item
        /// without it
        #[arg(long, value_enum)]
        on_conflict: Option<pack_cli::OnConflict>,
    },
    /// The last versions of the configuration, and restoring one
    Backup {
        #[command(subcommand)]
        what: BackupCmd,
    },
    /// Show or change multi-device sync. Talks to the running core
    //
    // 给没有界面的服务器（远程 core）用：和界面走同一套控制面端点
    Sync {
        #[command(subcommand)]
        what: SyncCmd,
    },
    /// Replace this twcore with another release from GitHub; the configuration is left alone
    //
    // 只给单独装的 twcore 用（服务器上）。桌面应用包里的那份由应用更新，见 upgrade.rs
    Upgrade {
        /// Only compare with the release and report; change nothing
        #[arg(long)]
        check: bool,
        /// Restart twcore.service afterwards when systemd runs it
        #[arg(long)]
        restart: bool,
        /// Install this version rather than the latest, such as 0.47.0
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Print the sync state, the location and the devices
    Status,
    /// Sync once now
    Now,
    /// Turn sync on; asks for the location and the passphrase
    Enable,
    /// Turn sync off; this device keeps its configuration
    Disable {
        /// Also delete every version, device record and the keyring at the sync location
        #[arg(long)]
        purge: bool,
    },
    /// After the sync location was emptied or lost its keyring: upload this device's
    /// configuration again (and put back the keyring this device keeps)
    Republish,
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Print whether the remote control port is open and where to connect
    Show,
    /// Open the remote control port. A running core starts listening within a second
    Enable {
        /// loopback, all, an interface name such as eth0, or an address; all by default
        #[arg(long)]
        bind: Option<String>,
        /// The port; a random one is picked the first time
        #[arg(long)]
        port: Option<u16>,
        /// A source allowed to connect (CIDR or address); repeat for several. Replaces the
        /// list; the private ranges by default
        #[arg(long = "allow")]
        allow: Vec<String>,
    },
    /// Close the remote control port; the port and the allowed sources are kept
    Disable,
}

#[derive(Subcommand)]
enum BackupCmd {
    /// List the versions, newest first
    List,
    /// Go back to a version. Settings that belong to this machine, ChatGPT sign-ins and the
    /// gateway keys clients set up here use are kept
    Restore {
        /// The id from `twcore backup list`
        id: String,
        /// Show what would change and change nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the configuration as it is, with its version
    Show,
    /// Change one field; a path is written as /providers/<name>/base_url
    Set {
        /// Locate by name rather than by index
        //
        // 下标会在重排之后指向另一个东西
        path: String,
        value: String,
        /// Write an integer rather than a string; --int 8788 differs from 8788
        #[arg(long, conflicts_with_all = ["bool_value", "null"])]
        int: bool,
        #[arg(long = "bool", conflicts_with_all = ["int", "null"])]
        bool_value: bool,
        #[arg(long, conflicts_with_all = ["int", "bool_value"])]
        null: bool,
    },
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
    // 网关的代码 panic 时，那个请求的结局写得出哪一行、为什么（stderr 在桌面版里没人看）
    tw_gateway::panic_note::install();

    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(tw_config::default_path);
    // 配置包、备份按「代码」「数据」分开比较插件：拆法要插件那一套
    tw_control::backups::install_codec();

    match cli.command {
        Command::Init { force } => cmd_init(&path, force),
        Command::Check => cmd_check(&path),
        Command::Serve { port, safe, parent } => cmd_serve(&path, port, safe, parent),
        Command::Speed { provider, proxy } => cmd_speed(&path, provider, proxy),
        Command::Config { what } => cmd_config(&path, what),
        Command::ControlKey { rotate } => cmd_control_key(&path, rotate),
        Command::Remote { what } => cmd_remote(&path, what.unwrap_or(RemoteCmd::Show)),
        Command::Export {
            providers,
            no_secrets,
            encrypt,
            out,
        } => pack_cli::export(&path, providers, no_secrets, encrypt, out),
        Command::Import {
            file,
            dry_run,
            on_conflict,
        } => pack_cli::import(&path, &file, dry_run, on_conflict),
        Command::Backup { what } => match what {
            BackupCmd::List => pack_cli::list(&path),
            BackupCmd::Restore { id, dry_run } => pack_cli::restore(&path, &id, dry_run),
        },
        Command::Call {
            path: endpoint,
            method,
            data,
            out,
        } => call::run(&path, &endpoint, &method, data, out),
        Command::Sync { what } => match what {
            SyncCmd::Status => sync_cmd::status(&path),
            SyncCmd::Now => sync_cmd::now(&path),
            SyncCmd::Enable => sync_cmd::enable(&path),
            SyncCmd::Disable { purge } => sync_cmd::disable(&path, purge),
            SyncCmd::Republish => sync_cmd::republish(&path),
        },
        Command::Upgrade {
            check,
            restart,
            version,
        } => upgrade::run(upgrade::Opts {
            check,
            restart,
            version,
        }),
    }
}

/// 改配置。
///
/// **不连控制面，直接操作文件。**理由是这个命令必须在 core 没跑的时候
/// 也能用 —— 「配置写坏了导致 core 起不来」正是最需要 `backup restore`
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
                        .with_context(|| format!("`{value}` is not an integer"))?,
                )
            } else if bool_value {
                tw_yaml::Scalar::Bool(
                    value
                        .parse()
                        .with_context(|| format!("`{value}` is neither true nor false"))?,
                )
            } else {
                tw_yaml::Scalar::Str(value)
            };
            let steps = tw_control::resolve_path(&cur.text, &pointer)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            // 和 `PATCH /config` 同一份规矩：配置里的字段一律单行
            if let tw_yaml::Scalar::Str(s) = &scalar {
                tw_config::edit::check_line_breaks(s)?;
            }
            let next = tw_yaml::set(&cur.text, &steps, &scalar)?;
            // **先校验再写。**写完才发现读不回来，那份坏配置已经在盘上了。
            tw_config::try_parse(&next).map_err(|r| anyhow::anyhow!("{r}"))?;
            // 前后各留一个备份版本，这样这条命令也能被 `twcore backup restore` 撤销
            let _ =
                tw_pack::backup::record_before(path, &cur.text, tw_config::history::Origin::Cli);
            tw_config::store::write_if_unchanged(path, &cur.fingerprint, &next)?;
            let _ = tw_pack::backup::record(path, &next, tw_config::history::Origin::Cli);
            println!(
                "changed; the new version is {}",
                tw_config::store::version_of(&next)
            );
            eprintln!("(a running core picks this up within a second)");
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

/// L1 测速。**逐个测，不并发** —— 六条线一起抢带宽测出来的
/// 握手时间不是任何一条线的真实值，而这一层存在的全部意义就是那几个
/// 数字准不准。
fn cmd_speed(path: &Path, provider: Option<String>, proxy: Option<String>) -> Result<()> {
    let cfg = tw_config::load(path).with_context(|| format!("reading {}", path.display()))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        if let Some(name) = proxy {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .with_context(|| format!("no proxy named `{name}`"))?;
            let hop = tw_gateway::hop_of(px).map_err(anyhow::Error::msg)?;
            let (host, port) = tw_gateway::proxy_target(&cfg, &px.name);
            let r = tw_gateway::l1_proxy(&hop, &host, port).await;
            print_l1(
                &format!(
                    "proxy `{}` (connecting to {host}:{port} through it)",
                    px.name
                ),
                None,
                &r,
            );
            return Ok(());
        }
        let targets: Vec<&tw_config::Provider> = match &provider {
            Some(n) => vec![
                cfg.providers
                    .iter()
                    .find(|p| p.name == *n)
                    .with_context(|| format!("no upstream named `{n}`"))?,
            ],
            None => cfg.providers.iter().collect(),
        };
        if targets.is_empty() {
            println!("No upstreams are configured yet. Add one under `providers`.");
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
            "This upstream goes through the system proxy, whose address is only decided by \
             the environment when the connection is made, so a link test cannot measure it. \
             Configure the proxy as a named entry to measure this route."
        ),
        name => {
            let px = cfg
                .proxies
                .iter()
                .find(|x| x.name == name)
                .with_context(|| format!("`{name}` is not defined under `proxies`"))?;
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
        Some(v) => format!("{target} (through {v})"),
        None => target.to_string(),
    };
    println!("{}  {}", if r.ok { "✅" } else { "❌" }, head);
    // 28 宽：最长的一个是 `TLS handshake to the proxy`
    for seg in &r.segments {
        println!("     {:<28} {:>6} ms", seg.stage.label(), seg.ms);
    }
    if r.ok {
        println!("     {:<28} {:>6} ms", "connect, total", r.total_ms);
    }
    if let Some(e) = &r.error {
        match r.failed {
            Some(stage) => println!("     {}: {e}", stage.label()),
            None => println!("     {e}"),
        }
    }
    for s in &r.skipped {
        println!("     · {}", s.label());
    }
    println!();
}

/// 打印控制面的钥匙，或者换一把。
///
/// **只在 core 那台机器上能跑**：它读写的是本机的配置文件。换钥匙不经过
/// 控制面 —— 经控制面进来的一方改得动这扇门，就改得掉别人进来的路。
fn cmd_control_key(path: &Path, rotate: bool) -> Result<()> {
    if !path.exists() {
        anyhow::bail!(
            "{} does not exist. twcore serve writes it on first start; twcore init writes it \
             without starting",
            path.display()
        );
    }
    if rotate {
        let key = tw_config::control_key::rotate_file(path)?;
        println!("{}", key.to_hex());
        eprintln!(
            "(a running core takes the new key within a second and closes the connections made \
             with the previous one; the desktop app on this machine reconnects by itself, and one \
             on another machine needs the new key)"
        );
        print_remote(path);
        return Ok(());
    }
    // 还没有钥匙的旧配置：补上再打印。`serve` 起来时也会这样补，这里先补
    // 不改变任何行为，只是省得让人先去起一次 core
    tw_config::control_key::ensure_file(path)?;
    let key = tw_link::read_key(path)?;
    // **标准输出只有钥匙**：`$(twcore control-key)` 拿到的就是它。连接要的其余
    // 几样（地址、端口）走标准错误，终端上照样看得见
    println!("{}", key.to_hex());
    print_remote(path);
    Ok(())
}

/// 远程控制端口开没开、从别的机器该连哪儿。**按配置文件说**：core 可能没在跑，
/// 在跑的话它听的就是这里写的（绑不上时 `twcore call /status` 说为什么）。
fn print_remote(path: &Path) {
    let Ok(cfg) = tw_config::load(path) else {
        return;
    };
    match cfg.listen.control.remote.filter(|r| r.enabled) {
        None => eprintln!("remote control: off (twcore remote enable opens it)"),
        Some(r) => {
            let addrs = match r.bind.resolve() {
                Ok(ip) => tw_control::remote::reachable(std::net::SocketAddr::new(ip, r.port)),
                Err(e) => {
                    eprintln!("remote control: port {}, but {e}", r.port);
                    return;
                }
            };
            if addrs.is_empty() {
                eprintln!(
                    "remote control: port {}, listening on loopback only, so no other machine can \
                     connect",
                    r.port
                );
            } else {
                eprintln!(
                    "remote control: port {}; connect to {}",
                    r.port,
                    addrs.join(" or ")
                );
            }
            eprintln!("allowed sources: {}", r.allow_from.join(", "));
        }
    }
}

fn cmd_remote(path: &Path, what: RemoteCmd) -> Result<()> {
    if !path.exists() {
        anyhow::bail!(
            "{} does not exist. twcore init writes it; twcore serve writes it on first start",
            path.display()
        );
    }
    match what {
        RemoteCmd::Show => {}
        RemoteCmd::Enable { bind, port, allow } => {
            let bind = match bind {
                None => None,
                Some(b) => Some(
                    serde_yaml_ng::from_value::<tw_config::Bind>(serde_yaml_ng::Value::String(
                        b.trim().to_string(),
                    ))
                    .with_context(|| {
                        format!(
                            "--bind takes loopback, all, an interface name or an address; it \
                             reads {b}"
                        )
                    })?,
                ),
            };
            let e = tw_config::remote::Enable {
                bind,
                port,
                allow_from: (!allow.is_empty()).then_some(allow),
            };
            tw_config::remote::enable_file(path, &e)?;
            eprintln!("(a running core opens the port within a second)");
        }
        RemoteCmd::Disable => {
            tw_config::remote::disable_file(path)?;
            eprintln!(
                "(a running core closes the port within a second, and the connections made \
                 through it)"
            );
        }
    }
    print_remote(path);
    Ok(())
}

fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} 已存在。如需重新生成，请添加 --force。\n\
             这会覆盖当前配置，其中的密钥也将丢失",
            path.display()
        );
    }
    let mut cfg = tw_config::generate_initial();
    // **服务器上手工部署才跑 init**：远程控制端口这一节写出来、关着，端口现挑。
    // 要用时把 enabled 改成 true（或者 twcore remote enable）
    cfg.listen.control.remote = Some(tw_config::RemoteListen {
        enabled: false,
        bind: tw_config::Bind::All,
        port: tw_config::generate_remote_port(cfg.listen.gateway.port),
        allow_from: tw_config::default_allow_from(),
    });
    let key = cfg.clients[0].key.clone();
    write_config(path, &cfg)?;
    println!("wrote {}", path.display());
    println!();
    println!("gateway key: {key}");
    println!(
        "(configure a client with it. The tw- prefix tells a gateway key from an upstream API key.)"
    );
    println!();
    println!(
        "The configuration also holds the control key, which the desktop app connects with; \
         twcore control-key prints it. To let a desktop app on another machine connect, run \
         twcore remote enable."
    );
    println!();
    println!("Next: add an upstream under providers, then run twcore serve.");
    Ok(())
}

fn write_config(path: &Path, cfg: &tw_config::Config) -> Result<()> {
    // 权限、原子写、目录创建都在 tw-config::write 里。两处各写一遍就是
    // 两处会漂移 —— 而漂移的那一处大概率是漏了 0600 的那处。
    Ok(tw_config::write(path, cfg)?)
}

/// 读配置、按 `serve` 会看到的样子校验。
///
/// **缺控制面的钥匙不算错**：`serve` 起来时会补上（只补那一行），这里在内存里
/// 同样补一把再校验，说一句就好 —— 否则升级之后的第一次 check 会把一份好好的
/// 配置报成坏的。
fn check_like_serve(path: &Path) -> Result<(tw_config::Config, bool)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (text, added) = match tw_config::control_key::ensure(&text) {
        Ok(Some(with_key)) => (with_key, true),
        _ => (text, false),
    };
    let cfg = tw_config::try_parse(&text).map_err(|r| anyhow::anyhow!("{r}"))?;
    Ok((cfg, added))
}

fn cmd_check(path: &Path) -> Result<()> {
    match check_like_serve(path) {
        Ok((cfg, key_missing)) => {
            println!("✅ {} is valid", path.display());
            if key_missing {
                println!("   · it has no control key yet; twcore serve adds one when it starts");
            }
            println!(
                "   {} gateway keys, {} upstreams",
                cfg.clients.len(),
                cfg.providers.len()
            );
            for p in &cfg.providers {
                let proto = match p.effective_protocol() {
                    Some(x) => format!("{x:?}"),
                    // 猜不出来不是错误，但值得说一句 —— M0 按 Anthropic 走。
                    None => "not recognized (forwarded in the Anthropic format)".to_string(),
                };
                // 说来源而不是值。
                let credential = match (&p.key, &p.oauth, &p.aws) {
                    (Some(k), _, _) => {
                        format!("API key {} ({})", k.describe(), p.auth_header().0)
                    }
                    (None, Some(_), _) => "OAuth".to_string(),
                    // 签名用的密钥：说来源（环境变量、写死的还是 profile），不说值
                    (None, None, Some(a)) => format!(
                        "{}, signed for {}",
                        match (&a.profile, &a.access_key_id) {
                            (Some(profile), _) => format!("AWS profile `{profile}`"),
                            (None, Some(id)) => format!("AWS access key {}", id.describe()),
                            (None, None) => "no AWS access key".to_string(),
                        },
                        p.bedrock_region().unwrap_or("an unknown region")
                    ),
                    (None, None, None) if !p.headers.is_empty() => "headers".to_string(),
                    (None, None, None) => "no credential".to_string(),
                };
                println!(
                    "   · {} → {} [{}]  {credential}",
                    p.name,
                    tw_secret::redact_url(&p.base_url),
                    proto,
                );
                for h in p.headers.iter() {
                    let raw = h.value.raw();
                    let shown = if tw_secret::is_public_header(&h.name)
                        || tw_secret::is_reference_only(raw)
                    {
                        raw.to_string()
                    } else {
                        tw_secret::mask_secret(raw)
                    };
                    println!("     · header {}: {shown}", h.name);
                }
                // OAuth 不在这里换 token：那是一次网络往返，而 check
                // 是个用户期望立刻返回的命令。但**能离线查的都要查** ——
                // 这几样写错了，症状全是网关起来之后一片 401。
                if let Some(o) = &p.oauth {
                    if o.refresh.trim().is_empty() {
                        println!(
                            "     ⚠ the refresh token is empty, so no access token can be obtained"
                        );
                    }
                    // **本机的 http 不算明文过网。**报它是个假警报，而
                    // 假警报的代价是用户学会忽略这一栏的所有话（
                    // 没有风险的时候要说「安全」，不是把话说满）。
                    // tw-guard 的内网规则出于同一个理由排除回环。
                    let loopback = o.endpoint.starts_with("http://127.0.0.1")
                        || o.endpoint.starts_with("http://localhost")
                        || o.endpoint.starts_with("http://[::1]");
                    if !o.endpoint.starts_with("https://") && !loopback {
                        // refresh token 换得出无数个 access token。
                        // 它走明文 = 整个凭据走明文
                        println!(
                            "     ⚠ the token endpoint is not https ({}), so the refresh token travels in the clear",
                            tw_secret::redact_url(&o.endpoint)
                        );
                    }
                    if let Some(raw) = &o.refresh_before
                        && tw_config::parse_duration_secs(raw).is_none()
                    {
                        // 静默走默认值是对的（一个写错的提前量不该让上游
                        // 整个不可用），但**不说出来就没人会发现自己写错了**
                        println!(
                            "     ⚠ refresh_before is `{raw}`, which is not recognized (it is written as 30s, 5m or 1h); using 300 s"
                        );
                    }
                    println!(
                        "     · OAuth: getting a token needs the network, and happens once the gateway starts"
                    );
                } else if let Err(e) = p.outbound_headers(None) {
                    println!("     ⚠ the credential could not be obtained: {e}");
                } else if let Err(e) = p.aws_credentials() {
                    println!("     ⚠ the AWS access keys could not be obtained: {e}");
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

fn already_running(dir: &Path, pid: u32) -> anyhow::Error {
    anyhow::anyhow!(
        "已有 twcore 实例正在运行（pid {pid}）。\n\
         两个实例无法共用同一个网关端口，请先停止该实例。\n\
         如果该进程实际已退出，请删除 {} 后重试",
        dir.join("twcore.lock").display()
    )
}

fn cmd_serve(path: &Path, port: Option<u16>, safe: bool, parent: Option<u32>) -> Result<()> {
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(tw_api::data::dir);
    // **数据目录在这儿建，而且只在这儿。**后面每一步（锁、控制面的 socket、
    // 端口文件）都会顺手 `create_dir_all`，谁先到谁建 —— 而 Windows 上「谁
    // 建的」决定了它的 ACL，也就决定了那份明文密钥同机的其他用户读不读得到。
    tw_config::private_dir::create(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // **锁被一个活着的 core 占着时，先问它是不是在等接替者**（升级不断线，见 `handover`）。
    // 是的话接管它；不是的话照旧报「已有实例在运行」
    let (mut lock, mut incoming) = match LockFile::acquire(&dir)? {
        LockOutcome::Acquired(l) => (Some(l), None),
        LockOutcome::AlreadyRunning { pid } => match rt.block_on(handover::approach(&dir, pid)) {
            handover::Approach::Proceed(inc) => {
                tracing::info!(pid, "taking over from the running core");
                (None, Some(inc))
            }
            handover::Approach::StartFresh => match LockFile::acquire(&dir)? {
                LockOutcome::Acquired(l) => (Some(l), None),
                LockOutcome::AlreadyRunning { pid } => return Err(already_running(&dir, pid)),
            },
            handover::Approach::NotArmed => return Err(already_running(&dir, pid)),
        },
    };
    let taking_over = incoming.is_some();
    // **还在排空的前任**（见 `lockfile::Draining`）：接替者在它排空完之前退了，这个进程是照常
    // 拿到的锁，可前任还在写请求库。这时候和接替者一样：等它们都退了才开库，请求号跳过它们的
    let draining = lockfile::draining(&dir);
    let mut predecessors: Vec<u32> = draining.iter().map(|d| d.pid).collect();
    if let Some(inc) = &incoming {
        predecessors.push(inc.predecessor);
    }
    predecessors.sort_unstable();
    predecessors.dedup();
    let orphaned_ids = (!taking_over)
        .then(|| draining.iter().map(|d| d.ids_below).max())
        .flatten();
    if orphaned_ids.is_some() {
        tracing::warn!(
            ?predecessors,
            "a core that handed over is still draining; the request history opens once it exits"
        );
    }
    // 还没进库的记录有几条（库开之前退出时说一声）
    let pending = record::Pending::default();
    let pending_outer = pending.clone();

    // **配置不存在就地生成**（第 1 步）。
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
            .with_context(|| format!("writing the initial configuration {}", path.display()))?;
        tracing::info!(path = %path.display(), "first run; wrote an initial configuration");
    } else {
        // **控制面的钥匙在监听之前写好**：桌面端看到 socket 出现就来读它。旧配置
        // 没有这一行就只补这一行（最小替换，注释和排版不动）—— 升级不需要迁移。
        // 补不上（YAML 写坏了）不在这里停：下一步加载会说清哪一行坏了
        match tw_config::control_key::ensure_file(path) {
            Ok(true) => {
                tracing::info!(path = %path.display(), "added a control key to the configuration")
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("a control key could not be added to the configuration: {e}"),
        }
    }
    // **安全模式下配置读不了也要起控制面。**安全模式就是给「配置出了问题」准备的：用户要能
    // 看到哪一行错了、在界面里改、回滚、一键修复。以前这一步读不了就退出，安全模式也一样 ——
    // 守护连败五次进安全模式，安全模式的 core 又退出，界面上只剩一句「已停止」，原因只在日志里。
    //
    // 顶上的是一份临时的空配置，只有控制面的钥匙取自原文（桌面端从同一个文件读它）。原文连
    // 钥匙都找不到（YAML 坏到解析不了）就照旧退出：那时起了控制面也没人进得来
    //
    // **接管时也一样**（H3）：新版本读不进这份配置，就只接管控制面、等配置修好，网关的监听
    // 留在旧 core 上按旧配置转发
    let (cfg, stood_in) = match tw_config::load(path) {
        Ok(cfg) => (cfg, None),
        Err(e) => {
            let standin = (safe || taking_over)
                .then(|| std::fs::read_to_string(path).ok())
                .flatten()
                .and_then(|text| tw_config::stand_in(&text));
            match standin {
                Some((cfg, r)) => {
                    tracing::error!(
                        "the configuration does not load ({r}); serving the control plane with a stand-in"
                    );
                    (cfg, Some(r))
                }
                None => {
                    return Err(e).with_context(|| format!("loading {}", path.display()));
                }
            }
        }
    };
    // `--port` 是一个**显式的覆盖**，配置文件不该推翻它。所以给了它
    // 之后就不再跟着配置里的监听地址走（「温」那一级）。
    let overridden = port.is_some();
    let mut listen = cfg.listen.gateway.clone();
    if let Some(p) = port {
        listen.port = p;
    }
    // **`bind` 写的是网卡名时，这里不去问系统。**那张网卡此刻不在（开机自启时
    // WSL 还没起来，它的虚拟网卡也就还没有）不该让网关起不来：监听那一边先只
    // 听回环，网卡出现了再补上，换了地址也跟着换，见 `tw_gateway::serve_at`
    let listen_at = format!("{}:{}", listen.bind, listen.port);

    // 控制面听在哪由平台决定，见 `tw_api::control::Address`。
    let endpoint = tw_api::control::Address::in_dir(&dir);
    // **在起任何东西之前问**。等到 bind 失败时，网关已经在监听、客户端
    // 可能已经连上来了，而这条错误当时只会进日志。
    tw_control::endpoint_usable(&endpoint)?;
    // 「该退了」的开关。**在起任何东西之前建好** —— 控制面和主循环各拿
    // 一份，而控制面可能在主循环开始等之前就收到那条请求。
    let shutdown = tw_control::Shutdown::default();
    let config_path = path.to_path_buf();
    // 后台事务的运行时（见 `background`）。**在这里留一份**：进程退出时要在异步上下文之外关掉它
    let bg = Arc::new(Mutex::new(Background::default()));
    let bg_outer = bg.clone();
    let r = rt.block_on(async move {
        let state = tw_gateway::AppState::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
        // 默认价目表：上次联网刷新存下的那份，或者内置的。自定义价目表已经
        // 随配置进了价格簿
        state.set_price_table(tw_control::pricing::load_table(&config_path));
        // 接替者：旧 core 的内存状态接进来，**在第一个请求之前**
        if let Some(inc) = incoming.as_mut() {
            // 提交之前续期归旧 core（见 `Cache::hold_renewals`）
            state.oauth.hold_renewals(true);
            inc.import(&state);
        }
        if let Some(below) = orphaned_ids {
            state.bus.resume_after(below.saturating_add(handover::ID_GAP));
        }

        // 观测这一层。**起不来不是致命的** —— 历史记录看不见，而网关
        // 照常转发。所以这里所有的失败都只记一行日志。
        //
        // body 的通道在这里建：**它是唯一同时看得见网关和存储的地方**，
        // 而两边各有各的同形结构，是为了不让「观测」挂到「转发」下面。
        let (body_tx, body_rx) = tw_gateway::bodies::channel();
        // 插件在每个请求上的运行记录，和正文同一个道理：网关交出去，存储层落库
        let (run_tx, run_rx) = tokio::sync::mpsc::channel(tw_gateway::plugin::RUN_CHANNEL_CAP);
        let wiring = record::Wiring {
            bus: state.bus.clone(),
            pricing: state.pricing.clone(),
            bodies: body_rx,
            runs: run_rx,
            settled: tw_control::key_limits::settle_hook(&state),
        };
        let (store, store_opened) = if predecessors.is_empty() {
            let store = record::open_now(&dir, wiring);
            if let Some(rec) = &store {
                // 密钥的用量上限：这一天、这一周、这个月已经用了多少，从记录里加回来。**在第一个
                // 请求之前**（网关还没开始听）
                tw_control::key_limits::rebuild(&state, rec.lock().await.db());
                // 之后一期的开头变了（到了下一期、换了时区），从记录里把新的那一期加起来
                tw_control::key_limits::follow(&state, rec.clone());
            }
            (store, watch::channel(true).1)
        } else {
            // 接替者（或者前任还在排空）：前任都退了才打开请求库（R3，见 `record`）。用量从
            // 记录重建在打开的那一刻；排空标记在那时清掉
            let gw = state.clone();
            let marks = dir.clone();
            let (store, opened) = record::open_later(
                &dir,
                wiring,
                predecessors.clone(),
                Box::new(move |db| {
                    tw_control::key_limits::rebuild(&gw, db);
                    lockfile::prune_draining(&marks);
                }),
                pending,
            );
            if let Some(rec) = store.clone() {
                let (gw, mut opened) = (state.clone(), opened.clone());
                tokio::spawn(async move {
                    if opened.wait_for(|o| *o).await.is_ok() {
                        tw_control::key_limits::follow(&gw, rec);
                    }
                });
            }
            (store, opened)
        };
        if store.is_some() {
            state.set_body_sink(body_tx);
            state.set_plugin_sink(run_tx);
        }

        // 配置的唯一入口。UI、CLI、文件监听都从这里进。
        let manager = tw_control::ConfigManager::new(config_path, state.clone(), state.bus.clone());
        let manager = std::sync::Arc::new(match &stood_in {
            Some(r) => manager.standing_in(r),
            None => manager,
        });
        // 磁盘上此刻的样子和最新的备份版本不一样就留一版：第一次运行、升级之后、core 没在跑时
        // 命令行改过。配置读不进来时也留：一键修复之前的那一份在备份里
        manager.record_startup();
        // 默认插件（随 core 发的那几个）：没给过的装上（停用着），没动过的换成新版。
        // 启动时在控制面起来之前走一遍，界面第一次取插件就看得到它们；之后每换入一份
        // 配置再走一遍。**不挡启动**：哪个没办成只记一行、说一声。安全模式不走 ——
        // 那时只有控制面，不替人往配置里写东西。**接替者提交之后才走**：之前写配置的是旧 core
        let mut seeder = None;
        if !safe && !taking_over {
            let s = tw_control::plugins::defaults::Seeder::shipped();
            s.seed(&manager).await;
            seeder = Some(s);
        }
        // **监听要留着** —— 扔掉它就停止监听，而那个失效是静默的。
        // 起不来不是致命的：手改文件不会自动生效，但界面和 CLI 照常能用，
        // 所以说一句就继续。
        let mut watchers: Vec<Box<dyn Send>> = Vec::new();
        match tw_control::spawn_watcher(manager.clone()) {
            Ok(w) => watchers.push(Box::new(w)),
            Err(e) => {
                tracing::warn!("the configuration file cannot be watched, so a hand edit will not take effect on its own: {e}");
            }
        }
        // 插件目录也盯着：**插件文件被改了，那个插件马上停用**，不等下一次改配置。
        // 盯不住时退回到每次换配置时重算哈希，所以同样只说一句
        match tw_control::plugins::spawn_watcher(state.clone(), manager.path()) {
            Ok(w) => watchers.push(Box::new(w)),
            Err(e) => {
                tracing::warn!("the plugin directory cannot be watched, so a changed plugin file is noticed only at the next configuration change: {e}");
            }
        }

        // 控制面无论如何都要起来 —— **网关挂了的时候，用户最需要的恰恰
        // 是能改配置**。安全模式就是「只有这一半」。
        let control = tw_control::ControlState {
            shutdown: shutdown.clone(),
            remote: Default::default(),
            handover: Default::default(),
            started: std::time::Instant::now(),
            gateway: state.clone(),
            cfg: manager.clone(),
            store: store.clone(),
            price_updater: Default::default(),
            chatgpt: Default::default(),
            zai: Default::default(),
        };
        // 安全模式没有数据面：没有可交接的，arm 一律拒绝
        control.handover.set_no_data_plane(safe);
        // 前任还在排空、却没有交接连接（它的接替者先退了）：照「前任在排空」起来，等它退了回到平常
        if orphaned_ids.is_some() {
            handover::follow_vanished(predecessors.clone(), control.handover.clone());
        }
        let gateway = tw_gateway::listen::Handoff::default();
        if let Some(inc) = incoming.as_mut() {
            // 接替者：控制面在旧 core 交过来的监听上服务；网关、远程控制端口提交之前不跟配置走
            control.handover.set_phase(tw_api::HandoverState::TakingOver);
            if let Some(s) = inc.control.take() {
                control.handover.control().inherit(s);
            }
            control.remote.hold();
            gateway.hold();
        }
        // 后台事务（写回、同步、价目表、默认插件、模型目录、余额、日志回收）。**接替者提交
        // 之后才起**：之前做这些事的是旧 core
        let mut sync = None;
        if !taking_over {
            let started = bg
                .lock()
                .map_err(|_| anyhow::anyhow!("the background lock is poisoned"))?
                .start(background::Start {
                    control: &control,
                    safe,
                    seeder: seeder.take(),
                    store: store.clone(),
                    store_opened: store_opened.clone(),
                })
                .context("starting the background tasks")?;
            sync = Some(started);
        }

        let at = endpoint.clone();
        // **控制面没了就得退，不能只记一行日志。**
        //
        // 一个没有控制面的 core 是 UI 完全够不着的：连不上、改不了配置、
        // 也关不掉。守护那边的心跳会因此失败，然后按重启阶梯反复拉起来，
        // 而用户看到的是「core 连续失败」——真正的原因（比如 socket 路径
        // 太长）只在日志里躺着。退出让那句话有机会走到人眼前。
        let (control_died, mut control_dead) = tokio::sync::oneshot::channel();
        {
            let control = control.clone();
            tokio::spawn(async move {
                let r = tw_control::serve(control, &at).await;
                let msg = match r {
                    Err(e) => format!("{e}"),
                    // serve 正常返回意味着 accept 循环结束了，同样是没了
                    Ok(()) => "the control plane ended unexpectedly".to_string(),
                };
                let _ = control_died.send(msg);
            });
        }

        // 父进程守望：GUI 没了我们跟着退。轮询而不是用 kqueue，是因为
        // 这段代码要能在被重新 parent 之后仍然正确 —— macOS 上收养我们的
        // 是 launchd（getppid() 变成 1），Linux 桌面会话里多半是
        // `systemd --user`（它是 subreaper，getppid() 变成它的 pid，不是 1）。
        // 所以**不看 getppid()**，只看原来那个 pid 还在不在，哪个平台、
        // 谁来收养都一样。
        //
        // **arm 之后关掉**（见 `handover`）：桌面端装好更新要重启，core 留下来等接替者
        let parent_watch = parent.map(handover::watch_parent);

        if safe {
            // 安全模式：只起控制面。数据面不动，让用户还能改配置、回滚。
            // **这时候控制面就是全部** —— 它没了，这个进程一件事都干不了。
            //
            // 安全模式的接替者只接管控制面，**一直握着交接连接**：松手的话旧 core 回到待交接，
            // 又从控制面上接新连接了
            tracing::warn!("safe mode: starting the control plane only, not the data plane");
            let held = async {
                match incoming {
                    Some(inc) => inc.hold().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                msg = control_dead => {
                    anyhow::bail!("{}", msg.unwrap_or_else(|_| "the control plane stopped".into()))
                }
                _ = shutdown_requested(&shutdown) => {}
                _ = held => {}
            }
            return Ok(());
        }

        let follow = !overridden;
        let mut serve = match incoming {
            None => {
                tracing::info!(listen = %listen_at, "starting");
                tokio::spawn(tw_gateway::listen::serve_with(
                    state.clone(),
                    listen,
                    follow,
                    gateway.clone(),
                    Vec::new(),
                ))
            }
            Some(mut inc) => {
                // H3：配置读不进来就只接管控制面，等配置修好（换入成功）再接着走
                if stood_in.is_some() {
                    tracing::warn!("taking over the control plane only until the configuration loads; the previous core keeps forwarding");
                    tokio::select! {
                        _ = config_fixed(&manager) => {
                            tracing::info!("the configuration loads now; taking over the gateway");
                        }
                        msg = &mut control_dead => {
                            anyhow::bail!("{}", msg.unwrap_or_else(|_| "the control plane stopped".into()))
                        }
                        _ = shutdown_requested(&shutdown) => return Ok(()),
                    }
                }
                let inherited = tokio::select! {
                    r = inc.listeners(&state) => r?,
                    msg = &mut control_dead => {
                        anyhow::bail!("{}", msg.unwrap_or_else(|_| "the control plane stopped".into()))
                    }
                    _ = shutdown_requested(&shutdown) => return Ok(()),
                };
                // 配置换入过（H3 修好了），按此刻的配置跟
                let listen = if follow {
                    state.config().listen.gateway.clone()
                } else {
                    listen
                };
                let serve = tokio::spawn(tw_gateway::listen::serve_with(
                    state.clone(),
                    listen,
                    follow,
                    gateway.clone(),
                    inherited.gateway,
                ));
                if let Some((want, l)) = inherited.remote {
                    control.remote.adopt(want, l);
                }
                // 已经在全部监听上 accept 了才提交（不可逆，H5）
                tokio::time::timeout(handover::commit_within(), gateway.listening())
                    .await
                    .context("the listeners handed over could not be served in time")?;
                // 旧 core 回了 CommitAck（或者它已经没了）才算提交；它还在却没回，这边退出，
                // 守护重来（这时它仍握着全部监听）
                inc.commit().await?;
                // ── 从这里起不再回退（H5）。旧 core 已经停止 accept：**下面任何一步出错都只记
                // 一行、照常服务**，退出的话就没人在听了
                //
                // 从此续期由这一边做（旧 core 交出第二份快照起已暂停）
                state.oauth.hold_renewals(false);
                // 成为锁的持有者：写上自己的 pid（旧 core 已经放弃了锁），记下它在排空
                match LockFile::claim(&dir) {
                    Ok(l) => lock = Some(l),
                    Err(e) => tracing::warn!("the lock file could not be taken over: {e}"),
                }
                if let Err(e) = lockfile::mark_draining(
                    &dir,
                    lockfile::Draining::new(inc.predecessor, inc.ids_from),
                ) {
                    tracing::warn!("the draining core could not be noted down: {e}");
                }
                gateway.follow();
                control.remote.follow();
                control.handover.set_phase(tw_api::HandoverState::Draining);
                tracing::info!("took over; the previous core is draining");
                let s = tw_control::plugins::defaults::Seeder::shipped();
                s.seed(&manager).await;
                let started = match bg.lock() {
                    Ok(mut g) => g
                        .start(background::Start {
                            control: &control,
                            safe: false,
                            seeder: Some(s),
                            store: store.clone(),
                            store_opened: store_opened.clone(),
                        })
                        .map_err(|e| e.to_string()),
                    Err(_) => Err("the background lock is poisoned".to_string()),
                };
                match started {
                    Ok(started) => sync = Some(started),
                    Err(e) => tracing::error!(
                        "the background tasks could not start, so token write-back, sync and price updates are off this run; forwarding is unaffected: {e}"
                    ),
                }
                inc.follow_drain(state.clone(), control.handover.clone());
                serve
            }
        };

        // 旧 core 那一半：接 arm、等接替者、交接、排空（见 `handover`）
        let core = Arc::new(handover::Core {
            dir: dir.clone(),
            state: state.clone(),
            control: control.clone(),
            gateway: gateway.clone(),
            background: bg.clone(),
            sync: Mutex::new(sync.clone()),
            watchers: Mutex::new(watchers),
            lock: Mutex::new(lock),
            parent: Mutex::new(parent_watch),
            parent_pid: parent,
            store_opened,
            handed_over: Default::default(),
        });
        let mut exit = handover::serve_arms(core.clone());

        let mut serving = true;
        let mut armed_task = true;
        loop {
            tokio::select! {
                r = &mut serve, if serving => match r {
                    // 交接提交之后停止了 accept：在跑的请求还在，等交接那一边说退
                    Ok(Ok(())) => serving = false,
                    Ok(Err(e)) => {
                        return Err(e).with_context(|| format!("{listen_at} could not be listened on. If the port is taken, check whether an earlier instance has fully exited"));
                    }
                    Err(e) => anyhow::bail!("the gateway stopped: {e}"),
                },
                msg = &mut control_dead => {
                    anyhow::bail!("{}", msg.unwrap_or_else(|_| "the control plane stopped".into()))
                }
                // 系统的退出信号、`/shutdown`：照旧立即退出（F9；`/shutdown` 的语义不变）
                _ = shutdown_requested(&shutdown) => {
                    tracing::info!("got a shutdown signal");
                    // 还没上传的本机改动，退出前上传（等不了太久：没传成的下次启动时传）。
                    // **交出去了就不传**：同步已经归接替者
                    if !core.handed_over.load(std::sync::atomic::Ordering::SeqCst)
                        && let Some(sync) = &sync
                    {
                        sync.flush().await;
                    }
                    return Ok(());
                }
                e = &mut exit, if armed_task => match e {
                    Ok(e) => {
                        if e == handover::Exit::Retired
                            && let Some(sync) = &sync
                        {
                            sync.flush().await;
                        }
                        return Ok(());
                    }
                    // 交接那一路自己没了（panic）：不是退出的理由，照常服务
                    Err(_) => {
                        tracing::error!("the handover task ended unexpectedly; serving on without handovers");
                        armed_task = false;
                    }
                },
            }
        }
    });
    // 前任还在写库、这边的库还没开就退了：排着的记录写不进去，**说一声丢了多少**
    let left = pending_outer.get();
    if left > 0 {
        tracing::warn!(
            events = left,
            "exiting before the previous core finished draining; {left} queued request events are not recorded"
        );
    }
    // 后台事务的运行时在异步上下文之外关（在里面丢掉一个运行时会 panic）
    let leftover = bg_outer.lock().ok().and_then(|mut g| g.take_runtime());
    if let Some(bg_rt) = leftover {
        bg_rt.shutdown_timeout(std::time::Duration::from_secs(1));
    }
    r
}

/// 等配置读得进来了（H3）：换入了一份、不再是被拒的那一份。
async fn config_fixed(manager: &tw_control::ConfigManager) {
    let mut events = manager.bus().subscribe();
    loop {
        match events.recv().await {
            Ok(tw_api::Event::ConfigReloaded { .. })
            | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if manager.rejected().is_none() {
                    return;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return std::future::pending().await;
            }
        }
    }
}

/// 等一个「该退了」。
///
/// 三个来源：Ctrl-C、SIGTERM，以及控制面上的那条请求。
///
/// **第三个不是锦上添花。**Windows 上根本没有 SIGTERM，而桌面端要在改完配置
/// 之后重启 core、装更新之前停掉它 —— 那个平台上剩下的只有强杀，而强杀意味着
/// WAL 没收尾、在途请求断在半路。见 `tw_control::shutdown`。
async fn shutdown_requested(asked: &tw_control::Shutdown) {
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
    tokio::select! { _ = ctrl_c => {}, _ = term => {}, _ = asked.asked() => {} }
}

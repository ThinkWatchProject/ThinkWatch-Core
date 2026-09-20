//! twcore —— 本地 AI 网关引擎。
//!
//! 它和 UI 在用户眼里是同一个程序：UI 起它、UI 关它、
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
    /// 配置文件路径，默认为 ~/.thinkwatch/config.yaml
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动网关
    Serve {
        /// 覆盖配置中的端口。设为 0 时由系统分配（用于测试）
        #[arg(long)]
        port: Option<u16>,
        /// 仅启动控制面，不启动数据面
        //
        // 守护连续失败后进这个模式 —— 网关挂了的时候，用户最需要的恰恰是
        // 能改配置
        #[arg(long)]
        safe: bool,
        /// 父进程 pid。父进程退出后本进程随之退出
        //
        // 「让用户认为他俩就是一个程序」
        #[arg(long)]
        parent: Option<u32>,
    },
    /// 生成初始配置
    Init {
        /// 配置文件已存在时仍然覆盖
        #[arg(long)]
        force: bool,
    },
    /// 校验配置并输出结果
    Check,
    /// 修改配置
    //
    // **和界面走同一套代码** —— 两套实现就是两套行为
    Config {
        #[command(subcommand)]
        what: ConfigCmd,
    },
    /// 测试链路连通性及各阶段耗时。不发送业务请求，不产生费用
    Speed {
        /// 只测试该上游。不指定时测试全部上游
        provider: Option<String>,
        /// 只测试指定的代理
        #[arg(long)]
        proxy: Option<String>,
    },
    /// 扫描本机客户端配置面：hooks、MCP、skill 与指令文件。只读取，不修改
    Scan {
        /// 额外扫描的项目目录。不会自动查找项目
        #[arg(long)]
        project: Vec<PathBuf>,
        /// 同时输出完整清单，而不仅是发现的问题
        #[arg(long)]
        inventory: bool,
    },
    /// 列出本机的 AI 客户端及其指向的地址
    Clients {
        #[command(subcommand)]
        what: ClientsCmd,
    },
}

#[derive(Subcommand)]
enum ClientsCmd {
    /// 扫描本机客户端。只读取，不修改
    List,
    /// 诊断配置未生效的原因，逐层检查配置的优先级
    Why {
        /// 客户端 id，例如 claude-code
        client: String,
        /// 当前项目目录，用于检查项目级配置是否覆盖了用户级配置
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// 计算接管改动并输出，不写入文件
    Plan { client: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 输出当前配置的原文和版本号
    Show,
    /// 修改一个字段，路径写成 /providers/官方/base_url 的形式
    Set {
        /// 按名称定位，而不是按下标
        //
        // 下标会在重排之后指向另一个东西
        path: String,
        value: String,
        /// 按整数写入，而不是字符串。--int 8788 与 8788 不同
        #[arg(long, conflicts_with_all = ["bool_value", "null"])]
        int: bool,
        #[arg(long = "bool", conflicts_with_all = ["int", "null"])]
        bool_value: bool,
        #[arg(long, conflicts_with_all = ["int", "bool_value"])]
        null: bool,
    },
    /// 列出历史版本
    History,
    /// 回滚到指定版本
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
        Command::Scan { project, inventory } => cmd_scan(&path, project, inventory),
    }
}

/// 静态扫描。**只报告，不删任何东西。**
fn cmd_scan(config: &Path, projects: Vec<PathBuf>, inventory: bool) -> Result<()> {
    // 规则住在 config.yaml 的 `security.scan_rules` 里（只有一份
    // 配置文件）。读不出配置时用内置那套 —— 扫描不该因为配置坏了就停摆
    let user = tw_config::load(config)
        .map(|c| c.security.scan_rules.clone())
        .unwrap_or_default();
    let rules = tw_scan::rules::build(&user)?;
    for w in &rules.warnings {
        println!("⚠ {w}");
    }
    let mut srcs = tw_scan::sources::user_level(&home());
    for p in &projects {
        srcs.extend(tw_scan::sources::in_project(p));
    }
    println!("已扫描 {} 个文件（规则：{}）", srcs.len(), rules.summary());
    let r = tw_scan::report::scan(&srcs, &rules);

    if inventory {
        let conflicting = tw_scan::report::conflicting(&r.mcp);
        println!("\nMCP server（{}）：", r.mcp.len());
        for m in &r.mcp {
            let mark = if conflicting.contains(&m.name) {
                " ⚠ 同名但配置不同"
            } else {
                ""
            };
            let off = if m.enabled { "" } else { "（已停用）" };
            let what = match &m.url {
                Some(u) => format!("远端 {u}"),
                None => format!("{} {}", m.command, m.args.join(" ")),
            };
            println!("  {:<20} {:<14} {what}{off}{mark}", m.name, m.client);
            if !m.env_keys.is_empty() {
                println!(
                    "  {:<20} {:<14} 读取环境变量 {}",
                    "",
                    "",
                    m.env_keys.join("、")
                );
            }
        }
        println!("\nhook（{}）：", r.hooks.len());
        for h in &r.hooks {
            println!("  {:<14} {:<14} {}", h.event, h.client, h.command);
        }
        println!("\nskill（{}）：", r.skills.len());
        for s in &r.skills {
            println!("  {:<20} {}", s.name, s.path.display());
        }
    }

    for u in &r.unreadable {
        println!("⚠ 无法读取：{u}");
    }
    if r.findings.is_empty() {
        // 没风险的时候要说「安全」，而不是什么都不显示
        println!("\n✓ 未发现问题。");
        return Ok(());
    }
    println!("\n发现 {} 处问题：", r.findings.len());
    for f in &r.findings {
        let mark = match f.level {
            tw_scan::report::Level::High => "✗ 高危",
            tw_scan::report::Level::Medium => "? 可疑",
            tw_scan::report::Level::Low => "· 提示",
        };
        println!("{mark}  {}", f.title);
        println!("       {}:{}", f.path.display(), f.line);
        println!("       {}", f.excerpt.trim());
        println!("       {}", f.detail);
    }
    println!("\n扫描只报告问题，不会删除任何内容。");
    Ok(())
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
                    (false, _, _) => "未安装".to_string(),
                    (true, Some(at), Some(ep)) => format!("已接管 {} → {ep}", fmt_time(at)),
                    // **「我们写过」和「现在还是那样」是两回事。**
                    (true, Some(at), None) => {
                        format!("曾于 {} 接管，但配置中已不包含接管写入的字段", fmt_time(at))
                    }
                    (true, None, Some(ep)) => format!("未接管，当前指向 {ep}"),
                    (true, None, None) => "已安装，未接管".to_string(),
                };
                println!("{:<14} {:<12} {state}", d.id, d.name);
                println!("               {}", d.real.display());
                if d.real != d.path {
                    println!("               （{} 是符号链接）", d.path.display());
                }
                for s in &d.shadows {
                    println!("               ⚠ {} 优先级更高", s.display());
                }
                if d.verified == tw_adopt::clients::Verified::FieldsOnly {
                    println!("               ⓘ {}", d.verified.note());
                }
            }
            // 配置读不了时照样列出步骤，地址按默认端口给
            let port = tw_config::load(path)
                .map(|c| c.listen.gateway.port)
                .unwrap_or(tw_config::DEFAULT_GATEWAY_PORT);
            let gw = Gateway {
                base: format!("http://127.0.0.1:{port}"),
                key: None,
            };
            println!();
            println!("不支持自动接管、需要手动配置的客户端：");
            for m in manual_only() {
                println!("  {:<12} {}", m.name, m.how(&gw));
                println!("               {}", m.caveat);
            }
            Ok(())
        }
        ClientsCmd::Why { client, project } => {
            let c = adoptable()
                .into_iter()
                .find(|c| c.id == client)
                .ok_or_else(|| anyhow::anyhow!("未知的客户端 {client}"))?;
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
                .ok_or_else(|| anyhow::anyhow!("未知的客户端 {client}"))?;
            let cfg = tw_config::load(path)?;
            // 0.0.0.0 是监听地址，不是能填进客户端配置的地址 —— 客户端
            // 得知道往哪儿连，那永远是 127.0.0.1
            let gw = Gateway {
                base: format!("http://127.0.0.1:{}", cfg.listen.gateway.port),
                key: None,
            };
            let plan = tw_adopt::plan::plan_adopt(&c, &home(), &gw)?;
            println!("将修改：{}", plan.path.display());
            if plan.is_noop() {
                println!("（配置已是目标状态，无需修改）");
                return Ok(());
            }
            for n in &plan.notes {
                println!("  · {n}");
            }
            println!("\n--- 修改后 ---");
            println!("{}", plan.after);
            println!("--- 以上为预览，未写入文件 ---");
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
                println!("尚无历史版本，首次修改配置后会生成。");
                return Ok(());
            }
            let now = tw_config::store::read(path).map(|c| c.version()).ok();
            // 新的在前 —— 要找的几乎总是最近那几版
            for v in all.iter().rev() {
                let mark = if Some(&v.version) == now.as_ref() {
                    "← 当前"
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
            println!("回滚到指定版本：twcore config rollback <版本号>（输入前几位即可）");
            Ok(())
        }
        ConfigCmd::Rollback { version } => {
            let text = tw_config::history::rollback(path, &version)?;
            println!("已回滚到 {}", tw_config::store::version_of(&text));
            eprintln!("（core 正在运行时，会在一秒内自动加载）");
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
                        .with_context(|| format!("「{value}」不是整数"))?,
                )
            } else if bool_value {
                tw_yaml::Scalar::Bool(
                    value
                        .parse()
                        .with_context(|| format!("「{value}」不是 true 或 false"))?,
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
            println!("已修改，新版本为 {}", tw_config::store::version_of(&next));
            eprintln!("（core 正在运行时，会在一秒内自动加载）");
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
    let cfg = tw_config::load(path).with_context(|| format!("读取 {}", path.display()))?;
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
        println!("     · {}", s.reason.label());
    }
    println!();
}

fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} 已存在。如需重新生成，请添加 --force。\n\
             这会覆盖当前配置，其中的密钥也将丢失",
            path.display()
        );
    }
    let cfg = tw_config::generate_initial();
    let key = cfg.clients[0].key.clone();
    write_config(path, &cfg)?;
    println!("已生成 {}", path.display());
    println!();
    println!("网关密钥：{key}");
    println!("（请在客户端中配置该密钥。tw- 前缀用于区分网关密钥与上游的 API 密钥）");
    println!();
    println!("下一步：在 providers 中添加上游，然后运行 twcore serve。");
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
            println!("✅ {} 校验通过", path.display());
            println!(
                "   {} 个网关密钥，{} 个上游",
                cfg.clients.len(),
                cfg.providers.len()
            );
            for p in &cfg.providers {
                let proto = match p.effective_protocol() {
                    Some(x) => format!("{x:?}"),
                    // 猜不出来不是错误，但值得说一句 —— M0 按 Anthropic 走。
                    None => "未识别（按 Anthropic 格式转发）".to_string(),
                };
                // 说来源而不是值。
                let credential = match (&p.key, &p.oauth) {
                    (Some(k), _) => format!("API 密钥 {}（{}）", k.describe(), p.auth_header().0),
                    (None, Some(_)) => "OAuth".to_string(),
                    (None, None) if !p.headers.is_empty() => "请求头".to_string(),
                    (None, None) => "无凭据".to_string(),
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
                    println!("     · 请求头 {}: {shown}", h.name);
                }
                // OAuth 不在这里换 token：那是一次网络往返，而 check
                // 是个用户期望立刻返回的命令。但**能离线查的都要查** ——
                // 这几样写错了，症状全是网关起来之后一片 401。
                if let Some(o) = &p.oauth {
                    if o.refresh.trim().is_empty() {
                        println!("     ⚠ refresh token 为空，无法获取 access token");
                    }
                    // **本机的 http 不算明文过网。**报它是个假警报，而
                    // 假警报的代价是用户学会忽略这一栏的所有话（
                    // 没有风险的时候要说「安全」，不是把话说满）。
                    // tw-redact 的内网规则出于同一个理由排除回环。
                    let loopback = o.endpoint.starts_with("http://127.0.0.1")
                        || o.endpoint.starts_with("http://localhost")
                        || o.endpoint.starts_with("http://[::1]");
                    if !o.endpoint.starts_with("https://") && !loopback {
                        // refresh token 换得出无数个 access token。
                        // 它走明文 = 整个凭据走明文
                        println!(
                            "     ⚠ token 端点不是 https 地址：{}，refresh token 将以明文传输",
                            tw_secret::redact_url(&o.endpoint)
                        );
                    }
                    if let Some(raw) = &o.refresh_before
                        && tw_config::parse_duration_secs(raw).is_none()
                    {
                        // 静默走默认值是对的（一个写错的提前量不该让上游
                        // 整个不可用），但**不说出来就没人会发现自己写错了**
                        println!(
                            "     ⚠ refresh_before 的值「{raw}」无法识别（应为 30s、5m、1h 的形式），按 300 秒处理"
                        );
                    }
                    println!("     · OAuth 凭据：获取 token 需要联网，将在网关启动后进行");
                } else if let Err(e) = p.outbound_headers(None, None) {
                    println!("     ⚠ 无法获取凭据：{e}");
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
                "已有 twcore 实例正在运行（pid {pid}）。\n\
                 两个实例无法共用同一个网关端口，请先停止该实例。\n\
                 如果该进程实际已退出，请删除 {} 后重试",
                dir.join("twcore.lock").display()
            );
        }
    };

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
            .with_context(|| format!("生成初始配置 {} 失败", path.display()))?;
        tracing::info!(path = %path.display(), "首次运行，已生成初始配置");
    }
    let cfg = tw_config::load(path).with_context(|| format!("加载 {} 失败", path.display()))?;
    // `--port` 是一个**显式的覆盖**，配置文件不该推翻它。所以给了它
    // 之后就不再跟着配置里的监听地址走（「温」那一级）。
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
        // 默认价目表：上次联网刷新存下的那份，或者内置的。自定义价目表已经
        // 随配置进了价格簿
        state.set_price_table(tw_control::pricing::load_table(&config_path));

        // 观测这一层。**起不来不是致命的** —— 历史记录看不见，而网关
        // 照常转发。所以这里所有的失败都只记一行日志。
        //
        // body 的通道在这里建：**它是唯一同时看得见网关和存储的地方**，
        // 而两边各有各的同形结构，是为了不让「观测」挂到「转发」下面。
        let (body_tx, body_rx) = tokio::sync::mpsc::channel(tw_gateway::bodies::CHANNEL_CAP);
        let store = build_store(&dir, state.bus.clone(), state.pricing.clone(), body_rx);
        if store.is_some() {
            state.set_body_sink(body_tx);
        }

        // 配置的唯一入口。UI、CLI、文件监听都从这里进。
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
                tracing::warn!("无法监听配置文件，手动修改的配置不会自动生效：{e}");
                None
            }
        };

        // 控制面无论如何都要起来 —— **网关挂了的时候，用户最需要的恰恰
        // 是能改配置**。安全模式就是「只有这一半」。
        let control = tw_control::ControlState {
            home: tw_control::home_dir(),
            started: std::time::Instant::now(),
            gateway: state.clone(),
            cfg: manager,
            gateway_addr: if safe { None } else { Some(addr.to_string()) },
            store,
            price_updater: Default::default(),
            chatgpt: Default::default(),
        };
        // 盯着客户端配置面。**只报告** —— 这条路径上没有任何
        // 一处会改用户的文件。盯不住就只是少了「变更时告警」，页面上
        // 那份「打开时扫一次」照常可用，所以说一句就继续。
        // 凭据轮换要写回 config.yaml。**这是这个程序里唯一一次
        // 不是人发起的配置写入** —— 理由是服务器换发新 refresh token 的
        // 那一刻旧的就作废了，不写回等于让配置文件从那一秒起就是坏的。
        tw_control::rotation::spawn(control.clone());
        // 定期刷新默认价目表（`pricing.auto_update`，默认开）
        tw_control::pricing::spawn(control.clone());

        let _scan_watch = match tw_control::scan::spawn_watcher(control.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("无法监控客户端配置面，发生变更时不会告警：{e}");
                None
            }
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
                        tracing::info!(ppid, "父进程已退出，随之退出");
                        std::process::exit(0);
                    }
                }
            });
        }

        if safe {
            // 安全模式：只起控制面。数据面不动，让用户还能改配置、回滚。
            // **这时候控制面就是全部** —— 它没了，这个进程一件事都干不了。
            tracing::warn!("安全模式：仅启动控制面，不启动数据面");
            tokio::select! {
                msg = control_dead => {
                    anyhow::bail!("{}", msg.unwrap_or_else(|_| "控制面已停止".into()))
                }
                _ = shutdown_signal() => {}
            }
            return Ok(());
        }

        // 模型目录：后台去问每个上游有哪些模型。**不挡启动** ——
        // 探测要打网络，而网关不该因为一次探测慢而起不来。
        tw_gateway::models::spawn(state.clone());

        tracing::info!(%addr, "启动");
        tokio::select! {
            r = async {
                if overridden {
                    tw_gateway::serve(state, addr).await
                } else {
                    tw_gateway::serve_following_config(state, addr).await
                }
            } => {
                r.with_context(|| format!("监听 {addr} 失败。如果端口被占用，请检查上一个实例是否已完全退出"))
            }
            msg = control_dead => {
                anyhow::bail!("{}", msg.unwrap_or_else(|_| "控制面已停止".into()))
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
/// 处理方式是「不转发了」。
fn build_store(
    dir: &Path,
    // **收整条总线，不只是一个订阅端。**存储层算完价钱要往回报一条
    // （见 `Event::RequestPriced`）—— 它是这条链上唯一知道单价的地方。
    bus: tw_observe::EventBus,
    // **和网关同一份价格簿**，不是一份副本：改了价目表，下一个结束的请求就按新价算
    pricing: tw_pricing::Shared,
    bodies: tokio::sync::mpsc::Receiver<tw_gateway::BodyRecord>,
) -> Option<std::sync::Arc<tokio::sync::Mutex<tw_store::Recorder>>> {
    let events = bus.subscribe();
    let db = match tw_store::Db::open(&dir.join("data.db")) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("请求记录无法启动，本次运行不记录请求，转发不受影响：{e}");
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
        // 算完价钱往回报一条 —— 见 `Event::RequestPriced`。这里是唯一
        // 同时看得见总线和存储层的地方，所以接线在这儿完成。
        tw_store::Recorder::new(db, blobs, pricing).reporting_to(bus),
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

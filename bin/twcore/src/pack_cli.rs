//! `twcore export` / `import` / `backup`：配置包和本机备份的命令行。
//!
//! **不连控制面，直接操作文件**，和 `twcore config` 一样：没有界面的服务器（远程 core）、
//! 配置坏到 core 起不来时都用得上。core 正在跑时，它的文件监听在一秒之内换入写下去的配置，
//! 插件目录的监听重读插件。
//!
//! 规矩和界面上的一样（同一份 `tw-pack`）：导入逐项加、冲突逐项选，恢复留着本机设置、
//! ChatGPT 的登录和接管的客户端在用的密钥。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tw_config::history::Origin;
use tw_pack::model::{ItemId, ItemKind};
use tw_pack::{Pack, PackOrigin};

/// 冲突怎么处理（`--on-conflict`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OnConflict {
    /// Replace the item here with the one in the pack
    Overwrite,
    /// Import it under a new name, such as `relay (2)`
    Rename,
    /// Keep the item here
    Skip,
}

/// 口令：环境变量 `TWCORE_PASSPHRASE`，没有就在终端上问（不回显）
fn passphrase(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("TWCORE_PASSPHRASE") {
        return Ok(p);
    }
    let first = ask_hidden("Passphrase: ")?;
    if confirm {
        if first.chars().count() < tw_pack::MIN_PASSPHRASE {
            bail!(
                "the passphrase needs at least {} characters",
                tw_pack::MIN_PASSPHRASE
            );
        }
        let again = ask_hidden("Passphrase again: ")?;
        if again != first {
            bail!("the two passphrases differ");
        }
    }
    Ok(first)
}

fn ask_hidden(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let _quiet = Quiet::on();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    eprintln!();
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// 读口令时关掉终端的回显，走的时候恢复。不是终端（管道）就什么都不做。
/// **只在 unix 上关**：Windows 的控制台照常回显，要藏起来就用环境变量
struct Quiet {
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

impl Quiet {
    fn on() -> Self {
        #[cfg(unix)]
        {
            // SAFETY: 只对标准输入调 tcgetattr / tcsetattr，传进去的是栈上的结构体
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::isatty(0) == 1 && libc::tcgetattr(0, &mut t) == 0 {
                    let saved = t;
                    t.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(0, libc::TCSANOW, &t);
                    return Quiet { saved: Some(saved) };
                }
            }
            Quiet { saved: None }
        }
        #[cfg(not(unix))]
        {
            Quiet {}
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(t) = self.saved {
            // SAFETY: 同上
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

fn ask(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn current(path: &Path, origin: PackOrigin) -> Result<(tw_config::Loaded, Pack)> {
    let cur = tw_config::store::read(path)?;
    let pack = tw_pack::disk::current(path, &cur.text, origin);
    Ok((cur, pack))
}

fn model(e: tw_pack::model::ModelError) -> anyhow::Error {
    anyhow!("{e}")
}

/// 写下去：插件文件、通用偏好、配置，配置没写成就把前两样还原。前后各留一个备份版本
fn write_back(
    path: &Path,
    cur: &tw_config::Loaded,
    before: &Pack,
    after: &Pack,
    origin: Origin,
) -> Result<()> {
    let _ = tw_pack::backup::record_before(path, &cur.text, origin);
    let undo = tw_pack::disk::write_plugins(path, &before.plugins, &after.plugins)
        .context("writing the plugin files")?;
    let prefs_changed = after.client_prefs != before.client_prefs;
    if prefs_changed && let Err(e) = tw_pack::disk::write_prefs(path, after.client_prefs.as_ref()) {
        undo.restore();
        return Err(e).context("writing the client preferences");
    }
    if let Err(e) = tw_config::store::write_if_unchanged(path, &cur.fingerprint, &after.config_yaml)
    {
        undo.restore();
        if prefs_changed {
            let _ = tw_pack::disk::write_prefs(path, before.client_prefs.as_ref());
        }
        return Err(e.into());
    }
    let _ = tw_pack::backup::record(path, &after.config_yaml, origin);
    Ok(())
}

fn describe(id: &ItemId) -> String {
    let kind = match id.kind {
        ItemKind::Provider => "upstream",
        ItemKind::Proxy => "proxy",
        ItemKind::Key => "gateway key",
        ItemKind::Alias => "alias",
        ItemKind::Group => "group",
        ItemKind::Route => "route",
        ItemKind::PriceSheet => "price sheet",
        ItemKind::SecurityRule => "custom rule",
        ItemKind::Plugin => "plugin",
        ItemKind::ProviderOrder => "the order of upstreams",
        ItemKind::RouteOrder => "the order of routes",
        ItemKind::PluginOrder => "the order of plugins",
        ItemKind::Global => "settings",
        ItemKind::ClientPrefs => "the app's preferences",
    };
    if id.name.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} `{}`", id.name)
    }
}

// ─────────────────────────────────────────────────────────── export

pub fn export(
    path: &Path,
    providers: Vec<String>,
    no_secrets: bool,
    encrypt: bool,
    out: PathBuf,
) -> Result<()> {
    let (_, local) = current(path, PackOrigin::Export)?;
    let scope = (!providers.is_empty()).then(|| {
        providers
            .iter()
            .map(|p| ItemId::new(ItemKind::Provider, p.trim()))
            .collect::<Vec<_>>()
    });
    if let Some(ids) = &scope {
        let snap = tw_pack::model::snapshot(&local).map_err(model)?;
        if let Some(missing) = ids.iter().find(|id| !snap.items.contains_key(*id)) {
            bail!("there is no upstream named `{}`", missing.name);
        }
    }
    let exported = tw_pack::export::export(
        &local,
        tw_pack::export::Options {
            scope,
            secrets: !no_secrets,
            client_prefs: None,
            origin: PackOrigin::Cli,
        },
    )
    .map_err(model)?;
    let pass = encrypt.then(|| passphrase(true)).transpose()?;
    let seal = match &pass {
        Some(p) => tw_pack::Seal::Passphrase(p),
        None => tw_pack::Seal::None,
    };
    let bytes = tw_pack::write(&exported.pack, seal).map_err(|e| anyhow!("{e}"))?;
    tw_pack::disk::write_private(&out, &bytes)
        .with_context(|| format!("writing {}", out.display()))?;
    println!(
        "exported {} items to {}",
        exported.included.len(),
        out.display()
    );
    if !exported.omitted_logins.is_empty() {
        println!(
            "not exported: the ChatGPT sign-in of {}; sign in again after importing",
            exported.omitted_logins.join(", ")
        );
    }
    if !no_secrets && pass.is_none() {
        eprintln!("the file holds keys in plain text; keep it safe");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────── import

fn read_pack(file: &Path) -> Result<Pack> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let pass = tw_pack::is_encrypted(&bytes)
        .then(|| passphrase(false))
        .transpose()?;
    let unseal = match &pass {
        Some(p) => tw_pack::Unseal::Passphrase(p),
        None => tw_pack::Unseal::None,
    };
    tw_pack::read(&bytes, unseal).map_err(|e| anyhow!("{e}"))
}

pub fn import(
    path: &Path,
    file: &Path,
    dry_run: bool,
    on_conflict: Option<OnConflict>,
) -> Result<()> {
    use tw_pack::import::{Action, Decision, Status};
    let incoming = read_pack(file)?;
    let (cur, local) = current(path, PackOrigin::Import)?;
    let ctx = tw_pack::import::Context {
        has_aws_profile: &|p| tw_config::aws_profile::credentials(p).is_ok(),
        new_key: &tw_config::generate_key,
        with_prefs: true,
    };
    let rows = tw_pack::import::preview(&local, &incoming, &ctx).map_err(|e| anyhow!("{e}"))?;
    let mut decisions = Vec::new();
    for r in &rows {
        let what = describe(&r.item);
        let action = match r.status {
            Status::Same => {
                println!("  same      {what}");
                continue;
            }
            Status::Error => {
                let why = r.error.as_ref().map(|e| e.text.clone()).unwrap_or_default();
                println!("  error     {what}: {why}");
                continue;
            }
            Status::New => {
                println!("  new       {what}");
                Action::Import
            }
            Status::Conflict => {
                println!("  differs   {what} ({})", r.differs.join(", "));
                let choice = match on_conflict {
                    Some(c) => c,
                    None if dry_run => continue,
                    None => loop {
                        let a = if r.global {
                            ask("    [o]verwrite or [s]kip? ")?
                        } else {
                            ask("    [o]verwrite, [r]ename or [s]kip? ")?
                        };
                        match a.as_str() {
                            "o" | "overwrite" => break OnConflict::Overwrite,
                            "r" | "rename" if !r.global => break OnConflict::Rename,
                            "s" | "skip" => break OnConflict::Skip,
                            _ => {}
                        }
                    },
                };
                match (choice, &r.suggested_name) {
                    (OnConflict::Overwrite, _) => Action::Overwrite,
                    (OnConflict::Rename, Some(n)) => Action::Rename(n.clone()),
                    _ => Action::Skip,
                }
            }
        };
        decisions.push(Decision {
            item: r.item.clone(),
            action,
        });
    }
    if dry_run {
        println!("(dry run: nothing was written)");
        return Ok(());
    }
    let applied =
        tw_pack::import::apply(&local, &incoming, &decisions, &ctx).map_err(|e| anyhow!("{e}"))?;
    write_back(path, &cur, &local, &applied.pack, Origin::Import)?;
    println!(
        "imported; the new version is {}",
        tw_config::store::version_of(&applied.pack.config_yaml)
    );
    eprintln!("(a running core picks this up within a second)");
    Ok(())
}

// ─────────────────────────────────────────────────────────── backup

pub fn list(path: &Path) -> Result<()> {
    let all = tw_pack::backup::list(path).map_err(|e| anyhow!("{e}"))?;
    if all.is_empty() {
        println!("No versions yet; the first change to the configuration makes one.");
        return Ok(());
    }
    let now = current(path, PackOrigin::Ui)
        .ok()
        .filter(|(_, p)| tw_pack::model::snapshot(p).is_ok())
        .map(|(_, p)| p.content_id());
    let mut marked = false;
    for b in all.iter().rev() {
        let Ok(pack) = tw_pack::backup::open(b) else {
            continue;
        };
        let current = !marked && Some(pack.content_id()) == now;
        marked |= current;
        println!(
            "{}  {}  {}  {:<22}  upstreams {}, plugins {}  core {}",
            if current { "← current" } else { "         " },
            b.id,
            crate::fmt_time(b.at_ms),
            b.origin.label(),
            pack.manifest.counts.providers,
            pack.manifest.counts.plugins,
            pack.manifest.core_version,
        );
    }
    println!();
    println!("To restore one: twcore backup restore <id>");
    Ok(())
}

pub fn restore(path: &Path, id: &str, dry_run: bool) -> Result<()> {
    use tw_pack::restore::{Change, Kept};
    let b = tw_pack::backup::find(path, id).map_err(|e| anyhow!("{e}"))?;
    let backup = tw_pack::backup::open(&b).map_err(|e| anyhow!("{e}"))?;
    let (cur, local) = current(path, PackOrigin::Restore)?;
    let pinned = tw_pack::disk::read_pinned(path);
    let after = if tw_pack::model::snapshot(&local).is_err() {
        println!("the configuration here does not load; the whole version comes back");
        tw_pack::restore::whole(&backup, &cur.text)
    } else {
        let rows = tw_pack::restore::preview(&local, &backup, &pinned, true).map_err(model)?;
        if rows.is_empty() {
            println!("this version is the same as the configuration now");
            return Ok(());
        }
        for r in &rows {
            let change = match r.change {
                Change::Add => "add   ",
                Change::Remove => "remove",
                Change::Modify => "change",
            };
            let kept = match r.kept {
                Some(Kept::KeyInUse) => " (kept: a client set up on this machine uses it)",
                None => "",
            };
            println!("  {change}  {}{kept}", describe(&r.item));
        }
        if dry_run {
            println!("(dry run: nothing was written)");
            return Ok(());
        }
        tw_pack::restore::apply(&local, &backup, None, &pinned, true)
            .map_err(model)?
            .pack
    };
    if dry_run {
        println!("(dry run: nothing was written)");
        return Ok(());
    }
    tw_config::try_parse(&after.config_yaml).map_err(|r| anyhow!("{r}"))?;
    write_back(path, &cur, &local, &after, Origin::Restore)?;
    println!(
        "restored; the new version is {}",
        tw_config::store::version_of(&after.config_yaml)
    );
    eprintln!("(a running core picks this up within a second)");
    Ok(())
}

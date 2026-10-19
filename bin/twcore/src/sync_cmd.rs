//! `twcore sync status | now | enable | disable`：给没有界面的服务器（远程 core）用。
//!
//! 和界面走同一套控制面端点（正在跑的 core 来做），这里只是把它们问成终端里的几个问题。

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::call;

fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) if !d.is_empty() => eprint!("{prompt} [{d}]: "),
        _ => eprint!("{prompt}: "),
    }
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim().to_string();
    Ok(if line.is_empty() {
        default.unwrap_or_default().to_string()
    } else {
        line
    })
}

fn yes(prompt: &str, default: bool) -> Result<bool> {
    let d = if default { "Y/n" } else { "y/N" };
    let a = ask(&format!("{prompt} ({d})"), None)?;
    Ok(match a.to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// 发一个请求：2xx 时把响应体解析成 `T`，别的把控制面那句话说出来
fn send<T: serde::de::DeserializeOwned>(
    config: &Path,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let (status, bytes) = call::request(config, method, path, body.map(|b| b.to_string()))
        .context("asking the running core (start it with twcore serve)")?;
    if !(200..300).contains(&status) {
        let m: Option<tw_api::Msg> = serde_json::from_slice(&bytes).ok();
        match m {
            Some(m) => bail!("{}", m.text),
            None => bail!(
                "the core answered {status}: {}",
                String::from_utf8_lossy(&bytes)
            ),
        }
    }
    serde_json::from_slice(&bytes).context("reading the answer")
}

fn when(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| ms.to_string())
}

fn print_view(v: &tw_api::SyncView) {
    println!("state: {}", v.state.slug());
    if let Some(t) = &v.target {
        let place = match t {
            tw_api::SyncTarget::Folder { path, .. } => format!("folder {path}"),
            tw_api::SyncTarget::Webdav { url, .. } => format!("WebDAV {url}"),
            tw_api::SyncTarget::S3 {
                endpoint,
                bucket,
                prefix,
                ..
            } => format!("S3 {endpoint} bucket {bucket} prefix {prefix}"),
        };
        println!("location: {place}");
        println!("encrypted: {}", if v.encrypted { "yes" } else { "no" });
    }
    if let Some(d) = &v.device {
        println!("this device: {} ({})", d.name, &d.id[..8.min(d.id.len())]);
    }
    if let Some(ms) = v.last_synced_ms {
        println!("last synced: {}", when(ms));
    }
    if let Some(e) = &v.error {
        println!("problem: {}", e.text);
    }
    for n in &v.notices {
        println!("note: {}", n.text);
    }
    if v.conflicts > 0 {
        println!(
            "conflicts: {} (resolve them in ThinkWatch Lite, Settings → Sync and backup)",
            v.conflicts
        );
    }
    if !v.excluded.is_empty() {
        println!("not synced: {}", v.excluded.join(", "));
    }
    if v.skipped > 0 {
        println!(
            "skipped: {} file(s) at the sync location could not be read",
            v.skipped
        );
    }
    match v.state {
        tw_api::SyncState::Emptied | tw_api::SyncState::KeyringMissing => println!(
            "to go on: `twcore sync republish` uploads this device's configuration again, \
             `twcore sync disable` stops syncing"
        ),
        _ => {}
    }
    for d in &v.devices {
        let mut notes = Vec::new();
        if d.this {
            notes.push("this device");
        }
        if d.remote_core {
            notes.push("remote core");
        }
        if d.paused_update {
            notes.push("paused: needs an update");
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join(", "))
        };
        println!(
            "  {}  core {}  last synced {}{notes}",
            d.name,
            d.core_version,
            when(d.last_seen_ms)
        );
    }
}

pub fn status(config: &Path) -> Result<()> {
    let v: tw_api::SyncView = send(config, "GET", "/sync", None)?;
    print_view(&v);
    Ok(())
}

pub fn now(config: &Path) -> Result<()> {
    let v: tw_api::SyncView = send(config, "POST", "/sync/now", None)?;
    print_view(&v);
    Ok(())
}

/// 同步位置空了、keyring 不见了之后：以本机配置重新上传（keyring 放回本机记着的那一份）
pub fn republish(config: &Path) -> Result<()> {
    let v: tw_api::SyncView = send(config, "POST", "/sync/republish", None)?;
    print_view(&v);
    Ok(())
}

pub fn disable(config: &Path, purge: bool) -> Result<()> {
    if purge
        && !yes(
            "Delete every version, device record and the keyring at the sync location? Other \
             devices are told the location is empty",
            false,
        )?
    {
        bail!("nothing was changed");
    }
    let _: () = send(
        config,
        "POST",
        "/sync/disable",
        Some(serde_json::json!({ "purge": purge })),
    )?;
    println!("sync is off; this device keeps its configuration");
    Ok(())
}

/// 一段机器名能不能当设备名：去掉首尾空白，不能有控制字符，太长的截短
fn usable_name(raw: &str) -> Option<String> {
    let n = raw.trim();
    if n.is_empty() || n.chars().any(char::is_control) {
        return None;
    }
    Some(n.chars().take(tw_config::sync::DEVICE_NAME_MAX).collect())
}

/// 跑一个系统自带的命令，取它输出的第一行
#[cfg(unix)]
fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    usable_name(String::from_utf8_lossy(&out.stdout).lines().next()?)
}

/// 这台计算机的名称，当设备名称的缺省值：macOS 是系统设置里的「电脑名称」（ComputerName，
/// 取不到用 LocalHostName），Linux 是主机名，Windows 是计算机名
fn host_name() -> String {
    #[cfg(target_os = "macos")]
    let found = command_line("/usr/sbin/scutil", &["--get", "ComputerName"])
        .or_else(|| command_line("/usr/sbin/scutil", &["--get", "LocalHostName"]))
        .or_else(|| command_line("/bin/hostname", &[]));
    #[cfg(windows)]
    let found = std::env::var("COMPUTERNAME")
        .ok()
        .and_then(|v| usable_name(&v));
    #[cfg(all(unix, not(target_os = "macos")))]
    let found = ["/etc/hostname", "/proc/sys/kernel/hostname"]
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| usable_name(&s))
        })
        .or_else(|| command_line("hostname", &[]))
        .or_else(|| std::env::var("HOSTNAME").ok().and_then(|v| usable_name(&v)));
    #[cfg(not(any(unix, windows)))]
    let found: Option<String> = None;
    found.unwrap_or_else(|| "server".into())
}

/// 合并加入时，同步数据里和本机不一样的一项怎么办。**缺省是采用同步数据**：留本机的那一份
/// 会随这次加入上传，把别的设备上的那一项也改成本机的 —— 那要明确选
fn decide(row: &tw_api::ImportRow) -> Result<serde_json::Value> {
    let differs = if row.differs.is_empty() {
        String::new()
    } else {
        format!(" ({})", row.differs.join(", "))
    };
    let what = match row.item.name.as_str() {
        "" => row.item.kind.slug().to_string(),
        name => format!("{} `{name}`", row.item.kind.slug()),
    };
    eprintln!("{what}{differs} differs between this device and the sync data:");
    eprintln!("  u  use the sync data (default): this device takes the other devices' version");
    eprintln!(
        "  k  keep this device's version: it is uploaded with this join and replaces the \
         other devices' version"
    );
    let rename = row.suggested_name.clone().filter(|_| !row.global);
    if let Some(n) = &rename {
        eprintln!(
            "  r  keep both: the sync data's version is added here as {n}; this device's \
             version keeps the name and is uploaded with this join, replacing the item of \
             that name on the other devices (they get {n} too)"
        );
    }
    loop {
        let a = ask("Choice", Some("u"))?;
        let action = match a.to_ascii_lowercase().as_str() {
            "u" => "overwrite",
            "k" => "skip",
            "r" if rename.is_some() => "rename",
            _ => {
                eprintln!("type u, k{}", if rename.is_some() { " or r" } else { "" });
                continue;
            }
        };
        let mut d = serde_json::json!({ "item": row.item, "action": action });
        if action == "rename" {
            d["new_name"] = rename.clone().into();
        }
        return Ok(d);
    }
}

pub fn enable(config: &Path) -> Result<()> {
    let kind = ask("Sync location: folder, webdav or s3", Some("folder"))?;
    let proxy = || -> Result<String> {
        ask(
            "Outbound proxy: direct, system or a proxy name",
            Some("direct"),
        )
    };
    let target = match kind.to_ascii_lowercase().as_str() {
        "folder" => serde_json::json!({
            "type": "folder",
            "path": ask("Folder path on this machine", None)?,
            "proxy": "direct",
        }),
        "webdav" => serde_json::json!({
            "type": "webdav",
            "url": ask("WebDAV address (https://…)", None)?,
            "user": ask("User name", None)?,
            "pass": ask("Password or app password", None)?,
            "proxy": proxy()?,
        }),
        "s3" => serde_json::json!({
            "type": "s3",
            "endpoint": ask("Endpoint (https://…)", None)?,
            "region": ask("Region", Some("auto"))?,
            "bucket": ask("Bucket", None)?,
            "prefix": ask("Prefix", Some(""))?,
            "access_key_id": ask("Access key ID", None)?,
            "secret_access_key": ask("Secret access key", None)?,
            "proxy": proxy()?,
        }),
        other => bail!("{other} is not one of folder, webdav, s3"),
    };
    let probe: tw_api::SyncProbe = send(
        config,
        "POST",
        "/sync/test",
        Some(serde_json::json!({ "target": target })),
    )?;
    if !probe.writable {
        bail!(
            "the sync location cannot be written: {}",
            probe.error.map(|e| e.text).unwrap_or_default()
        );
    }
    let device_name = ask("Name of this device", Some(&host_name()))?;
    let mut body = serde_json::json!({
        "target": target,
        "device_name": device_name,
        "encrypt": true,
        "join": "use_remote",
        "decisions": [],
    });
    match &probe.existing {
        None => {
            let encrypt = yes("Encrypt the sync data with a passphrase", true)?;
            body["encrypt"] = encrypt.into();
            if encrypt {
                eprintln!(
                    "The passphrase is typed in the clear here. At least {} characters; every \
                     device joining needs it",
                    tw_config::sync::PASSPHRASE_MIN
                );
                let p = ask("Sync passphrase", None)?;
                let again = ask("Sync passphrase again", None)?;
                if p != again {
                    bail!("the two passphrases differ");
                }
                body["passphrase"] = p.into();
            }
        }
        Some(e) => {
            if e.encrypted {
                body["passphrase"] = ask("Sync passphrase", None)?.into();
            }
            let preview: tw_api::SyncJoinPreview = send(
                config,
                "POST",
                "/sync/enable/preview",
                Some(serde_json::json!({ "target": target, "passphrase": body.get("passphrase") })),
            )?;
            for a in &preview.auto {
                match a.reason {
                    tw_api::SyncJoinAutoReason::DefaultKey => eprintln!(
                        "The default gateway key ({}) takes the sync data's value; clients set up \
                         on this device are updated to it",
                        a.item.name
                    ),
                }
            }
            if !preview.initial {
                let auto: Vec<&tw_api::ItemRef> = preview.auto.iter().map(|a| &a.item).collect();
                let conflicts: Vec<&tw_api::ImportRow> = preview
                    .rows
                    .iter()
                    .filter(|r| {
                        r.status == tw_api::ImportStatus::Conflict && !auto.contains(&&r.item)
                    })
                    .collect();
                let new = preview
                    .rows
                    .iter()
                    .filter(|r| r.status == tw_api::ImportStatus::New)
                    .count();
                eprintln!(
                    "This device already has a configuration: {new} item(s) arrive from the sync \
                     data, {} differ",
                    conflicts.len()
                );
                let replace = yes(
                    "Replace this device's configuration with the sync data (no merges them; \
                     each item that differs is asked)",
                    false,
                )?;
                if !replace {
                    body["join"] = "merge".into();
                    let mut decisions = Vec::new();
                    for r in &conflicts {
                        decisions.push(decide(r)?);
                    }
                    body["decisions"] = decisions.into();
                }
            }
        }
    }
    let _: tw_api::ConfigWritten = send(config, "POST", "/sync/enable", Some(body))?;
    let v: tw_api::SyncView = send(config, "GET", "/sync", None)?;
    print_view(&v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_name_is_trimmed_and_cut_to_a_device_name() {
        assert_eq!(usable_name("  Mac mini \n").as_deref(), Some("Mac mini"));
        assert_eq!(usable_name("   "), None);
        assert_eq!(usable_name("a\u{7}b"), None);
        let long = "名".repeat(100);
        assert_eq!(
            usable_name(&long).unwrap().chars().count(),
            tw_config::sync::DEVICE_NAME_MAX
        );
    }

    /// macOS 上缺省是「电脑名称」（ComputerName），不是 `server`
    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_the_default_device_name_is_the_computer_name() {
        let want = command_line("/usr/sbin/scutil", &["--get", "ComputerName"])
            .or_else(|| command_line("/usr/sbin/scutil", &["--get", "LocalHostName"]));
        let got = host_name();
        assert_ne!(got, "server");
        if let Some(w) = want {
            assert_eq!(got, w);
        }
    }

    /// 别的 Unix 上是主机名
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn elsewhere_the_default_device_name_is_the_host_name() {
        let got = host_name();
        assert!(!got.is_empty());
    }
}

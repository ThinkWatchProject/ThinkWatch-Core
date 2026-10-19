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
    if v.conflicts > 0 {
        println!(
            "conflicts: {} (resolve them in ThinkWatch Lite, Settings → Sync and backup)",
            v.conflicts
        );
    }
    if !v.excluded.is_empty() {
        println!("not synced: {}", v.excluded.join(", "));
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

pub fn disable(config: &Path, purge: bool) -> Result<()> {
    if purge
        && !yes(
            "Delete every version, device record and the keyring at the sync location? Other \
             devices stop syncing",
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

fn host_name() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "server".into())
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
            if !preview.initial {
                let conflicts: Vec<&tw_api::ImportRow> = preview
                    .rows
                    .iter()
                    .filter(|r| r.status == tw_api::ImportStatus::Conflict)
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
                    "Replace this device's configuration with the sync data (no keeps this \
                     device's version of every item that differs)",
                    false,
                )?;
                if !replace {
                    body["join"] = "merge".into();
                    body["decisions"] = conflicts
                        .iter()
                        .map(|r| serde_json::json!({ "item": r.item, "action": "skip" }))
                        .collect();
                }
            }
        }
    }
    let _: tw_api::ConfigWritten = send(config, "POST", "/sync/enable", Some(body))?;
    let v: tw_api::SyncView = send(config, "GET", "/sync", None)?;
    print_view(&v);
    Ok(())
}

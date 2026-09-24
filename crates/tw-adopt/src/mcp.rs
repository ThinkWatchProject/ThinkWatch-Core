//! 把一个 MCP server 从一个客户端搬到另一个。
//!
//! **写入复用接管那一套**：字段级合并、写前全文备份、展示 diff 让用户
//! 确认、认符号链接、写完读回来对一遍。风险和接管完全一样，
//! 所以规矩也一样。
//!
//! 「从所有客户端移除」那一项是**应急开关的替代品**：发现某个 server
//! 有问题时，一次操作从所有客户端拿掉，不用去五个文件里各删一遍。它比
//! 「留一个 `enabled: false` 的中间状态」更直接 —— 它真的删了。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tw_types::{Msg, msg};

use crate::clients::Format;
use crate::foreign::{self, Applied, Change, ForeignError};
use crate::json::Val;
use crate::paths::Loc;

/// 搬 MCP server 时的失败。
///
/// **英文只写一遍**：`Display` 就是 [`McpError::msg`] 的原句，界面拿码去翻。
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("{}", self.msg())]
    UnknownClient(String),
    #[error("{}", self.msg())]
    Parse { client: String, msg: String },
    #[error(transparent)]
    Write(#[from] ForeignError),
    #[error("{}", self.msg())]
    NotThere { client: String, name: String },
    /// 不能写的那个理由，带码。见 [`Target::why_not`]
    #[error("{}", self.msg())]
    NotCopyable { client: String, why: Msg },
}

impl McpError {
    /// 给人看的那句话，带码。
    pub fn msg(&self) -> Msg {
        match self {
            McpError::UnknownClient(client) => msg!(
                "adopt.mcp.unknown_client", client = client =>
                "{client} is not a client we know"
            ),
            McpError::Parse { client, msg } => msg!(
                "adopt.mcp.parse_failed", client = client, detail = msg =>
                "the MCP configuration of {client} could not be parsed, so nothing was changed: \
                 {detail}"
            ),
            McpError::Write(e) => e.msg(),
            McpError::NotThere { client, name } => msg!(
                "adopt.mcp.not_there", client = client, name = name =>
                "{client} has no MCP server named `{name}`"
            ),
            // **理由那半句本来就有自己的码**（`adopt.mcp.unverified_format` 那几条），
            // 套进一句「格式未验证」里就又成了英文 —— 直接说理由
            McpError::NotCopyable { why, .. } => why.clone(),
        }
    }
}

/// 一个能被写入的 MCP 配置位置。
#[derive(Debug, Clone)]
pub struct Target {
    pub client: &'static str,
    pub name: &'static str,
    /// 按优先级从高到低；读写的是在的里面最高的那一个，见
    /// [`crate::paths::first_existing`]
    pub config: &'static [Loc],
    pub format: Format,
    /// server 挂在哪个键下面
    pub key: &'static str,
    /// 能不能往里写。
    ///
    /// **不能写的照样列在清单里**（看得见是第一目标），只是
    /// 不给复制按钮。
    pub copyable: bool,
    /// 不能写的话，为什么。能写的这里是 `None`
    pub why_not: Option<(&'static str, &'static str)>,
}

impl Target {
    /// 不能写的理由，带码。
    pub fn why_not(&self) -> Option<Msg> {
        self.why_not.map(|(code, text)| Msg {
            code: code.to_string(),
            args: BTreeMap::new(),
            text: text.to_string(),
        })
    }
}

/// 能往里写的那几个，以及为什么另外两个不行。
///
/// 判据是**我们有没有实际见过那个形状**。`mcpServers` 那三家和 Codex 的
/// `mcp_servers` 在本机都有真实样本，字段名一致（`command` / `args` /
/// `env`）；opencode 和 Zed 的 MCP 段本机没有样本，**照着猜写进去，
/// 用户拿到的是一份客户端读不懂的配置** —— 那比不提供这个功能糟得多。
pub fn targets() -> Vec<Target> {
    vec![
        Target {
            client: "claude-code",
            name: "Claude Code",
            config: &[Loc::Home(".claude.json")],
            format: Format::Json,
            key: "mcpServers",
            copyable: true,
            why_not: None,
        },
        Target {
            client: "claude-desktop",
            name: "Claude Desktop",
            config: &[crate::paths::CLAUDE_DESKTOP_CONFIG],
            format: Format::Json,
            key: "mcpServers",
            copyable: true,
            why_not: None,
        },
        Target {
            client: "cursor",
            name: "Cursor",
            config: &[Loc::Home(".cursor/mcp.json")],
            format: Format::Json,
            key: "mcpServers",
            copyable: true,
            why_not: None,
        },
        Target {
            client: "codex",
            name: "Codex",
            config: &[Loc::Home(".codex/config.toml")],
            format: Format::Toml,
            key: "mcp_servers",
            copyable: true,
            why_not: None,
        },
        Target {
            client: "opencode",
            name: "opencode",
            config: crate::paths::OPENCODE_CONFIGS,
            format: Format::Json,
            key: "mcp",
            copyable: false,
            why_not: Some((
                code!("adopt.mcp.unverified_format"),
                "this client's MCP configuration format is not verified yet, and writing to it could leave the client unable to read its own configuration",
            )),
        },
        Target {
            client: "zed",
            name: "Zed",
            config: &[crate::paths::ZED_SETTINGS],
            format: Format::Json,
            key: "context_servers",
            copyable: false,
            why_not: Some((
                code!("adopt.mcp.zed_structure"),
                "Zed's context servers use a different structure and do not take the command/args form",
            )),
        },
    ]
}

pub fn target(client: &str) -> Result<Target, McpError> {
    targets()
        .into_iter()
        .find(|t| t.client == client)
        .ok_or_else(|| McpError::UnknownClient(client.to_string()))
}

impl Target {
    pub fn path(&self, home: &Path) -> PathBuf {
        self.config[crate::paths::first_existing(self.config, home)].resolve(home)
    }
    /// 给人看的路径。
    pub fn shown(&self) -> String {
        let i =
            crate::paths::env_home().map_or(0, |h| crate::paths::first_existing(self.config, &h));
        self.config[i].shown()
    }
    fn check(&self) -> Result<(), McpError> {
        if self.copyable {
            Ok(())
        } else {
            Err(McpError::NotCopyable {
                client: self.client.to_string(),
                // 这一条是护栏：界面对不能写的目标根本不给按钮
                why: self.why_not().unwrap_or_else(|| {
                    msg!(
                        "adopt.mcp.not_copyable", client = self.client =>
                        "the MCP configuration format of {client} is unverified, so it is not \
                         written to"
                    )
                }),
            })
        }
    }
}

fn parse_err(client: &str, e: impl std::fmt::Display) -> McpError {
    McpError::Parse {
        client: client.into(),
        msg: e.to_string(),
    }
}

fn semantic(t: &Target, text: &str) -> Result<Val, McpError> {
    match t.format {
        Format::Json => crate::json::value(text).map_err(|e| parse_err(t.client, e)),
        Format::Toml => crate::toml::value(text).map_err(|e| parse_err(t.client, e)),
        Format::Yaml => Err(parse_err(t.client, "MCP configuration is not YAML")),
    }
}

fn put(t: &Target, text: &str, path: &[&str], v: &Val) -> Result<String, McpError> {
    match t.format {
        Format::Json => crate::json::set(text, path, v).map_err(|e| parse_err(t.client, e)),
        Format::Toml => crate::toml::set(text, path, v).map_err(|e| parse_err(t.client, e)),
        Format::Yaml => Err(parse_err(t.client, "MCP configuration is not YAML")),
    }
}

fn drop_(t: &Target, text: &str, path: &[&str]) -> Result<String, McpError> {
    match t.format {
        Format::Json => crate::json::remove(text, path).map_err(|e| parse_err(t.client, e)),
        Format::Toml => crate::toml::remove(text, path).map_err(|e| parse_err(t.client, e)),
        Format::Yaml => Err(parse_err(t.client, "MCP configuration is not YAML")),
    }
}

fn empty(f: Format) -> &'static str {
    match f {
        Format::Json => "{}\n",
        _ => "",
    }
}

/// 一次改动，算好了还没落盘。
#[derive(Debug, Clone)]
pub struct Plan {
    pub client: String,
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
    pub noop: bool,
    /// 这次改的是哪个键，按层级（`mcpServers`、server 名）
    pub field: Vec<String>,
    /// 删掉这个键。否则是写入
    pub remove: bool,
}

/// 读一个 server 的配置原样。
pub fn read_server(t: &Target, home: &Path, name: &str) -> Result<Val, McpError> {
    let text = foreign::read(&t.path(home))?.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(McpError::NotThere {
            client: t.client.into(),
            name: name.into(),
        });
    }
    let v = semantic(t, &text)?;
    let Val::Obj(root) = &v else {
        return Err(parse_err(t.client, "the root is not an object"));
    };
    let servers = root.iter().find(|(k, _)| k == t.key).map(|(_, v)| v);
    match servers {
        Some(Val::Obj(ms)) => ms
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| McpError::NotThere {
                client: t.client.into(),
                name: name.into(),
            }),
        _ => Err(McpError::NotThere {
            client: t.client.into(),
            name: name.into(),
        }),
    }
}

/// 把一份 server 配置写进某个客户端。**只动那一个键。**
pub fn plan_copy(t: &Target, home: &Path, name: &str, value: &Val) -> Result<Plan, McpError> {
    t.check()?;
    let path = t.path(home);
    let before = foreign::read(&path)?;
    let base = before
        .clone()
        .unwrap_or_else(|| empty(t.format).to_string());
    let after = put(t, &base, &[t.key, name], value)?;
    Ok(Plan {
        noop: before.as_deref() == Some(after.as_str()),
        field: vec![t.key.to_string(), name.to_string()],
        remove: false,
        client: t.client.into(),
        path,
        before,
        after,
    })
}

/// 从某个客户端拿掉一个 server。
pub fn plan_remove(t: &Target, home: &Path, name: &str) -> Result<Plan, McpError> {
    let path = t.path(home);
    let before = foreign::read(&path)?;
    let Some(base) = before.clone() else {
        return Err(McpError::NotThere {
            client: t.client.into(),
            name: name.into(),
        });
    };
    let after = drop_(t, &base, &[t.key, name])?;
    Ok(Plan {
        noop: after == base,
        field: vec![t.key.to_string(), name.to_string()],
        remove: true,
        client: t.client.into(),
        path,
        before,
        after,
    })
}

/// 落盘。**和接管走同一套护栏。**
pub fn apply(t: &Target, plan: &Plan, backup_root: &Path) -> Result<Applied, McpError> {
    let base = plan
        .before
        .clone()
        .unwrap_or_else(|| empty(t.format).to_string());
    let expect = semantic(t, &base)?;
    let want = semantic(t, &plan.after)?;
    // 除了那一个键，其余必须逐字段一致
    let key = t.key;
    let strip = |v: &Val| -> Val {
        match v {
            Val::Obj(ms) => Val::Obj(
                ms.iter()
                    .filter(|(k, _)| k != key)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            other => other.clone(),
        }
    };
    let untouched = strip(&expect).normalized();

    Ok(foreign::apply(
        &Change {
            path: &plan.path,
            before: plan.before.as_deref(),
            after: &plan.after,
            // MCP 的 env 里可能有密钥，而我们正把它抄进另一个文件
            carries_secret: true,
        },
        backup_root,
        |text| {
            let got = match t.format {
                Format::Json => crate::json::value(text).map_err(|e| e.to_string())?,
                Format::Toml => crate::toml::value(text).map_err(|e| e.to_string())?,
                Format::Yaml => return Err("MCP configuration is not YAML".into()),
            };
            if strip(&got).normalized() != untouched {
                return Err(format!("something other than {key} changed"));
            }
            if got.normalized() != want.normalized() {
                return Err("the edited content is not what was expected".into());
            }
            Ok(())
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        for (rel, text) in files {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        }
        (d, home)
    }

    const CLAUDE: &str = r#"{
  "numStartups": 42,
  "tipsHistory": { "x": 1 },
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "server-filesystem", "/path/to/workspace"] }
  }
}
"#;

    #[test]
    fn copying_a_server_leaves_the_rest_of_the_file_alone() {
        // ~/.claude.json 有六万多字节，里面装着一堆和我们无关的状态。
        let (d, home) = home_with(&[(".claude.json", CLAUDE), (".cursor/mcp.json", "{}\n")]);
        let src = target("claude-code").unwrap();
        let dst = target("cursor").unwrap();
        let v = read_server(&src, &home, "filesystem").unwrap();

        let p = plan_copy(&dst, &home, "filesystem", &v).unwrap();
        apply(&dst, &p, &d.path().join("backups")).unwrap();

        let out = std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap();
        assert!(out.contains("server-filesystem"), "{out}");
        // 源文件一个字节都不该动
        assert_eq!(
            std::fs::read_to_string(home.join(".claude.json")).unwrap(),
            CLAUDE
        );
    }

    #[test]
    fn copying_into_a_file_full_of_other_settings_keeps_them() {
        let (d, home) = home_with(&[
            (".claude.json", CLAUDE),
            (
                ".cursor/mcp.json",
                "{\n  \"我的设置\": \"别动\",\n  \"mcpServers\": {\n    \"别的\": { \"command\": \"x\" }\n  }\n}\n",
            ),
        ]);
        let v = read_server(&target("claude-code").unwrap(), &home, "filesystem").unwrap();
        let dst = target("cursor").unwrap();
        let p = plan_copy(&dst, &home, "filesystem", &v).unwrap();
        apply(&dst, &p, &d.path().join("backups")).unwrap();
        let out = std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap();
        assert!(out.contains("\"我的设置\": \"别动\""), "{out}");
        assert!(out.contains("\"别的\""), "{out}");
        assert!(out.contains("server-filesystem"), "{out}");
    }

    #[test]
    fn copying_into_codex_writes_toml_not_json() {
        let (d, home) = home_with(&[
            (".claude.json", CLAUDE),
            (
                ".codex/config.toml",
                "model = \"gpt-5\"\n\n[projects.\"/a\"]\ntrust_level = \"trusted\"\n",
            ),
        ]);
        let v = read_server(&target("claude-code").unwrap(), &home, "filesystem").unwrap();
        let dst = target("codex").unwrap();
        let p = plan_copy(&dst, &home, "filesystem", &v).unwrap();
        apply(&dst, &p, &d.path().join("backups")).unwrap();
        let out = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(out.contains("[mcp_servers.filesystem]"), "{out}");
        // 用户的项目授权一条都不能少
        assert!(out.contains("[projects.\"/a\"]"), "{out}");
        assert!(out.contains("model = \"gpt-5\""), "{out}");
    }

    #[test]
    fn removing_takes_only_that_one_server() {
        let two = r#"{
  "numStartups": 42,
  "mcpServers": {
    "filesystem": { "command": "npx" },
    "postgres": { "command": "mcp-postgres" }
  }
}
"#;
        let (d, home) = home_with(&[(".claude.json", two)]);
        let t = target("claude-code").unwrap();
        let p = plan_remove(&t, &home, "filesystem").unwrap();
        apply(&t, &p, &d.path().join("backups")).unwrap();
        let out = std::fs::read_to_string(home.join(".claude.json")).unwrap();
        assert!(!out.contains("filesystem"), "{out}");
        assert!(out.contains("postgres"), "{out}");
        assert!(out.contains("numStartups"), "{out}");
    }

    #[test]
    fn a_client_whose_shape_we_have_not_verified_is_refused_out_loud() {
        // **照着猜写进去，用户拿到的是一份客户端读不懂的配置** ——
        // 那比不提供这个功能糟得多。
        let (_d, home) = home_with(&[(".claude.json", CLAUDE)]);
        let v = read_server(&target("claude-code").unwrap(), &home, "filesystem").unwrap();
        for c in ["zed", "opencode"] {
            let t = target(c).unwrap();
            let e = plan_copy(&t, &home, "filesystem", &v).unwrap_err();
            assert!(matches!(e, McpError::NotCopyable { .. }), "{e}");
            // 而且要说清为什么
            assert!(e.to_string().len() > 20, "{e}");
        }
    }

    #[test]
    fn a_verification_failure_leaves_the_target_untouched() {
        // 和接管走同一套护栏：写回校验过不了就一个字节都不写。
        let (d, home) = home_with(&[(".cursor/mcp.json", "{ 坏的 }")]);
        let t = target("cursor").unwrap();
        let e = plan_copy(&t, &home, "x", &Val::Obj(vec![])).unwrap_err();
        assert!(matches!(e, McpError::Parse { .. }), "{e}");
        assert_eq!(
            std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap(),
            "{ 坏的 }"
        );
        let _ = d;
    }

    #[test]
    fn removing_something_that_is_not_there_is_a_noop_not_an_error() {
        let (_d, home) = home_with(&[(".claude.json", CLAUDE)]);
        let t = target("claude-code").unwrap();
        let p = plan_remove(&t, &home, "没这个").unwrap();
        assert!(p.noop);
    }

    #[test]
    fn reading_from_a_client_that_does_not_have_it_says_so() {
        let (_d, home) = home_with(&[(".claude.json", CLAUDE)]);
        let t = target("claude-code").unwrap();
        let e = read_server(&t, &home, "postgres").unwrap_err();
        assert!(matches!(e, McpError::NotThere { .. }), "{e}");
    }

    #[test]
    fn the_backup_is_made_before_a_copy_just_like_an_adoption() {
        let (d, home) = home_with(&[(".claude.json", CLAUDE), (".cursor/mcp.json", "{}\n")]);
        let v = read_server(&target("claude-code").unwrap(), &home, "filesystem").unwrap();
        let dst = target("cursor").unwrap();
        let p = plan_copy(&dst, &home, "filesystem", &v).unwrap();
        let a = apply(&dst, &p, &d.path().join("backups")).unwrap();
        assert_eq!(std::fs::read_to_string(&a.backup).unwrap(), "{}\n");
    }
}

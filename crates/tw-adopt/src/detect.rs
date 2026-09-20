//! 「我明明配了，为什么没生效」。
//!
//! 这是接管类工具最常见的支持问题。原因有六种，而**它们的排查难度差得
//! 很远** —— 所以这里不做「一个笼统的健康检查」，而是把六条各自查一遍、
//! 各自给出能直接执行的下一步。
//!
//! 一条纪律贯穿全文件：**报告是我们的职责，修改是他的权利**。
//! 我们会说出「你的 ~/.zshrc 第 42 行导出了 ANTHROPIC_BASE_URL」，并给出
//! 那条 `sed` 命令，但绝不替他执行 —— 那是他的 shell 配置，不是我们的。
//!
//! 还有一条更要紧的：**静态扫描证明不了「接管真的生效了」**。优先级链
//! 有五层，任何一层出意外都会让静态结论出错。真正可靠的验证只有一个：
//! 等一个真实请求过来（观察窗口，见 [`crate::watch`]）。

use std::path::{Path, PathBuf};

use crate::clients::{Client, Format, TakesEffect, Verified, adoptable};
use crate::foreign;
use crate::sentinel::{self, SidecarRecord};

/// 一个客户端此刻的样子。
#[derive(Debug, Clone)]
pub struct Detected {
    pub id: &'static str,
    pub name: &'static str,
    pub path: PathBuf,
    /// 跟完符号链接的真实路径
    pub real: PathBuf,
    /// 这台机器上装了它
    pub installed: bool,
    /// 配置文件存在
    pub has_config: bool,
    /// 接管过，时间戳来自旁文件
    pub adopted_at_ms: Option<u64>,
    /// 配置里此刻的端点。**读出来的，不是我们记的** —— 「我们写过」
    /// 和「现在还是那样」是两回事
    pub endpoint: Option<String>,
    pub shadows: Vec<PathBuf>,
    pub takes_effect: TakesEffect,
    pub verified: Verified,
    pub format: Format,
    pub costs: Vec<String>,
}

fn endpoint_of(c: &Client, text: &str) -> Option<String> {
    let path: Vec<&str> = match c.id {
        "claude-code" => vec!["env", "ANTHROPIC_BASE_URL"],
        "codex" => vec!["model_providers", crate::clients::PROVIDER_ID, "base_url"],
        "opencode" => vec![
            "provider",
            crate::clients::PROVIDER_ID,
            "options",
            "baseURL",
        ],
        "zed" => vec![
            "language_models",
            "openai_compatible",
            "ThinkWatch",
            "api_url",
        ],
        "aider" => vec!["openai-api-base"],
        _ => return None,
    };
    match c.format {
        Format::Json => crate::json::get(text, &path)
            .ok()
            .flatten()
            .map(|v| v.to_line()),
        Format::Toml => crate::toml::get(text, &path)
            .ok()
            .flatten()
            .map(|v| v.to_line()),
        Format::Yaml => crate::yaml::get(text, &path).ok().flatten(),
    }
}

pub fn detect_one(c: &Client, home: &Path) -> Detected {
    let path = c.config_path(home);
    let real = foreign::resolve(&path).unwrap_or_else(|_| path.clone());
    let text = std::fs::read_to_string(&real).ok();
    let rec: Option<SidecarRecord> = std::fs::read_to_string(sentinel::sidecar_path(&real))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    Detected {
        id: c.id,
        name: c.name,
        installed: c.marker.iter().any(|m| home.join(m).exists()) || text.is_some(),
        has_config: text.is_some(),
        adopted_at_ms: rec
            .filter(|r: &SidecarRecord| r.client == c.id)
            .map(|r| r.adopted_at_ms),
        endpoint: text.as_deref().and_then(|t| endpoint_of(c, t)),
        shadows: c
            .shadow_paths(home)
            .into_iter()
            .filter(|p| p.exists())
            .collect(),
        takes_effect: c.takes_effect,
        verified: c.verified,
        format: c.format,
        costs: c.costs.iter().map(|s| s.to_string()).collect(),
        path,
        real,
    }
}

/// 扫一遍本机。**只读，不写任何东西。**
pub fn detect(home: &Path) -> Vec<Detected> {
    adoptable().iter().map(|c| detect_one(c, home)).collect()
}

// ---------------------------------------------------------------- 诊断

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 这就是原因
    Blocking,
    /// 可疑，但不一定是它
    Suspect,
    /// 查过了，没问题。**要说出来** —— 没风险的时候要说「安全」，
    /// 而不是让这一项消失
    Clear,
}

/// 一条发现。
#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// 用户可以自己执行的下一步。**我们不替他执行。**
    pub fix: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `ps` 的 `etime`：`[[dd-]hh:]mm:ss`。
///
/// macOS 的 `ps` 没有 `etimes`（整秒），只有这个格式；`lstart` 是本地化
/// 日期，解析它反而更脆。
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.trim().parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    while parts.len() < 3 {
        parts.insert(0, 0);
    }
    Some(days * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2])
}

/// 正在跑的进程里，匹配这些片段的那些各自启动于什么时候（毫秒时间戳）。
fn running_since(markers: &[&str]) -> Vec<u64> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-Ao", "pid=,etime=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let now = now_ms();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let _pid = it.next()?;
            let etime = it.next()?;
            let cmd = line.split_once(etime).map(|(_, r)| r).unwrap_or("");
            let name = cmd.rsplit('/').next().unwrap_or(cmd);
            if !markers.iter().any(|m| name.contains(m)) {
                return None;
            }
            Some(now.saturating_sub(parse_etime(etime)? * 1000))
        })
        .collect()
}

/// shell 配置里 export 了同名变量的那些行。
fn shell_exports(home: &Path, names: &[&str]) -> Vec<(PathBuf, usize, String)> {
    let files = [
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".config/fish/config.fish",
    ];
    let mut out = Vec::new();
    for f in files {
        let p = home.join(f);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            // 注释掉的不算。**这个判断很便宜，但漏掉它就会天天误报** ——
            // 而误报几次之后，真正该看的那一次也不会被看。
            if t.starts_with('#') {
                continue;
            }
            for n in names {
                if (t.starts_with("export ")
                    || t.starts_with("set -x ")
                    || t.starts_with("setenv "))
                    && t.contains(n)
                {
                    out.push((p.clone(), i + 1, n.to_string()));
                }
            }
        }
    }
    out
}

/// macOS 上的管理策略文件。**优先级压过一切**，包括用户自己的配置。
const MANAGED: &str = "/Library/Application Support/ClaudeCode/managed-settings.json";

/// 走一遍优先级链。`project` 是当前项目目录（有的话）。
pub fn diagnose(c: &Client, home: &Path, project: Option<&Path>) -> Vec<Finding> {
    let d = detect_one(c, home);
    let mut out = Vec::new();

    // 一、客户端没重启。**最常见，而且判据便宜得离谱**
    match d.adopted_at_ms {
        Some(at) => {
            let started = running_since(c.process);
            let stale: Vec<_> = started.iter().filter(|s| **s < at).collect();
            if started.is_empty() {
                out.push(Finding {
                    level: Level::Clear,
                    title: format!("{} is not running", c.name),
                    detail: "It reads the new configuration the next time it starts.".into(),
                    fix: None,
                });
            } else if !stale.is_empty() {
                out.push(Finding {
                    level: Level::Blocking,
                    title: format!("{} was started before the change", c.name),
                    detail: format!(
                        "{} processes were started before the change and are still on the old configuration. {}",
                        stale.len(),
                        c.takes_effect.note()
                    ),
                    fix: Some(format!("Quit {} and open it again", c.name)),
                });
            } else {
                out.push(Finding {
                    level: Level::Clear,
                    title: format!("{} was started after the change", c.name),
                    detail: "It has read the new configuration.".into(),
                    fix: None,
                });
            }
        }
        None => out.push(Finding {
            level: Level::Suspect,
            title: "This client has not been pointed at the gateway".into(),
            detail: format!("{} carries no record.", d.real.display()),
            fix: None,
        }),
    }

    // 二、优先级更高的文件里有残留（cc-switch #6828）
    if d.shadows.is_empty() {
        out.push(Finding {
            level: Level::Clear,
            title: "Nothing takes precedence over this file".into(),
            detail: if c.shadowed_by.is_empty() {
                "This client has no configuration file that takes precedence.".into()
            } else {
                format!("{} does not exist.", c.shadowed_by.join(", "))
            },
            fix: None,
        });
    } else {
        for s in &d.shadows {
            let text = std::fs::read_to_string(s).unwrap_or_default();
            let hits: Vec<_> = c.env_vars.iter().filter(|v| text.contains(**v)).collect();
            out.push(Finding {
                level: if hits.is_empty() {
                    Level::Suspect
                } else {
                    Level::Blocking
                },
                title: format!(
                    "{} takes precedence over what was written here",
                    s.display()
                ),
                detail: if hits.is_empty() {
                    "The file exists, but carries none of the fields in question.".into()
                } else {
                    format!(
                        "The file carries {}, which overrides what was written here.",
                        hits.iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
                fix: Some(format!("Look at those fields in {}", s.display())),
            });
        }
    }

    // 三、项目级配置盖住了用户级
    if let Some(proj) = project {
        let local = proj.join(c.config);
        if local.exists() {
            out.push(Finding {
                level: Level::Suspect,
                title: "This project has a configuration file of the same name".into(),
                detail: format!(
                    "{} overrides the user-level configuration.",
                    local.display()
                ),
                fix: Some(format!("Look at {}", local.display())),
            });
        }
    }

    // 四、管理策略文件。**最高优先级，压过一切**
    if c.id == "claude-code" {
        if Path::new(MANAGED).exists() {
            let text = std::fs::read_to_string(MANAGED).unwrap_or_default();
            let hits: Vec<_> = c.env_vars.iter().filter(|v| text.contains(**v)).collect();
            out.push(Finding {
                level: if hits.is_empty() {
                    Level::Suspect
                } else {
                    Level::Blocking
                },
                title: "This machine has a managed-policy file".into(),
                detail: format!("{MANAGED} takes precedence over everything else, including the user's own configuration."),
                fix: None,
            });
        } else {
            out.push(Finding {
                level: Level::Clear,
                title: "This machine has no managed-policy file".into(),
                detail: "There is no managed-policy file taking precedence over everything else."
                    .into(),
                fix: None,
            });
        }
    }

    // 五、shell 里 export 了同名变量
    let exports = shell_exports(home, c.env_vars);
    if exports.is_empty() {
        out.push(Finding {
            level: Level::Clear,
            title: "No shell file exports a variable of the same name".into(),
            detail: "Checked .zshrc, .zprofile, .bashrc and the rest.".into(),
            fix: None,
        });
    } else {
        for (f, line, name) in exports {
            // **同一条发现，对不同客户端的结论相反。**不区分的话就会
            // 给出一条错误的诊断。
            let (level, detail) = if c.config_beats_env {
                (
                    Level::Suspect,
                    format!(
                        "It does not affect {}, whose configuration file takes precedence, but it does affect every client that reads the environment.",
                        c.name
                    ),
                )
            } else {
                (
                    Level::Blocking,
                    format!(
                        "{} reads the environment, so this line overrides what was written here.",
                        c.name
                    ),
                )
            };
            out.push(Finding {
                level,
                title: format!("{} exports {name} on line {line}", f.display()),
                detail,
                // 命令给出来，执行与否是他的事
                fix: Some(format!("sed -i '' '{line}d' {}", f.display())),
            });
        }
    }

    // 六、我们写的字段被别的工具改回去了
    match (&d.adopted_at_ms, &d.endpoint) {
        (Some(_), None) => out.push(Finding {
            level: Level::Blocking,
            title: "The fields written here are no longer in the configuration".into(),
            detail: format!(
                "{} no longer carries the endpoint that was written here; something else may have changed it.",
                d.real.display()
            ),
            fix: Some("Point this client at the gateway again".into()),
        }),
        (Some(_), Some(ep)) => out.push(Finding {
            level: Level::Clear,
            title: "The endpoint in the configuration is the one written here".into(),
            detail: format!("It points at {ep}."),
            fix: None,
        }),
        _ => {}
    }

    // **静态扫描证明不了「生效了」。**这句话必须留在结论里，否则一屏
    // 绿色的「查过了没问题」会让人以为已经确认过。
    out.push(Finding {
        level: Level::Suspect,
        title: "Everything above is a static check".into(),
        detail: "A static check cannot tell whether the configuration is actually in use. Only a real request from this client settles that."
            .into(),
        fix: None,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str) -> Client {
        adoptable().into_iter().find(|c| c.id == id).unwrap()
    }

    #[test]
    fn etime_parses_every_shape_ps_emits() {
        assert_eq!(parse_etime("05:12"), Some(5 * 60 + 12));
        assert_eq!(parse_etime("01:05:12"), Some(3600 + 5 * 60 + 12));
        assert_eq!(
            parse_etime("09-14:11:52"),
            Some(9 * 86400 + 14 * 3600 + 11 * 60 + 52)
        );
        assert_eq!(parse_etime("垃圾"), None);
    }

    #[test]
    fn a_commented_out_export_is_not_reported() {
        // 漏掉这个判断就会天天误报，而误报几次之后真正该看的那一次
        // 也不会被看。
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".zshrc"),
            "# export ANTHROPIC_BASE_URL=https://old\nexport PATH=/usr/bin\n",
        )
        .unwrap();
        assert!(shell_exports(d.path(), &["ANTHROPIC_BASE_URL"]).is_empty());
    }

    #[test]
    fn a_real_export_is_reported_with_its_line_number() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".zshrc"),
            "x=1\ny=2\nexport ANTHROPIC_BASE_URL=https://old\n",
        )
        .unwrap();
        let hits = shell_exports(d.path(), &["ANTHROPIC_BASE_URL"]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 3, "行号错了，那条 sed 命令就会删错行");
    }

    #[test]
    fn the_same_shell_export_is_blocking_for_codex_but_only_a_note_for_claude_code() {
        // Claude Code 的 env 块会盖住 shell 的 export，Codex 不会。
        // **同一条发现，两个相反的结论。**
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".zshrc"),
            "export ANTHROPIC_BASE_URL=https://old\nexport OPENAI_BASE_URL=https://old\n",
        )
        .unwrap();

        let cc = diagnose(&c("claude-code"), home, None);
        let f = cc
            .iter()
            .find(|f| f.title.contains("ANTHROPIC_BASE_URL"))
            .unwrap();
        assert_eq!(f.level, Level::Suspect, "{:?}", f);
        assert!(f.detail.contains("takes precedence"), "{}", f.detail);

        let cx = diagnose(&c("codex"), home, None);
        let f = cx
            .iter()
            .find(|f| f.title.contains("OPENAI_BASE_URL"))
            .unwrap();
        assert_eq!(f.level, Level::Blocking, "{:?}", f);
    }

    #[test]
    fn a_clean_machine_still_says_something_rather_than_showing_nothing() {
        // 没风险的时候要说「安全」，而不是让这一项消失。
        let d = tempfile::tempdir().unwrap();
        let out = diagnose(&c("claude-code"), d.path(), None);
        assert!(out.iter().any(|f| f.level == Level::Clear), "{out:?}");
        assert!(out.len() >= 4, "查了几条就该说几条：{out:?}");
    }

    #[test]
    fn the_report_never_claims_a_static_check_proves_it_works() {
        // 优先级链有五层。一屏绿色不等于「生效了」。
        let d = tempfile::tempdir().unwrap();
        let out = diagnose(&c("claude-code"), d.path(), None);
        assert!(
            out.iter().any(|f| f.detail.contains("real request")),
            "结论里没留下这句话：{out:?}"
        );
    }

    #[test]
    fn the_fix_is_a_command_we_hand_over_not_one_we_run() {
        // 报告是我们的职责，修改是他的权利。
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".zshrc"),
            "export OPENAI_BASE_URL=https://old\n",
        )
        .unwrap();
        let out = diagnose(&c("codex"), d.path(), None);
        let f = out
            .iter()
            .find(|f| f.title.contains("OPENAI_BASE_URL"))
            .unwrap();
        assert!(f.fix.as_ref().unwrap().contains("sed -i ''"), "{:?}", f.fix);
        // 文件还在，我们没动它
        assert!(d.path().join(".zshrc").exists());
        assert!(
            std::fs::read_to_string(d.path().join(".zshrc"))
                .unwrap()
                .contains("export")
        );
    }

    #[test]
    fn detection_reads_the_endpoint_from_the_file_not_from_our_own_record() {
        // 「我们写过」和「现在还是那样」是两回事 —— 第六种原因就是
        // 「被别的工具改回去了」。
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8080"}}"#,
        )
        .unwrap();
        let got = detect_one(&c("claude-code"), home);
        assert!(got.installed);
        assert_eq!(got.endpoint.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(got.adopted_at_ms, None, "没有旁文件就不算接管过");
    }

    #[test]
    fn a_client_that_is_not_installed_is_reported_as_such() {
        let d = tempfile::tempdir().unwrap();
        let all = detect(d.path());
        assert!(
            all.iter().all(|x| !x.installed),
            "空目录里不该检测出任何客户端"
        );
        assert_eq!(all.len(), adoptable().len());
    }
}

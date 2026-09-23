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

use tw_types::{Msg, msg};

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
    pub costs: Vec<Msg>,
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
        costs: c
            .costs
            .iter()
            .map(|(code, text)| Msg {
                code: (*code).into(),
                args: Default::default(),
                text: (*text).into(),
            })
            .collect(),
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
///
/// **三句话都带码。**这一屏是「为什么没生效」的答案，桌面版要用中文
/// 说出来；英文原句是给命令行和不认识这个码的客户端的退路。
#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub title: Msg,
    pub detail: Msg,
    /// 用户可以自己执行的下一步。**我们不替他执行。**
    pub fix: Option<Msg>,
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
#[cfg(not(windows))]
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
#[cfg(not(windows))]
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

/// Windows 上没有 `ps`。
///
/// 起 PowerShell 问 `Get-Process` 也能拿到，但那要半秒钟才出结果，而这一条
/// 是诊断页面上的一行字 —— 直接枚举，快装接口都在 kernel32 里。
///
/// **打不开的进程直接跳过**：别的用户跑的、以及系统进程，`OpenProcess` 会
/// 失败。那不是错误，只是我们看不见它 —— 而我们要找的客户端是这个用户自己
/// 起的，本来就在能看见的那一堆里。
#[cfg(windows)]
fn running_since(markers: &[&str]) -> Vec<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    /// FILETIME 从 1601-01-01 起算，单位 100 纳秒。这个常数是它到 unix
    /// 纪元之间的毫秒数。
    const EPOCH_DELTA_MS: u64 = 11_644_473_600_000;

    /// 进程的创建时刻，毫秒时间戳。
    fn started_ms(pid: u32) -> Option<u64> {
        // SAFETY: 只问信息，不动进程。失败返回空句柄，下面判掉了。
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return None;
        }
        let mut created = FILETIME::default();
        let (mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: 句柄有效，四个出参都是本地变量。后三个用不上，但这个
        // 函数不接受空指针。
        let ok = unsafe {
            windows_sys::Win32::System::Threading::GetProcessTimes(
                h,
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        // SAFETY: 上面刚开的，只关这一次。
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        // 1601 年之前没有进程；真拿到个小得离谱的值也不该算出一个负的
        // 时间戳来，所以用 checked_sub
        (ticks / 10_000).checked_sub(EPOCH_DELTA_MS)
    }

    // SAFETY: 参数是常量，失败返回 INVALID_HANDLE_VALUE，下面判掉了。
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut e = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: 句柄有效，`e.dwSize` 已经按文档填好 —— 不填这个函数会直接失败。
    let mut more = unsafe { Process32FirstW(snap, &mut e) } != 0;
    while more {
        let name = String::from_utf16_lossy(
            &e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)],
        );
        if markers.iter().any(|m| name.contains(m))
            && let Some(ms) = started_ms(e.th32ProcessID)
        {
            out.push(ms);
        }
        // SAFETY: 同上。返回 0 表示枚举完了。
        more = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    // SAFETY: 快照句柄，只关这一次。
    unsafe { CloseHandle(snap) };
    out
}

/// 查过哪些地方。**说出来**，否则「没有同名变量」这句话没人知道它有多可信。
#[cfg(not(windows))]
const LOOKED_IN: &str = ".zshrc, .zprofile, .bashrc and the rest";
#[cfg(windows)]
const LOOKED_IN: &str = "the user and machine environment in the registry";

/// 一处「同名变量在别处被设过」。
///
/// **位置不一定是个路径**：unix 上是一个文件，Windows 上是注册表里的一个
/// 键 —— 后者没有文件，也没有行号。做成 `PathBuf` 就得在 Windows 那一支编
/// 一个假路径出来。
///
/// 它会被填进一句英文里（`{name} is already set in {at}`），所以**写英文**，
/// 而且要是个名词短语。
pub struct EnvConflict {
    /// 在哪儿设的，照着这句话去找得到。
    pub at: String,
    /// 哪个变量。
    pub name: String,
    /// 怎么把它去掉。**一句照着做就行的话**，不是一条通用建议 ——
    /// 而两个平台照着做的东西完全不同，所以由各自那一支给出来。
    pub fix: Msg,
}

/// 别处设过同名变量的地方。
///
/// 这件事要紧的原因见 `diagnose` 里那一段：**环境变量会盖住我们写进配置文件的
/// 值**，而那正是「接管了但没生效」最常见的一种。
#[cfg(not(windows))]
fn env_conflicts(home: &Path, names: &[&str]) -> Vec<EnvConflict> {
    shell_exports(home, names)
        .into_iter()
        .map(|(f, line, name)| EnvConflict {
            // 行号不进这里：`fix` 那条 sed 命令已经把它带上了
            at: f.display().to_string(),
            name,
            // 命令给出来，执行与否是他的事
            fix: msg!(
                "adopt.diag.delete_line",
                path = f.display(),
                line = line
                => "sed -i '' '{line}d' {path}"
            ),
        })
        .collect()
}

/// Windows 上没有 shell 配置这回事。
///
/// 同名变量设在注册表里：`HKCU\Environment` 是这个用户的，
/// `HKLM\…\Session Manager\Environment` 是整台机器的。**两处都要看** ——
/// 只看用户那一处的话，一个由管理员设在机器级的变量会照样盖住我们写的值，
/// 而诊断会说「没有同名变量」。
///
/// 只报名字，不报值：这些变量里可能装着别的服务的密钥，而这一条要回答的
/// 只是「有没有」。
#[cfg(windows)]
fn env_conflicts(_home: &Path, names: &[&str]) -> Vec<EnvConflict> {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_ANY, RegGetValueW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn is_set(root: HKEY, sub: &str, name: &str) -> bool {
        let (sub, name) = (wide(sub), wide(name));
        let mut len: u32 = 0;
        // SAFETY: 两个字符串都以 NUL 结尾；缓冲区传空指针只为问「在不在」，
        // 函数那时只回写需要的字节数。
        let rc = unsafe {
            RegGetValueW(
                root,
                sub.as_ptr(),
                name.as_ptr(),
                RRF_RT_ANY,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        rc == 0
    }

    const USER: &str = "Environment";
    const MACHINE: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let mut out = Vec::new();
    for n in names {
        if is_set(HKEY_CURRENT_USER, USER, n) {
            out.push(EnvConflict {
                at: format!(r"HKCU\{USER}"),
                name: (*n).to_string(),
                // **删掉，不是设成空**：一个设成空串的变量仍然是「设过的」，
                // 照样会盖住配置文件里的值。改完要重开终端才生效。
                fix: msg!(
                    "adopt.diag.unset_env", name = n, root = "HKCU", key = USER
                    => "reg delete \"{root}\\{key}\" /v {name} /f  (open a new terminal afterwards)"
                ),
            });
        }
        if is_set(HKEY_LOCAL_MACHINE, MACHINE, n) {
            out.push(EnvConflict {
                at: format!(r"HKLM\{MACHINE}"),
                name: (*n).to_string(),
                // 机器级的那份要管理员才改得动，说出来免得他照着跑一次被拒
                fix: msg!(
                    "adopt.diag.unset_env_machine", name = n, root = "HKLM", key = MACHINE
                    => "reg delete \"{root}\\{key}\" /v {name} /f  (needs an administrator terminal)"
                ),
            });
        }
    }
    out
}

/// shell 配置里 export 了同名变量的那些行。
#[cfg(not(windows))]
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
                    title: msg!("adopt.diag.not_running", client = c.name => "{client} is not running"),
                    detail: msg!("adopt.diag.not_running.detail" => "It reads the new configuration the next time it starts."),
                    fix: None,
                });
            } else if !stale.is_empty() {
                out.push(Finding {
                    level: Level::Blocking,
                    title: msg!("adopt.diag.started_before", client = c.name => "{client} was started before the change"),
                    // `takes_effect` 传的是词表里的那个词，不是那句话本身
                    // —— 句子在两边各写各的，码和词是共同的那部分
                    detail: msg!(
                        "adopt.diag.started_before.detail",
                        count = stale.len(),
                        takes_effect = c.takes_effect.slug(),
                        => "{count} processes were started before the change and are still on the old configuration. {}",
                        c.takes_effect.note()
                    ),
                    fix: Some(msg!("adopt.diag.restart", client = c.name => "Quit {client} and open it again")),
                });
            } else {
                out.push(Finding {
                    level: Level::Clear,
                    title: msg!("adopt.diag.started_after", client = c.name => "{client} was started after the change"),
                    detail: msg!("adopt.diag.started_after.detail" => "It has read the new configuration."),
                    fix: None,
                });
            }
        }
        None => out.push(Finding {
            level: Level::Suspect,
            title: msg!("adopt.diag.not_adopted" => "This client has not been pointed at the gateway"),
            detail: msg!("adopt.diag.not_adopted.detail", path = d.real.display() => "{path} carries no record."),
            fix: None,
        }),
    }

    // 二、优先级更高的文件里有残留（cc-switch #6828）
    if d.shadows.is_empty() {
        out.push(Finding {
            level: Level::Clear,
            title: msg!("adopt.diag.no_shadow" => "Nothing takes precedence over this file"),
            detail: if c.shadowed_by.is_empty() {
                msg!("adopt.diag.no_shadow.none" => "This client has no configuration file that takes precedence.")
            } else {
                msg!("adopt.diag.no_shadow.absent", files = c.shadowed_by.join(", ") => "{files} does not exist.")
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
                title: msg!(
                    "adopt.diag.shadowed",
                    path = s.display()
                    => "{path} takes precedence over what was written here"
                ),
                detail: if hits.is_empty() {
                    msg!("adopt.diag.shadowed.no_fields" => "The file exists, but carries none of the fields in question.")
                } else {
                    msg!(
                        "adopt.diag.shadowed.fields",
                        fields = hits
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                        => "The file carries {fields}, which overrides what was written here."
                    )
                },
                fix: Some(msg!("adopt.diag.look_at_fields", path = s.display() => "Look at those fields in {path}")),
            });
        }
    }

    // 三、项目级配置盖住了用户级
    if let Some(proj) = project {
        let local = proj.join(c.config);
        if local.exists() {
            out.push(Finding {
                level: Level::Suspect,
                title: msg!("adopt.diag.project_config" => "This project has a configuration file of the same name"),
                detail: msg!(
                    "adopt.diag.project_config.detail",
                    path = local.display()
                    => "{path} overrides the user-level configuration."
                ),
                fix: Some(msg!("adopt.diag.look_at", path = local.display() => "Look at {path}")),
            });
        }
    }

    // 四、管理策略文件。**最高优先级，压过一切**
    if c.id == "claude-code" {
        let managed = crate::paths::managed_settings();
        if managed.exists() {
            let text = std::fs::read_to_string(&managed).unwrap_or_default();
            let hits: Vec<_> = c.env_vars.iter().filter(|v| text.contains(**v)).collect();
            out.push(Finding {
                level: if hits.is_empty() {
                    Level::Suspect
                } else {
                    Level::Blocking
                },
                title: msg!("adopt.diag.managed" => "This machine has a managed-policy file"),
                detail: msg!("adopt.diag.managed.detail", path = managed.display() => "{path} takes precedence over everything else, including the user's own configuration."),
                fix: None,
            });
        } else {
            out.push(Finding {
                level: Level::Clear,
                title: msg!("adopt.diag.no_managed" => "This machine has no managed-policy file"),
                detail: msg!("adopt.diag.no_managed.detail" => "There is no managed-policy file taking precedence over everything else."),
                fix: None,
            });
        }
    }

    // 五、别处设了同名的环境变量
    let exports = env_conflicts(home, c.env_vars);
    if exports.is_empty() {
        out.push(Finding {
            level: Level::Clear,
            title: msg!("adopt.diag.no_exports" => "Nothing else sets a variable of the same name"),
            detail: msg!(
                "adopt.diag.no_exports.detail",
                looked = LOOKED_IN
                => "Looked in {looked}."
            ),
            fix: None,
        });
    } else {
        for EnvConflict { at: f, name, fix } in exports {
            // **同一条发现，对不同客户端的结论相反。**不区分的话就会
            // 给出一条错误的诊断。
            let (level, detail) = if c.config_beats_env {
                (
                    Level::Suspect,
                    msg!(
                        "adopt.diag.shell_export.harmless",
                        client = c.name
                        => "It does not affect {client}, whose configuration file takes precedence, but it does affect every client that reads the environment."
                    ),
                )
            } else {
                (
                    Level::Blocking,
                    msg!(
                        "adopt.diag.shell_export.overrides",
                        client = c.name
                        => "{client} reads the environment, so this line overrides what was written here."
                    ),
                )
            };
            out.push(Finding {
                level,
                title: msg!(
                    "adopt.diag.shell_export",
                    path = f,
                    name = name
                    => "{name} is already set in {path}"
                ),
                detail,
                fix: Some(fix),
            });
        }
    }

    // 六、我们写的字段被别的工具改回去了
    match (&d.adopted_at_ms, &d.endpoint) {
        (Some(_), None) => out.push(Finding {
            level: Level::Blocking,
            title: msg!("adopt.diag.fields_gone" => "The fields written here are no longer in the configuration"),
            detail: msg!(
                "adopt.diag.fields_gone.detail",
                path = d.real.display()
                => "{path} no longer carries the endpoint that was written here; something else may have changed it."
            ),
            fix: Some(msg!("adopt.diag.adopt_again" => "Point this client at the gateway again")),
        }),
        (Some(_), Some(ep)) => out.push(Finding {
            level: Level::Clear,
            title: msg!("adopt.diag.endpoint_ok" => "The endpoint in the configuration is the one written here"),
            detail: msg!("adopt.diag.endpoint_ok.detail", endpoint = ep => "It points at {endpoint}."),
            fix: None,
        }),
        _ => {}
    }

    // **静态扫描证明不了「生效了」。**这句话必须留在结论里，否则一屏
    // 绿色的「查过了没问题」会让人以为已经确认过。
    out.push(Finding {
        level: Level::Suspect,
        title: msg!("adopt.diag.static_only" => "Everything above is a static check"),
        detail: msg!("adopt.diag.static_only.detail" => "A static check cannot tell whether the configuration is actually in use. Only a real request from this client settles that."),
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
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
            .find(|f| f.title.text.contains("ANTHROPIC_BASE_URL"))
            .unwrap();
        assert_eq!(f.level, Level::Suspect, "{:?}", f);
        assert!(f.detail.text.contains("takes precedence"), "{}", f.detail);

        let cx = diagnose(&c("codex"), home, None);
        let f = cx
            .iter()
            .find(|f| f.title.text.contains("OPENAI_BASE_URL"))
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
            out.iter().any(|f| f.detail.text.contains("real request")),
            "结论里没留下这句话：{out:?}"
        );
    }

    #[test]
    #[cfg(not(windows))]
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
            .find(|f| f.title.text.contains("OPENAI_BASE_URL"))
            .unwrap();
        assert!(
            f.fix.as_ref().unwrap().text.contains("sed -i ''"),
            "{:?}",
            f.fix
        );
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

    /// **每一条诊断都要带码。**
    ///
    /// 这一屏是「为什么没生效」的答案，漏一个码不会报错，只会让中文
    /// 界面上那一行悄悄变成英文。走一遍每个客户端，把能走到的分支
    /// 都过一次。
    #[test]
    fn every_finding_carries_a_code() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        // 让「有配置文件」「有残留」「shell 里 export 了」这几条都成立
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/settings.local.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://old"}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".zshrc"),
            "export ANTHROPIC_BASE_URL=https://old
",
        )
        .unwrap();
        let mut n = 0;
        for c in adoptable() {
            for f in diagnose(&c, home, Some(home)) {
                n += 1;
                assert!(!f.title.code.is_empty(), "{}：「{}」没有码", c.id, f.title);
                assert!(
                    !f.detail.code.is_empty(),
                    "{}：「{}」没有码",
                    c.id,
                    f.detail
                );
                assert!(
                    !f.title.text.is_empty(),
                    "{}：{} 没有英文原句",
                    c.id,
                    f.title.code
                );
                if let Some(fix) = &f.fix {
                    assert!(!fix.code.is_empty(), "{}：「{fix}」没有码", c.id);
                }
            }
        }
        assert!(n > 10, "只走到 {n} 条，分支没覆盖到");
    }
}

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
        installed: c.marker.iter().any(|m| m.resolve(home).exists()) || text.is_some(),
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

/// 只有 `ps` 那一支要：它拿到的是「跑了多久」，得从现在往回倒。Windows
/// 那一支直接拿到创建时刻。
#[cfg(not(windows))]
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

/// `ps -Ao pid=,etime=,comm=` 的一行：跑了多少秒、进程名。
///
/// 两个平台给的 `comm` 不是一回事：macOS 上是可执行文件的**完整路径**
/// （可以带空格，`Claude Helper (Renderer).app/…`），Linux 上是内核记的
/// 进程名，最多 15 个字节、不带路径。取最后一个 `/` 之后那段，两边就都是
/// 名字了。
#[cfg(not(windows))]
fn parse_ps_line(line: &str) -> Option<(u64, &str)> {
    let line = line.trim_start();
    let (_pid, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (etime, comm) = rest.split_once(char::is_whitespace)?;
    let comm = comm.trim();
    let name = comm.rsplit('/').next().unwrap_or(comm);
    Some((parse_etime(etime)?, name))
}

/// 进程名是不是这些标记之一开头的。
///
/// **比开头，不比包含**：包含的话 `zed` 会认下任何名字里带这三个字母的
/// 进程。也不比全名：Linux 把进程名截到 15 个字节，带目标三元组的二进制
/// 名截完只剩前一段，而那一段仍然以标记开头。
#[cfg(not(windows))]
fn is_one_of(name: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| name.starts_with(m))
}

/// 正在跑的进程里，匹配这些标记的那些各自启动于什么时候（毫秒时间戳）。
///
/// **npm 装的客户端认的是真正干活的那个原生进程**：Codex 的 npm 包是个
/// node 包装层（进程名 `node`），它再起 `vendor/…/codex`；Claude Code 和
/// opencode 的 npm 包直接装原生二进制。认 `node` 等于认下这台机器上所有
/// node 程序，所以不认。
#[cfg(not(windows))]
fn running_since(markers: &[&str]) -> Vec<u64> {
    // 写死绝对路径：按 PATH 找的话，谁往 PATH 前面塞一个同名程序，它就跟着
    // core 一起跑起来了。macOS 和常见 Linux 发行版上都在这里（/bin 在
    // Fedora、Arch、新的 Debian/Ubuntu 上是指向 /usr/bin 的链接）。
    //
    // 极简发行版可能根本没装 procps：那时起不来，当成「没在跑」 —— 这一条
    // 本来就是个提示，不值得为它报错。
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-Ao", "pid=,etime=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let now = now_ms();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .filter(|(_, name)| is_one_of(name, markers))
        .map(|(secs, _)| now.saturating_sub(secs * 1000))
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

/// 「查过了，没有同名变量」那一条的标题和正文。
///
/// **每个平台一套消息码，不共用一句再往里填词。**界面按码翻译整句：两个
/// 平台说的本来就是两句话（一个是 shell 文件，一个是注册表），共用一个码的
/// 话，界面那边的译文只能照着其中一个平台写 —— 另一个平台就会读到一句不对
/// 的中文。查过哪些地方要**说出来**，否则「没有」这句话没人知道它有多可信。
#[cfg(not(windows))]
fn nothing_else_sets_it() -> (Msg, Msg) {
    (
        msg!("adopt.diag.no_exports" => "No shell file exports a variable of the same name"),
        msg!("adopt.diag.no_exports.detail" => "Checked .zshrc, .zprofile, .bashrc and the rest."),
    )
}
#[cfg(windows)]
fn nothing_else_sets_it() -> (Msg, Msg) {
    (
        msg!("adopt.diag.no_registry_env" => "No environment variable of the same name is set"),
        msg!("adopt.diag.no_registry_env.detail" => "Checked the user and the machine environment variables."),
    )
}

/// 一处「同名变量在别处被设过」。
///
/// **句子由各平台那一支整句给出**，理由见 `nothing_else_sets_it`：unix 上是
/// 「某文件第几行导出了它」，Windows 上是「注册表里某个键下设了它」—— 后者
/// 没有文件，也没有行号。
pub struct EnvConflict {
    /// 在哪儿、设了哪个变量。
    pub title: Msg,
    /// 怎么把它去掉。**一句照着做就行的话**，不是一条通用建议。
    pub fix: Msg,
    /// 「这个客户端读环境变量，于是它盖住了写进配置的值」。unix 上说的是
    /// 「这一行」，Windows 上是「这个变量」—— 也是两句话。
    pub overrides: fn(client: &str) -> Msg,
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
            title: msg!(
                "adopt.diag.shell_export",
                path = f.display(),
                name = name,
                line = line
                => "{path} exports {name} on line {line}"
            ),
            // 命令给出来，执行与否是他的事
            fix: delete_line(&f, line),
            overrides: |client| {
                msg!(
                    "adopt.diag.shell_export.overrides",
                    client = client
                    => "{client} reads the environment, so this line overrides what was written here."
                )
            },
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

    fn registry_overrides(client: &str) -> Msg {
        msg!(
            "adopt.diag.registry_env.overrides",
            client = client
            => "{client} reads the environment, so this variable overrides what was written here."
        )
    }

    const USER: &str = "Environment";
    const MACHINE: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let mut out = Vec::new();
    for n in names {
        if is_set(HKEY_CURRENT_USER, USER, n) {
            out.push(EnvConflict {
                title: msg!(
                    "adopt.diag.registry_env", name = n, key = format!(r"HKCU\{USER}")
                    => "{name} is set in the registry under {key}"
                ),
                overrides: registry_overrides,
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
                title: msg!(
                    "adopt.diag.registry_env", name = n, key = format!(r"HKLM\{MACHINE}")
                    => "{name} is set in the registry under {key}"
                ),
                overrides: registry_overrides,
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

/// 删掉某个文件第几行的那条命令。
///
/// **两个平台的 sed 不是一个 sed。**macOS 的 BSD sed 里 `-i` 必须跟一个备份
/// 后缀（空串就是不备份）；GNU sed 的后缀是贴在 `-i` 上的，`-i ''` 会把 `''`
/// 当成脚本、把 `'3d'` 当成文件名 —— 照抄过去只会报「找不到文件 3d」。两句
/// 不同的命令，各用各的码。
#[cfg(target_os = "macos")]
fn delete_line(path: &Path, line: usize) -> Msg {
    msg!(
        "adopt.diag.delete_line",
        path = path.display(),
        line = line
        => "sed -i '' '{line}d' {path}"
    )
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn delete_line(path: &Path, line: usize) -> Msg {
    msg!(
        "adopt.diag.delete_line_gnu",
        path = path.display(),
        line = line
        => "sed -i '{line}d' {path}"
    )
}

/// 要看的 shell 配置文件，相对 home，以及它是不是 fish 的。
///
/// bash 登录时读 `.bash_profile`、`.bash_login`、`.profile` 里先找到的那一个，
/// 交互时读 `.bashrc`；Linux 上多数人用的是 bash，这几个都得在。
#[cfg(not(windows))]
const SHELL_FILES: &[(&str, bool)] = &[
    (".zshrc", false),
    (".zprofile", false),
    (".zshenv", false),
    (".bashrc", false),
    (".bash_profile", false),
    (".bash_login", false),
    (".profile", false),
    (".config/fish/config.fish", true),
];

/// 这一行导出了哪些变量。
///
/// **只认真的导出语句，而且按变量名整个比**，不看「这一行里有没有这串字」：
/// `export ANTHROPIC_BASE_URL_OLD=…` 不是在设 `ANTHROPIC_BASE_URL`，而
/// `echo $OPENAI_BASE_URL` 什么也没设。
///
/// - sh 系：`export A=1 B=2`、`export A`（把已有的变量导出去，一样算）。
/// - fish：`set` 带上导出标志才算 —— `-x`、`-gx`、`-Ux`、`--export`，标志
///   可以分开写。`set -e` 是删掉它，不算。**`set -x` 在 bash 里是另一回事**
///   （打开命令回显），所以这一套只用在 fish 的文件上。
///
/// 注释掉的不算。**这个判断很便宜，但漏掉它就会天天误报** —— 而误报几次
/// 之后，真正该看的那一次也不会被看。
#[cfg(not(windows))]
fn exported_names(line: &str, fish: bool) -> Vec<&str> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return Vec::new();
    }
    let mut words = t.split_whitespace();
    let is_name = |w: &str| {
        !w.is_empty()
            && !w.starts_with(|c: char| c.is_ascii_digit())
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if fish {
        if words.next() != Some("set") {
            return Vec::new();
        }
        let (mut export, mut erase) = (false, false);
        for w in words {
            if let Some(long) = w.strip_prefix("--") {
                export |= long == "export";
                erase |= long == "erase";
            } else if let Some(short) = w.strip_prefix('-') {
                export |= short.contains('x');
                erase |= short.contains('e');
            } else {
                // 标志之后第一个词就是变量名
                return if export && !erase && is_name(w) {
                    vec![w]
                } else {
                    Vec::new()
                };
            }
        }
        Vec::new()
    } else {
        if words.next() != Some("export") {
            return Vec::new();
        }
        words
            .take_while(|w| !w.starts_with('#'))
            .map(|w| w.split_once('=').map_or(w, |(n, _)| n))
            .filter(|n| is_name(n))
            .collect()
    }
}

/// shell 配置里 export 了同名变量的那些行。
#[cfg(not(windows))]
fn shell_exports(home: &Path, names: &[&str]) -> Vec<(PathBuf, usize, String)> {
    let mut out = Vec::new();
    for &(f, fish) in SHELL_FILES {
        let p = crate::paths::under(home, f);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let set = exported_names(line, fish);
            for n in names {
                if set.contains(n) {
                    out.push((p.clone(), i + 1, n.to_string()));
                }
            }
        }
    }
    out
}

/// 一份优先级更高的文件里，会盖住我们写的那些字段。
///
/// 多数客户端认的是环境变量那一块（Claude Code 的 `env` 块），出现变量名就
/// 算。opencode 逐层**深合并**：更高的那份文件只有写了 `provider.thinkwatch`
/// 才会盖住我们写的东西，别的键（`$schema`、别的 provider）和我们并存。
fn overriding_fields(c: &Client, text: &str) -> Vec<String> {
    match c.id {
        "opencode" => {
            let path = ["provider", crate::clients::PROVIDER_ID];
            match crate::json::get(text, &path) {
                Ok(Some(_)) => vec![path.join(".")],
                _ => Vec::new(),
            }
        }
        _ => c
            .env_vars
            .iter()
            .filter(|v| text.contains(**v))
            .map(|v| v.to_string())
            .collect(),
    }
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
            let hits = overriding_fields(c, &text);
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
                        fields = hits.join(", ")
                        => "The file carries {fields}, which overrides what was written here."
                    )
                },
                fix: Some(msg!("adopt.diag.look_at_fields", path = s.display() => "Look at those fields in {path}")),
            });
        }
    }

    // 三、项目级配置盖住了用户级
    if let Some(proj) = project {
        // 只有跟着 home 走的那几种说得上「项目里有一份同名的」；XDG 目录下的
        // 全局配置在项目里没有对应的位置
        let local = c.config[0].home_rel().map(|r| crate::paths::under(proj, r));
        if let Some(local) = local.filter(|p| p.exists()) {
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
    //
    // `managed-settings.json` 和 `managed-settings.d/` 里的分片合起来是同一个
    // 来源：先读前者，再按字母序合并分片，同一个键后读的赢。每个文件各报
    // 一条 —— 用户要去改的是具体哪一个文件。
    if c.id == "claude-code" {
        let managed = crate::paths::managed_settings();
        let dropins = crate::paths::managed_settings_dropins();
        let level = |p: &Path| {
            let text = std::fs::read_to_string(p).unwrap_or_default();
            if overriding_fields(c, &text).is_empty() {
                Level::Suspect
            } else {
                Level::Blocking
            }
        };
        if managed.exists() {
            out.push(Finding {
                level: level(&managed),
                title: msg!("adopt.diag.managed" => "This machine has a managed-policy file"),
                detail: msg!("adopt.diag.managed.detail", path = managed.display() => "{path} takes precedence over everything else, including the user's own configuration."),
                fix: None,
            });
        }
        for p in &dropins {
            out.push(Finding {
                level: level(p),
                title: msg!("adopt.diag.managed_dropin" => "This machine has a managed-policy drop-in file"),
                detail: msg!(
                    "adopt.diag.managed_dropin.detail", path = p.display()
                    => "{path} is merged after managed-settings.json; like it, it takes precedence over everything else, including the user's own configuration."
                ),
                fix: None,
            });
        }
        if !managed.exists() && dropins.is_empty() {
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
        let (clear_title, clear_detail) = nothing_else_sets_it();
        out.push(Finding {
            level: Level::Clear,
            title: clear_title,
            detail: clear_detail,
            fix: None,
        });
    } else {
        for EnvConflict {
            title,
            fix,
            overrides,
        } in exports
        {
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
                (Level::Blocking, overrides(c.name))
            };
            out.push(Finding {
                level,
                title,
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
        let fix = &f.fix.as_ref().unwrap().text;
        // 各平台自己那个 sed 的写法，见 `delete_line`
        if cfg!(target_os = "macos") {
            assert!(fix.starts_with("sed -i '' '1d' "), "{fix}");
        } else {
            assert!(fix.starts_with("sed -i '1d' "), "{fix}");
        }
        // 文件还在，我们没动它
        assert!(d.path().join(".zshrc").exists());
        assert!(
            std::fs::read_to_string(d.path().join(".zshrc"))
                .unwrap()
                .contains("export")
        );
    }

    /// Linux 上 `ps` 真实吐出来的样子：右对齐的 pid、`etime`、15 字节以内的
    /// 进程名（不带路径）。
    #[test]
    #[cfg(not(windows))]
    fn linux_ps_lines_are_read_as_names() {
        let lines = [
            "      1 12-03:04:05 systemd",
            "   2141    01:02:03 zed-editor",
            "   2230       05:12 claude",
            "   2301       00:09 node",
            "   2302       00:09 codex",
            "   2400       00:30 codex-x86_64-un",
            "   2500       00:30 opencode",
            "   2600       00:30 aider",
            "   2700       00:30 zeitgeist-daemon",
        ];
        let parsed: Vec<_> = lines.iter().filter_map(|l| parse_ps_line(l)).collect();
        assert_eq!(parsed.len(), lines.len());
        assert_eq!(parsed[0], (12 * 86400 + 3 * 3600 + 4 * 60 + 5, "systemd"));
        assert_eq!(parsed[1], (3600 + 2 * 60 + 3, "zed-editor"));
        let hits = |m: &[&str]| {
            parsed
                .iter()
                .filter(|(_, n)| is_one_of(n, m))
                .map(|(_, n)| *n)
                .collect::<Vec<_>>()
        };
        let by = |id: &str| adoptable().into_iter().find(|c| c.id == id).unwrap();
        assert_eq!(hits(by("claude-code").process), ["claude"]);
        // npm 装的 codex：node 包装层不算，它起的原生进程算；截断的三元组名也算
        assert_eq!(hits(by("codex").process), ["codex", "codex-x86_64-un"]);
        assert_eq!(hits(by("opencode").process), ["opencode"]);
        assert_eq!(hits(by("aider").process), ["aider"]);
        if cfg!(target_os = "linux") {
            // 「Zed」大写认不出 Linux 上的编辑器，「zed」包含又会认下 zeitgeist
            assert_eq!(hits(by("zed").process), ["zed-editor"]);
        }
    }

    /// macOS 的 `comm` 是完整路径，可以带空格。
    #[test]
    #[cfg(not(windows))]
    fn macos_ps_lines_are_read_as_names() {
        let l = "11315    19:42:58 /Users/u/Library/Application Support/Claude/claude-code/2.1.280/claude.app/Contents/MacOS/claude";
        assert_eq!(parse_ps_line(l), Some((19 * 3600 + 42 * 60 + 58, "claude")));
        let l = "48711    06:13:11 /Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)";
        assert_eq!(parse_ps_line(l).unwrap().1, "Claude Helper (Renderer)");
        let l = "  900  1-00:00:00 /Applications/Zed.app/Contents/MacOS/zed";
        let (_, name) = parse_ps_line(l).unwrap();
        if cfg!(target_os = "macos") {
            let zed = adoptable().into_iter().find(|c| c.id == "zed").unwrap();
            assert!(is_one_of(name, zed.process), "{name}");
        }
        assert_eq!(parse_ps_line("垃圾"), None);
        assert_eq!(parse_ps_line(""), None);
    }

    /// bash 用户：`.bashrc`、`.bash_profile`、`.profile` 都在看的范围里。
    #[test]
    #[cfg(not(windows))]
    fn bash_files_are_checked() {
        for f in [".bashrc", ".bash_profile", ".bash_login", ".profile"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(d.path().join(f), "export OPENAI_BASE_URL=https://old\n").unwrap();
            let hits = shell_exports(d.path(), &["OPENAI_BASE_URL"]);
            assert_eq!(hits.len(), 1, "{f}");
        }
    }

    /// 按变量名整个比：名字相近的、只是被引用的，都不算。
    #[test]
    #[cfg(not(windows))]
    fn only_a_real_export_of_that_exact_name_counts() {
        assert_eq!(exported_names("export A=1 B=2", false), ["A", "B"]);
        assert_eq!(
            exported_names("  export OPENAI_API_KEY", false),
            ["OPENAI_API_KEY"]
        );
        assert_eq!(
            exported_names("export OPENAI_BASE_URL_OLD=x", false),
            ["OPENAI_BASE_URL_OLD"]
        );
        assert!(exported_names("echo $OPENAI_BASE_URL", false).is_empty());
        assert!(exported_names("OPENAI_BASE_URL=x", false).is_empty());
        assert!(exported_names("# export OPENAI_BASE_URL=x", false).is_empty());
        // bash 里的 `set -x` 是打开命令回显，不是导出
        assert!(exported_names("set -x OPENAI_BASE_URL x", false).is_empty());
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".bashrc"),
            "export OPENAI_BASE_URL_OLD=x\necho $OPENAI_BASE_URL\n",
        )
        .unwrap();
        assert!(shell_exports(d.path(), &["OPENAI_BASE_URL"]).is_empty());
    }

    /// fish 的导出是 `set` 加导出标志；没有导出标志的、删掉变量的都不算。
    #[test]
    #[cfg(not(windows))]
    fn fish_set_is_read_by_its_flags() {
        for l in [
            "set -x OPENAI_BASE_URL https://old",
            "set -gx OPENAI_BASE_URL https://old",
            "set -Ux OPENAI_BASE_URL https://old",
            "set --export OPENAI_BASE_URL https://old",
            "set -g -x OPENAI_BASE_URL https://old",
        ] {
            assert_eq!(exported_names(l, true), ["OPENAI_BASE_URL"], "{l}");
        }
        for l in [
            "set -g OPENAI_BASE_URL https://old",
            "set OPENAI_BASE_URL https://old",
            "set -e OPENAI_BASE_URL",
            "set -ex OPENAI_BASE_URL",
            "# set -gx OPENAI_BASE_URL https://old",
            "export OPENAI_BASE_URL=https://old",
        ] {
            assert!(exported_names(l, true).is_empty(), "{l}");
        }
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".config/fish/config.fish");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "set -g X 1\nset -gx OPENAI_BASE_URL https://old\n").unwrap();
        let hits = shell_exports(d.path(), &["OPENAI_BASE_URL"]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 2);
    }

    /// 项目里的同名配置只对跟着 home 走的客户端说得通。
    #[test]
    fn a_project_copy_is_only_looked_for_where_it_means_something() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let proj = d.path().join("proj");
        std::fs::create_dir_all(proj.join(".config/opencode")).unwrap();
        std::fs::write(proj.join(".config/opencode/opencode.json"), "{}").unwrap();
        std::fs::create_dir_all(proj.join(".codex")).unwrap();
        std::fs::write(proj.join(".codex/config.toml"), "").unwrap();
        let has = |id: &str| {
            diagnose(&c(id), &home, Some(&proj))
                .iter()
                .any(|f| f.title.code == "adopt.diag.project_config")
        };
        assert!(has("codex"));
        assert!(!has("opencode"));
    }

    fn adopt(id: &str, home: &Path, backups: &Path) {
        let gw = crate::clients::Gateway {
            base: "http://127.0.0.1:8080".into(),
            key: None,
        };
        let p = crate::plan::plan_adopt(&c(id), home, &gw).unwrap();
        crate::plan::apply(&c(id), &p, backups).unwrap();
    }

    /// 刚装好的 opencode 自己建的是 `opencode.jsonc`：写进它，而不是另起一份
    /// 会被它盖住的 `opencode.json`，也不为那一行 `$schema` 报「被盖住」。
    #[test]
    fn opencode_is_written_where_opencode_itself_writes() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let jsonc = crate::paths::OPENCODE_CONFIGS[0].resolve(&home);
        std::fs::create_dir_all(jsonc.parent().unwrap()).unwrap();
        std::fs::write(
            &jsonc,
            "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}",
        )
        .unwrap();
        adopt("opencode", &home, &d.path().join("b"));

        assert_eq!(c("opencode").config_path(&home), jsonc);
        assert!(std::fs::read_to_string(&jsonc).unwrap().contains("baseURL"));
        assert!(!crate::paths::OPENCODE_CONFIGS[1].resolve(&home).exists());
        let got = detect_one(&c("opencode"), &home);
        assert!(got.shadows.is_empty(), "{:?}", got.shadows);
        assert!(got.adopted_at_ms.is_some());
    }

    /// 接管之后用户才建了 `opencode.jsonc`：仍然认原来那一份（记录和原文在
    /// 它旁边），并且说出更高的那份盖住了什么。
    #[test]
    fn a_later_opencode_jsonc_is_reported_as_taking_precedence() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let json = crate::paths::OPENCODE_CONFIGS[1].resolve(&home);
        let jsonc = crate::paths::OPENCODE_CONFIGS[0].resolve(&home);
        std::fs::create_dir_all(json.parent().unwrap()).unwrap();
        std::fs::write(&json, "{}\n").unwrap();
        adopt("opencode", &home, &d.path().join("b"));
        assert_eq!(c("opencode").config_path(&home), json);

        let shadow = |jsonc_text: &str| {
            std::fs::write(&jsonc, jsonc_text).unwrap();
            assert_eq!(c("opencode").config_path(&home), json, "换了文件就还原不了");
            diagnose(&c("opencode"), &home, None)
                .into_iter()
                .find(|f| f.title.code == "adopt.diag.shadowed")
                .expect("没报被盖住")
        };
        // 别的键和我们并存，只是提一句
        let f = shadow("{ // 我的\n  \"theme\": \"x\" }");
        assert_eq!(f.level, Level::Suspect);
        assert_eq!(f.detail.code, "adopt.diag.shadowed.no_fields");
        // 写了同名 provider 的，才真的盖住
        let f = shadow("{ \"provider\": { \"thinkwatch\": { \"options\": {} } } }");
        assert_eq!(f.level, Level::Blocking);
        assert_eq!(f.detail.arg("fields"), "provider.thinkwatch");
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

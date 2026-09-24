//! 各家客户端的配置放在哪。
//!
//! **一处出处。**同一条路径以前在三个地方各写了一遍（接管的 MCP 目标、扫描
//! 的来源清单、诊断），而它在 Windows 上和 macOS 上不是同一条 —— 三份各改
//! 一次就是三份会漂，漏掉的那一份表现为「这台机器上的 Claude Desktop 没被
//! 发现」，而扫描漏掉一个 MCP 配置正是它存在要挡的事。
//!
//! # 为什么相对 home，而不是读 `%APPDATA%`
//!
//! 这个 crate 写的是**用户其他软件的配置文件**，所以它的测试必须能被隔离：
//! 传一个临时目录当 home。直接读环境变量会把那层隔离拆掉，而拆掉之后一次
//! 跑偏的测试改的是真的 `~/.claude`。
//!
//! 代价是 APPDATA 被重定向过的机器（漫游配置、或者用户自己搬过）找不到那份
//! 配置。默认位置就在 home 底下，这是绝大多数；而「没发现」比「改错文件」
//! 便宜得多。
//!
//! XDG 目录是个例外：那几个客户端**自己**认 `$XDG_CONFIG_HOME`，设了它的
//! 用户，`~/.config` 下那份就不是在用的那份 —— 照默认位置去改，改的正是一份
//! 错文件。所以它们要读变量，而隔离靠 [`Loc::resolve`] 里那条规矩守住。
//!
//! 机器级的那一个（管理策略）没有 home 可言，它是个固定路径。

use std::path::{Path, PathBuf};

/// 把一条用 `/` 写的相对路径接到 `base` 下面。
///
/// **按 `/` 拆开逐段接，不直接 `join`。**这些相对路径在源码里一律用 `/` 写，
/// 直接 `base.join(".claude/settings.json")` 在 Windows 上得到的是
/// `C:\Users\x\.claude/settings.json` —— 文件照样找得到（Windows 两种分隔符
/// 都认），但界面上显示的就是这么一串正反斜杠混着的路径。逐段接出来的是
/// 那个平台自己的写法。
pub fn under(base: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|c| !c.is_empty())
        .fold(base.to_path_buf(), |p, c| p.join(c))
}

/// 一条配置路径从哪儿算起。
///
/// 大多数客户端的配置在 home 底下一个固定的相对位置；有几个跟着 XDG 走
/// （opencode 在每个平台上，Zed 和 Claude Desktop 在 Linux 上）。**要能在
/// 常量里构造**：它们放在 `marker: &[…]` 那种静态切片里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loc {
    /// 相对 home
    Home(&'static str),
    /// 相对 XDG 配置目录：`$XDG_CONFIG_HOME`，没设就是 `~/.config`
    XdgConfig(&'static str),
    /// 相对 XDG 数据目录：`$XDG_DATA_HOME`，没设就是 `~/.local/share`
    XdgData(&'static str),
}

impl Loc {
    /// 在 `home` 这个用户底下，它落在哪儿。
    pub fn resolve(&self, home: &Path) -> PathBuf {
        self.resolve_with(home, env_home().as_deref(), |v| std::env::var_os(v))
    }

    /// [`Loc::resolve`] 去掉进程环境的那一半，测试直接喂。
    ///
    /// **XDG 变量只在 `home` 就是这个进程自己的 home 时才认。**变量描述的是
    /// 跑着这个进程的那个用户；调用方传来别的 home（测试传的临时目录就是）
    /// 时，拿开发者自己的 `$XDG_CONFIG_HOME` 去套它，测试改到的就是真的配置
    /// —— 这正是本文件开头那条隔离要挡的事。
    ///
    /// 相对路径的值不认：XDG 规范要求这类值是绝对路径，相对的当没设（Zed
    /// 用的 `dirs` 也是这么做的）。
    fn resolve_with(
        &self,
        home: &Path,
        proc_home: Option<&Path>,
        var: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> PathBuf {
        let xdg = |name: &str, default: &str, rel: &str| {
            let base = var(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && proc_home == Some(home))
                .unwrap_or_else(|| under(home, default));
            under(&base, rel)
        };
        match *self {
            Loc::Home(rel) => under(home, rel),
            Loc::XdgConfig(rel) => xdg("XDG_CONFIG_HOME", ".config", rel),
            Loc::XdgData(rel) => xdg("XDG_DATA_HOME", ".local/share", rel),
        }
    }

    /// 相对 home 的那一段。XDG 那两种没有固定的一段，所以是 `None` ——
    /// 「项目里有一份同名配置」（`<项目>/.claude/settings.json` 那种）只对
    /// 前者说得通。
    pub fn home_rel(&self) -> Option<&'static str> {
        match *self {
            Loc::Home(rel) => Some(rel),
            Loc::XdgConfig(_) | Loc::XdgData(_) => None,
        }
    }

    /// 给人看的写法：「打开 … 」那一步里的路径。
    ///
    /// macOS 和 Linux 上是 `~/.claude/settings.json`；Windows 上没有 `~`，
    /// 写成资源管理器地址栏里能直接粘贴的 `%USERPROFILE%\.claude\settings.json`。
    /// XDG 目录被挪到 home 外面的，给完整路径。
    pub fn shown(&self) -> String {
        let home = env_home().unwrap_or_default();
        let p = self.resolve(&home);
        let rel = match p.strip_prefix(&home) {
            Ok(rel) if !home.as_os_str().is_empty() => rel,
            _ => return p.display().to_string(),
        };
        let parts: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        #[cfg(windows)]
        {
            format!(r"%USERPROFILE%\{}", parts.join(r"\"))
        }
        #[cfg(not(windows))]
        {
            format!("~/{}", parts.join("/"))
        }
    }
}

/// 这个进程自己的 home。和 tw-control 的 `home_dir` 读的是同一个变量。
pub fn env_home() -> Option<PathBuf> {
    #[cfg(windows)]
    const VAR: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const VAR: &str = "HOME";
    std::env::var_os(VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Zed 的设置文件。
///
/// 三个平台三个地方，都照 Zed 自己的 `paths::config_dir()`：Windows 上在漫游
/// 的 AppData 下；Linux 上是 `$XDG_CONFIG_HOME/zed`（经 `dirs::config_dir`）；
/// macOS 上**写死** `~/.config/zed`，不看 XDG。
pub const ZED_SETTINGS: Loc = if cfg!(windows) {
    Loc::Home("AppData/Roaming/Zed/settings.json")
} else if cfg!(target_os = "macos") {
    Loc::Home(".config/zed/settings.json")
} else {
    Loc::XdgConfig("zed/settings.json")
};

/// Zed 装过的痕迹：它的设置目录。
pub const ZED_DIR: Loc = if cfg!(windows) {
    Loc::Home("AppData/Roaming/Zed")
} else if cfg!(target_os = "macos") {
    Loc::Home(".config/zed")
} else {
    Loc::XdgConfig("zed")
};

/// opencode 的全局配置。**三个文件、按优先级从高到低。**
///
/// 目录用 `xdg-basedir`，**每个平台都**认 `$XDG_CONFIG_HOME`，没设才是
/// `~/.config/opencode`。目录里它依次读 `config.json`、`opencode.json`、
/// `opencode.jsonc` 再逐层深合并，后读的赢；它自己要写全局配置时，写的是
/// 这三个里按 jsonc → json → config.json 找到的第一个，一个都没有就新建
/// `opencode.jsonc` —— 所以刚装好的 opencode 手里就是一份 `opencode.jsonc`。
///
/// 我们照它自己的挑法挑：写进它认作「那份全局配置」的文件，也就是在的几个
/// 里优先级最高的那一个。只认 `opencode.json` 的话，刚装好的用户每个人都会
/// 看到一条「有文件盖住了它」的提示，而那份 jsonc 里其实只有一行 `$schema`。
pub const OPENCODE_CONFIGS: &[Loc] = &[
    Loc::XdgConfig("opencode/opencode.jsonc"),
    Loc::XdgConfig("opencode/opencode.json"),
    Loc::XdgConfig("opencode/config.json"),
];

/// 按优先级从高到低排好的几个位置里，第一个存在的是第几个；都不在就是
/// 第一个（该新建的那一个）。
pub fn first_existing(locs: &[Loc], home: &Path) -> usize {
    locs.iter()
        .position(|l| l.resolve(home).exists())
        .unwrap_or(0)
}

/// Claude Desktop 的配置。
///
/// 它是 Electron 应用，这个文件在 `userData` 下，也就是 Electron 的 `appData`
/// 加应用名：macOS 上是 `Library/Application Support/Claude/`，Windows 上是
/// 漫游的 AppData，Linux（beta，只出 Debian 系的包）上是
/// `$XDG_CONFIG_HOME/Claude`，没设就是 `~/.config/Claude`。
pub const CLAUDE_DESKTOP_CONFIG: Loc = if cfg!(windows) {
    Loc::Home("AppData/Roaming/Claude/claude_desktop_config.json")
} else if cfg!(target_os = "macos") {
    Loc::Home("Library/Application Support/Claude/claude_desktop_config.json")
} else {
    Loc::XdgConfig("Claude/claude_desktop_config.json")
};

/// 机器级的管理策略文件。**优先级压过一切**，包括用户自己的配置。
///
/// 机器级，所以这一个不相对 home。位置照 Claude Code 文档「Deploy managed
/// settings」：macOS `/Library/Application Support/ClaudeCode/`，Linux（和
/// WSL）`/etc/claude-code/`，Windows `C:\Program Files\ClaudeCode\`。Windows
/// 上旧的 `C:\ProgramData\ClaudeCode\` 文档明说**不再读**，看它等于看一个
/// 已经不起作用的文件。
pub fn managed_settings() -> PathBuf {
    #[cfg(windows)]
    {
        // **不写死 `C:\`** —— 系统盘不一定是 C，而写死的后果是在那些机器上
        // 报「这台机器没有管理策略」，也就是对一个真的压过用户配置的东西
        // 视而不见。
        std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
            .join("ClaudeCode")
            .join("managed-settings.json")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

/// 管理策略的分片：和 [`managed_settings`] 同一目录下的 `managed-settings.d/`。
///
/// 文档（「Split a file-based policy across teams」）：先读
/// `managed-settings.json`，再按字母序合并目录里每个 `*.json`，同一个键**后读
/// 的赢**（`env` 这类嵌套块逐键合并）；隐藏文件和不以 `.json` 结尾的不读。
/// 两者合起来是同一个管理策略来源，一样压过用户自己的配置 —— 只看
/// `managed-settings.json` 的话，放在分片里的 `ANTHROPIC_BASE_URL` 我们看
/// 不见，而它恰恰盖在最上面。
pub fn managed_settings_dropins() -> Vec<PathBuf> {
    let file = managed_settings();
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    dropins_in(&dir.join("managed-settings.d"))
}

/// 一个目录里 Claude Code 会读的那些分片，按它读的顺序。
fn dropins_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            !n.starts_with('.') && n.ends_with(".json")
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对 home，不是绝对路径 —— 否则 `home.join(…)` 会把 home 整个丢掉。
    #[test]
    fn the_desktop_config_is_under_home() {
        let h = Path::new("/tmp/h");
        let p = CLAUDE_DESKTOP_CONFIG.resolve_with(h, None, |_| None);
        assert!(p.starts_with(h), "{}", p.display());
        assert!(p.ends_with("claude_desktop_config.json"));
    }

    /// 接出来的每一段都是一个组件：没有哪一段里还夹着 `/`。
    #[test]
    fn a_relative_path_is_joined_one_component_at_a_time() {
        let p = under(Path::new("/h"), ".claude/settings.json");
        let parts: Vec<_> = p.components().map(|c| c.as_os_str().to_owned()).collect();
        assert_eq!(parts.last().unwrap(), "settings.json");
        assert_eq!(parts[parts.len() - 2], ".claude");
        assert!(
            parts
                .iter()
                .all(|c| !c.to_string_lossy().contains('/') || c == "/")
        );
        assert_eq!(
            under(Path::new("/h"), "a//b/"),
            Path::new("/h").join("a").join("b")
        );
    }

    #[test]
    fn the_shown_path_is_written_the_platform_way() {
        let s = Loc::Home(".claude/settings.json").shown();
        if cfg!(windows) {
            assert_eq!(s, r"%USERPROFILE%\.claude\settings.json");
        } else {
            assert_eq!(s, "~/.claude/settings.json");
        }
    }

    /// 机器级的那一个反过来：必须是绝对的。
    #[test]
    fn the_managed_policy_is_machine_wide() {
        assert!(managed_settings().is_absolute());
        assert!(managed_settings().ends_with("managed-settings.json"));
        if cfg!(target_os = "linux") {
            assert_eq!(
                managed_settings(),
                Path::new("/etc/claude-code/managed-settings.json")
            );
        }
    }

    /// 每个平台指向自己那套习惯的位置。
    #[test]
    fn each_platform_points_at_its_own_convention() {
        let h = Path::new("/h");
        let p = CLAUDE_DESKTOP_CONFIG.resolve_with(h, None, |_| None);
        let z = ZED_SETTINGS.resolve_with(h, None, |_| None);
        if cfg!(windows) {
            assert!(p.starts_with(h.join("AppData")), "{}", p.display());
        } else if cfg!(target_os = "macos") {
            assert!(p.starts_with(h.join("Library")), "{}", p.display());
            assert_eq!(z, under(h, ".config/zed/settings.json"));
        } else {
            assert_eq!(p, under(h, ".config/Claude/claude_desktop_config.json"));
            assert_eq!(z, under(h, ".config/zed/settings.json"));
        }
    }

    /// 设了 `$XDG_CONFIG_HOME` 的用户，认它的客户端就不在 `~/.config` 了。
    ///
    /// Windows 上 `/home/u` 不是绝对路径，这条只在 unix 上跑。
    #[test]
    #[cfg(not(windows))]
    fn xdg_config_home_is_honoured_for_the_process_own_home() {
        let h = Path::new("/home/u");
        let var = |n: &str| (n == "XDG_CONFIG_HOME").then(|| "/home/u/.cfg".into());
        assert_eq!(
            OPENCODE_CONFIGS[1].resolve_with(h, Some(h), var),
            Path::new("/home/u/.cfg/opencode/opencode.json")
        );
        // 没设就是默认位置
        assert_eq!(
            OPENCODE_CONFIGS[1].resolve_with(h, Some(h), |_| None),
            Path::new("/home/u/.config/opencode/opencode.json")
        );
        // 数据目录走它自己的变量
        let data = |n: &str| (n == "XDG_DATA_HOME").then(|| "/data/u".into());
        assert_eq!(
            Loc::XdgData("opencode").resolve_with(h, Some(h), data),
            Path::new("/data/u/opencode")
        );
        // 跟 home 走的那些不看它
        assert_eq!(
            Loc::Home(".codex/config.toml").resolve_with(h, Some(h), var),
            Path::new("/home/u/.codex/config.toml")
        );
    }

    /// **隔离。**传进来的不是这个进程的 home（测试的临时目录就是），变量
    /// 描述的就不是它 —— 认了的话，测试改的是开发者真的配置。
    #[test]
    fn xdg_config_home_is_ignored_for_any_other_home() {
        let var = |_: &str| Some("/home/me/.config".into());
        let tmp = Path::new("/tmp/test-home");
        assert_eq!(
            OPENCODE_CONFIGS[1].resolve_with(tmp, Some(Path::new("/home/me")), var),
            Path::new("/tmp/test-home/.config/opencode/opencode.json")
        );
        assert_eq!(
            OPENCODE_CONFIGS[1].resolve_with(tmp, None, var),
            Path::new("/tmp/test-home/.config/opencode/opencode.json")
        );
    }

    /// 分片按文件名排好，隐藏的和不是 `.json` 的不算。
    #[test]
    fn managed_dropins_are_read_in_claude_code_order() {
        let d = tempfile::tempdir().unwrap();
        for f in [
            "20-b.json",
            "10-a.json",
            ".hidden.json",
            "notes.txt",
            "x.json.bak",
        ] {
            std::fs::write(d.path().join(f), "{}").unwrap();
        }
        std::fs::create_dir(d.path().join("30-dir.json")).unwrap();
        let got: Vec<_> = dropins_in(d.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, ["10-a.json", "20-b.json"]);
        assert!(dropins_in(&d.path().join("没有这个目录")).is_empty());
    }

    /// 在的几个里优先级最高的那一个；都不在就是该新建的第一个。
    #[test]
    fn the_opencode_file_is_the_one_opencode_itself_would_write() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        assert_eq!(first_existing(OPENCODE_CONFIGS, h), 0, "都不在：新建 jsonc");
        let json = OPENCODE_CONFIGS[1].resolve(h);
        std::fs::create_dir_all(json.parent().unwrap()).unwrap();
        std::fs::write(OPENCODE_CONFIGS[2].resolve(h), "{}").unwrap();
        assert_eq!(first_existing(OPENCODE_CONFIGS, h), 2);
        std::fs::write(&json, "{}").unwrap();
        assert_eq!(first_existing(OPENCODE_CONFIGS, h), 1);
        std::fs::write(OPENCODE_CONFIGS[0].resolve(h), "{}").unwrap();
        assert_eq!(first_existing(OPENCODE_CONFIGS, h), 0);
    }

    /// 相对路径的值当没设，这是 XDG 规范的要求。
    #[test]
    fn a_relative_xdg_value_is_ignored() {
        let h = Path::new("/home/u");
        let var = |_: &str| Some("cfg".into());
        assert_eq!(
            OPENCODE_CONFIGS[1].resolve_with(h, Some(h), var),
            Path::new("/home/u/.config/opencode/opencode.json")
        );
    }
}

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
//! 机器级的那一个（管理策略）没有 home 可言，它读环境变量。

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

/// 给人看的写法：「打开 … 」那一步里的路径。
///
/// macOS 上是 `~/.claude/settings.json`；Windows 上没有 `~`，写成资源管理器
/// 地址栏里能直接粘贴的 `%USERPROFILE%\.claude\settings.json`。
pub fn shown(rel: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"%USERPROFILE%\{}", rel.replace('/', r"\"))
    }
    #[cfg(not(windows))]
    {
        format!("~/{rel}")
    }
}

/// Zed 的设置文件，相对 home。
///
/// Windows 上的 Zed 按那个平台的习惯放在漫游的 AppData 下，不是 `~/.config`。
pub fn zed_settings() -> &'static str {
    #[cfg(windows)]
    {
        "AppData/Roaming/Zed/settings.json"
    }
    #[cfg(not(windows))]
    {
        ".config/zed/settings.json"
    }
}

/// Zed 装过的痕迹：它的设置目录。**是常量不是函数**：它要放进
/// `marker: &[…]` 那个静态切片里，函数的返回值进不去。
pub const ZED_DIR: &str = if cfg!(windows) {
    "AppData/Roaming/Zed"
} else {
    ".config/zed"
};

/// Claude Desktop 的配置，相对 home。
///
/// macOS 上在 `Library/Application Support/` 下，Windows 上在漫游的 AppData 下
/// —— 两边都是各自平台放这类东西的地方，不是我们挑的。
pub fn claude_desktop_config() -> &'static str {
    #[cfg(windows)]
    {
        "AppData/Roaming/Claude/claude_desktop_config.json"
    }
    #[cfg(not(windows))]
    {
        "Library/Application Support/Claude/claude_desktop_config.json"
    }
}

/// 机器级的管理策略文件。**优先级压过一切**，包括用户自己的配置。
///
/// 机器级，所以这一个不相对 home。
pub fn managed_settings() -> PathBuf {
    #[cfg(windows)]
    {
        // **不写死 `C:\`** —— 系统盘不一定是 C，而写死的后果是在那些机器上
        // 报「这台机器没有管理策略」，也就是对一个真的压过用户配置的东西
        // 视而不见。
        std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("ClaudeCode")
            .join("managed-settings.json")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对 home，不是绝对路径 —— 否则 `home.join(…)` 会把 home 整个丢掉。
    #[test]
    fn the_desktop_config_is_relative_to_home() {
        let p = claude_desktop_config();
        assert!(
            !PathBuf::from(p).is_absolute(),
            "绝对路径会让 home.join() 忽略 home：{p}"
        );
        assert!(p.ends_with("claude_desktop_config.json"));
        assert_eq!(
            std::path::Path::new("/tmp/h").join(p),
            std::path::Path::new("/tmp/h").join(p),
        );
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
        let s = shown(".claude/settings.json");
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
    }

    /// 每个平台指向自己那套习惯的位置。
    #[test]
    fn each_platform_points_at_its_own_convention() {
        let p = claude_desktop_config();
        if cfg!(windows) {
            assert!(p.starts_with("AppData/"), "{p}");
        } else {
            assert!(p.starts_with("Library/"), "{p}");
        }
    }
}

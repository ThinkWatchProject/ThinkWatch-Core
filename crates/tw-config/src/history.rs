//! 一次配置改动的来源。
//!
//! 每一版配置存在哪儿、留几版、怎么恢复，在 `tw-pack` 的本机备份里（`tw_pack::backup`）：
//! 一版是一个完整的配置包，连同插件和 Lite 的通用偏好。这里只剩「谁改的」这个词 ——
//! 写配置的每一条路都要说它，而那几条路不都够得着 `tw-pack`。

/// 一次改动的来源。**记下来是为了让备份列表能读懂** ——
/// 「界面改的」和「你自己在编辑器里改的」在排查时是完全不同的线索。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Ui,
    Cli,
    /// 外部编辑（编辑器、脚本），我们是从文件监听发现的
    External,
    /// core 自己装上它自带的默认插件、或者把没动过的默认插件换成新版。不是人发起的，
    /// 要一眼认得出来
    Defaults,
    /// 应用了另一台设备经同步位置传来的版本
    Sync,
    /// 导入了一个配置包
    Import,
    /// 恢复了一个备份版本
    Restore,
    /// 一键修复
    Repair,
    /// token 端点换发了新的凭据，我们把它写回去了。
    ///
    /// **不是人发起的写入，也不留备份版本**：恢复到一次轮换之前，拿到的是一个已经作废的
    /// token；会轮换的服务器一小时一次，几天就把用户自己改过的那些版本挤出去了
    Rotation,
}

impl Origin {
    /// 控制面发给界面的值，也是备份文件名里的那一段。
    pub fn slug(&self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Cli => "cli",
            Origin::External => "external",
            Origin::Defaults => "defaults",
            Origin::Sync => "sync",
            Origin::Import => "import",
            Origin::Restore => "restore",
            Origin::Repair => "repair",
            Origin::Rotation => "rotation",
        }
    }

    /// 反过来。认不出的是外部编辑：一个说不出来源的版本，最可能就是有人直接改了文件
    pub fn parse(s: &str) -> Origin {
        match s {
            "ui" => Origin::Ui,
            "cli" => Origin::Cli,
            "defaults" => Origin::Defaults,
            "sync" => Origin::Sync,
            "import" => Origin::Import,
            "restore" => Origin::Restore,
            "repair" => Origin::Repair,
            "rotation" => Origin::Rotation,
            _ => Origin::External,
        }
    }

    /// 命令行里的说法
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Ui => "the interface",
            Origin::Cli => "the command line",
            Origin::External => "an outside edit",
            Origin::Defaults => "the default plugins",
            Origin::Sync => "sync",
            Origin::Import => "an import",
            Origin::Restore => "a restore",
            Origin::Repair => "a repair",
            Origin::Rotation => "a credential rotation",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_origin_reads_back_from_its_word() {
        for o in [
            Origin::Ui,
            Origin::Cli,
            Origin::External,
            Origin::Defaults,
            Origin::Sync,
            Origin::Import,
            Origin::Restore,
            Origin::Repair,
            Origin::Rotation,
        ] {
            assert_eq!(Origin::parse(o.slug()), o);
        }
        assert_eq!(Origin::parse("rollback"), Origin::External);
    }
}

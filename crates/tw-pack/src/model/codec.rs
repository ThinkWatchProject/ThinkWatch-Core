//! 插件源码怎么拆成「代码」和「数据」。
//!
//! 插件的设置写在它自己的文件里（manifest 字面量里的出错时怎么办、范围、设置的值）。比较
//! 时这两样分开：一台设备改了设置、另一台改了代码，各改各的，不算冲突。读 manifest 要
//! 插件运行时那一套（编它要一个能出 wasm 的 clang），这个 crate 不依赖它：Core 启动时把
//! 真的拆法装进来（[`super::install_codec`]），没装时整份文件当代码（[`WholeFile`]）。

use serde_json::Value as Json;

/// 插件源码的拆法。
pub trait PluginCodec: Send + Sync {
    /// 拆成两份：**代码**是数据换成缺省之后的完整源码（manifest 那一段按固定的样子重写，
    /// 只差数据的两份源码拆出来的代码一字不差），**数据**是出错时怎么办、范围和设置的值。
    /// 读不出 manifest 的是 None：整份当代码
    fn split(&self, source: &str) -> Option<(String, Json)>;

    /// 把数据写进源码，别的字节不动。写不进去（数据和源码声明的对不上）是 None
    fn join(&self, source: &str, data: &Json) -> Option<String>;

    /// 随这一份 Core 发的默认插件的源码。不是默认插件是 None
    fn shipped(&self, id: &str) -> Option<String>;

    /// 改得了回答里的工具调用（权限有 `reply_tool_calls`）：这种插件每台设备分别批准，
    /// 别处来的版本落到本机时先不批准。读不出来的按要批准算
    fn needs_approval(&self, source: &str) -> bool;
}

/// 不拆：整份文件是代码，没有数据，没有默认插件，一律要批准。
pub struct WholeFile;

impl PluginCodec for WholeFile {
    fn split(&self, _source: &str) -> Option<(String, Json)> {
        None
    }

    fn join(&self, source: &str, data: &Json) -> Option<String> {
        data.is_null().then(|| source.to_string())
    }

    fn shipped(&self, _id: &str) -> Option<String> {
        None
    }

    fn needs_approval(&self, _source: &str) -> bool {
        true
    }
}

//! 一个编好的插件。**数据面通过它跑钩子**（请求钩子、回答钩子），由运行时的适配层
//! 实现；测试有自己的替身。
//!
//! 这里先只有加载和展示要用的那两样，跑钩子的方法由数据面那一侧补上。

use crate::plugin::engine::Manifest;

pub trait PluginHost: Send + Sync {
    /// 编译时读到的 manifest，校验过的
    fn manifest(&self) -> &Manifest;
    /// 编出它的那一份字节的 SHA-256（不变式 I9 比对的就是它）
    fn sha256(&self) -> [u8; 32];
}

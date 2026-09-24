/// 标出一个消息码：原样给出那个字面量。
///
/// **表里的码都要用它包起来。**`msg!` 以外造 [`tw_types::Msg`] 的地方（接管的
/// 代价、手动配置的步骤、MCP 写不了的理由）码和句子分开存在表里，core 的码清单
/// 测试（tw-control 的 `tests/msg_codes.rs`）靠这个记号在源码里找到它们 ——
/// 少了它，那个码就不在桌面端拿去对照译文的清单上。
macro_rules! code {
    ($c:literal) => {
        $c
    };
}

pub mod clients;
pub mod detect;
pub mod foreign;
pub mod json;
pub mod mcp;
pub mod paths;
pub mod plan;
pub mod sentinel;
pub mod toml;
pub mod yaml;
pub mod yamlval;

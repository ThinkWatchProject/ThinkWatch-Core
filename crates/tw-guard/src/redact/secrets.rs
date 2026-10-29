//! 扩充的凭据规则：多前缀、`sk-` 短格式、保存的密钥、按位置认的、配置赋值、加密货币。

use std::ops::Range;

use super::rules::{Hit, RuleSet};

/// 这一组规则在一次扫描里的状态：扫之前问一次哪几条开着，扫的过程中攒着跨 token 的东西。
pub(super) struct Want {}

impl Want {
    pub(super) fn new(_set: &RuleSet) -> Self {
        Want {}
    }

    /// 整段文本过一遍的判据（跨 token 的、按位置找的）
    pub(super) fn text(&mut self, _text: &str, _json: bool, _out: &mut Vec<Hit>) {}

    /// 每个 token 一次。`quoted`：在 JSON 字符串里面（`json` 为真时才有意义）
    pub(super) fn token(
        &mut self,
        _text: &str,
        _tok: &str,
        _span: Range<usize>,
        _quoted: bool,
        _json: bool,
        _out: &mut Vec<Hit>,
    ) {
    }

    /// 扫完了：攒着没交出去的交出去
    pub(super) fn finish(&mut self, _text: &str, _out: &mut Vec<Hit>) {}
}

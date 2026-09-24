//! 日志留多久。

use serde::{Deserialize, Serialize};

/// 日志留多久。
///
/// **两个期限，因为两种东西的代价差三个数量级。**请求和响应的正文一条
/// 几十 KB，一天就能堆出几百 MB；而一行记录（时刻、模型、用量、金额）
/// 只有几百字节，留一个季度也不过几十 MB。用一个期限管住两者，等于
/// 要么早早丢掉「上个月花了多少」，要么让磁盘替正文买单。
///
/// **总量上限是给突发准备的。**按天数算出来的占用取决于用量，而用量
/// 会有一周十倍于平时的时候 —— 没有上限的话，那一周会把磁盘吃光，
/// 而用户直到硬盘满了才知道。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    /// 请求和响应的正文留几天。**大头在这儿**
    #[serde(default = "d_body_days")]
    pub body_days: u64,
    /// 一行记录留几天。它撑着「上个月花了多少」那类问题
    #[serde(default = "d_row_days")]
    pub row_days: u64,
    /// 正文总共最多占多少字节。超了从最旧的整天开始删
    #[serde(default = "d_body_max_bytes")]
    pub body_max_bytes: u64,
}

fn d_body_days() -> u64 {
    7
}
fn d_row_days() -> u64 {
    90
}
fn d_body_max_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            body_days: d_body_days(),
            row_days: d_row_days(),
            body_max_bytes: d_body_max_bytes(),
        }
    }
}

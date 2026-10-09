//! Codex 的远程压缩，在转给别家格式的上游时怎么做。
//!
//! # Codex 那边的约定
//!
//! 上游是 OpenAI（Codex 里叫 `OpenAI` 的 provider，ThinkWatch 接管时就是这个名字）时，Codex
//! 压缩前文走「远程压缩」（`codex-rs/core/src/compact_remote_v2.rs`）：
//!
//! - 发一个普通的流式 Responses 请求，`input` 是整段历史，末尾加一项
//!   `{"type": "compaction_trigger"}`，工具和平时一样
//! - 收流：`output_item.done` 里**必须恰好有一个** `{"type": "compaction", "encrypted_content": …}`，
//!   还要有 `response.completed`；别的项不看。少了、多了都算失败
//! - 之后的历史换成：保留下来的用户消息 + 这个 `compaction` 项 + 重新写的上下文。下一轮
//!   请求把这个项原样带回来，OpenAI 读它里面的密文接上前文
//!
//! # 转给别家时
//!
//! 别家写不出 OpenAI 的密文，也读不懂。所以：
//!
//! - 请求：历史原样，末尾加一条用户消息，请上游写一份交接摘要（[`INSTRUCTION`]）。工具、
//!   推理设置、工具选择都不动 —— 和上一轮请求同一个开头，提示缓存照样命中（Anthropic 改了
//!   `tool_choice` 整段对话的缓存就作废）。这条是网关写的，不在原文里，内容过滤不看它
//! - 回答：上游写的文字收齐，作为**唯一一个** `compaction` 项交回去，`encrypted_content` 是
//!   [`carry`] 写的 `tw1.c.<摘要的 base64url>`。Codex 只把它当一串不透明的字符存着、带回来
//! - 下一轮：认出这个前缀，解回摘要，放在原位：对话中途的一条系统消息
//!   （[`crate::ir::Role::System`]，开头是 [`SUMMARY_PREFIX`]）。摘要是上游写的，不是调用方
//!   说的话，内容过滤和脱敏不看它。OpenAI 自己的密文照旧拒绝：那段前文只有 OpenAI 读得出来
//! - 这段对话之后换到 OpenAI 的上游直通时，`convert::strip_carried` 把它原位换成同样内容的
//!   一条 developer 消息 —— OpenAI 不认我们写的「密文」
//!
//! 写摘要的这一次和别的请求一样计用量和费用：上游的回答按它自己的格式嗅用量。

/// 转换写出去的压缩项的前缀：`tw1.` 是转换写出去的东西（见 [`crate::ir::CARRIED`]），
/// `c.` 是压缩
pub const CARRIED_PREFIX: &str = "tw1.c.";

/// 请上游写摘要的那条用户消息。
///
/// 摘要之后只剩它、用户最近说的话和系统提示，接手的模型要能只凭这些接着干：所以要的是
/// 交接，不是概括 —— 文件路径、做过的决定、试过不行的路、跑着的东西、没做完的事都要在。
pub const INSTRUCTION: &str = "\
The conversation above is about to be compacted: everything before this message will be replaced \
by a summary, and only the summary, the user's recent messages and the system instructions will \
remain. Write that summary now, as a handoff that lets the work continue from it alone.

Do not call any tools. Reply with the summary only, in the language the user has been writing in.

Include:
- The user's goal, and every explicit request, preference and constraint they stated; quote them \
where the exact wording matters.
- What has been done: decisions made and why, approaches tried and how they turned out, including \
failures and their causes.
- The current state: files created, changed or examined (with their paths), commands run and \
their important results, tests and whether they pass, and anything left half-done.
- State that still matters: running processes or sessions, plans or to-do items and their status, \
facts learned about the environment (paths, versions, configuration).
- What remains: the next concrete steps, and any open questions or blockers.
- If an earlier summary appears above, carry forward everything in it that is still relevant.

Be specific: keep exact names, paths, identifiers, error messages and numbers. Leave out anything \
that no longer matters.";

/// 下一轮把摘要放回对话时，放在它前面的那句话
pub const SUMMARY_PREFIX: &str = "\
The earlier part of this conversation was compacted. This summary of it replaces those messages, \
which are no longer available:";

/// 摘要 → `compaction` 项的 `encrypted_content`
pub fn carry(summary: &str) -> String {
    format!("{CARRIED_PREFIX}{}", base64url(summary.as_bytes()))
}

/// `encrypted_content` 是不是转换写出去的
pub fn is_carried(encrypted: &str) -> bool {
    encrypted.starts_with(CARRIED_PREFIX)
}

/// 读回转换写出去的摘要。不是这个前缀、或者解不开的是 `None`
pub fn read(encrypted: &str) -> Option<String> {
    let bytes = unbase64url(encrypted.strip_prefix(CARRIED_PREFIX)?)?;
    String::from_utf8(bytes).ok()
}

/// 摘要放回对话时的正文
pub fn restored(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}\n\n{summary}")
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url，不补 `=`。这个 crate 只依赖 serde，为两个函数不值得多拉一个依赖
fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn unbase64url(s: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| ALPHABET.iter().position(|a| *a == c).map(|i| i as u32);
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    for chunk in s.as_bytes().chunks(4) {
        // 一个字符凑不出一个字节
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= digit(*c)? << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_goes_out_opaque_and_comes_back_unchanged() {
        for s in [
            "",
            "a",
            "ab",
            "abc",
            "Fix the test in src/lib.rs — 还差一步 🚀",
        ] {
            let c = carry(s);
            assert!(is_carried(&c), "{c}");
            assert!(
                c[CARRIED_PREFIX.len()..]
                    .bytes()
                    .all(|b| ALPHABET.contains(&b)),
                "{c}"
            );
            assert_eq!(read(&c).as_deref(), Some(s));
        }
        assert_eq!(carry("hi?"), "tw1.c.aGk_");
    }

    #[test]
    fn something_else_is_not_read_as_a_summary() {
        // OpenAI 自己的密文
        assert_eq!(read("gAAAAABo"), None);
        // 前缀对、内容坏了
        assert_eq!(read("tw1.c.a"), None);
        assert_eq!(read("tw1.c.a+b="), None);
        // 不是 UTF-8
        assert_eq!(read(&format!("{CARRIED_PREFIX}_w")), None);
    }
}

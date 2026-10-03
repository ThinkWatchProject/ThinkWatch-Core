//! 一个请求体在出站脱敏里怎么走：先看一遍、编好号（[`look`]），每一跳发出去之前换
//! （[`replace`]）。两个网关都用这一份。
//!
//! # 查的是整个请求体
//!
//! 不只是用户消息：系统提示、模型以前的回答、工具调用的参数里都可能躺着一把密钥。
//! 规则直接跑在请求体的 JSON 原文上（见 [`crate::redact::rules::scan`]：它认得 JSON
//! 的转义，换下来的值放回去还是合法的 JSON）。两样东西不算：
//!
//! - **base64 载荷**（图片、文件、推理签名、`data:` URI）：一段编码过的图片里碰巧
//!   有一截像密钥的字母，换掉它换坏的是那张图，报出来的是一条没人看得懂的误报；
//! - **我们自己的占位符**（见 [`hits`]）。
//!
//! # 观察档和替换档用同一套规则
//!
//! 以前观察档按全部类别检测、替换档按上游的类别替换，于是同一个请求观察时报「检测
//! 到」，切到替换后一处不换 —— 用户看到的证据，和他切过去之后得到的保护，说的不是
//! 一件事。现在两档找的是同一批，只是一个换、一个不换。
//!
//! # 一个值一个占位符，整个请求里都一样
//!
//! 替换档下 [`look`] 按客户端原文里出现的先后给找到的值编好号，每一跳都接着这本账换。
//! 各跳各起一本账的话，转换过格式、字段换了顺序的那一跳，同一把密钥可能是 2 号，而
//! 上一跳、存下来的那份请求里它是 1 号 —— 请求和回答对不上号。

use std::ops::Range;

use crate::policy::Mode;
use crate::redact::replace::{Ledger, Scheme};
use crate::redact::rules::{Finding, Hit, RuleSet};

/// 按规则找一遍，**不算我们自己的占位符，也不进 base64 载荷**。
///
/// 连接串里写着 `postgres://app:<<TW_SECRET_2>>@db` 的那一段，在口令那条规则看来就是
/// 一个口令 —— 可它是我们换上去的：存下来的请求拿去重放、用户把详情里看到的东西贴回
/// 对话，都会带着它。当成凭据的话，它会被再换一次、在安全日志里报一次、落盘时被打成
/// `<<TW_…_2>>`。压在一个占位符上的命中都不算。
pub fn hits(text: &str, rules: &RuleSet) -> Vec<Hit> {
    let mut hits = crate::redact::rules::scan(text, rules);
    if hits.is_empty() {
        return hits;
    }
    off_placeholders(text, &mut hits);
    // 大多数请求一处都不命中：载荷在哪儿等有了命中再找
    if !hits.is_empty() {
        outside(&mut hits, base64_payloads(text));
    }
    hits
}

/// 去掉压在我们自己的占位符上的命中（见 [`hits`]）。在解码过的正文上找的调用方（桌面版的
/// 插件那一层）也用它。
pub fn off_placeholders(text: &str, hits: &mut Vec<Hit>) {
    if !hits.is_empty() && text.contains(Scheme::SECRET.open) {
        let ours = Scheme::SECRET.find_in(text);
        outside(hits, ours.into_iter().map(|(at, _, _)| at).collect());
    }
}

/// 一段**纯文本**按它出现在请求体里时的样子找（同 [`hits`]），区间是原文里的。管理界面
/// 上的「测试」用它：用户贴进来的是一段正文，网关找的是装着它的 JSON —— 结论得一致。
pub fn hits_plain(text: &str, rules: &RuleSet) -> Vec<Hit> {
    crate::redact::rules::plain_with(text, |encoded| hits(encoded, rules))
}

/// 只留和 `spans` 里哪一段都不重叠的命中。
///
/// **不拿每个命中去和每一段比**：一个请求里命中几万处、占位符或载荷又有几万段时（重放
/// 一份换过的请求），那是平方级的。`spans` 按起点排好，起点在命中终点之前的是一个前缀，
/// 前缀里最远的终点越过了命中的起点，就是有一段和它重叠 —— 每个命中二分查一次。
fn outside(hits: &mut Vec<Hit>, mut spans: Vec<Range<usize>>) {
    if spans.is_empty() {
        return;
    }
    spans.sort_unstable_by_key(|s| s.start);
    // reach[i]：前 i + 1 段里最远的终点
    let reach: Vec<usize> = spans
        .iter()
        .scan(0, |far, s| {
            *far = s.end.max(*far);
            Some(*far)
        })
        .collect();
    hits.retain(|h| {
        let before = spans.partition_point(|s| s.start < h.bytes.end);
        before == 0 || reach[before - 1] <= h.bytes.start
    });
}

/// 找一遍。**观察档和替换档都找**，关闭时不找。
///
/// **不是 UTF-8 就不看。**图片之类的二进制体里不会有粘贴进来的 key。
pub fn find(mode: Mode, rules: &RuleSet, body: &[u8]) -> Vec<Finding> {
    if !mode.detects() || rules.is_empty() {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    crate::redact::rules::findings(text, &hits(text, rules))
}

/// 一本新账，让开 `body` 里已经写着的占位符（见 [`Ledger::avoiding`]）。
///
/// 存下来的请求（替换档下存的是换过的那一份）拿去重放时，里面写着的 1 号不能再发给
/// 新找到的值 —— 回显里的 1 号会被还原成那个新值。
pub fn ledger_for(body: &[u8]) -> Ledger {
    let fresh = Ledger::new(Scheme::SECRET);
    match std::str::from_utf8(body) {
        Ok(text) => fresh.avoiding(text),
        Err(_) => fresh,
    }
}

/// 看一遍客户端发来的原文：报出去的记录（同 [`find`]），和这个请求的账本。
///
/// 记录是**一个不同的值一条、一条不少**（见 [`crate::redact::rules::findings`]）：报几条、
/// 怎么聚合由调用方定，总共有几个不同的值就是它的长度。
///
/// **替换档下账本在这里就编好号**：原文里找到的每个值按出现的先后发号，让开原文里本来
/// 就写着的占位符。之后每一跳都接着这本账换（[`replace`]），存下来的那份请求也照它换。
/// 不在替换档时账本是空的。
pub fn look(mode: Mode, rules: &RuleSet, body: &[u8]) -> (Vec<Finding>, Ledger) {
    look_from(mode, rules, body, Ledger::new(Scheme::SECRET))
}

/// [`look`]，**接着 `seed` 的账编号**：一个请求改过之后再看一遍时（桌面版的插件改写过的
/// 请求），改过的这一份接着原文那本账编，同一个值还是同一个号，新出现的值接着往后编。
/// 不在替换档时账本是空的，`seed` 用不上。
pub fn look_from(mode: Mode, rules: &RuleSet, body: &[u8], seed: Ledger) -> (Vec<Finding>, Ledger) {
    let empty = || Ledger::new(Scheme::SECRET);
    if !mode.detects() || rules.is_empty() {
        return (Vec::new(), empty());
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return (Vec::new(), empty());
    };
    let hits = hits(text, rules);
    let found = crate::redact::rules::findings(text, &hits);
    if !mode.acts() {
        return (found, empty());
    }
    let seed = seed.avoiding(text);
    let ledger = if hits.is_empty() {
        seed
    } else {
        crate::redact::replace::apply(text, &hits, seed).ledger
    };
    (found, ledger)
}

/// 替换档下换掉要发出去的这一份，**接着 `ledger` 的账**（见 [`look`]）。返回换过的体和
/// 还原用的账本；**不在替换档、或者没找到东西时与进来时逐字节相同**，账本就是交进来的那本。
pub fn replace(
    mode: Mode,
    rules: &RuleSet,
    body: bytes::Bytes,
    ledger: &Ledger,
) -> (bytes::Bytes, Ledger) {
    if !mode.acts() || rules.is_empty() {
        return (body, ledger.clone());
    }
    // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求
    let Ok(text) = std::str::from_utf8(&body) else {
        return (body, ledger.clone());
    };
    let hits = hits(text, rules);
    if hits.is_empty() {
        // 没命中就原样返回，连一次拷贝都不做
        return (body, ledger.clone());
    }
    let r = crate::redact::replace::apply(text, &hits, ledger.clone());
    (bytes::Bytes::from(r.text), r.ledger)
}

/// 装 base64 的那几个键：图片和文件的内容（Anthropic 的 `data`、Bedrock 的 `bytes`、
/// Chat 的 `file_data`），推理的签名和加密内容（Anthropic 的 `signature`、Gemini 的
/// `thoughtSignature`、Responses 的 `encrypted_content`）
const CARRIERS: &[&str] = &[
    "data",
    "bytes",
    "file_data",
    "signature",
    "thoughtSignature",
    "thought_signature",
    "encrypted_content",
];

/// 多长才算载荷。**一把密钥写在 `data` 下面照样要找**：最长的密钥也就一两百个字符，
/// 图片、签名动辄上千
const PAYLOAD_MIN: usize = 256;

/// 一段 JSON 原文里装着 base64 的那些字符串（引号里面的部分）：上面那几个键下面、
/// 长得像 base64 的值，和任何地方的 `data:…;base64,` URI。
///
/// **只扫一遍、不解析**：请求体可以有几百 MB，为找这几段建一棵树不值得。不是 JSON
/// 的文字（开头不是 `{` 或 `[`）没有载荷可言。
fn base64_payloads(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    if !text.trim_start().starts_with(['{', '[']) {
        return out;
    }
    // 上一个字符串是不是一个键（后面跟着冒号），是的话它是哪个
    let mut key: Option<Range<usize>> = None;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = j.min(b.len());
                let s = start..end;
                i = end + 1;
                // 后面（跳过空白）是冒号：这是一个键
                let mut k = i;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if b.get(k) == Some(&b':') {
                    key = Some(s);
                    i = k + 1;
                    continue;
                }
                let value = &text[s.clone()];
                let carried = key
                    .as_ref()
                    .is_some_and(|k| CARRIERS.contains(&&text[k.clone()]));
                if data_uri(value) || (carried && looks_base64(value)) {
                    out.push(s);
                }
                key = None;
            }
            // 一个键的值不是字符串（对象、数组、数）：那个键管不到里面
            b'{' | b'[' | b',' => {
                key = None;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// `data:image/png;base64,…`。JSON 原文里斜杠可能写成 `\/`
fn data_uri(s: &str) -> bool {
    s.starts_with("data:") && s.get(..s.len().min(256)).unwrap_or(s).contains(";base64,")
}

/// 足够长，而且只有 base64（连同 URL 安全的那一种）的字符。JSON 原文里斜杠可能写成 `\/`
fn looks_base64(s: &str) -> bool {
    if s.len() < PAYLOAD_MIN {
        return false;
    }
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1) == Some(&b'/') => i += 2,
            c if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'-' | b'_' | b'=') => {
                i += 1
            }
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn body() -> bytes::Bytes {
        bytes::Bytes::from(format!(
            "{{\"messages\":[{{\"content\":\"我的 key 是 {KEY}\"}}]}}"
        ))
    }

    fn fresh() -> Ledger {
        Ledger::new(Scheme::SECRET)
    }

    #[test]
    fn enforce_replaces_with_a_placeholder_and_keeps_the_body_valid_json() {
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), body(), &fresh());
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(!text.contains(KEY), "{text}");
        assert!(text.contains("<<TW_SECRET_1>>"), "{text}");
        assert_eq!(ledger.len(), 1);
        // 换完还得是合法 JSON —— 占位符里没有需要转义的字符
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            v["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("<<TW_SECRET_1>>")
        );
    }

    #[test]
    fn observe_finds_what_enforce_would_replace_and_changes_nothing() {
        // 观察档**只记录，不改变任何行为**；它报的和替换档会换的是同一批
        let seen = find(Mode::Observe, &RuleSet::defaults(), &body());
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].masked.contains("AAAAAAAAAAAA"));
        assert_eq!(seen, find(Mode::Enforce, &RuleSet::defaults(), &body()));
        let (out, ledger) = replace(Mode::Observe, &RuleSet::defaults(), body(), &fresh());
        assert_eq!(out, body());
        assert!(ledger.is_empty());
    }

    #[test]
    fn off_does_not_even_look() {
        assert!(find(Mode::Off, &RuleSet::defaults(), &body()).is_empty());
        let (out, _) = replace(Mode::Off, &RuleSet::defaults(), body(), &fresh());
        assert_eq!(out, body());
    }

    #[test]
    fn a_binary_body_is_left_alone_instead_of_being_mangled() {
        // 按字节乱切一个非 UTF-8 的体，得到的是一份坏掉的请求。
        let raw = bytes::Bytes::from(vec![0xff, 0xfe, 0x00, 0x01]);
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), raw.clone(), &fresh());
        assert_eq!(out, raw);
        assert!(l.is_empty());
        assert!(find(Mode::Enforce, &RuleSet::defaults(), &raw).is_empty());
    }

    #[test]
    fn a_body_with_nothing_to_redact_is_returned_untouched() {
        let plain = bytes::Bytes::from_static(b"{\"messages\":[]}");
        let (out, l) = replace(Mode::Enforce, &RuleSet::defaults(), plain.clone(), &fresh());
        assert_eq!(out, plain);
        assert!(l.is_empty());
    }

    /// 每一跳接着原文那本账换：同一把密钥在每一跳都是同一个号，哪怕那一跳发出去的那份
    /// 把字段换了顺序（转换过格式，或者改写参数时按键名重排过）
    #[test]
    fn every_hop_numbers_a_value_the_way_the_client_body_did() {
        let other = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let client =
            format!(r#"{{"system":"{KEY}","messages":[{{"role":"user","content":"{other}"}}]}}"#);
        let (found, l0) = look(Mode::Enforce, &RuleSet::defaults(), client.as_bytes());
        assert_eq!((found.len(), l0.len()), (2, 2));
        let hop =
            format!(r#"{{"messages":[{{"role":"user","content":"{other}"}}],"system":"{KEY}"}}"#);
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), hop.clone().into(), &l0);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"messages":[{"role":"user","content":"<<TW_SECRET_2>>"}],"system":"<<TW_SECRET_1>>"}"#
        );
        assert_eq!(ledger.len(), 2);
        // 各起一本账的话号就对调了 —— 这条测试防的就是它
        let (alone, _) = replace(Mode::Enforce, &RuleSet::defaults(), hop.into(), &fresh());
        assert!(
            String::from_utf8(alone.to_vec())
                .unwrap()
                .contains(r#""system":"<<TW_SECRET_2>>""#)
        );
    }

    #[test]
    fn look_numbers_only_under_enforce_and_reports_the_same_either_way() {
        let (seen, l) = look(Mode::Observe, &RuleSet::defaults(), &body());
        assert_eq!(seen, find(Mode::Observe, &RuleSet::defaults(), &body()));
        assert!(l.is_empty(), "观察档不该编号");
        let (acted, l) = look(Mode::Enforce, &RuleSet::defaults(), &body());
        assert_eq!(acted, seen);
        assert_eq!(l.len(), 1);
        let (none, l) = look(Mode::Off, &RuleSet::defaults(), &body());
        assert!(none.is_empty() && l.is_empty());
    }

    /// 接着种子账本编：原文那本账里的值还是原来的号，新出现的往后编
    #[test]
    fn look_from_goes_on_numbering_from_the_seed() {
        let other = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let (_, seed) = look(Mode::Enforce, &RuleSet::defaults(), &body());
        let changed = format!(r#"{{"messages":[{{"content":"{other} 和 {KEY}"}}]}}"#);
        let (found, ledger) = look_from(
            Mode::Enforce,
            &RuleSet::defaults(),
            changed.as_bytes(),
            seed.clone(),
        );
        assert_eq!(found.len(), 2);
        let (out, _) = replace(Mode::Enforce, &RuleSet::defaults(), changed.into(), &ledger);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains("<<TW_SECRET_2>> 和 <<TW_SECRET_1>>"), "{out}");
        // 观察档不编号，种子也用不上
        let (_, l) = look_from(Mode::Observe, &RuleSet::defaults(), &body(), seed);
        assert!(l.is_empty());
    }

    /// 连接串里的占位符长得像口令，可它不是凭据：不再换一次、不报、原样留着
    #[test]
    fn a_placeholder_where_a_password_would_be_is_not_a_password() {
        let t =
            format!("postgres://app:<<TW_SECRET_2>>@db/x 和 postgres://app:hunter2@db/y 和 {KEY}");
        let found: Vec<String> = hits(&t, &RuleSet::defaults())
            .iter()
            .map(|h| t[h.bytes.clone()].to_string())
            .collect();
        assert_eq!(found, vec!["hunter2".to_string(), KEY.to_string()]);
        let body = format!(r#"{{"content":"{t}"}}"#);
        assert_eq!(
            find(Mode::Observe, &RuleSet::defaults(), body.as_bytes()).len(),
            2
        );
        let (out, _) = replace(
            Mode::Enforce,
            &RuleSet::defaults(),
            body.clone().into(),
            &ledger_for(body.as_bytes()),
        );
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains("postgres://app:<<TW_SECRET_2>>@db/x"), "{out}");
        assert!(out.contains("postgres://app:<<TW_SECRET_3>>@db/y"), "{out}");
    }

    #[test]
    fn a_placeholder_already_in_the_client_body_is_not_handed_out_again() {
        // 用户把请求详情里看到的请求贴回了对话：里面写着 1 号
        let pasted = format!(
            r#"{{"messages":[{{"role":"user","content":"上次发的是 <<TW_SECRET_1>>，这次是 {KEY}"}}]}}"#
        );
        let (_, l0) = look(Mode::Enforce, &RuleSet::defaults(), pasted.as_bytes());
        let (out, ledger) = replace(Mode::Enforce, &RuleSet::defaults(), pasted.into(), &l0);
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(out.contains("这次是 <<TW_SECRET_2>>"), "{out}");
        assert_eq!(
            crate::redact::replace::restore("<<TW_SECRET_1>> / <<TW_SECRET_2>>", &ledger),
            format!("<<TW_SECRET_1>> / {KEY}")
        );
        // 重放用的那本新账也让开它
        let stored = format!("<<TW_SECRET_1>> {KEY}");
        let (out, _) = replace(
            Mode::Enforce,
            &RuleSet::defaults(),
            bytes::Bytes::from(stored.clone()),
            &ledger_for(stored.as_bytes()),
        );
        assert_eq!(&out[..], b"<<TW_SECRET_1>> <<TW_SECRET_2>>");
    }

    /// 一段 base64 图片里碰巧有一截像密钥：报出来是没人看得懂的误报，换掉它换坏的是那张图
    #[test]
    fn base64_payloads_are_not_looked_into() {
        // 一截 AWS 访问密钥的样子，夹在一长串 base64 里（`+`、`/` 把它切成了一个 token）
        let blob = format!(
            "{}+AKIAABCDEFGHIJKLMNOP/{}",
            "iVBORw0KGgo".repeat(30),
            "A".repeat(300)
        );
        let typed = "AKIAQRSTUVWXYZ234567";
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": format!("my key {typed}")},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": blob}},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{blob}")}},
                {"type": "thinking", "thinking": "…", "signature": blob},
            ]}]
        })
        .to_string();
        let found: Vec<&str> = hits(&body, &RuleSet::defaults())
            .iter()
            .map(|h| &body[h.bytes.clone()])
            .collect();
        assert_eq!(found, [typed], "只有调用方打的那一把");
        let (out, _) = replace(
            Mode::Enforce,
            &RuleSet::defaults(),
            body.clone().into(),
            &fresh(),
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let parts = &v["messages"][0]["content"];
        assert_eq!(parts[0]["text"], "my key <<TW_SECRET_1>>");
        assert_eq!(parts[1]["source"]["data"], blob.as_str(), "图片原样");
        assert_eq!(parts[3]["signature"], blob.as_str());
    }

    #[test]
    fn a_short_value_under_a_payload_key_is_still_looked_at() {
        // 一把密钥写在 `data` 下面，照样要找：载荷是很长的那种
        let body = format!(r#"{{"tool_result":{{"data":"{KEY}"}}}}"#);
        assert_eq!(hits(&body, &RuleSet::defaults()).len(), 1);
        // 带空格的、不像 base64 的长文字也照样找
        let prose = format!("{} {KEY}", "word ".repeat(80));
        let body = serde_json::json!({ "data": prose }).to_string();
        assert_eq!(hits(&body, &RuleSet::defaults()).len(), 1);
    }

    /// 排序加二分，和拿每个命中去和每一段比，留下的一模一样：段可以乱序、套着、挨着、
    /// 是空的
    #[test]
    fn keeping_hits_outside_the_spans_agrees_with_checking_every_pair() {
        use crate::redact::rules::Rule;
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..300 {
            // 命中：排好序、互不重叠（scan 给的就是这样）
            let mut hits = Vec::new();
            let mut at = 0;
            for _ in 0..next(40) {
                let start = at + next(5);
                let end = start + 1 + next(6);
                hits.push(Hit {
                    bytes: start..end,
                    rule: Rule::Builtin("aws-access-key-id"),
                    label: None,
                });
                at = end;
            }
            let spans: Vec<Range<usize>> = (0..next(30))
                .map(|_| {
                    let start = next(at + 5);
                    start..start + next(12)
                })
                .collect();
            let mut pairwise = hits.clone();
            pairwise.retain(|h| {
                !spans
                    .iter()
                    .any(|s| s.start < h.bytes.end && h.bytes.start < s.end)
            });
            let mut kept = hits;
            outside(&mut kept, spans.clone());
            assert_eq!(kept, pairwise, "{spans:?}");
        }
    }

    #[test]
    fn payloads_are_found_where_json_puts_them() {
        let long = "QUJD".repeat(80);
        let text = format!(
            r#"{{"a":"x","data":"{long}","nested":{{"bytes" : "{long}"}},"list":["data","{long}"],"u":"data:image\/png;base64,{long}","data2":"{long}"}}"#
        );
        let got: Vec<&str> = base64_payloads(&text)
            .into_iter()
            .map(|r| &text[r])
            .collect();
        // `data` 和 `bytes` 下面的、data URI；数组里紧跟在字符串 "data" 后面的不算，别的键下面的不算
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[2].starts_with("data:image"));
        assert!(base64_payloads("plain text \"data\": \"x\"").is_empty());
    }
}

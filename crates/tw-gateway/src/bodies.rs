//! 把请求体和响应体交出去存起来。
//!
//! **走一条有界的通道，不是一次直接调用。**直接调用意味着文件 I/O 跑在
//! 转发那条路上 —— 一次慢磁盘写就会变成一次慢请求，而观测永远不该有这
//! 个权力。通道满了就丢：丢的是一条观测记录，而等它是在惩罚
//! 真实用户。
//!
//! # 落盘的不是原文
//!
//! 交出去的是原文，落盘之前由收的那一头换掉、打码（[`BodyRecord::for_disk`]）：
//! 那要把整份正文扫好几遍，不该在转发那条路上做。**脱敏规则认得出的值不会原样进磁盘**，
//! 哪一档都一样，关着也一样：
//!
//! - 拦截档下，请求里的值换成**发给上游的那个占位符**：存下来的就是上游收到的那一份，
//!   回答里出现的占位符和它对得上号（见 [`crate::guard::look`]）
//! - 别的一律打码，和安全日志里报的是同一种写法
//! - 最后整段再按形状打一遍码，和读的时候是同一个函数（[`tw_secret::mask_body`]），
//!   兜住规则没认出来的
//! - **转换时压缩出的前文摘要**（`compaction` 项里 `tw1.c.` 开头的那串，见
//!   [`tw_dialect::compaction`]）是 base64 包着的一段文字，上面两道都看不见里面：解开，
//!   照同样的规矩换、打码，再包回原位（[`carried_summaries`]）。Codex 每一轮都把它带回来，
//!   摘要里提到过的密钥否则每一轮都原样落一次盘
//!
//! 以前存的是客户端发来的原文，密钥只在读出来的时候才打码：磁盘上躺着的一直是真值。

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::{Hit, Kind, Rule, RuleSet};

/// 打码要多看的那一截。
///
/// 一把密钥正好跨在截断处的话，只看前半截认不出它（私钥要见到结尾那一行才算），存下来
/// 的开头里就留着它的前半截。多看一截，跨在截断处的那一个整个认得出、整个换掉，然后再截。
/// 64 KB 比任何一种认得出的凭据都长得多：一把 8192 位的 RSA 私钥不到 7 KB。
pub const MARGIN: usize = 64 * 1024;

/// 交去落盘的一份最多带多少字节：存下来的那 [`tw_api::BODY_MAX`]，加上打码要多看的
/// [`MARGIN`]。更长的请求体只交开头这么多，响应体也只攒这么多。
pub const WINDOW: usize = tw_api::BODY_MAX + MARGIN;

/// 响应体最多攒多少（[`WINDOW`]）。
///
/// **存下来的和请求体一样长**，都是 [`tw_api::BODY_MAX`]（4 MB），多攒的那一截只给打码
/// 看。以前这里是 256 KB，比存储层肯收的少十几倍：一个长回答的后半截 —— 最后那几个工具
/// 调用、停止的原因 —— 总在被扔掉的那一段里。
///
/// **内存**：每个在飞的回答一份，**按实际长度长**（见 [`ResponseTap::feed`]），攒到上限
/// 为止。绝大多数回答几 KB 到几百 KB；攒得满 4 MB 的是很长的流 —— SSE 里每几个字就
/// 包着一帧，几万 token 的回答就有几 MB。32 个同时在流、个个都过了 4 MB，是 130 MB 上下，
/// 流一结束就还回去。观测层没起来时一个字节都不攒（`crate::ending::Ending::feed`）。交出去
/// 之后由 [`QUEUED_MAX`] 管着。
pub const RESPONSE_TAP_MAX: usize = WINDOW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Request,
    Response,
    /// 插件改过之后的请求体（`Request` 存的是客户端发来的那一份）。**只有插件真的改了
    /// 才存**，挨着 `Request` 放。请求钩子每一跳跑一次，存的是最后发出去的那一跳收到的
    /// 那一份 —— 回答的那一家收到的就是它（客户端那种格式、转换之前，插件交回的占位符
    /// 已经换回原值，见 [`crate::plugin::request`]），带着那一跳的 [`Redaction`]：落盘前和
    /// 别的正文一样换掉、打码（[`BodyRecord::for_disk`]）
    AfterPlugins,
}

/// 落盘之前怎么处理一份正文。
///
/// 带着做这件事要的东西：这个请求开始时生效的那套规则（换了配置，已经在路上的照旧），
/// 和拦截档下这个请求的账本（见 [`crate::guard::look`]）。
#[derive(Clone)]
pub struct Redaction {
    pub rules: Arc<RuleSet>,
    /// 原值 → 发给上游的占位符。拦截档下才有东西
    pub ledger: Ledger,
}

/// **不打印账本**：里面是原值。
impl std::fmt::Debug for Redaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redaction")
            .field("replaced", &self.ledger.len())
            .finish_non_exhaustive()
    }
}

impl Default for Redaction {
    /// 没人交代的时候：出厂的那套规则，不换、只打码。**宁可多打。**
    fn default() -> Self {
        Self {
            rules: Arc::new(RuleSet::defaults()),
            ledger: Ledger::new(Scheme::SECRET),
        }
    }
}

impl Redaction {
    /// 换过、打过码的样子。
    ///
    /// 两道：脱敏规则认出的每一处，账本里有的换成那个占位符（上游收到的就是它），没有的
    /// 打码；然后整段再按形状打一遍（[`tw_secret::mask_body`]）兜住规则没认出来的。
    ///
    /// **读的时候还会再打一遍**，打第二遍不再改动什么（`mask_body` 认得自己打过的码）。
    pub fn apply(&self, text: &str) -> String {
        self.apply_found(text, None)
    }

    /// [`apply`](Self::apply)，`found` 是按这套规则在 `text` 上已经找过的命中（[`crate::guard::hits`]
    /// 的结果）：不再找一遍。没找过的是 `None`
    fn apply_found(&self, text: &str, found: Option<&[Hit]>) -> String {
        // 我们自己的占位符不算（见 `crate::guard::hits`）：回答里回显的、重放过来的，原样存
        let fresh;
        let hits = match found {
            Some(hits) => hits,
            None => {
                fresh = crate::guard::hits(text, &self.rules);
                &fresh[..]
            }
        };
        let sent: HashMap<&str, &str> = self.ledger.replacements().collect();
        // 摘要以外的各段照常（找过的命中照用）；摘要解开了重新找、换、打码，再包回去
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (span, summary) in carried_summaries(text) {
            out.push_str(&self.span(text, at..span.start, hits, &sent));
            out.push_str(&tw_dialect::compaction::carry(&self.apply(&summary)));
            at = span.end;
        }
        out.push_str(&self.span(text, at..text.len(), hits, &sent));
        out
    }

    /// `text` 里的一段：落在这一段里的命中换掉或打码，再按形状打一遍。各段在 token 的边界上
    /// 分开（见 [`carried_summaries`]），一段一段打和整段一起打是一样的
    fn span(
        &self,
        text: &str,
        range: Range<usize>,
        hits: &[Hit],
        sent: &HashMap<&str, &str>,
    ) -> String {
        let mut out = String::with_capacity(range.len());
        let mut at = range.start;
        for h in hits
            .iter()
            .filter(|h| h.bytes.start >= range.start && h.bytes.end <= range.end)
        {
            out.push_str(&text[at..h.bytes.start]);
            let value = &text[h.bytes.clone()];
            match sent.get(value) {
                Some(placeholder) => out.push_str(placeholder),
                None => out.push_str(&masked(&h.rule, value)),
            }
            at = h.bytes.end;
        }
        out.push_str(&text[at..range.end]);
        tw_secret::mask_body(&out)
    }
}

/// 正文里转换时压缩出的前文摘要：`tw1.c.` 开头、解得开的那串（[`tw_dialect::compaction`]），
/// 在正文里的位置和解开的摘要，按出现的顺序。
///
/// 一串按 [`tw_secret::mask_body`] 认 token 的办法划边界（字母、数字、`-`、`_`、`.`），
/// 所以把它挖出去之后，剩下的各段打码和整段打码一样。**解不开的（截断了的、坏了的）
/// 不算**：它照别的字一样打码 —— 一长串不透明的字符，按形状整串打掉
fn carried_summaries(text: &str) -> Vec<(Range<usize>, String)> {
    fn is_tok(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
    }
    let bytes = text.as_bytes();
    let prefix = tw_dialect::compaction::CARRIED_PREFIX.as_bytes();
    let mut out: Vec<(Range<usize>, String)> = Vec::new();
    for start in memchr::memmem::find_iter(bytes, prefix) {
        // 一串的开头，不是别的字中间
        if start > 0 && is_tok(bytes[start - 1]) {
            continue;
        }
        if out.last().is_some_and(|(r, _)| start < r.end) {
            continue;
        }
        let end = bytes[start..]
            .iter()
            .position(|&b| !is_tok(b))
            .map_or(bytes.len(), |n| start + n);
        if let Some(summary) = tw_dialect::compaction::read(&text[start..end]) {
            out.push((start..end, summary));
        }
    }
    out
}

/// 一个认出来、没换成占位符的值存成什么样。
///
/// 和安全日志里报的是同一种写法（[`tw_guard::redact::rules::masked`]），只有内网地址和
/// 内部域名不同：日志里它们原样报（打了码就说不出是哪台机器），正文里一样打掉 —— 打开
/// 这两条规则的人，就是不想让它们留在别处的人。
///
/// **反斜杠去掉**：一个值里带着转义时，打码可能正好留下半个，存下来的那份就不再是 JSON。
fn masked(rule: &Rule, value: &str) -> String {
    let m = match rule.kind() {
        Kind::Internal => tw_secret::mask_secret(value),
        _ => tw_guard::redact::rules::masked(rule, value),
    };
    m.replace('\\', "")
}

/// 一份要存起来的 body。
#[derive(Debug)]
pub struct BodyRecord {
    pub id: u64,
    pub at_ms: i64,
    pub kind: BodyKind,
    /// 原文。交进通道的最多 [`WINDOW`] 字节
    pub body: Bytes,
    /// 原始长度。**截断了要能说出来** —— 不说的话，用户会以为请求本身
    /// 就长这样
    pub original_len: usize,
    /// 落盘之前怎么换、怎么打码
    pub redaction: Redaction,
    /// 请求路上按 `redaction` 的规则在这份正文上已经找过的命中（见 [`Self::found`]）
    found: Option<Arc<[Hit]>>,
    /// 占着的那份额度（见 [`QUEUED_MAX`]）
    held: Option<Held>,
}

impl BodyRecord {
    pub fn new(
        id: u64,
        at_ms: i64,
        kind: BodyKind,
        body: Bytes,
        original_len: usize,
        redaction: Redaction,
    ) -> Self {
        Self {
            id,
            at_ms,
            kind,
            body,
            original_len,
            redaction,
            found: None,
            held: None,
        }
    }

    /// 请求路上出站脱敏已经按 `redaction` 的规则在 `body` 上找过一遍（见
    /// [`crate::guard::look_hits`]）：落盘前打码用它找到的，不再把整份正文扫一遍。
    ///
    /// 只在交来的是整份正文、它又是 UTF-8 时用得上 —— 那时落盘前看的文字就是请求路上看的
    /// 那一份；截过的、不是 UTF-8 的照旧再找（[`Self::for_disk`]）。
    pub fn found(mut self, hits: Option<Arc<[Hit]>>) -> Self {
        self.found = hits;
        self
    }

    /// 落盘的那一份：换过、打过码（[`Redaction::apply`]），和要记下的原本长度。
    ///
    /// **在阻塞线程上调**：一份 4 MB 的正文要扫好几遍。
    ///
    /// 不是 UTF-8 的照读的时候的办法转成文字（坏字节换成 U+FFFD）：读的人看到的本来就是
    /// 那样，而二进制的正文里没有能看的东西。
    pub fn for_disk(self) -> ForDisk {
        let window = &self.body[..self.body.len().min(WINDOW)];
        let text = String::from_utf8_lossy(window);
        // 请求路上找过的那一遍看的就是这些字：整份都在窗口里，原文就是 UTF-8
        let entire = window.len() == self.body.len() && self.original_len <= self.body.len();
        let found = self
            .found
            .filter(|_| entire && matches!(text, std::borrow::Cow::Borrowed(_)));
        let body = self.redaction.apply_found(&text, found.as_deref());
        // 截过的（交来的只是开头）报原本的长度。没截过的就是换过、打过码的这一份的长度：
        // 比存储层的上限还长的，由存储层截、由它记下（`tw_store::Blobs::put_with_len`）
        let whole = self.original_len.max(self.body.len());
        let original_len = if whole > window.len() {
            whole
        } else {
            body.len()
        };
        ForDisk {
            id: self.id,
            at_ms: self.at_ms,
            kind: self.kind,
            body: Bytes::from(body),
            original_len,
            held: self.held,
        }
    }
}

/// 落盘的那一份（[`BodyRecord::for_disk`]）。
#[derive(Debug)]
pub struct ForDisk {
    pub id: u64,
    pub at_ms: i64,
    pub kind: BodyKind,
    /// 换过、打过码的那一份。可能比 [`tw_api::BODY_MAX`] 长一点，由存储层截
    pub body: Bytes,
    /// 原始长度，交给 `tw_store::Blobs::put_with_len`
    pub original_len: usize,
    /// 还占着的额度。**拿着它，直到存储层收下这一份**：交接途中的那一份也是攒在内存里的
    pub held: Option<Held>,
}

/// 一份正文占着的额度（见 [`QUEUED_MAX`]）。**被丢掉时还回去**：写完了、通道满了、
/// 收的那一头不在了，都一样。
#[derive(Debug)]
pub struct Held {
    n: usize,
    queued: Arc<AtomicUsize>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.queued.fetch_sub(self.n, Ordering::Relaxed);
    }
}

/// 往哪儿交，和交出去还没写完的有多少字节。`None` 表示观测层没起来 —— 那时什么都不做，
/// 转发照旧。
#[derive(Debug, Clone)]
pub struct BodySink {
    tx: tokio::sync::mpsc::Sender<BodyRecord>,
    queued: Arc<AtomicUsize>,
}

/// 建一条通道：发的那一头给网关（`AppState::set_body_sink`），收的那一头交给落盘。
pub fn channel() -> (BodySink, tokio::sync::mpsc::Receiver<BodyRecord>) {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAP);
    let sink = BodySink {
        tx,
        queued: Arc::default(),
    };
    (sink, rx)
}

/// 通道最多攒几份。**按字节的上限在 [`QUEUED_MAX`]**，这个数管住的是一大堆很小的正文。
///
/// 攒不下就丢：攒着一堆等着写盘的 body 本身就说明磁盘跟不上，那时留着它们也没用。
pub const CHANNEL_CAP: usize = 64;

/// 交出去、还没写完的正文合计最多多少字节。超了就丢新来的。
///
/// **按字节记，不按份数。**一份正文最多 4 MB 出头，按份数定上限的话，64 份就是 256 MB
/// 攒在内存里等磁盘；而平时攒着的几乎都是几 KB 的小正文，把份数压小又会在忙的时候白白
/// 丢掉它们。单独一份总放得下：[`WINDOW`] 比它小得多。
pub const QUEUED_MAX: usize = 32 * 1024 * 1024;

/// 交一份出去。**满了就丢，绝不等待。**
///
/// 比 [`WINDOW`] 长的只交开头 —— **拷出来**，不切片：切片拽着整个请求体（最大 256 MB）
/// 一起等在通道里。
pub fn offer(sink: &Option<BodySink>, mut rec: BodyRecord) {
    let Some(s) = sink else { return };
    if rec.body.len() > WINDOW {
        rec.body = Bytes::copy_from_slice(&rec.body[..WINDOW]);
        // 找过的是整份；落盘前看的只是开头这一截，要重新找（见 `BodyRecord::found`）
        rec.found = None;
    }
    let n = rec.body.len();
    // 先占额度，占不下就丢
    if s.queued.fetch_add(n, Ordering::Relaxed) + n > QUEUED_MAX {
        s.queued.fetch_sub(n, Ordering::Relaxed);
        return;
    }
    rec.held = Some(Held {
        n,
        queued: s.queued.clone(),
    });
    // 交不出去的那一份（满了、收的那一头不在了）就地丢掉，额度跟着还回去。
    //
    // 不记日志：这条路上每个请求都会走一次，而写盘跟不上的时候
    // 日志会跟着刷屏 —— 那才是真的把事情变糟。
    let _ = s.tx.try_send(rec);
}

/// 一边流一边攒响应体，**攒到上限就停**。
#[derive(Debug, Default)]
pub struct ResponseTap {
    buf: Vec<u8>,
    total: usize,
}

impl ResponseTap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let room = RESPONSE_TAP_MAX.saturating_sub(self.buf.len());
        if room == 0 {
            return;
        }
        let take = chunk.len().min(room);
        // **按实际长度长，到上限为止。**交给 Vec 自己翻倍的话，攒到 4 MB 出头时一下就要
        // 8 MB
        let need = self.buf.len() + take;
        if need > self.buf.capacity() {
            let want = (self.buf.capacity() * 2).max(need).min(RESPONSE_TAP_MAX);
            self.buf.reserve_exact(want - self.buf.len());
        }
        self.buf.extend_from_slice(&chunk[..take]);
    }

    /// 攒到的那部分，以及**原始的总长度**。
    pub fn finish(self) -> (Bytes, usize) {
        (Bytes::from(self.buf), self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn a_short_response_is_kept_whole() {
        let mut t = ResponseTap::new();
        t.feed(b"hello ");
        t.feed(b"world");
        let (b, total) = t.finish();
        assert_eq!(&b[..], b"hello world");
        assert_eq!(total, 11);
    }

    #[test]
    fn a_long_response_is_capped_but_reports_its_real_length() {
        // **截断了要能说出来** —— 不说的话，用户会以为响应本身就这么长。
        let mut t = ResponseTap::new();
        let chunk = vec![b'x'; 100 * 1024];
        for _ in 0..50 {
            t.feed(&chunk);
        }
        assert!(t.buf.capacity() <= RESPONSE_TAP_MAX, "攒满时多占了内存");
        let (b, total) = t.finish();
        assert_eq!(b.len(), RESPONSE_TAP_MAX);
        assert_eq!(total, 50 * 100 * 1024, "原始长度没记住");
    }

    #[test]
    fn the_tap_keeps_as_much_as_the_store_does_and_a_little_more() {
        // 存下来的回答和请求一样长；多攒的那一截只给打码看
        assert_eq!(RESPONSE_TAP_MAX, tw_api::BODY_MAX + MARGIN);
        assert_eq!(tw_api::BODY_MAX, 4 * 1024 * 1024);
    }

    #[test]
    fn a_chunk_that_straddles_the_cap_is_partly_kept() {
        let mut t = ResponseTap::new();
        t.feed(&vec![b'a'; RESPONSE_TAP_MAX - 10]);
        t.feed(b"1234567890EXTRA");
        let (b, total) = t.finish();
        assert_eq!(b.len(), RESPONSE_TAP_MAX);
        assert_eq!(&b[b.len() - 10..], b"1234567890");
        assert_eq!(total, RESPONSE_TAP_MAX + 5);
    }

    fn record(kind: BodyKind, body: &str, redaction: Redaction) -> BodyRecord {
        BodyRecord::new(
            1,
            0,
            kind,
            Bytes::from(body.to_string()),
            body.len(),
            redaction,
        )
    }

    fn written(rec: BodyRecord) -> String {
        String::from_utf8(rec.for_disk().body.to_vec()).unwrap()
    }

    /// 拦截档下这个请求的账本：和 `start` 里一样，按客户端原文编号
    fn enforced(body: &str) -> Redaction {
        let rules = RuleSet::defaults();
        let (_, ledger) =
            crate::guard::look(tw_config::SecurityMode::Enforce, &rules, body.as_bytes());
        Redaction {
            rules: Arc::new(rules),
            ledger,
        }
    }

    #[test]
    fn under_enforce_the_stored_request_carries_what_the_upstream_got() {
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"我的 key 是 {KEY}，身份证 11010519491231002X"}}]}}"#
        );
        let r = enforced(&body);
        let (sent, _) = crate::guard::replace(
            tw_config::SecurityMode::Enforce,
            &r.rules,
            Bytes::from(body.clone()),
            &r.ledger,
        );
        let stored = written(record(BodyKind::Request, &body, r));
        assert!(!stored.contains(KEY), "{stored}");
        // 一个字节都不差：存下来的就是发给上游的那一份
        assert_eq!(stored.as_bytes(), &sent[..]);
        assert!(
            stored.contains("<<TW_SECRET_1>>，身份证 <<TW_ID_NUMBER_1>>"),
            "{stored}"
        );
        serde_json::from_str::<serde_json::Value>(&stored).expect("存下来的还是 JSON");
    }

    #[test]
    fn without_enforce_what_the_rules_recognize_is_masked_before_it_is_written() {
        // 观察档、关着的时候上游收到的是原值，存下来的打码 —— 和安全日志里一种写法
        let body = format!(
            r#"{{"messages":[{{"role":"user","content":"key {KEY} 库 postgres://app:hunter2hunter2@db/x 卡 6222 0212 3456 7894"}}]}}"#
        );
        let stored = written(record(BodyKind::Request, &body, Redaction::default()));
        for secret in [KEY, "hunter2hunter2", "6222 0212 3456 7894"] {
            assert!(!stored.contains(secret), "{secret} 原样进了磁盘：{stored}");
        }
        assert!(stored.contains("key sk-an…AAAA"), "{stored}");
        assert!(
            stored.contains("postgres://app:hunte…ter2@db/x"),
            "{stored}"
        );
        assert!(stored.contains("卡 …7894"), "{stored}");
        assert!(
            !stored.contains("<<TW_"),
            "没换的地方不该出现占位符：{stored}"
        );
        serde_json::from_str::<serde_json::Value>(&stored).expect("存下来的还是 JSON");
        // 读的时候再打一遍，什么都不变
        assert_eq!(tw_secret::mask_body(&stored), stored);
    }

    /// 插件改过的请求体走同一条路：插件交回的占位符原样留着（上游收到的就是它），插件
    /// 自己写进去的、认得出的值打码
    #[test]
    fn the_request_after_plugins_is_stored_the_same_way() {
        let request = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let r = enforced(&request);
        let added = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let after = format!(
            r#"{{"system":"today is Friday, {added}","messages":[{{"role":"user","content":"<<TW_SECRET_1>>"}}]}}"#
        );
        let stored = written(record(BodyKind::AfterPlugins, &after, r));
        assert!(
            stored.contains("\"content\":\"<<TW_SECRET_1>>\""),
            "{stored}"
        );
        assert!(stored.contains("today is Friday"), "{stored}");
        assert!(!stored.contains(added), "{stored}");
        serde_json::from_str::<serde_json::Value>(&stored).expect("存下来的还是 JSON");
    }

    /// 请求路上找过的命中拿来打码，落盘的和自己再找一遍的一字不差；交来的只是开头一截时
    /// 不用它（它说的是整份），照旧自己找
    #[test]
    fn the_hits_found_on_the_way_in_are_used_only_for_the_whole_body() {
        let body = format!(
            r#"{{"messages":[{{"role":"user","content":"key {KEY} 库 postgres://app:hunter2hunter2@db/x"}}]}}"#
        );
        for mode in [
            tw_config::SecurityMode::Observe,
            tw_config::SecurityMode::Enforce,
        ] {
            let rules = RuleSet::defaults();
            let seen = crate::guard::look_hits(mode, &rules, body.as_bytes());
            assert_eq!(seen.hits.as_ref().map(Vec::len), Some(2));
            let r = Redaction {
                rules: Arc::new(rules),
                ledger: seen.ledger,
            };
            let again = written(record(BodyKind::Request, &body, r.clone()));
            let reused =
                written(record(BodyKind::Request, &body, r).found(seen.hits.map(Arc::from)));
            assert_eq!(reused, again);
            assert!(!reused.contains(KEY), "{reused}");
        }
        // 说「什么都没找到」的命中：整份交来时照它（证明真的用上了），截过的不照它
        let lie = || Some(Arc::from(Vec::new()));
        let whole = written(record(BodyKind::Request, &body, Redaction::default()).found(lie()));
        assert!(whole.contains("sk-an…AAAA"), "{whole}");
        assert!(
            whole.contains("hunter2hunter2"),
            "规则那一道真的没再找：{whole}"
        );
        let mut cut = record(BodyKind::Request, &body, Redaction::default()).found(lie());
        cut.original_len = body.len() + 1;
        let cut = written(cut);
        assert!(!cut.contains("hunter2hunter2"), "{cut}");
    }

    const AWS: &str = "AKIAIOSFODNN7EXAMPLE";

    /// 摘要里提到过的密钥：一个规则认得出（Anthropic 的钥匙），一个只认得出形状（AWS 的
    /// 访问密钥 ID）
    fn summary() -> String {
        format!("Set ANTHROPIC_API_KEY={KEY} and aws key {AWS} in .env; tests pass.")
    }

    /// 存下来的那份里每一个转换时写的摘要，解开
    fn summaries_in(stored: &str) -> Vec<String> {
        carried_summaries(stored)
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    /// 转换时压缩出的摘要（`tw1.c.` + base64）里的密钥：解开、照样打码、包回原位。Codex
    /// 每一轮的请求都把它带回来；直通 OpenAI 时上游的回答里没有它，转换时客户端收到的那份
    /// 不落盘 —— 但正文里凡是出现了，一样处理
    #[test]
    fn a_secret_inside_a_carried_summary_is_masked_before_it_is_written() {
        let item = tw_dialect::compaction::carry(&summary());
        let request = format!(
            r#"{{"model":"gpt-5.4","input":[{{"type":"message","role":"user","content":"fix it"}},{{"type":"compaction","encrypted_content":"{item}"}},{{"type":"message","role":"user","content":"next {KEY}"}}]}}"#
        );
        let streamed = format!(
            "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"compaction\",\"encrypted_content\":\"{item}\"}}}}\n\n"
        );
        let whole =
            format!(r#"{{"output":[{{"type":"compaction","encrypted_content":"{item}"}}]}}"#);
        for (kind, body) in [
            (BodyKind::Request, &request),
            (BodyKind::Response, &streamed),
            (BodyKind::Response, &whole),
        ] {
            let stored = written(record(kind, body, Redaction::default()));
            let inside = summaries_in(&stored);
            assert_eq!(inside.len(), 1, "{stored}");
            let s = &inside[0];
            for secret in [KEY, AWS] {
                assert!(!s.contains(secret), "{secret} 原样进了磁盘：{s}");
                assert!(!stored.contains(secret), "{stored}");
            }
            assert!(s.contains("ANTHROPIC_API_KEY=sk-an…AAAA"), "{s}");
            assert!(s.contains("tests pass."), "摘要的其余部分要留着：{s}");
            // 摘要外面的照常打码
            if kind == BodyKind::Request {
                assert!(stored.contains("next sk-an…AAAA"), "{stored}");
                serde_json::from_str::<serde_json::Value>(&stored).expect("存下来的还是 JSON");
            }
        }
    }

    /// 请求路上找过的命中照用（找过的是摘要外面的那些），摘要里面另外找；拦截档下摘要里
    /// 的值换成发给上游的那个占位符
    #[test]
    fn the_hits_found_on_the_way_in_still_cover_the_rest_and_the_ledger_reaches_inside() {
        let item = tw_dialect::compaction::carry(&summary());
        let body = format!(
            r#"{{"input":[{{"type":"compaction","encrypted_content":"{item}"}},{{"role":"user","content":"key {KEY} 库 postgres://app:hunter2hunter2@db/x"}}]}}"#
        );
        for mode in [
            tw_config::SecurityMode::Observe,
            tw_config::SecurityMode::Enforce,
        ] {
            let rules = RuleSet::defaults();
            let seen = crate::guard::look_hits(mode, &rules, body.as_bytes());
            let r = Redaction {
                rules: Arc::new(rules),
                ledger: seen.ledger,
            };
            let again = written(record(BodyKind::Request, &body, r.clone()));
            let reused =
                written(record(BodyKind::Request, &body, r).found(seen.hits.map(Arc::from)));
            assert_eq!(reused, again, "{mode:?}");
            assert!(!reused.contains("hunter2hunter2"), "{reused}");
            let inside = summaries_in(&reused);
            assert!(!inside[0].contains(KEY), "{mode:?}: {inside:?}");
            if mode == tw_config::SecurityMode::Enforce {
                // 上游收到的是占位符，摘要里存的也是它
                assert!(inside[0].contains("<<TW_SECRET_1>>"), "{inside:?}");
            }
        }
    }

    #[test]
    fn a_damaged_carried_summary_is_masked_as_text_and_never_panics() {
        for bad in [
            // 一个字符凑不出一个字节
            "tw1.c.a".to_string(),
            // 不是 base64url
            "tw1.c.!!!!".to_string(),
            // 截断在半个 UTF-8 字符上
            format!(
                "tw1.c.{}",
                &tw_dialect::compaction::carry("密钥")["tw1.c.".len()..][..3]
            ),
            // 一长串解不开的（base64 的标准字母表）
            format!("tw1.c.{}+/=", "QUtJQUlPU0ZPRE5ON0VYQU1QTEU".repeat(3)),
            // 前缀之后什么都没有
            "tw1.c.".to_string(),
        ] {
            let body =
                format!(r#"{{"input":[{{"type":"compaction","encrypted_content":"{bad}"}}]}}"#);
            let stored = written(record(BodyKind::Request, &body, Redaction::default()));
            // 和没有这回事时一样：按形状打码（只剩前缀的那个解出来是空摘要，原样）
            assert_eq!(stored, tw_secret::mask_body(&body), "{bad}");
            assert!(summaries_in(&stored).iter().all(String::is_empty), "{bad}");
        }
        // 正文被截断在一个摘要中间（存下来的只有开头 4 MB）：解得开的那半截照样在里面打码，
        // 解不开的整串打掉。哪一种都不会留下原值
        let item = tw_dialect::compaction::carry(&summary());
        for drop in 1..=8 {
            let cut = &item[..item.len() - drop];
            let body = format!(r#"{{"encrypted_content":"{cut}"#);
            let stored = written(record(BodyKind::Request, &body, Redaction::default()));
            let inside = summaries_in(&stored);
            for s in &inside {
                assert!(!s.contains(KEY) && !s.contains(AWS), "{drop}: {s}");
            }
            if inside.is_empty() {
                assert!(!stored.contains(&cut[10..40]), "{drop}: {stored}");
            }
        }
    }

    #[test]
    fn what_the_rules_miss_is_masked_by_its_shape() {
        // 网关自己的钥匙（`tw-`）没有一条脱敏规则认：读的时候那一道兜住它，写的时候也一样
        let body =
            r#"{"messages":[{"role":"user","content":"钥匙 tw-0123456789abcdef0123456789"}]}"#;
        let stored = written(record(BodyKind::Request, body, Redaction::default()));
        assert!(!stored.contains("0123456789abcdef"), "{stored}");
    }

    #[test]
    fn a_response_keeps_its_placeholders_and_loses_any_real_value() {
        // 拦截档：上游回显的是占位符，原样存。它另外说出来的一把密钥（不在账本里）打码
        let request = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let r = enforced(&request);
        let other = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let answer = format!(
            "event: content_block_delta\ndata: {{\"delta\":{{\"text\":\"你给的是 <<TW_SECRET_1>>，另一把是 {other}\"}}}}\n\n"
        );
        let stored = written(record(BodyKind::Response, &answer, r.clone()));
        assert!(stored.contains("你给的是 <<TW_SECRET_1>>"), "{stored}");
        assert!(!stored.contains(other), "{stored}");
        assert!(stored.contains("另一把是 ghp_B…BBBB"), "{stored}");

        // 观察档：上游看见过原值，回显出来的也是原值
        let echoed = format!("{{\"text\":\"你给的是 {KEY}\"}}");
        let stored = written(record(BodyKind::Response, &echoed, Redaction::default()));
        assert!(!stored.contains(KEY), "{stored}");
    }

    #[test]
    fn a_secret_across_the_cut_is_masked_whole_before_the_cut() {
        // 一把私钥正好跨在 4 MB 处：只看截下来的开头认不出它（要见到 END 才算）
        let pem = "-----BEGIN PRIVATE KEY-----\\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7\\n-----END PRIVATE KEY-----";
        let lead = tw_api::BODY_MAX - 40;
        let body = format!(
            r#"{{"content":"{}{pem}"}}"#,
            "x".repeat(lead - r#"{"content":""#.len())
        );
        let disk = record(BodyKind::Request, &body, Redaction::default()).for_disk();
        let text = String::from_utf8(disk.body.to_vec()).unwrap();
        assert!(!text.contains("MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7"));
        assert!(
            !text.contains("-----BEGIN PRIVATE KEY-----\\nMIIE"),
            "{}",
            &text[lead - 10..]
        );
        // 存储层截到 4 MB 时只剩打过码的那一份的开头
        assert!(disk.body.len() < tw_api::BODY_MAX + 100);
    }

    #[test]
    fn the_length_written_down_says_whether_it_was_cut() {
        // 没截过的：换过、打过码的这一份多长就是多长，存储层不会当它截过
        let body = format!(r#"{{"content":"{KEY}"}}"#);
        let disk = record(BodyKind::Request, &body, Redaction::default()).for_disk();
        assert_eq!(disk.original_len, disk.body.len());
        assert!(disk.original_len < body.len(), "打码之后变短了");

        // 交来的只是开头（响应体攒到了上限）：报原本的长度
        let mut r = record(BodyKind::Response, "data: {}\n\n", Redaction::default());
        r.original_len = 9 * 1024 * 1024;
        assert_eq!(r.for_disk().original_len, 9 * 1024 * 1024);

        // 整份交来、却比窗口长：只看窗口里的，报整份的长度
        let huge = "x".repeat(WINDOW + 10);
        let disk = record(BodyKind::Request, &huge, Redaction::default()).for_disk();
        assert_eq!(disk.body.len(), WINDOW);
        assert_eq!(disk.original_len, WINDOW + 10);
    }

    #[test]
    fn a_body_that_is_not_text_is_stored_the_way_it_reads() {
        let raw = Bytes::from(vec![0xff, b'a', 0xfe]);
        let disk =
            BodyRecord::new(1, 0, BodyKind::Response, raw, 3, Redaction::default()).for_disk();
        assert_eq!(&disk.body[..], "\u{fffd}a\u{fffd}".as_bytes());
    }

    #[test]
    fn the_ledger_never_shows_up_in_a_debug_print() {
        let r = enforced(&format!("{{\"k\":\"{KEY}\"}}"));
        let dump = format!("{r:?}");
        assert!(!dump.contains("AAAAAAAAAAAA"), "{dump}");
    }

    #[tokio::test]
    async fn offering_into_a_full_channel_drops_rather_than_waits() {
        // **等它是在惩罚真实用户。**观测永远不该有让请求变慢的权力。
        let (sink, _rx) = channel();
        let tx = Some(sink.clone());
        let rec = || {
            BodyRecord::new(
                1,
                0,
                BodyKind::Request,
                Bytes::from_static(b"x"),
                1,
                Redaction::default(),
            )
        };
        // 塞的比通道的份数多，不该挂住
        let start = std::time::Instant::now();
        for _ in 0..CHANNEL_CAP * 2 {
            offer(&tx, rec());
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "offer 挂住了"
        );
        // 丢掉的那些占的额度还回去了：攒着的只有收下的那些
        assert_eq!(sink.queued.load(Ordering::Relaxed), CHANNEL_CAP);
    }

    #[tokio::test]
    async fn what_waits_to_be_written_is_capped_in_bytes_and_given_back_once_written() {
        let (tx, mut rx) = channel();
        let sink = Some(tx.clone());
        let big = || {
            BodyRecord::new(
                1,
                0,
                BodyKind::Response,
                Bytes::from(vec![b'x'; WINDOW + 1000]),
                WINDOW + 1000,
                Redaction::default(),
            )
        };
        for _ in 0..20 {
            offer(&sink, big());
        }
        // 交进去的每一份都只带窗口那么长，合计不过上限
        let fits = QUEUED_MAX / WINDOW;
        let mut got = Vec::new();
        while let Ok(r) = rx.try_recv() {
            assert_eq!(r.body.len(), WINDOW);
            got.push(r);
        }
        assert_eq!(got.len(), fits);
        assert_eq!(tx.queued.load(Ordering::Relaxed), fits * WINDOW);
        // 写完一份（落盘的那一份被丢掉）就还一份
        let disk = got.pop().unwrap().for_disk();
        assert_eq!(tx.queued.load(Ordering::Relaxed), fits * WINDOW);
        drop(disk);
        assert_eq!(tx.queued.load(Ordering::Relaxed), (fits - 1) * WINDOW);
        drop(got);
        assert_eq!(tx.queued.load(Ordering::Relaxed), 0);
        // 腾出地方就又收了
        offer(&sink, big());
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn offering_when_there_is_no_sink_is_a_no_op() {
        // 观测层没起来时，转发照旧。
        offer(
            &None,
            BodyRecord::new(
                1,
                0,
                BodyKind::Request,
                Bytes::new(),
                0,
                Redaction::default(),
            ),
        );
    }
}

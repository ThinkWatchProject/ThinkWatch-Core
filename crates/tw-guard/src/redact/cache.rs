//! 历史消息的扫描结果复用。
//!
//! 客户端每一轮都把整段对话历史发上来：一段 200 轮的会话，第 200 轮的请求里 99% 的文字前
//! 199 轮已经扫过。这里按「一段原文 + 规则集」记下扫出来的命中，下一轮没变的那几段直接取，
//! 只扫新增的。
//!
//! # 以什么为单位
//!
//! **不是一个个正文字符串**：好几种判据要看字符串外面 —— 配置赋值、按字段名认的值看的是
//! 键（`"password": "…"` 的键和值是两个字符串），保存的凭据、口令、助记词的边界判断要往前
//! 看几个字节。一个字符串单拿出来扫，和它在请求体里时扫出来的不一样。
//!
//! 单位是请求体顶层的**一项**（[`split`]）：
//!
//! - 值是一个全是对象的数组（`messages`、`input`、`contents`、`tools`、数组写的 `system`）
//!   的，每个元素一段 —— 一条消息、一个条目、一个工具定义，**原文那一截本身就是一个合规的
//!   JSON**，单拿出来照样按 JSON 扫；
//! - 别的顶层项（`model`、字符串写的 `system`、`instructions`……）挨着的几项合成一段，扫的
//!   时候外面套一对花括号（`{"model":"…","system":"…"}`），键还在。顶层相邻的两项有的判据是
//!   连起来看的（`"lat":"…","lng":"…"` 是一对坐标），不拆开。
//!
//! 一条消息里的键和值、字段和它旁边的字段都在同一段里。各段之间隔着的是 JSON 的结构
//! （`},{`、`],"tools":[`），**判据在这里都断开**：跨 token 接起来认的（分组写的卡号只认
//! 单个空格、助记词只认空白和逗号、IBAN、MAC、坐标）遇到引号、括号就断；私钥块、自定义规则
//! 只在一个字符串里找；往前看几个字节的（转义的尾巴、`$` `{` 开头的变量名）看到的都是结构上
//! 的字；往后找值的（请求头、配置赋值）过不了 `}`。所以一段单拿出来扫的结果，平移到它在原文
//! 里的位置，就是整段请求体扫出来落在它里面的那些。
//!
//! **只有一种跨得过去**：命令行（`curl -u`、`mysql -p`）一直认到命令分隔或者换行为止，中间
//! 隔着字符串也接着认（`["curl","-u","用户:口令"]` 是数组里的几项）。一段末尾一条没结束的命令，
//! 在整段请求体里会把下一段开头、头一个换行之前的选项也认成它的。所以每段记着自己末尾有没有
//! 这样一条命令；有的话看一眼它在后面认到哪儿，那一截里有选项的开头就整段扫。
//!
//! # 不缓存的情形
//!
//! 有几条判据**看得到隔壁的消息**，开着其中任何一条就整个请求照旧整段扫（[`CONTEXTUAL`]）：
//!
//! - 要前文写明是什么的号码（护照、通行证、澳门身份证、日本个人番号、IMEI，和公网 IP
//!   的「版本号」排除）往前看 20 个字符：一条消息开头的号码，看得到上一条消息末尾的「护照」；
//! - 按字段名认的姓名、住址……：裸的 `name` 要同一个对象里、或者前后两行以内有别的个人信息
//!   字段才算，不是 JSON 的那一种跨得出一段。
//!
//! 这几条出厂都关着。另外，保存的凭据里有一项**本身带引号**的话也不缓存：按值找的时候它能
//! 跨过一个字符串的结尾，从一段连到另一段。
//!
//! 请求体不是顶层为对象的合规 JSON、嵌套太深、有不认得的写法的，一律整段扫（[`split`]）。
//! 拿不准就整段扫：结果和不缓存时**一模一样**，是这一层唯一的要求（对照测试拿多轮对话两条路
//! 各走一遍比）。
//!
//! # 键、存什么、存多少
//!
//! 键是 SHA-256（规则集的指纹 + 今天的日期 + 这一段的原文）。指纹见
//! [`RuleSet::fingerprint`]：改规则、改保存的密钥即整体失效。日期也算进去：身份证号、卡号
//! 的出生日期、有效期按「今天」核对，跨了一天同一段文字的结论可能不同。
//!
//! 存的只有命中的区间和规则 id，**不存值**：要换的时候从原文现取。
//!
//! 最近用过的 [`CAPACITY`] 段，LRU，只在内存里，跟着网关的运行时一起建、一起换（配置一重载
//! 就是一份新的）。太短的段（[`MIN_BYTES`] 以下）扫一遍比算哈希还快，不存；太长的
//! （[`MAX_BYTES`] 以上）照扫不存，免得一段就挤掉几百条。命中数也有总量上限（[`HITS_BUDGET`]）：
//! 一份贴进来的凭据清单可能一段就是几万处。
//!
//! 命中了多少、存了多少不进日志、不进界面：这是内部机制，用户不需要知道。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::rules::{Hit, RuleSet};

/// 最多记多少段
pub const CAPACITY: usize = 2_000;
/// 短于这么多字节的段不存：扫一遍不比算一次哈希、查一次表慢
pub const MIN_BYTES: usize = 64;
/// 长于这么多字节的段照扫、不存
pub const MAX_BYTES: usize = 1 << 20;
/// 一段的命中多于这么多处的不存
const ENTRY_HITS_MAX: usize = 4_096;
/// 所有段加起来最多存多少处命中。一处几十个字节，合起来几 MB
pub const HITS_BUDGET: usize = 65_536;
/// 请求体短于这么多字节的不走缓存：切段、算哈希的功夫比整段扫一遍还多
const BODY_MIN: usize = 1_024;
/// 嵌套深于这么多层的不切。serde_json 解析有层数上限（128）：一段单拿出来比在请求体里
/// 浅两层，深到边上的话，请求体解不开、单拿出来却解得开，两边就不再都按 JSON 扫了
const DEPTH_MAX: usize = 100;

/// 看得到隔壁那一段的规则（见模块说明）。开着任何一条，整个请求照旧整段扫
pub const CONTEXTUAL: &[&str] = &[
    "cn-passport",
    "mainland-travel-permit",
    "hk-macau-taiwan-id",
    "jp-my-number",
    "imei",
    "public-ip",
    "labeled-name",
    "labeled-address",
    "labeled-birth-date",
    "labeled-account",
    "labeled-health",
];

/// 扫描结果的缓存。一个网关运行时一份，并发的请求共用（内部加锁，锁里只查表、不扫）。
pub struct ScanCache {
    lru: Mutex<Lru>,
    /// 该存的段没存着、扫了一遍的次数（测试看缓存是不是真的用上了）
    #[cfg(test)]
    misses: std::sync::atomic::AtomicUsize,
}

impl Default for ScanCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 里面存的是区间和规则 id，打出来也没什么可看的
impl std::fmt::Debug for ScanCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanCache").finish_non_exhaustive()
    }
}

type Key = [u8; 32];

/// 一段扫出来的（[`Scanned`]）和它最近一次用到的时刻
struct Entry {
    scanned: Scanned,
    tick: u64,
}

/// 一段单拿出来扫的结果
#[derive(Clone)]
struct Scanned {
    /// 命中，区间是这一段单拿出来扫时的位置（套了花括号的从那个 `{` 算起）
    hits: Arc<[Hit]>,
    /// 这一段里有一条命令到结尾都没结束（[`super::secrets::place::command_open_at_end`]）：
    /// 整段请求体里它会接着认后面的选项，要看一眼后面
    open: bool,
}

#[derive(Default)]
struct Lru {
    map: HashMap<Key, Entry>,
    /// 最近用到的先后：tick → 键。最小的那个最久没用
    order: BTreeMap<u64, Key>,
    tick: u64,
    /// 存着的命中一共多少处
    hits: usize,
}

impl Lru {
    fn get(&mut self, k: &Key) -> Option<Scanned> {
        let e = self.map.get_mut(k)?;
        self.order.remove(&e.tick);
        self.tick += 1;
        e.tick = self.tick;
        self.order.insert(e.tick, *k);
        Some(e.scanned.clone())
    }

    fn put(&mut self, k: Key, scanned: Scanned) {
        if scanned.hits.len() > ENTRY_HITS_MAX {
            return;
        }
        if let Some(old) = self.map.remove(&k) {
            self.order.remove(&old.tick);
            self.hits -= old.scanned.hits.len();
        }
        self.tick += 1;
        self.hits += scanned.hits.len();
        self.order.insert(self.tick, k);
        self.map.insert(
            k,
            Entry {
                scanned,
                tick: self.tick,
            },
        );
        while self.map.len() > CAPACITY || self.hits > HITS_BUDGET {
            let Some((_, old)) = self.order.pop_first() else {
                break;
            };
            if let Some(e) = self.map.remove(&old) {
                self.hits -= e.scanned.hits.len();
            }
        }
    }
}

impl ScanCache {
    pub fn new() -> Self {
        Self {
            lru: Mutex::new(Lru::default()),
            #[cfg(test)]
            misses: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 锁坏了（另一个线程拿着锁 panic 了）照用：表里只有完整写进去的条目
    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.lru.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 存着几段
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// 到现在为止扫了几段该存的
    #[cfg(test)]
    pub(crate) fn misses(&self) -> usize {
        self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 存着的命中一共几处
    #[cfg(test)]
    pub(crate) fn hits_stored(&self) -> usize {
        self.lock().hits
    }

    /// 和 [`super::rules::scan_unresolved`] 一样（每条规则各自的命中，还没合并），**结果
    /// 一模一样**，只是没变的段取上次扫的。顺序可以不同：之后都要排序、合并
    pub(crate) fn scan_unresolved(&self, text: &str, rules: &RuleSet) -> Vec<Hit> {
        let whole = || super::rules::scan_unresolved(text, rules);
        if rules.is_empty() {
            return Vec::new();
        }
        if text.len() < BODY_MIN || !cacheable(rules) {
            return whole();
        }
        let Some(pieces) = split(text) else {
            return whole();
        };
        let fp = rules.fingerprint();
        let day = super::rules::today_ymd();
        let keys: Vec<Option<Key>> = pieces
            .iter()
            .map(|p| {
                (MIN_BYTES..=MAX_BYTES)
                    .contains(&p.range.len())
                    .then(|| key(&fp, day, p, text))
            })
            .collect();
        // 锁里只查表；没存着的在锁外面扫，扫完再一起存进去
        let mut got: Vec<Option<Scanned>> = {
            let mut lru = self.lock();
            keys.iter()
                .map(|k| k.as_ref().and_then(|k| lru.get(k)))
                .collect()
        };
        let cmdline = rules.is_on("command-line-password");
        let mut fresh: Vec<(Key, Scanned)> = Vec::new();
        for (i, p) in pieces.iter().enumerate() {
            if got[i].is_some() {
                continue;
            }
            let piece = p.text(text);
            // 整段是 JSON、单拿出来却不是的话（这一段本身就坏了），两边扫法不一样
            if !super::rules::json_like(&piece) {
                return whole();
            }
            let scanned = Scanned {
                hits: super::rules::scan_unresolved(&piece, rules).into(),
                open: cmdline && super::secrets::place::command_open_at_end(&text[p.range.clone()]),
            };
            if let Some(k) = keys[i] {
                #[cfg(test)]
                self.misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                fresh.push((k, scanned.clone()));
            }
            got[i] = Some(scanned);
        }
        if !fresh.is_empty() {
            let mut lru = self.lock();
            for (k, scanned) in fresh {
                lru.put(k, scanned);
            }
        }
        let mut out = Vec::new();
        // 没结束的命令往后看到了哪儿：后一段的命令结束在这之前的话，那一截已经看过了
        let mut looked = 0;
        for (p, scanned) in pieces.iter().zip(got) {
            let Some(Scanned { hits, open }) = scanned else {
                return whole();
            };
            // 一条命令跨出了这一段：整段扫的时候，它一直认到后面第一个命令分隔为止。那一截里
            // 有选项的开头的话，后面那一段单拿出来扫的结果不作数
            if open && p.range.end >= looked {
                let (option, end) =
                    super::secrets::place::option_before_command_end(text.as_bytes(), p.range.end);
                if option {
                    return whole();
                }
                looked = end;
            }
            let len = p.text_len();
            for h in hits.iter() {
                // 套上去的那对花括号不是原文：落在它上面的命中（不该有）对不回原文，整段扫
                if p.wrapped && (h.bytes.start == 0 || h.bytes.end >= len) {
                    return whole();
                }
                out.push(Hit {
                    bytes: p.to_body(h.bytes.start)..p.to_body(h.bytes.end),
                    rule: h.rule.clone(),
                    label: h.label.clone(),
                });
            }
        }
        out
    }
}

/// 这套规则下能不能按段缓存（见模块说明的「不缓存的情形」）
pub(crate) fn cacheable(rules: &RuleSet) -> bool {
    !CONTEXTUAL.iter().any(|id| rules.is_on(id))
        && !rules
            .saved()
            .is_some_and(|s| s.values().any(|v| v.contains('"')))
}

/// 一段的键：指纹、日期、套没套花括号、原文
fn key(fp: &[u8; 32], day: u32, p: &Piece, text: &str) -> Key {
    let mut h = Sha256::new();
    h.update(fp);
    h.update(day.to_le_bytes());
    h.update([u8::from(p.wrapped)]);
    h.update(&text.as_bytes()[p.range.clone()]);
    h.finalize().into()
}

/// 请求体里的一段（见模块说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    /// 在原文里的字节区间
    pub range: Range<usize>,
    /// 是顶层的一项（`"key": value`），单拿出来扫的时候外面套一对花括号；不是的话是数组里的
    /// 一个对象，原样扫
    pub wrapped: bool,
}

impl Piece {
    /// 单拿出来扫的那段文字
    fn text<'a>(&self, body: &'a str) -> Cow<'a, str> {
        let raw = &body[self.range.clone()];
        if self.wrapped {
            let mut s = String::with_capacity(raw.len() + 2);
            s.push('{');
            s.push_str(raw);
            s.push('}');
            Cow::Owned(s)
        } else {
            Cow::Borrowed(raw)
        }
    }

    fn text_len(&self) -> usize {
        self.range.len() + if self.wrapped { 2 } else { 0 }
    }

    /// 单拿出来扫时的位置 → 原文里的位置
    fn to_body(&self, at: usize) -> usize {
        self.range.start + at - usize::from(self.wrapped)
    }
}

/// JSON 的空白（只有这四个：别的空白字符在 JSON 里不合规）
fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

/// `b[i]` 是一个字符串开头的引号：返回结尾那个引号后面的位置。没结尾是 `None`
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        let k = memchr::memchr2(b'"', b'\\', b.get(j..)?)?;
        if b[j + k] == b'\\' {
            j += k + 2;
        } else {
            return Some(j + k + 1);
        }
    }
}

/// 从 `i` 起的一个值到哪儿为止，和它里面最深有几层（数和字面量是 0 层）。只认结构：括号
/// 配不配对、数写得对不对由单拿出来那一段的 JSON 校验管（[`super::rules::json_like`]）
fn value_end(b: &[u8], i: usize) -> Option<(usize, usize)> {
    match *b.get(i)? {
        b'"' => Some((string_end(b, i)?, 0)),
        b'{' | b'[' => {
            let (mut depth, mut deepest) = (0usize, 0usize);
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        deepest = deepest.max(depth);
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((j + 1, deepest));
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !is_ws(b[j]) && !matches!(b[j], b',' | b'}' | b']') {
                j += 1;
            }
            (j > i).then_some((j, 0))
        }
    }
}

/// 一个键能不能不经校验就算合规：没有转义、没有控制字符。按段扫时被拆开的数组的键不在
/// 任何一段里，没人替它校验
fn plain_key(k: &[u8]) -> bool {
    !k.iter().any(|&c| c == b'\\' || c < 0x20)
}

/// 把请求体切成段（见模块说明）。**整个请求体是一个顶层为对象的 JSON**，段与段之间只有
/// 结构上的字和空白，键都不重复；任何一样对不上就是 `None`，整段扫。
///
/// 各段单拿出来都是 JSON（[`super::rules::json_like`]，扫的时候核对），请求体就是 JSON：
/// 段外面的结构这里核对过了。反过来也是：请求体是 JSON 的话，它的每一项、数组的每个元素都是
pub(crate) fn split(text: &str) -> Option<Vec<Piece>> {
    let b = text.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(b, i + 1);
    let mut out = Vec::new();
    let mut keys: Vec<&[u8]> = Vec::new();
    // 正在接的几个挨着的、不拆开的顶层项
    let mut group: Option<Range<usize>> = None;
    if b.get(i) == Some(&b'}') {
        return None;
    }
    loop {
        // "key"
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let member = i;
        let key_end = string_end(b, i)?;
        let key = &b[i + 1..key_end - 1];
        // 重复的键：解析时后一个盖住前一个，哪些字符串算正文跟着变，按段扫说不清
        if keys.contains(&key) {
            return None;
        }
        keys.push(key);
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value = i;
        let (end, depth) = value_end(b, value)?;
        if depth + 1 > DEPTH_MAX {
            return None;
        }
        match elements(b, value, end) {
            Some(items) if plain_key(key) => {
                out.extend(group.take().map(|range| Piece {
                    range,
                    wrapped: true,
                }));
                for r in items {
                    out.push(Piece {
                        range: r,
                        wrapped: false,
                    });
                }
            }
            // 挨着的几项合成一段：顶层相邻的两项（`"lat":"…","lng":"…"`、`"a":["…Authorization"],
            // "x-api-key":"…"`）有的判据是连起来看的
            _ => {
                let start = group.take().map_or(member, |g| g.start);
                group = Some(start..end);
            }
        }
        i = skip_ws(b, end);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => break,
            _ => return None,
        }
    }
    out.extend(group.map(|range| Piece {
        range,
        wrapped: true,
    }));
    (skip_ws(b, i + 1) == b.len()).then_some(out)
}

/// `b[from..to]` 是一个**全是对象**、至少一个元素的数组的话，每个元素的区间
fn elements(b: &[u8], from: usize, to: usize) -> Option<Vec<Range<usize>>> {
    if b[from] != b'[' {
        return None;
    }
    let mut i = skip_ws(b, from + 1);
    let mut out = Vec::new();
    loop {
        if b.get(i) != Some(&b'{') {
            return None;
        }
        let (end, _) = value_end(b, i)?;
        out.push(i..end);
        i = skip_ws(b, end);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b']') if i + 1 == to => return Some(out),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests;

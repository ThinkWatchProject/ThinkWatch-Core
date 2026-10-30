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
//!   的，挨着的元素攒够 [`RUN_BYTES`] 算一段（一条长消息自己就够）—— 原文那一截外面套一对
//!   方括号就是一个合规的 JSON，单拿出来照样按 JSON 扫。从数组开头往后攒：历史只在末尾加长
//!   时，前面攒好的几段一字不变。不一条条拆：几万条短消息（Codex 的长会话）一条条单独解析、
//!   扫、查表，比整段扫一遍慢十几倍，也远远超出能存的条数；
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
//! # 存的是去掉不算的之后的命中
//!
//! 找完之后那几步 —— 去掉压在占位符上的、只留正文里的、去掉键和 base64 载荷上的
//! （[`super::flow::kept`]）—— 每一步只看命中落在哪儿，一段一段地做和整段一起做留下的一样。
//! 它们也按段做、跟着存下来：不然有命中的请求（历史里贴过一把密钥，之后每一轮都有）每轮还得
//! 把整段请求体解析一遍认正文。哪些字符串算正文只看这一段自己和它在哪个数组里
//! （[`tw_dialect::written`] 按协议的结构一项一项、一个元素一个元素地认），所以数组里的元素
//! 装进一个只有它的同名数组里认（`{"messages":[元素]}`），键里带上是哪个数组、按哪种形状认。
//! 合并重叠的命中（[`super::rules::resolve`]）在整个请求上做。
//!
//! # 头一回见的对话
//!
//! 按段找要给每一段算哈希、单独校验、单独扫，比整段扫一遍慢；第一轮存下来的又要到下一轮才用得
//! 上。所以一段没存过的对话（第一轮、重启之后、改过前面的历史）**照旧整段做，做完交给后台线程
//! 按段存**（[`ScanCache::fill`]）：存的就是整段做出来的命中按段分开，后台只切段、校验、算哈希。
//! 这个请求只多付一次抄原文的功夫。
//!
//! 没存过的不用切段就认得出：每段存过的对话留一个记号（[`Mark`]，请求体开头的几 KB 和对话
//! 历史里靠后的一截），两样都对上才切段、按段取。按段取时没存着的占了一大半（前面的历史改过）
//! 的，也改回整段做、后台重存。段数比能存的还多的请求不走缓存。
//!
//! 请求体短于 [`BODY_MIN`] 的不走缓存：整段扫一遍也就几十微秒，固定的功夫占得出一两成。
//!
//! 没有命中的段不认正文（解析一遍不便宜），请求里别处有命中时才补认、记下来（[`Found::readable`]）
//!
//! # 键、存什么、存多少
//!
//! 键是 SHA-256（规则集的指纹 + 今天的日期 + 在哪儿找（整段，还是哪种形状的正文）+ 是哪个
//! 数组里的元素 + 这一段的原文）。指纹见 [`RuleSet::fingerprint`]：改规则、改保存的密钥即整体
//! 失效；之后往规则集里加的、会改变命中的东西（例外值……）也要喂进指纹，那个函数把字段一个个
//! 拆开写，漏了编译不过。日期也算进去：身份证号、卡号的出生日期、有效期按「今天」核对，跨了
//! 一天同一段文字的结论可能不同。
//!
//! 存的只有命中的区间和规则 id，**不存值**：要换的时候从原文现取。
//!
//! 最近用过的 [`CAPACITY`] 段，LRU，只在内存里，跟着网关的运行时一起建、一起换（配置一重载
//! 就是一份新的）。太短的段（[`MIN_BYTES`] 以下）扫一遍比算哈希还快，不存；太长的
//! （[`MAX_BYTES`] 以上）照扫不存，免得一段就挤掉几百条。命中数也有总量上限（[`HITS_BUDGET`]）：
//! 一份贴进来的凭据清单可能一段就是几万处。
//!
//! 命中了多少、存了多少不进日志、不进界面：这是内部机制，用户不需要知道。

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::flow::{Scope, Shape};
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
/// 请求体短于这么多字节的不走缓存：整段扫一遍也就几十微秒，切段、算哈希、查表这些固定的
/// 功夫占得出一两成，省下来的却只有几十微秒
const BODY_MIN: usize = 64 * 1024;
/// 数组里挨着的元素攒到这么长才算一段（见 [`split`]）。一条条单拿出来的话，几万条短消息的
/// 请求（Codex 的长会话）每条都要单独解析、单独扫、单独查表，比整段扫一遍慢十几倍，也远远
/// 超出 [`CAPACITY`]
const RUN_BYTES: usize = 4_096;
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
    shared: Arc<Shared>,
    /// 请求体短于这么多字节的不走缓存（[`BODY_MIN`]；测试里放低，小请求体也走一遍）
    body_min: usize,
    /// 后台补存的那个线程的收件口（见 [`ScanCache::fill`]）。缓存建的时候就起好：起一个线程要
    /// 几十微秒，不该算在哪个请求头上。缓存扔掉时收件口跟着扔，线程收工。起不来是 `None`，那就
    /// 不补存（每轮照旧整段做）
    worker: Option<SyncSender<Job>>,
}

/// 请求和后台补存共用的那部分
struct Shared {
    lru: Mutex<Lru>,
    /// 交给后台、还没存完的请求有几个
    pending: AtomicUsize,
    /// 该存的段没存着、扫了一遍的次数（测试看缓存是不是真的用上了）
    #[cfg(test)]
    misses: AtomicUsize,
    /// 按段取来、找来的请求有几个（测试看缓存是不是真的用上了）
    #[cfg(test)]
    warm: AtomicUsize,
}

/// 记着最近多少段对话的记号（[`Mark`]）
const MARKS: usize = 256;
/// 请求的开头取多长（[`head`]）
const HEAD_BYTES: usize = 4_096;

/// 一段存过的对话的记号：请求的开头（[`head`]），和对话历史里靠后的一截在原文里的位置、它的
/// SHA-256。
///
/// 同一段对话一轮轮只在末尾加长，开头不变，靠后的那一截也还在原来的位置上；换了一段对话的，
/// 开头可以一样（几 KB 的系统提示、Codex 的 instructions 在最前面），那一截却对不上。两样都对上
/// 才去切段、按段取：头一回见的对话这样只多算两次几 KB 的哈希，不用切段
#[derive(Clone, PartialEq, Eq)]
struct Mark {
    head: Key,
    at: Range<usize>,
    sum: Key,
}

/// 后台最多压着几个没存完的请求。再多就不补了：下一轮照样整段做、再补一次
const PENDING_MAX: usize = 2;

/// [`ScanCache::kept`] 的结论
pub(crate) enum Kept {
    /// 按段取来、找来的（同 [`super::flow::kept`]，区间是原文里的）
    Hits(Vec<Hit>),
    /// 这个请求不按段走：调用方照旧整段做
    Whole,
    /// 这段对话还没存过（第一轮、重启之后、改过前面的历史）：调用方照旧整段做，做完交给
    /// [`ScanCache::fill`] 在后台按段存下来
    Cold(Stamp),
}

/// 一次整段扫描是按什么做的：这套规则的指纹（不用再算一遍），和找的时候的日期
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stamp {
    fp: [u8; 32],
    day: u32,
}

/// 交给后台补存的一个请求：原文、整段做出来的命中，和存的时候要的键的那几样
struct Job {
    text: String,
    hits: Vec<Hit>,
    fp: [u8; 32],
    day: u32,
    shape: Option<Shape>,
    cmdline: bool,
    /// 整段做的时候认过正文、认得出（[`Found::readable`]）
    readable: bool,
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

/// 一段找出来的（[`Found`]）和它最近一次用到的时刻
struct Entry {
    found: Found,
    tick: u64,
}

/// 一段单拿出来找的结果
#[derive(Clone)]
struct Found {
    /// 去掉不算的之后、合并之前的命中（[`super::flow::kept`]），区间是这一段单拿出来时的位置
    /// （套了花括号的从那个 `{` 算起）
    hits: Arc<[Hit]>,
    /// 这一段里有一条命令到结尾都没结束（[`super::secrets::place::command_open_at_end`]）：
    /// 整段请求体里它会接着认后面的选项，要看一眼后面
    open: bool,
    /// 这一段的正文认过、认得出（[`tw_dialect::written::ranges`]）。整段找的不用认，是
    /// `true`；没有命中的段先不认（解析一遍不便宜），等请求里别处有命中时再补（见
    /// [`ScanCache::kept`]）
    readable: bool,
}

#[derive(Default)]
struct Lru {
    map: HashMap<Key, Entry>,
    /// 最近用到的先后：tick → 键。最小的那个最久没用
    order: BTreeMap<u64, Key>,
    tick: u64,
    /// 存着的命中一共多少处
    hits: usize,
    /// 存过的对话各留一个记号（[`Mark`]），新的在后面。对不上的请求是一段没存过的对话，不用
    /// 切段就知道
    marks: std::collections::VecDeque<Mark>,
}

impl Lru {
    fn get(&mut self, k: &Key) -> Option<Found> {
        let e = self.map.get_mut(k)?;
        self.order.remove(&e.tick);
        self.tick += 1;
        e.tick = self.tick;
        self.order.insert(e.tick, *k);
        Some(e.found.clone())
    }

    /// 一个请求按段存过了：记下它的记号
    fn saw(&mut self, mark: Mark) {
        if !self.marks.contains(&mark) {
            if self.marks.len() >= MARKS {
                self.marks.pop_front();
            }
            self.marks.push_back(mark);
        }
    }

    /// 这一段的正文认过了、认得出（见 [`Found::readable`]）
    fn mark_readable(&mut self, k: &Key) {
        if let Some(e) = self.map.get_mut(k) {
            e.found.readable = true;
        }
    }

    fn put(&mut self, k: Key, found: Found) {
        if found.hits.len() > ENTRY_HITS_MAX {
            return;
        }
        if let Some(old) = self.map.remove(&k) {
            self.order.remove(&old.tick);
            self.hits -= old.found.hits.len();
        }
        self.tick += 1;
        self.hits += found.hits.len();
        self.order.insert(self.tick, k);
        self.map.insert(
            k,
            Entry {
                found,
                tick: self.tick,
            },
        );
        while self.map.len() > CAPACITY || self.hits > HITS_BUDGET {
            let Some((_, old)) = self.order.pop_first() else {
                break;
            };
            if let Some(e) = self.map.remove(&old) {
                self.hits -= e.found.hits.len();
            }
        }
    }
}

impl Shared {
    /// 锁坏了（另一个线程拿着锁 panic 了）照用：表里只有完整写进去的条目
    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.lru.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ScanCache {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            lru: Mutex::new(Lru::default()),
            pending: AtomicUsize::new(0),
            #[cfg(test)]
            misses: AtomicUsize::new(0),
            #[cfg(test)]
            warm: AtomicUsize::new(0),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(PENDING_MAX);
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("redact-cache".into())
                .spawn(move || {
                    for job in rx {
                        // 一个请求体在这里出了错，不能让整个后台停掉：停了之后这份缓存再也
                        // 存不进东西，每个没存过的请求还白抄一份原文
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            store(&shared, job)
                        }));
                        shared.pending.fetch_sub(1, Ordering::Release);
                    }
                })
                .ok()
                .map(|_| tx)
        };
        Self {
            shared,
            worker,
            body_min: BODY_MIN,
        }
    }

    /// 测试用：请求体短到 1 KB 也走缓存，对照测试不用造几十 KB 的请求体
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            body_min: 1_024,
            ..Self::new()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.shared.lock()
    }

    /// 存着几段
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// 到现在为止扫了几段该存的
    #[cfg(test)]
    pub(crate) fn misses(&self) -> usize {
        self.shared.misses.load(Ordering::Relaxed)
    }

    /// 到现在为止按段取来、找来的请求有几个
    #[cfg(test)]
    pub(crate) fn warm(&self) -> usize {
        self.shared.warm.load(Ordering::Relaxed)
    }

    /// 存着的命中一共几处
    #[cfg(test)]
    pub(crate) fn hits_stored(&self) -> usize {
        self.lock().hits
    }

    /// 等后台把交过去的都存完
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        while self.shared.pending.load(Ordering::Acquire) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// [`super::flow::hits_in`] 合并之前的样子（[`super::flow::kept`]：按规则找到、去掉了占位符
    /// 上的、正文外面的、键和载荷上的），**结果一模一样**，只是没变的段取上次的。顺序可以
    /// 不同：之后要排序、合并。
    ///
    /// **这段对话没存过的，不在这儿按段找**（[`Kept::Cold`]）：按段找要给每一段算哈希、单独
    /// 校验、单独扫，比整段扫一遍慢一半以上，而第一轮存下来的要到下一轮才用得上。所以先对一下
    /// 记号（[`Mark`]），对不上就让调用方整段做，做完在后台存。
    pub(crate) fn kept(&self, text: &str, rules: &RuleSet, scope: Scope) -> Kept {
        if rules.is_empty() || text.len() < self.body_min || !cacheable(rules) {
            return Kept::Whole;
        }
        let fp = rules.fingerprint();
        let day = super::rules::today_ymd();
        let shape = match scope {
            Scope::Whole => None,
            Scope::Written(shape) => Some(shape),
        };
        let tag = scope_tag(shape);
        // 这段对话没存过：不用切段（几万条短消息的请求，切一遍就要扫一遍的四分之一）
        let h = head(&fp, day, &tag, text);
        let marks: Vec<Mark> = self
            .lock()
            .marks
            .iter()
            .filter(|m| m.head == h)
            .cloned()
            .collect();
        let seen = marks.iter().any(|m| {
            text.as_bytes()
                .get(m.at.clone())
                .is_some_and(|w| <[u8; 32]>::from(Sha256::digest(w)) == m.sum)
        });
        if !seen {
            return Kept::Cold(Stamp { fp, day });
        }
        let Some(pieces) = split(text) else {
            return Kept::Whole;
        };
        // 段数比能存的还多：一轮存进去的，下一轮还没用到就挤掉了，算哈希、查表白费功夫
        if pieces.len() > CAPACITY {
            return Kept::Whole;
        }
        let keyed = |p: &Piece| (MIN_BYTES..=MAX_BYTES).contains(&p.range.len());
        let keys: Vec<Option<Key>> = pieces
            .iter()
            .map(|p| keyed(p).then(|| key(&fp, day, &tag, p, text)))
            .collect();
        // 锁里只查表；没存着的在锁外面找，找完再一起存进去
        let mut got: Vec<Option<Found>> = {
            let mut lru = self.lock();
            keys.iter()
                .map(|k| k.as_ref().and_then(|k| lru.get(k)))
                .collect()
        };
        // 没存着的占了一大半（前面的历史改过）：一段段找不如整段做、在后台重存
        let missing: usize = pieces
            .iter()
            .zip(&got)
            .filter(|(p, g)| g.is_none() && keyed(p))
            .map(|(p, _)| p.range.len())
            .sum();
        if missing * 2 > text.len() {
            return Kept::Cold(Stamp { fp, day });
        }
        let cmdline = rules.is_on("command-line-password");
        let mut fresh: Vec<(Key, Found)> = Vec::new();
        for (i, p) in pieces.iter().enumerate() {
            if got[i].is_some() {
                continue;
            }
            let Some(found) = find(text, p, rules, shape, cmdline) else {
                return Kept::Whole;
            };
            if let Some(k) = keys[i] {
                #[cfg(test)]
                self.shared.misses.fetch_add(1, Ordering::Relaxed);
                fresh.push((k, found.clone()));
            }
            got[i] = Some(found);
        }
        if !fresh.is_empty() {
            let mut lru = self.lock();
            for (k, found) in fresh {
                lru.put(k, found);
            }
        }
        let mut out = Vec::new();
        let found: Vec<&Found> = got.iter().flatten().collect();
        if found.len() != pieces.len() || leaks(text, &pieces, |i| found[i].open) {
            return Kept::Whole;
        }
        for (p, f) in pieces.iter().zip(&found) {
            out.extend(f.hits.iter().map(|h| Hit {
                bytes: p.to_body(h.bytes.start)..p.to_body(h.bytes.end),
                rule: h.rule.clone(),
                label: h.label.clone(),
            }));
        }
        // 有命中时，整段做的那一边要把整个请求体认一遍正文；认不出（一个超出范围的数、一个
        // 落单的代理对，只看结构的 JSON 校验放过了它们）就一处都不算。没有命中的段这时补认，
        // 认过的记下来，下一轮不再认
        if let Some(shape) = shape
            && !out.is_empty()
        {
            let mut known = Vec::new();
            for (i, p) in pieces.iter().enumerate() {
                if found[i].readable {
                    continue;
                }
                if written(text, p, shape).is_none() {
                    return Kept::Whole;
                }
                known.extend(keys[i]);
            }
            if !known.is_empty() {
                let mut lru = self.lock();
                for k in &known {
                    lru.mark_readable(k);
                }
            }
        }
        #[cfg(test)]
        self.shared.warm.fetch_add(1, Ordering::Relaxed);
        Kept::Hits(out)
    }

    /// [`Kept::Cold`] 的请求整段做完了：在后台按段存下来，下一轮就用得上。`hits` 是整段做出来、
    /// 合并之前的命中（[`super::flow::kept`]），`written`：做的时候认没认正文、认不认得出
    /// （没有命中时不认，是 `None`）。
    ///
    /// **存的就是整段做出来的、按段分开**：结果和一段段单拿出来找的一模一样（见模块说明），
    /// 不用再找一遍；后台只切段、校验、算哈希。原文抄一份交过去，存完就扔，不进缓存。后台压着
    /// 的太多就不补了（[`PENDING_MAX`]）
    pub(crate) fn fill(
        &self,
        text: &str,
        rules: &RuleSet,
        stamp: Stamp,
        scope: Scope,
        hits: &[Hit],
        written: Option<bool>,
    ) {
        // 身份证号、卡号按「今天」核对：找的时候和现在隔了零点，结果不能记在今天的键下
        if written == Some(false)
            || stamp.day != super::rules::today_ymd()
            || self.worker.is_none()
            || self.shared.pending.load(Ordering::Acquire) >= PENDING_MAX
        {
            return;
        }
        let job = Job {
            text: text.to_string(),
            hits: hits.to_vec(),
            fp: stamp.fp,
            day: stamp.day,
            shape: match scope {
                Scope::Whole => None,
                Scope::Written(shape) => Some(shape),
            },
            cmdline: rules.is_on("command-line-password"),
            readable: scope == Scope::Whole || written == Some(true),
        };
        let Some(tx) = &self.worker else {
            return;
        };
        self.shared.pending.fetch_add(1, Ordering::AcqRel);
        if tx.try_send(job).is_err() {
            self.shared.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// 记号里的那一截（见 [`Mark`]）：最大的那个数组（对话历史）倒数第三段，不到四段的取第一段。
/// 下一轮只在末尾加长的话，它还在原来的位置上、一字不差
fn window(pieces: &[Piece]) -> Option<Range<usize>> {
    let mut sizes: Vec<(&Range<usize>, usize)> = Vec::new();
    for p in pieces {
        if let Some(k) = &p.parent {
            match sizes.iter_mut().find(|(x, _)| *x == k) {
                Some((_, n)) => *n += p.range.len(),
                None => sizes.push((k, p.range.len())),
            }
        }
    }
    let (k, _) = sizes.into_iter().max_by_key(|(_, n)| *n)?;
    let of: Vec<&Piece> = pieces
        .iter()
        .filter(|p| p.parent.as_ref() == Some(k))
        .collect();
    let p = if of.len() >= 4 {
        of[of.len() - 3]
    } else {
        of[0]
    };
    Some(p.range.clone())
}

/// 有一段末尾一条没结束的命令伸进了后面、认得到后面的选项（见模块说明）：这时一段段单拿出来
/// 找的和整段做的不一样。`open(i)`：第 `i` 段末尾有这样一条命令
fn leaks(text: &str, pieces: &[Piece], open: impl Fn(usize) -> bool) -> bool {
    // 没结束的命令往后看到了哪儿：后一段的命令结束在这之前的话，那一截已经看过了
    let mut looked = 0;
    for (i, p) in pieces.iter().enumerate() {
        if open(i) && p.range.end >= looked {
            let (option, end) =
                super::secrets::place::option_before_command_end(text.as_bytes(), p.range.end);
            if option {
                return true;
            }
            looked = end;
        }
    }
    false
}

/// 后台：把整段做出来的命中按段分开存下来（见 [`ScanCache::fill`]）。哪一样对不上（切不开、
/// 不是 JSON、有伸进后面的命令、一处命中跨了段）就一段都不存
fn store(shared: &Shared, job: Job) {
    let text = job.text.as_str();
    let Some(pieces) = split(text) else {
        return;
    };
    if pieces.len() > CAPACITY || !super::rules::json_like(text) {
        return;
    }
    let open: Vec<bool> = pieces
        .iter()
        .map(|p| job.cmdline && super::secrets::place::command_open_at_end(&text[p.range.clone()]))
        .collect();
    if leaks(text, &pieces, |i| open[i]) {
        return;
    }
    let mut each: Vec<Vec<Hit>> = vec![Vec::new(); pieces.len()];
    for h in job.hits {
        let at = pieces.partition_point(|p| p.range.end <= h.bytes.start);
        let Some(p) = pieces.get(at) else {
            return;
        };
        if h.bytes.start < p.range.start || h.bytes.end > p.range.end {
            return;
        }
        // 单拿出来时前面多一个括号
        let shift = p.range.start - 1;
        each[at].push(Hit {
            bytes: h.bytes.start - shift..h.bytes.end - shift,
            ..h
        });
    }
    let tag = scope_tag(job.shape);
    let mark = window(&pieces).map(|at| Mark {
        head: head(&job.fp, job.day, &tag, text),
        sum: Sha256::digest(&text.as_bytes()[at.clone()]).into(),
        at,
    });
    let mut lru = shared.lock();
    if let Some(m) = mark {
        lru.saw(m);
    }
    for ((p, hits), open) in pieces.iter().zip(each).zip(open) {
        if !(MIN_BYTES..=MAX_BYTES).contains(&p.range.len()) {
            continue;
        }
        let k = key(&job.fp, job.day, &tag, p, text);
        lru.put(
            k,
            Found {
                hits: hits.into(),
                open,
                readable: job.readable,
            },
        );
    }
}

/// 这套规则下能不能按段缓存（见模块说明的「不缓存的情形」）
pub(crate) fn cacheable(rules: &RuleSet) -> bool {
    !CONTEXTUAL.iter().any(|id| rules.is_on(id))
        && !rules
            .saved()
            .is_some_and(|s| s.values().any(|v| v.contains('"')))
}

/// 一个请求的开头：键的那几样加上请求体的头 [`HEAD_BYTES`] 个字节（见 [`Mark`]）
fn head(fp: &[u8; 32], day: u32, scope: &str, text: &str) -> Key {
    let b = text.as_bytes();
    let mut h = Sha256::new();
    h.update(b"head");
    h.update(fp);
    h.update(day.to_le_bytes());
    h.update((scope.len() as u64).to_le_bytes());
    h.update(scope.as_bytes());
    h.update(&b[..b.len().min(HEAD_BYTES)]);
    h.finalize().into()
}

/// 键里的「在哪儿找」：整段是空的，正文是哪种形状
fn scope_tag(shape: Option<Shape>) -> String {
    shape.map(|s| format!("{s:?}")).unwrap_or_default()
}

/// 一段的键：指纹、日期、在哪儿找（整段，还是哪种形状的正文）、是哪个数组里的元素（还是
/// 套了花括号的几项）、原文
fn key(fp: &[u8; 32], day: u32, scope: &str, p: &Piece, text: &str) -> Key {
    let b = text.as_bytes();
    let mut h = Sha256::new();
    h.update(fp);
    h.update(day.to_le_bytes());
    h.update((scope.len() as u64).to_le_bytes());
    h.update(scope.as_bytes());
    match &p.parent {
        Some(k) => {
            h.update([p.open]);
            h.update((k.len() as u64).to_le_bytes());
            h.update(&b[k.clone()]);
        }
        None => h.update([0]),
    }
    h.update(&b[p.range.clone()]);
    h.finalize().into()
}

/// 这一段里哪些字符串是正文，区间是单拿出来时的位置。数组里的一串元素装进一个只有它们的同名
/// 数组里认（`{"messages":[元素, …]}`）—— 一个元素里哪些字符串算正文只看它自己和它在哪个数组
/// 里，不看它是第几个、旁边是谁。认不出是 `None`
fn written(text: &str, p: &Piece, shape: Shape) -> Option<Vec<Range<usize>>> {
    let piece = p.text(text);
    match &p.parent {
        None => tw_dialect::written::ranges(shape, &piece),
        Some(k) => {
            let head = format!("{{\"{}\":", &text[k.clone()]);
            let alone = format!("{head}{piece}}}");
            Some(
                tw_dialect::written::ranges(shape, &alone)?
                    .into_iter()
                    .map(|r| r.start - head.len()..r.end - head.len())
                    .collect(),
            )
        }
    }
}

/// 单拿出来找一段：扫、去掉不算的（[`super::flow::kept`]）。`shape`：只找这种形状的正文
/// （[`written`]）；`None` 是整段都找。`None`：这一段单拿出来不是 JSON、正文认不出、或者命中
/// 落在套上去的括号上，整个请求照旧整段做
fn find(
    text: &str,
    p: &Piece,
    rules: &RuleSet,
    shape: Option<Shape>,
    cmdline: bool,
) -> Option<Found> {
    let piece = p.text(text);
    // 整段是 JSON、单拿出来却不是的话（这一段本身就坏了），两边扫法不一样
    if !super::rules::json_like(&piece) {
        return None;
    }
    let raw = super::rules::scan_unresolved(&piece, rules);
    // 套上去的那对括号不是原文：落在它上面的命中（不该有）对不回原文
    let len = piece.len();
    if raw.iter().any(|h| h.bytes.start == 0 || h.bytes.end >= len) {
        return None;
    }
    let open = cmdline && super::secrets::place::command_open_at_end(&text[p.range.clone()]);
    let (hits, readable) = match shape {
        None => (super::flow::kept(&piece, raw, None::<fn() -> _>), true),
        Some(shape) => {
            // 没有命中就不认正文（见 `Found::readable`）
            let mut readable = None;
            let hits = super::flow::kept(
                &piece,
                raw,
                Some(|| {
                    let w = written(text, p, shape);
                    readable = Some(w.is_some());
                    w
                }),
            );
            match readable {
                Some(false) => return None,
                Some(true) => (hits, true),
                None => (hits, false),
            }
        }
    };
    Some(Found {
        hits: hits.into(),
        open,
        readable,
    })
}

/// 请求体里的一段（见模块说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    /// 在原文里的字节区间
    pub range: Range<usize>,
    /// 单拿出来扫的时候外面套的括号：挨着的几个顶层项（`"key": value, …`）套 `{`，数组里
    /// 挨着的几个元素（`{…}, {…}`）套 `[`
    pub open: u8,
    /// 数组里的元素：那个数组是哪个顶层键的值（键在原文里引号之间的那一段）
    pub parent: Option<Range<usize>>,
}

impl Piece {
    /// 单拿出来扫的那段文字
    fn text(&self, body: &str) -> String {
        let raw = &body[self.range.clone()];
        let mut s = String::with_capacity(raw.len() + 2);
        s.push(char::from(self.open));
        s.push_str(raw);
        s.push(if self.open == b'[' { ']' } else { '}' });
        s
    }

    /// 单拿出来扫时的位置 → 原文里的位置（前面多了一个括号）
    fn to_body(&self, at: usize) -> usize {
        self.range.start + at - 1
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
        // 短的（键、角色、类型）逐字节看完就到头了，比起一次 SIMD 查找还省；长的再交给它
        let near = (j + 24).min(b.len());
        let k = match b[j.min(near)..near]
            .iter()
            .position(|&c| c == b'"' || c == b'\\')
        {
            Some(k) => k,
            None if near == b.len() => return None,
            None => near - j + memchr::memchr2(b'"', b'\\', &b[near..])?,
        };
        if b[j + k] == b'\\' {
            j += k + 2;
        } else {
            return Some(j + k + 1);
        }
    }
}

/// 引号和括号
const STRUCTURE: [bool; 256] = {
    let mut t = [false; 256];
    t[b'"' as usize] = true;
    t[b'{' as usize] = true;
    t[b'}' as usize] = true;
    t[b'[' as usize] = true;
    t[b']' as usize] = true;
    t
};

/// 从 `i` 起的一个值到哪儿为止，和它里面最深有几层（数和字面量是 0 层）。只认结构：括号
/// 配不配对、数写得对不对由单拿出来那一段的 JSON 校验管（[`super::rules::json_like`]）
fn value_end(b: &[u8], i: usize) -> Option<(usize, usize)> {
    match *b.get(i)? {
        b'"' => Some((string_end(b, i)?, 0)),
        b'{' | b'[' => {
            let (mut depth, mut deepest) = (0usize, 0usize);
            let mut j = i;
            while j < b.len() {
                // 结构以外的字节（数、字面量、空白、逗号、冒号）一张表跳过
                if !STRUCTURE[usize::from(b[j])] {
                    j += 1;
                    continue;
                }
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
    let mut keys: Vec<Vec<u8>> = Vec::new();
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
        // 重复的键：解析时后一个盖住前一个，哪些字符串算正文跟着变，按段扫说不清。只差在
        // 大小写和下划线上的也算重复：Gemini 的 `systemInstruction` 和 `system_instruction`
        // 是同一项，两个都写着时只认一个
        let norm: Vec<u8> = key
            .iter()
            .filter(|&&c| c != b'_')
            .map(u8::to_ascii_lowercase)
            .collect();
        if keys.contains(&norm) {
            return None;
        }
        keys.push(norm);
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value = i;
        // 请求体可能就断在冒号后面（坏掉的客户端、插件的输出）：值不一定有
        let items = (b.get(value) == Some(&b'[') && plain_key(key))
            .then(|| elements(b, value))
            .flatten();
        let (end, depth) = match &items {
            Some((_, end, depth)) => (*end, *depth),
            None => value_end(b, value)?,
        };
        if depth + 1 > DEPTH_MAX {
            return None;
        }
        match items {
            Some((items, _, _)) => {
                out.extend(group.take().map(|range| Piece {
                    range,
                    open: b'{',
                    parent: None,
                }));
                // 挨着的元素攒够 `RUN_BYTES` 算一段。从数组开头往后攒：历史只在末尾加长时，
                // 前面攒好的那几段一字不变
                let mut run: Option<Range<usize>> = None;
                for r in items {
                    let r = run.take().map_or(r.clone(), |x| x.start..r.end);
                    if r.len() >= RUN_BYTES {
                        out.push(Piece {
                            range: r,
                            open: b'[',
                            parent: Some(member + 1..key_end - 1),
                        });
                    } else {
                        run = Some(r);
                    }
                }
                out.extend(run.map(|range| Piece {
                    range,
                    open: b'[',
                    parent: Some(member + 1..key_end - 1),
                }));
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
        open: b'{',
        parent: None,
    }));
    (skip_ws(b, i + 1) == b.len()).then_some(out)
}

/// `b[from..]` 是一个**全是对象**、至少一个元素的数组的话：每个元素的区间、数组在哪儿结束、
/// 里面最深有几层（同 [`value_end`]）
fn elements(b: &[u8], from: usize) -> Option<(Vec<Range<usize>>, usize, usize)> {
    let mut i = skip_ws(b, from + 1);
    let mut out = Vec::new();
    let mut deepest = 0;
    loop {
        if b.get(i) != Some(&b'{') {
            return None;
        }
        let (end, depth) = value_end(b, i)?;
        deepest = deepest.max(depth + 1);
        out.push(i..end);
        i = skip_ws(b, end);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b']') => return Some((out, i + 1, deepest)),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

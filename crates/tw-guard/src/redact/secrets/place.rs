//! 按位置认的凭据：请求头、命令行口令、URL 参数、机器人 Webhook，和跨 token 的几种结构
//! （Telegram、Azure、千帆，判据在 [`super::shapes`]）。
//!
//! **从 token 出发找落点，不另扫一遍全文。**这些位置都有一个 token 形状的标记 ——
//! `Authorization`、`curl`、`access_token`、`hooks.slack.com` —— 切 token 那一遍本来就要
//! 经过它们，在那儿看一眼前后就够了。另用一个多模式查找把整段再扫一遍，在几 MB 的请求体上
//! 要多花一倍的时间。
//!
//! **只换值那一段。**名字、地址、命令留着：模型要读懂的正是「这个请求头写得对不对」「这条
//! 命令连的是哪个库」。值是占位写法（`$TOKEN`、`<your-key>`、`{{x}}`）、像个变量名、像一段
//! 表达式（`"Bearer " + token`）的不换，见 [`super::credible`]。

use std::ops::Range;

use super::super::anchor::{Anchor, Bytes};
use super::super::rules::{Hit, RuleSet, builtin_hit};
use super::{credible, shapes, tok_byte};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Header,
    Curl,
    Mysql,
    EnvPassword,
    UrlParam,
    Feishu,
    Dingtalk,
    Wecom,
    Slack,
    Discord,
    Qianfan,
}

/// 标记是整个 token（不分大小写）
const WORDS: &[(&str, Mark)] = &[
    ("authorization", Mark::Header),
    ("x-api-key", Mark::Header),
    ("api-key", Mark::Header),
    ("x-goog-api-key", Mark::Header),
    ("curl", Mark::Curl),
    ("curl.exe", Mark::Curl),
    ("mysql", Mark::Mysql),
    ("mysql.exe", Mark::Mysql),
    ("mysqldump", Mark::Mysql),
    ("mysqladmin", Mark::Mysql),
    ("mysqlimport", Mark::Mysql),
    ("mysqlshow", Mark::Mysql),
    ("mysqlcheck", Mark::Mysql),
    ("mariadb", Mark::Mysql),
    ("mariadb-dump", Mark::Mysql),
    ("pgpassword", Mark::EnvPassword),
    ("mysql_pwd", Mark::EnvPassword),
    ("access_token", Mark::UrlParam),
    ("api_key", Mark::UrlParam),
    ("apikey", Mark::UrlParam),
    ("client_secret", Mark::UrlParam),
    ("sig", Mark::UrlParam),
    ("x-amz-signature", Mark::UrlParam),
    ("x-goog-signature", Mark::UrlParam),
    ("open-apis", Mark::Feishu),
    ("oapi.dingtalk.com", Mark::Dingtalk),
    ("qyapi.weixin.qq.com", Mark::Wecom),
    ("hooks.slack.com", Mark::Slack),
    ("discord.com", Mark::Discord),
    ("discordapp.com", Mark::Discord),
    ("ptb.discord.com", Mark::Discord),
    ("canary.discord.com", Mark::Discord),
    ("bce-v3", Mark::Qianfan),
];

/// 这一组的锚点：标记词整个 token 对上（不分大小写）；Telegram 的令牌以 `AA` 开头、至少 35 个
/// 字符；Azure 的客户端密码在 `Q~` 处被切开，前一个 token 以 `Q` 结尾；Teams 的主机名以
/// `webhook.office.com` 结尾（最后一个字母 `m`），至少 18 个字符
pub(in crate::redact) const ANCHORS: &[Anchor] = &{
    const N: usize = WORDS.len();
    let mut a = [Anchor::Word(""); N + 3];
    let mut i = 0;
    while i < N {
        a[i] = Anchor::Word(WORDS[i].0);
        i += 1;
    }
    a[N] = Anchor::Prefix("AA", 35);
    a[N + 1] = Anchor::Last(Bytes::of(b"Q"), 1);
    a[N + 2] = Anchor::Last(Bytes::of(b"m"), 18);
    a
};

/// [`WORDS`] 里出现过的「头一个字母 × 长度」：别的组合的 token 一个字节都不用比。按位存，
/// 第 `字母 * 32 + 长度` 位
const KEYS: [u64; 13] = {
    let mut m = [0u64; 13];
    let mut i = 0;
    while i < WORDS.len() {
        let k = (WORDS[i].0.as_bytes()[0] - b'a') as usize * 32 + WORDS[i].0.len();
        m[k / 64] |= 1 << (k % 64);
        i += 1;
    }
    m
};

/// [`WORDS`] 按长度分好：一个 token 只和同样长的那一两个比
fn by_len() -> &'static [Vec<(&'static str, Mark)>] {
    static T: std::sync::OnceLock<Vec<Vec<(&'static str, Mark)>>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![Vec::new(); 32];
        for &(w, m) in WORDS {
            t[w.len()].push((w, m));
        }
        t
    })
}

#[inline]
fn mark_of(tok: &str) -> Option<Mark> {
    let n = tok.len();
    let first = tok.as_bytes()[0] | 0x20;
    if n >= 32 || !first.is_ascii_lowercase() {
        return None;
    }
    let k = usize::from(first - b'a') * 32 + n;
    if KEYS[k / 64] & (1 << (k % 64)) == 0 {
        return None;
    }
    by_len()[n]
        .iter()
        .find(|(w, _)| w.eq_ignore_ascii_case(tok))
        .map(|&(_, m)| m)
}

/// 这一组里每条开没开（扫之前问一次）
pub(super) struct Want {
    header: bool,
    cmdline: bool,
    url: bool,
    webhook: bool,
    telegram: bool,
    azure: bool,
    qianfan: bool,
    any: bool,
}

impl Want {
    pub(super) fn new(set: &RuleSet) -> Self {
        let mut w = Want {
            header: set.is_on("auth-header-credential"),
            cmdline: set.is_on("command-line-password"),
            url: set.is_on("url-token"),
            webhook: set.is_on("bot-webhook"),
            telegram: set.is_on("telegram-bot-token"),
            azure: set.is_on("azure-ad-client-secret"),
            qianfan: set.is_on("qianfan-api-key"),
            any: false,
        };
        w.any = w.header || w.cmdline || w.url || w.webhook || w.telegram || w.azure || w.qianfan;
        w
    }

    pub(super) fn any(&self) -> bool {
        self.any
    }

    /// 每个 token 一次。`out` 收本身就是一种凭据的（Webhook、Telegram、Azure、千帆），
    /// `placed` 收按位置认的（扫完再看有没有让给别的规则，见 [`super::Want::finish`]）
    #[inline]
    pub(super) fn token(
        &self,
        text: &str,
        tok: &str,
        span: Range<usize>,
        out: &mut Vec<Hit>,
        placed: &mut Vec<Hit>,
    ) {
        if !self.any {
            return;
        }
        let b = text.as_bytes();
        let t = tok.as_bytes();
        if self.telegram
            && t.len() >= 35
            && t.starts_with(b"AA")
            && span.start > 0
            && b[span.start - 1] == b':'
            && let Some(r) = shapes::telegram(b, span.start - 1)
        {
            out.push(builtin_hit("telegram-bot-token", r));
        }
        if self.azure
            && t.last() == Some(&b'Q')
            && b.get(span.end) == Some(&b'~')
            && let Some(r) = shapes::azure(b, span.end - 1)
        {
            out.push(builtin_hit("azure-ad-client-secret", r));
        }
        if self.webhook && t.len() >= 18 && t[t.len() - 1] == b'm' && teams_host(tok) {
            teams(b, span.end, out);
        }
        let Some(mark) = mark_of(tok) else {
            return;
        };
        let (s, e) = (span.start, span.end);
        match mark {
            Mark::Header if self.header => header(text, s, e, placed),
            Mark::Curl if self.cmdline && command_word(b, e) => curl(text, e, placed),
            Mark::Mysql if self.cmdline && command_word(b, e) => mysql(text, e, placed),
            Mark::EnvPassword if self.cmdline => env_password(text, s, e, placed),
            Mark::UrlParam if self.url => url_param(text, s, e, placed),
            Mark::Feishu if self.webhook => {
                webhook_value(b, e, b"/bot/v2/hook/", 16, true, out);
            }
            Mark::Dingtalk if self.webhook => {
                webhook_value(b, e, b"/robot/send?access_token=", 32, false, out);
            }
            Mark::Wecom if self.webhook => {
                webhook_value(b, e, b"/cgi-bin/webhook/send?key=", 32, true, out);
            }
            Mark::Slack if self.webhook => slack(b, e, out),
            Mark::Discord if self.webhook => discord(b, e, out),
            Mark::Qianfan if self.qianfan => {
                if let Some(r) = shapes::qianfan(b, s) {
                    out.push(builtin_hit("qianfan-api-key", r));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- 小工具

/// `i` 处是不是一个引号：前面可以有几个反斜杠（JSON 里转义过的 `\"`，转义过两次的 `\\\"`）。
/// 返回 (这一串有几个字节, 引号是哪个)
pub(super) fn quote_at(b: &[u8], i: usize) -> Option<(usize, u8)> {
    let mut j = i;
    while j < b.len() && b[j] == b'\\' {
        j += 1;
    }
    match b.get(j) {
        Some(&q @ (b'"' | b'\'' | b'`')) => Some((j + 1 - i, q)),
        _ => None,
    }
}

pub(super) fn skip_blank(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// 从 `i` 起一段 `ok` 的字节在哪儿结束
pub(super) fn run(b: &[u8], mut i: usize, ok: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && ok(b[i]) {
        i += 1;
    }
    i
}

/// token68（RFC 7235）：Bearer、Basic 后面那一段能有的字符
fn token68(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

/// 命令行参数里一个值能有的字符：到空白、引号、反斜杠（JSON 里的转义）、命令分隔为止
fn arg_char(c: u8) -> bool {
    c.is_ascii_graphic() && !matches!(c, b'"' | b'\'' | b'`' | b'\\' | b';' | b'|' | b'&')
}

/// 一个值的区间：句末的句号不算
fn trimmed(b: &[u8], start: usize, mut end: usize) -> Range<usize> {
    if end > start && b[end - 1] == b'.' {
        end -= 1;
    }
    start..end
}

/// 找到的值能不能换：够长、像真值（见 [`credible`]）
fn push_if(text: &str, r: Range<usize>, min: usize, id: &'static str, placed: &mut Vec<Hit>) {
    if r.end > r.start && credible(&text[r.clone()], min) {
        placed.push(builtin_hit(id, r));
    }
}

// ---------------------------------------------------------------- 请求头

/// 鉴权方式的那个词（`Bearer`、`Basic` ……），后面跟着空格
const SCHEMES: &[&str] = &["bearer", "basic", "token", "bot"];

/// `Authorization: Bearer …`、`x-api-key: …` 的值。`s..e` 是头名。
///
/// 认的写法：HTTP 原文和 `curl -H` 里的 `名: 值`，代码里的字典（`"Authorization": "Bearer …"`、
/// `headers['x-api-key'] = '…'`），Go 的 `Header.Set("Authorization", "Bearer …")`。值紧跟
/// 在冒号、等号或逗号后面（中间可以有空格和一个引号）；其余写法（隔着别的词、换了行）不认
fn header(text: &str, s: usize, e: usize, placed: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let auth = e - s == "authorization".len();
    let mut i = e;
    if let Some((n, _)) = quote_at(b, i) {
        i += n;
        if b.get(i) == Some(&b']') {
            i += 1;
        }
    }
    i = skip_blank(b, i);
    match b.get(i) {
        Some(b':' | b',') => i += 1,
        Some(b'=') if b.get(i + 1) != Some(&b'=') => i += 1,
        _ => return,
    }
    i = skip_blank(b, i);
    if let Some((n, _)) = quote_at(b, i) {
        i += n;
    }
    if auth {
        let w = run(b, i, |c| c.is_ascii_alphabetic());
        if w > i
            && b.get(w) == Some(&b' ')
            && SCHEMES.iter().any(|x| x.eq_ignore_ascii_case(&text[i..w]))
        {
            i = skip_blank(b, w);
        }
    }
    let end = run(b, i, token68);
    // 后面紧跟着模板、调用、下标的是一段表达式的开头，不是一个值
    if matches!(b.get(end), Some(b'{' | b'(' | b'$' | b'[')) {
        return;
    }
    push_if(
        text,
        trimmed(b, i, end),
        12,
        "auth-header-credential",
        placed,
    );
}

// ---------------------------------------------------------------- 命令行

/// 紧跟在命令名后面的是空白或者引号（`["curl","-u",…]`）：这个词是一条命令，不是一个名字的
/// 一部分。切 token 的时候前面已经是边界了（`/usr/bin/curl` 的 `/` 不是 token 里的字符）
fn command_word(b: &[u8], e: usize) -> bool {
    b.get(e)
        .is_some_and(|&c| c.is_ascii_whitespace() || quote_at(b, e).is_some())
}

/// 从 `from` 起这条命令在哪儿结束：`;`、`|`、`&`、`)`、反引号、换行，和字符串里转义的
/// 换行（JSON 和代码里的 `\n`）
fn command_end(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b';' | b'|' | b'&' | b')' | b'`' | b'\n' | b'\r' => return i,
            b'\\' if b.get(i + 1) == Some(&b'\\') => i += 2,
            b'\\' if matches!(b.get(i + 1), Some(b'n' | b'r')) => return i,
            _ => i += 1,
        }
    }
    b.len()
}

/// 一个选项的开头：`-` 前面是空白或者引号（`"-u"` 是数组里的一项）
fn option_at(b: &[u8], k: usize) -> bool {
    b[k] == b'-' && k > 0 && (b[k - 1].is_ascii_whitespace() || matches!(b[k - 1], b'"' | b'\''))
}

/// `curl -u 用户:口令`、`--user 用户:口令`、`-u用户:口令` 里的口令。`e` 是 `curl` 的终点。
/// 只有用户名的（`-u user`、`-u sk_…:`，口令为空）不算 —— 后者的密钥在用户名那一段，归
/// 前缀规则
fn curl(text: &str, e: usize, placed: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let end = command_end(b, e);
    let mut k = e;
    while k < end {
        if !option_at(b, k) {
            k += 1;
            continue;
        }
        let at = if b[k..end].starts_with(b"--user")
            && b.get(k + 6).is_some_and(|&c| c == b'=' || !tok_byte(c))
        {
            k + 6
        } else if b.get(k + 1) == Some(&b'u') && b.get(k + 2).is_some_and(|&c| c != b'-') {
            k + 2
        } else {
            k += 1;
            continue;
        };
        let mut i = at;
        if b.get(i) == Some(&b'=') {
            i += 1;
        }
        i = skip_blank(b, i);
        if let Some((n, _)) = quote_at(b, i) {
            i += n;
        }
        let colon = run(b, i, |c| arg_char(c) && c != b':');
        if colon > i && b.get(colon) == Some(&b':') {
            let pass_end = run(b, colon + 1, arg_char).min(end);
            push_if(
                text,
                colon + 1..pass_end,
                4,
                "command-line-password",
                placed,
            );
        }
        k = at;
    }
}

/// `mysql -p口令`、`--password=口令`（mysqldump、mariadb 这一家都一样）。**`-p` 后面隔着空格
/// 的是库名**（`mysql -p mydb` 是「提示输入口令、连 mydb」），不算
fn mysql(text: &str, e: usize, placed: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let end = command_end(b, e);
    let mut k = e;
    while k < end {
        if !option_at(b, k) {
            k += 1;
            continue;
        }
        let at = if b[k..end].starts_with(b"--password=") {
            k + "--password=".len()
        } else if b.get(k + 1) == Some(&b'p') {
            k + 2
        } else {
            k += 1;
            continue;
        };
        let mut i = at;
        if let Some((n, _)) = quote_at(b, i) {
            i += n;
        }
        let v_end = run(b, i, arg_char).min(end);
        push_if(text, i..v_end, 4, "command-line-password", placed);
        k = at;
    }
}

/// `PGPASSWORD=口令 psql …`、`MYSQL_PWD=口令`
fn env_password(text: &str, s: usize, e: usize, placed: &mut Vec<Hit>) {
    let b = text.as_bytes();
    if b.get(e) != Some(&b'=') || (s > 0 && b[s - 1] == b'$') {
        return;
    }
    let mut i = e + 1;
    if let Some((n, _)) = quote_at(b, i) {
        i += n;
    }
    let end = run(b, i, arg_char);
    push_if(text, i..end, 4, "command-line-password", placed);
}

// ---------------------------------------------------------------- URL 参数

/// URL 里 `?access_token=…`、`&sig=…` 的值。参数名前面要是 `?`、`&` 或者 `#`（OAuth 的隐式
/// 授权把令牌放在片段里）：光有 `api_key=` 的是一句赋值，归配置赋值那条
fn url_param(text: &str, s: usize, e: usize, placed: &mut Vec<Hit>) {
    let b = text.as_bytes();
    if s == 0 || !matches!(b[s - 1], b'?' | b'&' | b'#') || b.get(e) != Some(&b'=') {
        return;
    }
    let end = run(b, e + 1, |c| {
        c.is_ascii_graphic()
            && !matches!(
                c,
                b'&' | b'#' | b'"' | b'\'' | b'\\' | b'<' | b'>' | b')' | b']' | b'}' | b','
            )
    });
    push_if(text, trimmed(b, e + 1, end), 8, "url-token", placed);
}

// ---------------------------------------------------------------- 机器人 Webhook

/// 一个 Webhook 令牌段像不像真的：够长、不是 `xxxx`、`****` 这种占位
fn real_token(v: &[u8], min: usize) -> bool {
    v.len() >= min
        && !v.iter().all(|&c| c == v[0])
        && std::str::from_utf8(v).is_ok_and(|s| !super::super::common::placeholder_value(s))
}

/// 主机名 token 之后紧跟着 `path`，再往后的那一段（字母数字，`dash` 时也可以有 `-`）是令牌
fn webhook_value(b: &[u8], e: usize, path: &[u8], min: usize, dash: bool, out: &mut Vec<Hit>) {
    if !b[e..].starts_with(path) {
        return;
    }
    let from = e + path.len();
    let end = run(b, from, |c| {
        c.is_ascii_alphanumeric() || (dash && c == b'-')
    });
    if real_token(&b[from..end], min) {
        out.push(builtin_hit("bot-webhook", from..end));
    }
}

/// URL 的路径在哪儿结束：空白、引号、反斜杠、尖括号、查询串
fn path_end(b: &[u8], from: usize) -> usize {
    run(b, from, |c| {
        c.is_ascii_graphic()
            && !matches!(c, b'"' | b'\'' | b'\\' | b'<' | b'>' | b'?' | b'#' | b')')
    })
}

/// `hooks.slack.com/services/T…/B…/令牌`（`workflows`、`triggers` 同理）：最后一段是令牌
fn slack(b: &[u8], e: usize, out: &mut Vec<Hit>) {
    if b.get(e) != Some(&b'/') {
        return;
    }
    let end = path_end(b, e);
    let path = &b[e..end];
    let last = path
        .iter()
        .rposition(|&c| c == b'/')
        .map_or(e, |p| e + p + 1);
    let segments = path.iter().filter(|&&c| c == b'/').count();
    if segments >= 3
        && b[last..end].iter().all(u8::is_ascii_alphanumeric)
        && real_token(&b[last..end], 16)
    {
        out.push(builtin_hit("bot-webhook", last..end));
    }
}

/// `discord.com/api/webhooks/<数字 id>/<令牌>`：令牌那一段
fn discord(b: &[u8], e: usize, out: &mut Vec<Hit>) {
    const PATH: &[u8] = b"/api/webhooks/";
    if !b[e..].starts_with(PATH) {
        return;
    }
    let id = e + PATH.len();
    let slash = run(b, id, |c| c.is_ascii_digit());
    if slash - id < 15 || b.get(slash) != Some(&b'/') {
        return;
    }
    let end = run(b, slash + 1, |c| {
        c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
    });
    if real_token(&b[slash + 1..end], 60) {
        out.push(builtin_hit("bot-webhook", slash + 1..end));
    }
}

fn teams_host(tok: &str) -> bool {
    const HOST: &str = "webhook.office.com";
    let n = tok.len();
    n >= HOST.len()
        && tok.as_bytes()[n - HOST.len()..].eq_ignore_ascii_case(HOST.as_bytes())
        && (n == HOST.len() || tok.as_bytes()[n - HOST.len() - 1] == b'.')
}

/// Teams 的传入 Webhook：`….webhook.office.com/webhookb2/…/IncomingWebhook/<令牌>/…`
fn teams(b: &[u8], e: usize, out: &mut Vec<Hit>) {
    const SEG: &[u8] = b"/IncomingWebhook/";
    if b.get(e) != Some(&b'/') {
        return;
    }
    let end = path_end(b, e);
    let Some(p) = memchr::memmem::find(&b[e..end], SEG) else {
        return;
    };
    let from = e + p + SEG.len();
    let to = run(b, from, |c| c.is_ascii_alphanumeric()).min(end);
    if real_token(&b[from..to], 16) {
        out.push(builtin_hit("bot-webhook", from..to));
    }
}

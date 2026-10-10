//! 安全记录的细节（[`tw_api::SecurityHitDetail`]）：每一处在哪儿、前后是什么、当时的规则、
//! 具体做了什么。**命中的那一刻算好**，跟着事件走，和记录一起存。
//!
//! # 前后文和存下来的正文打的是同一种码
//!
//! 前后文、命中的那一段都从原文里取，取出来之前过一遍这个请求的 [`Redaction`] —— 存下来
//! 的正文用的就是它：同样的规则、同样的账本，认出来的值换成占位符或者打码，再按形状打一遍
//! （[`Redaction::piece`]）。**先打码、再截短**：截短在后，截口落在一把密钥中间也只会截出
//! 半个打过码的样子，不会截出半把真的。
//!
//! 打码要看得见整个值，所以：
//!
//! - 命中按整段原文找好（出站脱敏在请求原文上找的那一遍；内容过滤、工具调用按命中所在的
//!   那个字符串或者那份参数整个找一遍），取的那一段只是照着换；
//! - 取的一段两头不切在一处命中或者一个 token 的中间（打码按整个 token 认）：切到的那一截
//!   不要；
//! - 内容过滤、工具调用命中的那一段两头落在一把密钥中间时，把那把密钥整个算进命中的那一段
//!   （打过码的样子）—— 分成两半各自打码，哪一半都认不出它。
//!
//! # 位置
//!
//! 出站脱敏的命中是请求原文里的字节区间：在原文上走一遍（[`tw_guard::locate::walk`]）找出
//! 它落在哪个字符串里、路径是什么。内容过滤的命中在调用方正文的某个字符串里
//! （[`tw_guard::content::places`]），路径就是那个字符串的。哪一部分、第几条消息、谁说的、
//! 哪个工具按客户端的格式读（[`tw_guard::locate::place`]）。

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use tw_api::{
    HIT_LOCATIONS_MAX, HitLocation, OutcomeDetail, RuleSnapshot, SecurityDirection,
    SecurityHitDetail,
};
use tw_dialect::ir::Dialect;
use tw_guard::locate::{self, Literal, Part, Place};
use tw_guard::redact::rules::{Finding, Hit, Rule};

use crate::bodies::Redaction;

/// 前后文各最多几个字符
pub const CONTEXT_CHARS: usize = 80;

/// 命中的那一段最多几个字符
pub const MATCHED_CHARS: usize = 200;

/// 切断的调用的参数最多留多少字节
pub const ARGUMENTS_MAX: usize = 4 * 1024;

/// 前后文先取多大一段原文去打码，再截成 [`CONTEXT_CHARS`] 个字符：80 个中文字符写成转义是
/// 480 个字节，多出来的给切掉的半个 token 留余地
const WINDOW: usize = 2048;

/// 认出它的 core 的版本（[`RuleSnapshot::core_version`]）
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 前后文从哪儿取：一段原文，和在它上面按脱敏规则找到的命中。
struct Source<'a> {
    text: &'a str,
    /// 命中所在的那一段：JSON 字符串引号里面的部分，不按 JSON 读的是整段
    span: Range<usize>,
    /// `text` 上按脱敏规则找到的命中（[`crate::guard::hits`]），按起点排好、互不重叠
    hits: &'a [Hit],
    /// `span` 是一个 JSON 字符串的原文：取出来的要解开转义，切口不落在转义中间
    lit: Option<Literal>,
}

/// 一处命中打过码的前后文
struct Excerpt {
    before: String,
    matched: String,
    after: String,
}

/// 一个字节是不是 token 的一部分。**和 [`tw_secret::mask_body`] 认的一样**：打码按整个
/// token 认，切口不能落在 token 中间
fn is_tok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// 跨在 `pos` 上的那一处命中（起点在它前面、终点在它后面）
fn straddling(hits: &[Hit], pos: usize) -> Option<&Hit> {
    let k = hits.partition_point(|h| h.bytes.start < pos);
    k.checked_sub(1)
        .map(|k| &hits[k])
        .filter(|h| h.bytes.end > pos)
}

impl Source<'_> {
    /// 挪到一个字符、一个转义序列的边界上
    fn snap(&self, mut pos: usize, forward: bool) -> usize {
        pos = pos.clamp(self.span.start, self.span.end);
        while !self.text.is_char_boundary(pos) {
            if forward {
                pos += 1;
            } else {
                pos -= 1;
            }
        }
        match &self.lit {
            Some(l) => l.snap(pos, forward),
            None => pos,
        }
    }

    /// 跨在 `pos` 上的那个 token（`pos` 两边都是 token 的字节）
    fn token_across(&self, pos: usize) -> Option<Range<usize>> {
        let b = self.text.as_bytes();
        if pos <= self.span.start || pos >= self.span.end || !is_tok(b[pos - 1]) || !is_tok(b[pos])
        {
            return None;
        }
        let mut s = pos;
        while s > self.span.start && is_tok(b[s - 1]) {
            s -= 1;
        }
        let mut e = pos;
        while e < self.span.end && is_tok(b[e]) {
            e += 1;
        }
        Some(s..e)
    }

    /// 命中的那一段两头落在一处脱敏命中、或者一个会被打码的 token 中间时，把它整个算进来
    fn widen(&self, mut m: Range<usize>) -> Range<usize> {
        let masks = |t: &Range<usize>| {
            let s = &self.text[t.clone()];
            tw_secret::mask_body(s) != s
        };
        for _ in 0..4 {
            let was = m.clone();
            if let Some(h) = straddling(self.hits, m.start) {
                m.start = h.bytes.start;
            }
            if let Some(h) = straddling(self.hits, m.end) {
                m.end = h.bytes.end;
            }
            if let Some(t) = self.token_across(m.start).filter(masks) {
                m = t.start..m.end.max(t.end);
            }
            if let Some(t) = self.token_across(m.end).filter(masks) {
                m = m.start.min(t.start)..t.end;
            }
            m = self.snap(m.start, false)..self.snap(m.end, true);
            if m == was {
                break;
            }
        }
        m
    }

    /// 一段原文打过码、解开转义之后的样子
    fn show(&self, red: &Redaction, sent: &HashMap<&str, &str>, range: Range<usize>) -> String {
        let piece = red.piece(self.text, range, self.hits, sent);
        match &self.lit {
            Some(_) => locate::unescape(&piece),
            None => piece,
        }
    }

    /// `m` 这一处的前后文。`matched` 是命中的那一段写成什么（出站脱敏：打码后的值），
    /// `None` 就照原文打码，两头落在密钥中间的整个算进来。`draw` 把码位规则认的字符画出来
    fn excerpt(
        &self,
        m: Range<usize>,
        red: &Redaction,
        sent: &HashMap<&str, &str>,
        matched: Option<String>,
        draw: &dyn Fn(&str) -> String,
    ) -> Excerpt {
        let b = self.text.as_bytes();
        let mut m = self.snap(m.start, false)..self.snap(m.end, true);
        if matched.is_none() {
            m = self.widen(m);
        }
        // 前面那一段：不从一处命中、一个 token 的中间开始
        let mut start = self.snap(m.start.saturating_sub(WINDOW), true).min(m.start);
        if let Some(h) = straddling(self.hits, start) {
            start = h.bytes.end.min(m.start);
        }
        if start > self.span.start {
            while start < m.start && is_tok(b[start - 1]) && is_tok(b[start]) {
                start += 1;
            }
        }
        let start = self.snap(start, true).min(m.start);
        // 后面那一段：不在一处命中、一个 token 的中间结束
        let mut end = self.snap(m.end + WINDOW, false).max(m.end);
        if let Some(h) = straddling(self.hits, end) {
            end = h.bytes.start.max(m.end);
        }
        if end < self.span.end {
            while end > m.end && is_tok(b[end - 1]) && is_tok(b[end]) {
                end -= 1;
            }
        }
        let end = self.snap(end, false).max(m.end);

        let before = self.show(red, sent, start..m.start);
        let after = self.show(red, sent, m.end..end);
        let matched = matched.unwrap_or_else(|| draw(&self.show(red, sent, m)));
        Excerpt {
            before: draw(&last_chars(&before, CONTEXT_CHARS)),
            matched: cap(&matched, MATCHED_CHARS),
            after: draw(&first_chars(&after, CONTEXT_CHARS)),
        }
    }
}

fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 最多 `n` 个字符，截过的末尾是 `…`
fn cap(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n - 1).collect();
    out.push('…');
    out
}

/// 一处命中写成线上的样子。角色、工具名、路径是请求里来的字，也过一遍打码
fn location(red: &Redaction, place: Place, path: &str, e: Excerpt) -> HitLocation {
    HitLocation {
        part: place.part,
        message_index: place.message_index,
        role: place.role.map(|r| red.apply(&r)),
        tool: place.tool.map(|t| red.apply(&t)),
        path: red.apply(path),
        before: e.before,
        matched: e.matched,
        after: e.after,
    }
}

/// 超出 [`HIT_LOCATIONS_MAX`] 的几处
fn more(total: usize) -> u32 {
    u32::try_from(total.saturating_sub(HIT_LOCATIONS_MAX)).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------- 出站脱敏

/// 出站脱敏在一份正文上看的那一遍：算每一条记录的细节要的。
pub struct Seen<'a> {
    /// 看的那一份：客户端发来的请求体（内容过滤删过字的话是删过的那一份），插件改过的那一跳
    /// 是改过的那一份，WebSocket 上是客户端的那一帧
    pub body: &'a [u8],
    /// 客户端的格式。认不出的（嵌入、旧版补全……）是 `None`，只说得出路径
    pub dialect: Option<Dialect>,
    /// 在 `body` 上找到的命中（[`crate::guard::hits`]）
    pub hits: &'a [Hit],
    /// 这个请求的规则和账本：存下来的正文按它换、打码，前后文也是
    pub redaction: &'a Redaction,
    /// 拦截档：找到的值换成了占位符
    pub replaced: bool,
}

/// 出站脱敏报出去的每一项的细节，和 `found` 一一对应（[`Finding`] 是「一条规则 × 一个值」，
/// 位置是这个值在 [`Seen::body`] 里出现的每一处）。
pub fn secrets(seen: &Seen<'_>, found: &[Finding]) -> Vec<SecurityHitDetail> {
    let blank = |f: &Finding| secret_detail(seen, f, Vec::new(), 0, None);
    let Ok(text) = std::str::from_utf8(seen.body) else {
        return found.iter().map(blank).collect();
    };
    // 同一个值的命中归成一组，按第一次出现的先后（和 `findings` 合并的办法一样）
    let mut groups: Vec<(&Rule, &str, Vec<usize>)> = Vec::new();
    let mut by_value: HashMap<(&Rule, &str), usize> = HashMap::new();
    for (k, h) in seen.hits.iter().enumerate() {
        let value = &text[h.bytes.clone()];
        let g = *by_value.entry((&h.rule, value)).or_insert_with(|| {
            groups.push((&h.rule, value, Vec::new()));
            groups.len() - 1
        });
        groups[g].2.push(k);
    }
    // 报出去的一项按「规则 × 打码后的样子」找回它那一组：两个值打出来一样时按先后各取一组
    let mut by_masked: HashMap<(&Rule, String), VecDeque<usize>> = HashMap::new();
    for (g, (rule, value, _)) in groups.iter().enumerate() {
        by_masked
            .entry((*rule, tw_guard::redact::rules::masked(rule, value)))
            .or_default()
            .push_back(g);
    }
    let picked: Vec<Option<usize>> = found
        .iter()
        .map(|f| {
            by_masked
                .get_mut(&(&f.rule, f.masked.clone()))
                .and_then(VecDeque::pop_front)
        })
        .collect();
    // 要列出来的那几处一次走完原文，找出各在哪个字符串里
    let shown: Vec<usize> = picked
        .iter()
        .flatten()
        .flat_map(|&g| groups[g].2.iter().take(HIT_LOCATIONS_MAX).copied())
        .collect();
    let offsets: Vec<usize> = shown.iter().map(|&k| seen.hits[k].bytes.start).collect();
    let at: HashMap<usize, Option<(Vec<locate::Step>, locate::Scalar)>> = shown
        .iter()
        .copied()
        .zip(locate::scalars_at(text, &offsets))
        .collect();
    let body: Option<serde_json::Value> = seen
        .dialect
        .filter(|_| !shown.is_empty())
        .and_then(|_| serde_json::from_slice(seen.body).ok());
    let sent = seen.redaction.sent();
    let mut literals: HashMap<usize, Literal> = HashMap::new();
    found
        .iter()
        .zip(picked)
        .map(|(f, g)| {
            let Some(g) = g else {
                return blank(f);
            };
            let (_, value, ks) = &groups[g];
            let locations = ks
                .iter()
                .take(HIT_LOCATIONS_MAX)
                .map(|&k| {
                    let h = &seen.hits[k];
                    let (place, path, src) = match at.get(&k).cloned().flatten() {
                        Some((path, scalar)) => {
                            let place = match &body {
                                Some(v) => locate::place(seen.dialect, v, &path),
                                None => Place::of(Part::Message),
                            };
                            let lit = scalar.string.then(|| {
                                literals
                                    .entry(scalar.raw.start)
                                    .or_insert_with(|| Literal::new(text, scalar.raw.clone()))
                                    .clone()
                            });
                            let src = Source {
                                text,
                                span: scalar.raw,
                                hits: seen.hits,
                                lit,
                            };
                            (place, locate::path_text(&path), src)
                        }
                        // 不在哪个字符串里（原文不是 JSON）：整段是一段
                        None => {
                            let src = Source {
                                text,
                                span: 0..text.len(),
                                hits: seen.hits,
                                lit: None,
                            };
                            (Place::of(Part::Message), String::new(), src)
                        }
                    };
                    let e = src.excerpt(
                        h.bytes.clone(),
                        seen.redaction,
                        &sent,
                        Some(f.masked.clone()),
                        &|s| s.to_string(),
                    );
                    location(seen.redaction, place, &path, e)
                })
                .collect();
            secret_detail(seen, f, locations, more(ks.len()), Some(value))
        })
        .collect()
}

fn secret_detail(
    seen: &Seen<'_>,
    f: &Finding,
    locations: Vec<HitLocation>,
    more_locations: u32,
    value: Option<&str>,
) -> SecurityHitDetail {
    let rules = &seen.redaction.rules;
    let rule_snapshot = match &f.rule {
        Rule::Builtin(id) => RuleSnapshot {
            builtin: true,
            id: id.to_string(),
            name: tw_guard::redact::rules::builtin(id)
                .map_or(*id, |b| b.name)
                .to_string(),
            pattern: None,
            matching: None,
            core_version: core_version(),
        },
        Rule::Custom(name) => RuleSnapshot {
            builtin: false,
            id: name.to_string(),
            name: name.to_string(),
            pattern: rules.custom_pattern(name).map(str::to_string),
            matching: None,
            core_version: core_version(),
        },
    };
    let outcome_detail = if seen.replaced {
        OutcomeDetail::Replaced {
            placeholders: value
                .and_then(|v| seen.redaction.ledger.placeholder_of(v))
                .map(str::to_string)
                .into_iter()
                .collect(),
        }
    } else {
        OutcomeDetail::Recorded {}
    };
    SecurityHitDetail {
        direction: SecurityDirection::Request,
        locations,
        more_locations,
        rule_snapshot,
        outcome_detail,
    }
}

// ---------------------------------------------------------------- 内容过滤

/// 内容过滤查的那一份：算每一条记录的细节要的。
pub struct Screened<'a> {
    /// 查的那一份原文：客户端发来的请求体（删之前的），插件改过的那一份，WebSocket 上的一帧
    pub body: &'a [u8],
    /// 按消息结构查的格式。没法按结构读、整段只用码位规则查的（[`crate::guard::screen_raw`]）
    /// 是 `None`
    pub dialect: Option<Dialect>,
    pub rules: &'a tw_guard::content::Rules,
    /// 这个请求的脱敏规则和账本：前后文按它打码
    pub redaction: &'a Redaction,
}

/// 内容过滤每一条命中的细节，和 `sc.hits` 一一对应。位置是这条规则在查过的正文里的每一处
/// （删过一遍之后才冒出来的也算），删除规则删掉了几段就是几处。
pub fn content(src: &Screened<'_>, sc: &tw_guard::content::Screening) -> Vec<SecurityHitDetail> {
    if sc.hits.is_empty() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(src.body);
    let value: Option<serde_json::Value> = src
        .dialect
        .and_then(|_| serde_json::from_slice(src.body).ok());
    let (spots, places) = match (src.dialect, &value) {
        (Some(d), Some(v)) => tw_guard::content::places(src.rules, d, v),
        (Some(_), None) => (Vec::new(), Vec::new()),
        (None, _) => (Vec::new(), tw_guard::content::places_text(src.rules, &text)),
    };
    let sent = src.redaction.sent();
    let notice = sc
        .refusal()
        .map(|r| crate::error::client_notice(&super::refusal(&r.hit)));
    // 每个字符串编一次、找一次：几条规则命中在同一个字符串里时共用
    let mut encoded: HashMap<usize, (String, Vec<Hit>)> = HashMap::new();
    let raw_hits = match src.dialect {
        None => crate::guard::hits(&text, &src.redaction.rules),
        Some(_) => Vec::new(),
    };
    sc.hits
        .iter()
        .map(|h| {
            let rule = src
                .rules
                .rules
                .iter()
                .find(|r| r.id == h.hit.rule && r.custom == h.hit.custom);
            let at: &[(usize, Range<usize>)] = places
                .iter()
                .find(|p| p.rule == h.hit.rule && p.custom == h.hit.custom)
                .map_or(&[], |p| p.at.as_slice());
            let mut locations = Vec::with_capacity(at.len().min(HIT_LOCATIONS_MAX));
            for (spot, range) in at.iter().take(HIT_LOCATIONS_MAX) {
                let loc = match (src.dialect, &value) {
                    (Some(d), Some(v)) => {
                        let Some(s) = spots.get(*spot) else { continue };
                        let decoded = s.get(v).unwrap_or_default();
                        // 解开的字符串重新写成 JSON 字符串：和存下来的正文一样按 JSON 原文找、
                        // 打码，位置换算回去
                        let (enc, hits) = encoded.entry(*spot).or_insert_with(|| {
                            let enc = serde_json::to_string(decoded).unwrap_or_default();
                            let hits = crate::guard::hits(&enc, &src.redaction.rules);
                            (enc, hits)
                        });
                        let lit = Literal::new(enc, 1..enc.len().saturating_sub(1).max(1));
                        let m = lit.raw_at(range.start)..lit.raw_at(range.end);
                        let source = Source {
                            text: enc,
                            span: lit.range.clone(),
                            hits,
                            lit: Some(lit),
                        };
                        let e = source.excerpt(m, src.redaction, &sent, None, &|t| match rule {
                            Some(r) => r.draw(t, false),
                            None => t.to_string(),
                        });
                        let place = locate::place(Some(d), v, s.path());
                        location(src.redaction, place, &locate::path_text(s.path()), e)
                    }
                    (Some(_), None) => continue,
                    (None, _) => {
                        let source = Source {
                            text: &text,
                            span: 0..text.len(),
                            hits: &raw_hits,
                            lit: None,
                        };
                        let e = source.excerpt(range.clone(), src.redaction, &sent, None, &|t| {
                            match rule {
                                Some(r) => r.draw(t, true),
                                None => t.to_string(),
                            }
                        });
                        location(src.redaction, Place::of(Part::Message), "", e)
                    }
                };
                locations.push(loc);
            }
            let rule_snapshot = RuleSnapshot {
                builtin: !h.hit.custom,
                id: h.hit.rule.clone(),
                name: h.hit.name.clone(),
                pattern: rule.filter(|r| r.custom).map(|r| r.pattern.clone()),
                matching: Some(tw_api::ContentMatch::of(h.hit.matching)),
                core_version: core_version(),
            };
            let outcome_detail = match h.outcome {
                tw_guard::content::Outcome::Recorded => OutcomeDetail::Recorded {},
                tw_guard::content::Outcome::Stripped => OutcomeDetail::Stripped {
                    segments: u32::try_from(at.len()).unwrap_or(u32::MAX),
                },
                tw_guard::content::Outcome::Blocked => OutcomeDetail::Blocked {
                    client_notice: notice.clone().unwrap_or_default(),
                },
            };
            SecurityHitDetail {
                direction: SecurityDirection::Request,
                locations,
                more_locations: more(at.len()),
                rule_snapshot,
                outcome_detail,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- 工具调用审查

/// 一个命中规则的工具调用的细节。位置是这条规则在这个调用的参数里的每一处；切断的话，
/// `notice` 是客户端在它的位置上收到的那句话。
///
/// 参数是还原过占位符的那一份：前后文、留下来的参数都照 `redaction` 换回占位符、打码。
pub fn tool_call(
    v: &tw_guard::tools::wall::Verdict,
    blocked: bool,
    redaction: &Redaction,
    notice: Option<&tw_types::Msg>,
) -> SecurityHitDetail {
    let args = v.arguments.as_str();
    let hits = crate::guard::hits(args, &redaction.rules);
    let sent = redaction.sent();
    let source = Source {
        text: args,
        span: 0..args.len(),
        hits: &hits,
        lit: None,
    };
    let locations = v
        .places
        .iter()
        .take(HIT_LOCATIONS_MAX)
        .filter(|r| r.end <= args.len())
        .map(|r| {
            let e = source.excerpt(r.clone(), redaction, &sent, None, &|s| s.to_string());
            let place = Place {
                tool: Some(v.tool.clone()),
                ..Place::of(Part::ToolCall)
            };
            location(redaction, place, &v.path, e)
        })
        .collect();
    let rule_snapshot = RuleSnapshot {
        builtin: !v.custom,
        id: v.rule.clone(),
        name: v.name.clone(),
        pattern: v.custom.then(|| v.pattern.clone()),
        matching: None,
        core_version: core_version(),
    };
    let outcome_detail = if blocked {
        let shown = redaction.apply(args);
        let truncated = v.capped || shown.len() > ARGUMENTS_MAX;
        let mut end = shown.len().min(ARGUMENTS_MAX);
        while !shown.is_char_boundary(end) {
            end -= 1;
        }
        OutcomeDetail::Cut {
            tool: v.tool.clone(),
            arguments: shown[..end].to_string(),
            truncated,
            client_notice: notice.map(crate::error::client_notice).unwrap_or_default(),
        }
    } else {
        OutcomeDetail::Recorded {}
    };
    SecurityHitDetail {
        direction: SecurityDirection::Response,
        locations,
        more_locations: more(v.places.len()),
        rule_snapshot,
        outcome_detail,
    }
}

#[cfg(test)]
mod tests;

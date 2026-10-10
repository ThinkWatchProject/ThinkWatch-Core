//! 一次会话读成一段对话（`GET /sessions/{id}/transcript`）。
//!
//! Claude Code 这类客户端每一轮都把整段历史发上来：请求 i 的消息 = 请求 i-1 的消息 + 上一轮
//! 的回答 + 新的用户消息或工具结果。所以按会话里的顺序一个一个往下读，**每个请求只交出它
//! 新带来的那几条**；回答从存下来的响应里读（见 [`answer`]）。
//!
//! # 怎么认「新的那几条」
//!
//! 上一个读得懂的请求的消息是这一个的开头（逐条比指纹），剩下的就是新的；剩下的第一条是
//! 助手消息的话，它是上一轮的回答，已经在上一轮的 `output` 里了，去掉。对不上（压缩过、
//! 改过历史），或者上一个请求读不懂，就交出这个请求的整段历史，标 `restart`。
//!
//! 比的是规整过的样子（见 [`read::Piece`]）：缓存断点、推理签名、键的顺序都不算。**推理
//! 整个不算**：有的客户端到了下一轮就把前面的推理去掉（接口本来也不读它们），算上的话，
//! 这样的客户端每一轮都对不上。
//!
//! # 读多少、留多少
//!
//! 几百轮的会话，每个请求几 MB。每份正文读一次、解析一次，读完这一轮就丢；从一个请求留到
//! 下一个的只有每条消息 16 字节的指纹和系统提示。
//!
//! **读过的不再读**（[`Cache`]）：界面开着一次会话时，每来一轮就要一次，从头读的话，两百轮
//! 的会话每来一轮就要读、解析几百 MB。记着最近几次会话读到了哪儿（读完时的 [`Chain`] 和读出
//! 来的几轮），下一次只读新来的请求。只记定下来的那一截：正文还可能变的那几轮每次都重读。

mod answer;
mod read;

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tw_api::{TranscriptGap, TranscriptMessage, TranscriptPart, TranscriptRole, TranscriptTurn};
use tw_dialect::ir::Dialect;

use crate::blobs::{Blobs, Which};
use crate::db::RequestRow;
use crate::search::text::{client_dialect, dialect_of};
use read::{Input, Piece, Role};

/// 一轮结束这么久之后就定下来了：它的正文要么已经在盘上，要么不会再来。正文和那一行各走
/// 各的路落盘（见 `crate::task`），先后差不了几毫秒，排队排得再久也到不了这么久
const SETTLE_MS: i64 = 120_000;

/// 记着最近几次会话读到了哪儿。一次几百轮的会话记下的是几 MB 的文字
const CACHED: usize = 4;

/// 整段读一遍，不记。`rows` 是这次会话的请求，和会话详情同样的顺序
/// （[`crate::Db::session_requests`]）。
///
/// **已脱敏**，和请求详情里的正文同一套打码（`tw_secret::mask_body`）。
pub fn build(session: &str, rows: &[RequestRow], blobs: &Blobs) -> tw_api::Transcript {
    Cache::default().read(
        blobs,
        &Ask {
            session,
            rows,
            from_turn: 0,
            running: None,
            now_ms: i64::MAX,
        },
    )
}

/// 要读哪一次会话、交出去哪几轮（见 [`Cache::read`]）。
pub struct Ask<'a> {
    pub session: &'a str,
    /// 这次会话的请求，和会话详情同样的顺序（[`crate::Db::session_requests`]）
    pub rows: &'a [RequestRow],
    /// 只交出从这一轮起的那些（从 0 数）。0 是整段
    pub from_turn: usize,
    /// 这次会话里还在跑的请求里最早开始的那个：（开始的时刻，请求号）。它落库时排在开始的
    /// 那一刻，不一定排在最后：排在它后面的几轮还没定下来（见
    /// [`tw_api::Transcript::settled_turns`]）
    pub running: Option<(i64, i64)>,
    /// 此刻，Unix 毫秒
    pub now_ms: i64,
}

/// 最近几次会话读到了哪儿（见 [`Cache::read`]）。存储层一份（[`crate::Recorder::transcripts`]）。
#[derive(Default)]
pub struct Cache {
    kept: std::sync::Mutex<Kept>,
}

#[derive(Default)]
struct Kept {
    /// 正文回收每删掉一次东西就加一。读到一半它变了的，读出来的就不记
    generation: u64,
    /// 最近用过的在最后
    sessions: Vec<(String, Arc<Progress>)>,
}

/// 一次会话读到了哪儿：读过的那几个请求，读完它们时的 [`Chain`]，读出来的几轮（没打码）。
#[derive(Clone, Default)]
struct Progress {
    ids: Vec<i64>,
    chain: Chain,
    doc: Doc,
}

/// 读出来的东西：系统提示和每一轮。**没打码**：前后比对、换工具调用的号都要原文
#[derive(Clone, Default)]
struct Doc {
    system: Option<String>,
    turns: Vec<TranscriptTurn>,
}

impl Cache {
    /// 正文回收删掉了东西：记着的都不作数了
    pub fn invalidate(&self) {
        let mut k = self.kept();
        k.generation += 1;
        k.sessions.clear();
    }

    /// 锁中毒了照样用里面的：记着的东西坏不了谁，最多重读一遍
    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 读成一段对话。**已脱敏**，和请求详情里的正文同一套打码（`tw_secret::mask_body`）。
    ///
    /// **接着上次读到的地方往下读**：记着的那几个请求还是这次会话开头的那几个，就只读后面
    /// 新来的；中间插进来一个（开始得早、结束得晚的请求）、开头的过期删掉了，都从头读。
    /// 记下的是定下来的那一截：最后几轮的正文可能还在路上，它们下一次还要重读。
    pub fn read(&self, blobs: &Blobs, ask: &Ask) -> tw_api::Transcript {
        let (generation, kept) = {
            let mut k = self.kept();
            let kept = k
                .sessions
                .iter()
                .position(|(s, _)| s == ask.session)
                .map(|i| {
                    let hit = k.sessions.remove(i);
                    let p = hit.1.clone();
                    k.sessions.push(hit);
                    p
                });
            (k.generation, kept)
        };
        let base = kept
            .filter(|p| {
                p.ids.len() <= ask.rows.len()
                    && p.ids.iter().zip(ask.rows).all(|(id, row)| *id == row.id)
            })
            .unwrap_or_default();
        let fresh = &ask.rows[base.ids.len()..];
        // 定下来的那一截有多长。记着的那一截本来就是定下来的
        let (whole, settled) = if fresh.is_empty() {
            let n = base.ids.len();
            (base, n)
        } else {
            let (whole, keep) = continue_reading(&base, fresh, blobs, ask.now_ms);
            let n = keep.ids.len();
            self.keep(ask.session, generation, keep);
            (whole, n)
        };

        let total = whole.doc.turns.len();
        // 后面的请求还会改写的那一轮：最后一个读得懂的请求的回答（见 `same_calls`）
        let rewritable = match whole.chain.last {
            Some((at, true)) => at,
            _ => total,
        };
        let before_running = ask.running.map_or(total, |r| {
            ask.rows.partition_point(|row| (row.at_ms, row.id) < r)
        });
        let from = ask.from_turn.min(total);
        let mut t = tw_api::Transcript {
            session: ask.session.to_string(),
            system: whole.doc.system.clone(),
            total_turns: total as u32,
            settled_turns: settled.min(rewritable).min(before_running) as u32,
            turns: whole.doc.turns[from..].to_vec(),
        };
        mask(&mut t);
        t
    }

    /// 记下一次会话读到的地方。正文回收在这期间删过东西的不记：读的时候它们可能还在
    fn keep(&self, session: &str, generation: u64, p: Arc<Progress>) {
        let mut k = self.kept();
        if k.generation != generation {
            return;
        }
        k.sessions.retain(|(s, _)| s != session);
        k.sessions.push((session.to_string(), p));
        if k.sessions.len() > CACHED {
            k.sessions.remove(0);
        }
    }
}

/// 从 `base` 读到的地方接着读 `fresh`。返回读完的样子，和其中定下来的那一截（要记下的）。
///
/// 一轮定下来了：它结束得够久（[`SETTLE_MS`]），或者它什么都不缺。还缺着正文、又刚结束的
/// 那一轮，正文可能还在路上；从它起都不算定下来。
fn continue_reading(
    base: &Progress,
    fresh: &[RequestRow],
    blobs: &Blobs,
    now_ms: i64,
) -> (Arc<Progress>, Arc<Progress>) {
    let mut p = base.clone();
    let mut settled: Option<Progress> = None;
    for row in fresh {
        let ended = row.at_ms.saturating_add(row.duration_ms.unwrap_or(0));
        let recent = now_ms.saturating_sub(ended) < SETTLE_MS;
        // 这一个可能定不下来：先留着读它之前的样子。读它只会改它自己和上一个读得懂的那一轮
        let before = (settled.is_none() && recent).then(|| {
            let last = p.chain.last.map(|(at, _)| (at, p.doc.turns[at].clone()));
            (p.chain.clone(), p.doc.system.clone(), last)
        });
        p.chain.turn(row, blobs, &mut p.doc);
        p.ids.push(row.id);
        if let Some((chain, system, last)) = before
            && p.doc.turns.last().is_some_and(|t| !t.gaps.is_empty())
        {
            let n = p.ids.len() - 1;
            let mut turns = p.doc.turns[..n].to_vec();
            if let Some((at, turn)) = last {
                turns[at] = turn;
            }
            settled = Some(Progress {
                ids: p.ids[..n].to_vec(),
                chain,
                doc: Doc { system, turns },
            });
        }
    }
    let p = Arc::new(p);
    let settled = settled.map_or_else(|| p.clone(), Arc::new);
    (p, settled)
}

/// 一条消息比对用的指纹
type Fp = [u8; 16];

/// 读到哪儿了：上一个读得懂的请求留下来的东西。
#[derive(Clone, Default)]
struct Chain {
    /// 它的每条消息的指纹（只有推理的消息不比，不在里面）。还没有读得懂的请求是 None
    prev: Option<Vec<Fp>>,
    /// 它的系统提示
    system: Option<String>,
    /// 它之后有没有读不懂的请求
    broken: bool,
    /// 它是第几轮，回答完整地交出去了没有
    last: Option<(usize, bool)>,
}

impl Chain {
    fn turn(&mut self, row: &RequestRow, blobs: &Blobs, t: &mut Doc) {
        let mut turn = TranscriptTurn {
            id: row.id.to_string(),
            restart: false,
            system_changed: None,
            input: Vec::new(),
            output: Vec::new(),
            gaps: Vec::new(),
        };
        // 不生成回答的调用（数 token、Responses 的压缩）：问的是这段对话，不是对话里的一句。
        // 不读，也不和前后比
        let Some(client) = client_dialect(&row.path) else {
            t.turns.push(turn);
            return;
        };
        let mut freeform = HashSet::new();
        let readable = match request(row, blobs) {
            Ok(v) => {
                self.read(client, &v, &mut turn, t, &mut freeform);
                true
            }
            Err(gap) => {
                turn.gaps.push(gap);
                self.broken = true;
                false
            }
        };
        let (output, gap) = output(row, blobs, upstream_of(row, client), &freeform);
        turn.output = output;
        turn.gaps.extend(gap);
        if readable {
            let shown = !turn.output.is_empty()
                && !turn.gaps.iter().any(|g| {
                    matches!(
                        g,
                        TranscriptGap::ResponseMissing
                            | TranscriptGap::ResponseTruncated
                            | TranscriptGap::ResponseUnreadable
                    )
                });
            self.last = Some((t.turns.len(), shown));
        }
        t.turns.push(turn);
    }

    /// 读得懂的请求：系统提示，新的消息
    fn read(
        &mut self,
        client: Dialect,
        v: &Value,
        turn: &mut TranscriptTurn,
        t: &mut Doc,
        freeform: &mut HashSet<String>,
    ) {
        let read::Body {
            system,
            items,
            freeform: tools,
        } = read::body(client, v);
        freeform.extend(tools);
        let system = system
            .iter()
            .map(|s| s.as_ref())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        match &self.system {
            None => t.system = (!system.is_empty()).then(|| system.clone()),
            Some(prev) if *prev != system => turn.system_changed = Some(system.clone()),
            Some(_) => {}
        }

        let messages = merge(items);
        let fps: Vec<(usize, Fp)> = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| fp(m).map(|f| (i, f)))
            .collect();
        let start = match (&self.prev, self.broken) {
            // 前面有读不懂的：它带来的那几条没交出去过，从头交
            (_, true) => None,
            (None, false) => Some(0),
            (Some(prev), false) => continues(prev, &fps),
        };
        turn.restart = start.is_none();
        let mut new = &messages[start.unwrap_or(0)..];
        // 剩下的第一条助手消息是上一轮的回答，已经在上一轮的 `output` 里。**那一轮的回答没有
        // 完整交出去的不去掉**（没存下、只存了开头、读不懂）：这一条就是它说过什么的记录
        if start.is_some()
            && let Some((at, true)) = self.last
            && let Some(said) = new.first().filter(|m| m.role == Role::Assistant)
        {
            same_calls(&mut t.turns[at].output, said);
            new = &new[1..];
        }
        turn.input = new.iter().map(message).collect();

        self.prev = Some(fps.into_iter().map(|(_, f)| f).collect());
        self.system = Some(system);
        self.broken = false;
    }
}

/// 读请求体：没存下来的、只存了开头的、解析不了的各是一种缺口。
fn request(row: &RequestRow, blobs: &Blobs) -> Result<Value, TranscriptGap> {
    let raw = blobs
        .get(row.at_ms, row.id, Which::Request)
        .ok_or(TranscriptGap::RequestMissing)?;
    if blobs
        .original_len(row.at_ms, row.id, Which::Request)
        .is_some_and(|n| n > raw.len())
    {
        return Err(TranscriptGap::RequestTruncated);
    }
    serde_json::from_slice(&raw).map_err(|_| TranscriptGap::RequestTruncated)
}

/// 回答它的那一家说的格式：转换过的记在行上，直通的就是客户端那一种（和按正文找一样）
fn upstream_of(row: &RequestRow, client: Dialect) -> Dialect {
    row.translated
        .as_deref()
        .and_then(|j| serde_json::from_str::<tw_api::TranslatedView>(j).ok())
        .map_or(client, |t| dialect_of(t.to))
}

/// 这一轮的回答，和读不出来的那个缺口。
fn output(
    row: &RequestRow,
    blobs: &Blobs,
    upstream: Dialect,
    freeform: &HashSet<String>,
) -> (Vec<TranscriptPart>, Option<TranscriptGap>) {
    // 上游接下了、回了 2xx 的才有回答可读
    let answered = row.status.is_some_and(|s| (200..300).contains(&s));
    let Some(body) = blobs.get(row.at_ms, row.id, Which::Response) else {
        // 没走到上游的、上游回了错误的，本来就没有回答；一个字节都没从上游收到客户端就走了的
        // 也是
        let missing = answered && row.received_bytes != Some(0);
        return (
            Vec::new(),
            missing.then_some(TranscriptGap::ResponseMissing),
        );
    };
    // 上游回了错误：存下来的是错误，不是回答。失败在会话详情里已经有了，这里不重复
    if !answered {
        return (Vec::new(), None);
    }
    let truncated = blobs
        .original_len(row.at_ms, row.id, Which::Response)
        .is_some_and(|n| n > body.len());
    let a = answer::read(&body, upstream, freeform);
    let gap = if truncated {
        Some(TranscriptGap::ResponseTruncated)
    } else if !a.recognized {
        Some(TranscriptGap::ResponseUnreadable)
    } else {
        None
    };
    (a.parts, gap)
}

/// 一条消息。同一个角色连着的几条并成一条：Responses 里推理、文字、几个调用是几个输入项，
/// 可它们是同一次回答；Chat 里几个调用的结果是几条 `tool` 消息。用户和系统消息不并
struct Message<'a> {
    role: Role,
    pieces: Vec<Piece<'a>>,
}

fn merge(items: Vec<read::Item<'_>>) -> Vec<Message<'_>> {
    let mut out: Vec<Message> = Vec::with_capacity(items.len());
    for it in items {
        if it.pieces.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last)
                if last.role == it.role && matches!(it.role, Role::Assistant | Role::Tool) =>
            {
                last.pieces.extend(it.pieces)
            }
            _ => out.push(Message {
                role: it.role,
                pieces: it.pieces,
            }),
        }
    }
    out
}

/// 一条消息比对用的指纹。推理不算（见模块的说明）；除了推理什么都没有的消息不比，是 None。
fn fp(m: &Message) -> Option<Fp> {
    fn field(h: &mut blake3::Hasher, s: &str) {
        h.update(&(s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    }
    let mut h = blake3::Hasher::new();
    h.update(&[m.role as u8]);
    let mut any = false;
    for p in &m.pieces {
        match p {
            Piece::Thinking(_) => continue,
            Piece::Text(t) => {
                h.update(&[1]);
                field(&mut h, t);
            }
            Piece::ToolCall { id, name, input } => {
                h.update(&[2]);
                field(&mut h, id);
                field(&mut h, name);
                match input {
                    Input::Args(s) | Input::Raw(s) => {
                        h.update(&[0]);
                        field(&mut h, s);
                    }
                    // 键按字母排着写出来（serde_json 的对象就是这么存的）：顺序不算
                    Input::Json(v) => {
                        h.update(&[1]);
                        let _ = serde_json::to_writer(&mut h, v);
                    }
                }
            }
            Piece::ToolResult {
                call_id,
                text,
                is_error,
            } => {
                h.update(&[3, *is_error as u8]);
                field(&mut h, call_id);
                field(&mut h, text);
            }
            Piece::Image {
                media_type,
                bytes,
                data,
            } => {
                h.update(&[4]);
                field(&mut h, media_type.unwrap_or_default());
                h.update(&bytes.unwrap_or(u64::MAX).to_le_bytes());
                field(&mut h, data.unwrap_or_default());
            }
            Piece::Other(label) => {
                h.update(&[5]);
                field(&mut h, label);
            }
        }
        any = true;
    }
    let mut out = [0; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    any.then_some(out)
}

/// 上一个请求的消息是不是这一个的开头。是的话，新的从第几条开始
fn continues(prev: &[Fp], cur: &[(usize, Fp)]) -> Option<usize> {
    if cur.len() < prev.len() || cur.iter().zip(prev).any(|((_, a), b)| a != b) {
        return None;
    }
    Some(prev.len().checked_sub(1).map_or(0, |last| cur[last].0 + 1))
}

/// 上一轮回答里工具调用的号，换成客户端记下的那个。
///
/// 转换过格式的回答里，号是上游给的（Gemini 根本不给，网关和这里各自现编一个）；下一轮的
/// 工具结果认的是客户端拿到的那个号。名字一个一个对得上才换。
fn same_calls(output: &mut [TranscriptPart], said: &Message) {
    let theirs: Vec<(&str, &str)> = said
        .pieces
        .iter()
        .filter_map(|p| match p {
            Piece::ToolCall { id, name, .. } => Some((id.as_ref(), name.as_ref())),
            _ => None,
        })
        .collect();
    let mine: Vec<(&mut String, &mut String)> = output
        .iter_mut()
        .filter_map(|p| match p {
            TranscriptPart::ToolCall { id, name, .. } => Some((id, name)),
            _ => None,
        })
        .collect();
    if mine.len() != theirs.len()
        || mine
            .iter()
            .zip(&theirs)
            .any(|((_, name), (_, said))| name.as_str() != *said)
    {
        return;
    }
    for ((id, _), (said, _)) in mine.into_iter().zip(theirs) {
        if id != said {
            *id = said.to_string();
        }
    }
}

fn message(m: &Message) -> TranscriptMessage {
    TranscriptMessage {
        role: match m.role {
            Role::User => TranscriptRole::User,
            Role::Assistant => TranscriptRole::Assistant,
            Role::Tool => TranscriptRole::Tool,
            Role::System => TranscriptRole::System,
        },
        parts: m.pieces.iter().map(part).collect(),
    }
}

/// 一块交出去的样子。**还没打码**：打码在最后一起做
fn part(p: &Piece) -> TranscriptPart {
    match p {
        Piece::Text(t) => TranscriptPart::Text {
            text: t.to_string(),
        },
        Piece::Thinking(t) => TranscriptPart::Thinking {
            text: t.to_string(),
        },
        Piece::ToolCall { id, name, input } => TranscriptPart::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input: input.text(),
        },
        Piece::ToolResult {
            call_id,
            text,
            is_error,
        } => TranscriptPart::ToolResult {
            call_id: call_id.to_string(),
            text: text.to_string(),
            is_error: *is_error,
        },
        Piece::Image {
            media_type, bytes, ..
        } => TranscriptPart::Image {
            media_type: media_type.map(str::to_string),
            bytes: *bytes,
        },
        Piece::Other(label) => TranscriptPart::Other {
            label: label.to_string(),
        },
    }
}

/// 打码，和请求详情里的正文同一套。**最后一起做**：前后比对、换工具调用的号都要原文
fn mask(t: &mut tw_api::Transcript) {
    fn m(s: &mut String) {
        if !s.is_empty() {
            *s = tw_secret::mask_body(s);
        }
    }
    if let Some(s) = &mut t.system {
        m(s);
    }
    for turn in &mut t.turns {
        if let Some(s) = &mut turn.system_changed {
            m(s);
        }
        let parts = turn
            .input
            .iter_mut()
            .flat_map(|msg| msg.parts.iter_mut())
            .chain(turn.output.iter_mut());
        for p in parts {
            match p {
                TranscriptPart::Text { text } | TranscriptPart::Thinking { text } => m(text),
                TranscriptPart::ToolCall { id, name, input } => {
                    m(id);
                    m(name);
                    m(input);
                }
                TranscriptPart::ToolResult { call_id, text, .. } => {
                    m(call_id);
                    m(text);
                }
                TranscriptPart::Image { media_type, .. } => {
                    if let Some(s) = media_type {
                        m(s);
                    }
                }
                TranscriptPart::Other { label } => m(label),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tw_api::{
        Transcript, TranscriptGap as Gap, TranscriptMessage, TranscriptPart as P,
        TranscriptRole as R,
    };

    use super::*;
    use crate::db::Db;
    use crate::db::tests::row;

    const NOW: i64 = 1_790_000_000_000;

    /// 一份测试用的盘：请求库和正文目录，记录都归在会话 `s` 里
    struct Disk {
        _dir: tempfile::TempDir,
        db: Db,
        blobs: Blobs,
        next: i64,
    }

    impl Disk {
        fn new() -> Disk {
            let dir = tempfile::tempdir().unwrap();
            Disk {
                blobs: Blobs::new(dir.path().join("blobs")),
                db: Db::in_memory().unwrap(),
                _dir: dir,
                next: 0,
            }
        }

        /// 一轮：请求体和回答都存下了
        fn turn(&mut self, path: &str, request: &Value, response: &[u8]) -> i64 {
            let request = request.to_string();
            self.put(path, Some(request.as_bytes()), Some(response), |_| {})
        }

        /// 一轮，存下了哪些自己定；`edit` 改这一行的记录
        fn put(
            &mut self,
            path: &str,
            request: Option<&[u8]>,
            response: Option<&[u8]>,
            edit: impl FnOnce(&mut RequestRow),
        ) -> i64 {
            self.next += 1;
            let id = self.next;
            let mut r = row(id, NOW + id * 1000);
            r.path = path.into();
            r.session = Some("s".into());
            edit(&mut r);
            self.db.insert(&r).unwrap();
            if let Some(b) = request {
                assert!(self.blobs.put(r.at_ms, id, Which::Request, b));
            }
            if let Some(b) = response {
                assert!(self.blobs.put(r.at_ms, id, Which::Response, b));
            }
            id
        }

        /// 存下来的只是开头：原本有 `len` 那么长
        fn cut(&self, id: i64, which: Which, len: usize) {
            let at = NOW + id * 1000;
            let body = self.blobs.get(at, id, which).unwrap();
            assert!(self.blobs.put_with_len(at, id, which, &body, len));
        }

        fn transcript(&self) -> Transcript {
            let rows = self.db.session_requests("s").unwrap();
            build("s", &rows, &self.blobs)
        }
    }

    fn sse(frames: &[(&str, Value)]) -> Vec<u8> {
        let mut s = String::new();
        for (event, data) in frames {
            if !event.is_empty() {
                s.push_str(&format!("event: {event}\n"));
            }
            s.push_str(&format!("data: {data}\n\n"));
        }
        s.into_bytes()
    }

    /// 一段字切成两半，按字符切
    fn halves(s: &str) -> (String, String) {
        let n = s.chars().count() / 2;
        (s.chars().take(n).collect(), s.chars().skip(n).collect())
    }

    /// Anthropic 的流：给几块内容，写成每块的开始、两段增量、结束。服务端工具那样的块只有
    /// 开始和结束
    fn anthropic_stream(blocks: &[Value]) -> Vec<u8> {
        let mut f = vec![(
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-4-5",
                "usage": {"input_tokens": 10}}}),
        )];
        for (i, b) in blocks.iter().enumerate() {
            let delta = |d: Value| {
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": i, "delta": d}),
                )
            };
            let start = |block: Value| {
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": i, "content_block": block}),
                )
            };
            match b["type"].as_str().unwrap() {
                "text" => {
                    f.push(start(json!({"type": "text", "text": ""})));
                    let (a, z) = halves(b["text"].as_str().unwrap());
                    f.push(delta(json!({"type": "text_delta", "text": a})));
                    f.push(delta(json!({"type": "text_delta", "text": z})));
                }
                "thinking" => {
                    f.push(start(json!({"type": "thinking", "thinking": ""})));
                    f.push(delta(
                        json!({"type": "thinking_delta", "thinking": b["thinking"]}),
                    ));
                    f.push(delta(
                        json!({"type": "signature_delta", "signature": b["signature"]}),
                    ));
                }
                "tool_use" => {
                    f.push(start(
                        json!({"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}),
                    ));
                    let (a, z) = halves(&b["input"].to_string());
                    f.push(delta(
                        json!({"type": "input_json_delta", "partial_json": a}),
                    ));
                    f.push(delta(
                        json!({"type": "input_json_delta", "partial_json": z}),
                    ));
                }
                _ => f.push(start(b.clone())),
            }
            f.push((
                "content_block_stop",
                json!({"type": "content_block_stop", "index": i}),
            ));
        }
        f.push((
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
        ));
        f.push(("message_stop", json!({"type": "message_stop"})));
        sse(&f)
    }

    fn text(t: &str) -> P {
        P::Text { text: t.into() }
    }

    fn thinking(t: &str) -> P {
        P::Thinking { text: t.into() }
    }

    fn call(id: &str, name: &str, input: &str) -> P {
        P::ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        }
    }

    fn result(call_id: &str, t: &str) -> P {
        P::ToolResult {
            call_id: call_id.into(),
            text: t.into(),
            is_error: false,
        }
    }

    fn msg(role: R, parts: Vec<P>) -> TranscriptMessage {
        TranscriptMessage { role, parts }
    }

    fn user(t: &str) -> Value {
        json!({"role": "user", "content": t})
    }

    fn assistant(t: &str) -> Value {
        json!({"role": "assistant", "content": t})
    }

    /// Anthropic 的一轮：一段 system、给出的消息，流式回答一段话
    fn anthropic(messages: &[Value]) -> Value {
        json!({"model": "claude-sonnet-4-5", "system": "你是助手", "stream": true, "messages": messages})
    }

    fn says(t: &str) -> Vec<u8> {
        anthropic_stream(&[json!({"type": "text", "text": t})])
    }

    fn with_cache(mut m: Value) -> Value {
        if let Some(last) = m["content"].as_array_mut().and_then(|c| c.last_mut()) {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        m
    }

    // ───────────────────────────────────────────────── 四种客户端格式

    /// 照 Claude Code 的样子：系统提示是几块，缓存断点每一轮挪到最后一条上，推理带着签名，
    /// 工具调用和结果，结果里有一张截图。每一轮只交出新的那几条
    #[test]
    fn a_claude_code_session_reads_turn_by_turn() {
        let mut d = Disk::new();
        let system = json!([
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.0.0"},
            {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}}
        ]);
        let tools = json!([{"name": "Read", "input_schema": {"type": "object"}},
            {"name": "Edit", "input_schema": {"type": "object"}}]);
        let body = |messages: Vec<Value>| {
            json!({"model": "claude-sonnet-4-5", "system": system, "tools": tools, "max_tokens": 32000,
                "stream": true, "messages": messages})
        };
        let u1 = json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>上下文</system-reminder>"},
            {"type": "text", "text": "修一下 main.rs 里的 bug"}
        ]});
        let a1 = |signature: &str| {
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "先读文件", "signature": signature},
                {"type": "text", "text": "我先看看文件。"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "src/main.rs"}}
            ]})
        };
        let r1 = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
            "content": "fn main() {\n    let key = \"sk-ant-api03-SECRETSECRETSECRET\";\n}"}]});
        let a2 = |with_thinking: bool| {
            let mut blocks = vec![
                json!({"type": "text", "text": "密钥写死了，我来改。"}),
                json!({"type": "tool_use", "id": "toolu_2", "name": "Edit",
                    "input": {"file_path": "src/main.rs", "old": "let key", "new": "let key = env()"}}),
            ];
            if with_thinking {
                blocks.insert(
                    0,
                    json!({"type": "thinking", "thinking": "要改成读环境变量", "signature": "sig-2"}),
                );
            }
            json!({"role": "assistant", "content": blocks})
        };
        let png = "QUJD".repeat(10);
        let r2 = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                {"type": "text", "text": "已修改"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}
            ]},
            {"type": "text", "text": "看起来不错，再跑一下测试"}
        ]});

        d.turn(
            "/v1/messages",
            &body(vec![with_cache(u1.clone())]),
            &anthropic_stream(&[
                json!({"type": "thinking", "thinking": "先读文件", "signature": "sig-1"}),
                json!({"type": "text", "text": "我先看看文件。"}),
                json!({"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "src/main.rs"}}),
            ]),
        );
        d.turn(
            "/v1/messages",
            &body(vec![u1.clone(), a1("sig-1"), with_cache(r1.clone())]),
            &anthropic_stream(&[
                json!({"type": "thinking", "thinking": "要改成读环境变量", "signature": "sig-2"}),
                json!({"type": "text", "text": "密钥写死了，我来改。"}),
                json!({"type": "tool_use", "id": "toolu_2", "name": "Edit",
                    "input": {"file_path": "src/main.rs", "old": "let key", "new": "let key = env()"}}),
            ]),
        );
        // 第三轮：第一轮推理的签名换了一个，第二轮的推理被客户端去掉了 —— 都不算改过历史
        d.turn(
            "/v1/messages",
            &body(vec![u1, a1("sig-1-again"), r1, a2(false), with_cache(r2)]),
            &says("好的。"),
        );

        let t = d.transcript();
        assert_eq!(t.session, "s");
        assert_eq!(
            t.system.as_deref(),
            Some("x-anthropic-billing-header: cc_version=2.0.0\n\nYou are Claude Code.")
        );
        assert_eq!(t.turns.len(), 3);
        assert_eq!(
            t.turns.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        for x in &t.turns {
            assert!(!x.restart, "{x:?}");
            assert_eq!(x.system_changed, None);
            assert!(x.gaps.is_empty(), "{x:?}");
        }

        let first = &t.turns[0];
        assert_eq!(
            first.input,
            [msg(
                R::User,
                vec![
                    text("<system-reminder>上下文</system-reminder>"),
                    text("修一下 main.rs 里的 bug")
                ]
            )]
        );
        assert_eq!(
            first.output,
            [
                thinking("先读文件"),
                text("我先看看文件。"),
                call("toolu_1", "Read", r#"{"file_path":"src/main.rs"}"#)
            ]
        );

        // 第二轮新的只有工具结果：上一轮的回答已经在上一轮里了。结果里的密钥打了码
        let second = &t.turns[1];
        assert_eq!(second.input.len(), 1, "{:?}", second.input);
        assert_eq!(second.input[0].role, R::Tool);
        let P::ToolResult {
            call_id,
            text: said,
            is_error,
        } = &second.input[0].parts[0]
        else {
            panic!("{:?}", second.input)
        };
        assert_eq!((call_id.as_str(), *is_error), ("toolu_1", false));
        assert!(said.contains("sk-an…CRET"), "{said}");
        assert!(!said.contains("SECRETSECRET"), "{said}");
        assert_eq!(second.output[1], text("密钥写死了，我来改。"));
        assert!(matches!(&second.output[2], P::ToolCall { name, .. } if name == "Edit"));

        // 第三轮：工具结果、结果里的截图（只有类型和大小）、一句话。有文字就不只是工具结果
        assert_eq!(
            t.turns[2].input,
            [msg(
                R::User,
                vec![
                    result("toolu_2", "已修改"),
                    P::Image {
                        media_type: Some("image/png".into()),
                        bytes: Some(30)
                    },
                    text("看起来不错，再跑一下测试")
                ]
            )]
        );
        assert_eq!(t.turns[2].output, [text("好的。")]);

        // 图片的数据、推理的签名都不出门
        let wire = serde_json::to_string(&t).unwrap();
        assert!(!wire.contains(&png), "{wire}");
        assert!(!wire.contains("sig-1"), "{wire}");
    }

    /// Chat：`tool_calls` 和 `tool` 消息，对话中途的系统消息照原位置留着，回答有流式的也有整包的
    #[test]
    fn an_openai_chat_session_with_tool_calls() {
        let mut d = Disk::new();
        let sys = json!({"role": "system", "content": "你是助手"});
        let ask = user("列一下文件");
        let a1 = json!({"role": "assistant", "content": "我来看看", "reasoning_content": "想一想",
            "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "ls", "arguments": "{\"dir\":\".\"}"}}]});
        let r1 = json!({"role": "tool", "tool_call_id": "call_1", "content": "README.md"});
        let a2 = assistant("有一个 README.md。");
        let body = |messages: Vec<Value>| json!({"model": "deepseek-chat", "stream": true, "messages": messages});
        let chunk = |delta: Value, finish: Value| {
            (
                "",
                json!({"id": "c", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}),
            )
        };
        let mut stream = sse(&[
            chunk(
                json!({"role": "assistant", "reasoning_content": "想一想"}),
                Value::Null,
            ),
            chunk(json!({"content": "我来"}), Value::Null),
            chunk(json!({"content": "看看"}), Value::Null),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "ls", "arguments": ""}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"dir\":"}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\".\"}"}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ]);
        stream.extend_from_slice(b"data: [DONE]\n\n");
        let whole = |t: &str| {
            json!({"choices": [{"message": {"role": "assistant", "content": t}, "finish_reason": "stop"}]})
                .to_string()
                .into_bytes()
        };
        d.turn(
            "/v1/chat/completions",
            &body(vec![sys.clone(), ask.clone()]),
            &stream,
        );
        d.turn(
            "/v1/chat/completions",
            &body(vec![sys.clone(), ask.clone(), a1.clone(), r1.clone()]),
            &whole("有一个 README.md。"),
        );
        d.turn(
            "/v1/chat/completions",
            &body(vec![
                sys,
                ask,
                a1,
                r1,
                a2,
                json!({"role": "system", "content": "之后用英文回答"}),
                user("谢谢"),
            ]),
            &whole("You're welcome."),
        );

        let t = d.transcript();
        assert_eq!(t.system.as_deref(), Some("你是助手"));
        assert_eq!(t.turns[0].input, [msg(R::User, vec![text("列一下文件")])]);
        assert_eq!(
            t.turns[0].output,
            [
                thinking("想一想"),
                text("我来看看"),
                call("call_1", "ls", r#"{"dir":"."}"#)
            ]
        );
        assert_eq!(
            t.turns[1].input,
            [msg(R::Tool, vec![result("call_1", "README.md")])]
        );
        assert_eq!(t.turns[1].output, [text("有一个 README.md。")]);
        assert_eq!(
            t.turns[2].input,
            [
                msg(R::System, vec![text("之后用英文回答")]),
                msg(R::User, vec![text("谢谢")])
            ]
        );
        assert_eq!(t.turns[2].output, [text("You're welcome.")]);
        assert!(t.turns.iter().all(|x| !x.restart && x.gaps.is_empty()));
    }

    /// Responses（Codex）：instructions 和开头的 developer 消息是系统提示；推理项、函数调用、
    /// 结果是一个个输入项，一次回答的几项并成一条；托管工具的调用是 `other`
    #[test]
    fn a_codex_session_on_the_responses_format() {
        let mut d = Disk::new();
        let dev = json!({"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "sandbox: workspace-write"}]});
        let env = json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]});
        let ask = json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "修好构建"}]});
        let args = r#"{"command":["cargo","build"]}"#;
        let rs1 = json!({"type": "reasoning", "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "看看报错"}], "encrypted_content": "gAAA1"});
        let fc1 =
            json!({"type": "function_call", "call_id": "c1", "name": "shell", "arguments": args});
        let out1 = json!({"type": "function_call_output", "call_id": "c1",
            "output": "error[E0308]: mismatched types"});
        let patch = "*** Begin Patch\n*** End Patch";
        let rs2 =
            json!({"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "gAAA2"});
        let ws = json!({"type": "web_search_call", "id": "ws_1", "status": "completed",
            "action": {"type": "search", "query": "E0308"}});
        let cc = json!({"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": patch});
        let out2 = json!({"type": "custom_tool_call_output", "call_id": "c2", "output": "Done!"});
        let body = |input: Vec<Value>| {
            json!({"model": "gpt-5.5", "instructions": "You are Codex.", "input": input, "stream": true,
                "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}},
                          {"type": "custom", "name": "apply_patch"}]})
        };
        let ev = |name: &str, mut data: Value| {
            data["type"] = json!(name);
            (name.to_string(), data)
        };
        let stream = |frames: Vec<(String, Value)>| {
            let frames: Vec<(&str, Value)> = frames
                .iter()
                .map(|(n, v)| (n.as_str(), v.clone()))
                .collect();
            sse(&frames)
        };
        let first = stream(vec![
            ev(
                "response.created",
                json!({"response": {"id": "resp_1", "model": "gpt-5.5"}}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
            ),
            ev(
                "response.reasoning_summary_text.delta",
                json!({"output_index": 0, "summary_index": 0, "delta": "看看报错"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": rs1}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 1, "item": {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": ""}}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"output_index": 1, "delta": "{\"command\":"}),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({"output_index": 1, "delta": "[\"cargo\",\"build\"]}"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 1, "item": fc1}),
            ),
            ev(
                "response.completed",
                json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
        ]);
        // 第二轮：推理只有密文（没有摘要），一次网页搜索，一个自由格式的工具调用
        let second = stream(vec![
            ev(
                "response.created",
                json!({"response": {"id": "resp_2", "model": "gpt-5.5"}}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_2", "summary": []}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": rs2}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 1, "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress"}}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 1, "item": ws}),
            ),
            ev(
                "response.output_item.added",
                json!({"output_index": 2, "item": {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": ""}}),
            ),
            ev(
                "response.custom_tool_call_input.delta",
                json!({"output_index": 2, "delta": "*** Begin Patch\n"}),
            ),
            ev(
                "response.custom_tool_call_input.delta",
                json!({"output_index": 2, "delta": "*** End Patch"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 2, "item": cc}),
            ),
            ev(
                "response.completed",
                json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
        ]);
        let whole = json!({"output": [{"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "构建好了。"}]}]});
        let path = "/backend-api/codex/responses";
        d.turn(
            path,
            &body(vec![dev.clone(), env.clone(), ask.clone()]),
            &first,
        );
        d.turn(
            path,
            &body(vec![
                dev.clone(),
                env.clone(),
                ask.clone(),
                rs1.clone(),
                fc1.clone(),
                out1.clone(),
            ]),
            &second,
        );
        d.turn(
            path,
            &body(vec![dev, env, ask, rs1, fc1, out1, rs2, ws, cc, out2]),
            whole.to_string().as_bytes(),
        );

        let t = d.transcript();
        assert_eq!(
            t.system.as_deref(),
            Some("You are Codex.\n\nsandbox: workspace-write")
        );
        // 用户消息不并：环境说明和要求是两条
        assert_eq!(
            t.turns[0].input,
            [
                msg(
                    R::User,
                    vec![text("<environment_context>cwd</environment_context>")]
                ),
                msg(R::User, vec![text("修好构建")])
            ]
        );
        assert_eq!(
            t.turns[0].output,
            [thinking("看看报错"), call("c1", "shell", args)]
        );
        assert_eq!(
            t.turns[1].input,
            [msg(
                R::Tool,
                vec![result("c1", "error[E0308]: mismatched types")]
            )]
        );
        // 只有密文的推理是一块空的推理
        assert_eq!(
            t.turns[1].output,
            [
                thinking(""),
                P::Other {
                    label: "web_search_call".into()
                },
                call("c2", "apply_patch", patch)
            ]
        );
        // 推理、搜索、调用三项是同一次回答，并成一条，作为上一轮的回答去掉
        assert_eq!(
            t.turns[2].input,
            [msg(R::Tool, vec![result("c2", "Done!")])]
        );
        assert_eq!(t.turns[2].output, [text("构建好了。")]);
        assert!(t.turns.iter().all(|x| !x.restart && x.gaps.is_empty()));
    }

    /// Gemini：`functionCall` / `functionResponse`，没有调用号的按函数名对上；推理是带
    /// `thought` 的文字
    #[test]
    fn a_gemini_session_with_function_calls() {
        let mut d = Disk::new();
        let path = "/v1beta/models/gemini-2.5-pro:streamGenerateContent";
        let sys = json!({"parts": [{"text": "You are a coding agent"}]});
        let ask = json!({"role": "user", "parts": [{"text": "跑一下测试"}]});
        let said = json!({"role": "model", "parts": [{"text": "好，我来跑"},
            {"functionCall": {"name": "run_shell_command", "args": {"command": "npm test"}}}]});
        let ran = json!({"role": "user", "parts": [{"functionResponse": {"name": "run_shell_command",
            "response": {"output": "3 passed"}}}]});
        let chunk = |parts: Value| {
            (
                "",
                json!({"candidates": [{"content": {"role": "model", "parts": parts}}]}),
            )
        };
        d.turn(
            path,
            &json!({"systemInstruction": sys, "contents": [ask.clone()]}),
            &sse(&[
                chunk(json!([{"text": "好，"}])),
                chunk(json!([{"text": "我来跑"},
                    {"functionCall": {"name": "run_shell_command", "args": {"command": "npm test"}}}])),
            ]),
        );
        d.turn(
            path,
            &json!({"system_instruction": sys, "contents": [ask, said, ran]}),
            &sse(&[
                chunk(json!([{"text": "测试都过了", "thought": true}])),
                chunk(json!([{"text": "全部通过。"}])),
            ]),
        );

        let t = d.transcript();
        assert_eq!(t.system.as_deref(), Some("You are a coding agent"));
        assert_eq!(t.turns[0].input, [msg(R::User, vec![text("跑一下测试")])]);
        // 上游没给调用号：回答里读的时候现编了一个，下一轮里客户端记下的是按函数名认的那个，换成它
        assert_eq!(
            t.turns[0].output,
            [
                text("好，我来跑"),
                call(
                    "run_shell_command",
                    "run_shell_command",
                    r#"{"command":"npm test"}"#
                )
            ]
        );
        assert_eq!(
            t.turns[1].input,
            [msg(R::Tool, vec![result("run_shell_command", "3 passed")])]
        );
        assert_eq!(
            t.turns[1].output,
            [thinking("测试都过了"), text("全部通过。")]
        );
        assert!(t.turns.iter().all(|x| !x.restart && x.gaps.is_empty()));
    }

    // ───────────────────────────────────────────────── 前后两个请求怎么比

    /// 压缩过、改过历史：对不上了，交出整段历史，标 `restart`
    #[test]
    fn a_compacted_or_edited_history_restarts_with_the_whole_history() {
        let mut d = Disk::new();
        d.turn("/v1/messages", &anthropic(&[user("一")]), &says("回一"));
        d.turn(
            "/v1/messages",
            &anthropic(&[user("一"), assistant("回一"), user("二")]),
            &says("回二"),
        );
        // 压缩：前文换成了一段摘要
        d.turn(
            "/v1/messages",
            &anthropic(&[user("前文摘要：说过一和二"), user("三")]),
            &says("回三"),
        );
        // 接着压缩之后的那段往下说
        d.turn(
            "/v1/messages",
            &anthropic(&[
                user("前文摘要：说过一和二"),
                user("三"),
                assistant("回三"),
                user("四"),
            ]),
            &says("回四"),
        );
        // 改过历史：早先的一条工具结果被清掉了（Claude Code 的 microcompact 就这么做）
        d.turn(
            "/v1/messages",
            &anthropic(&[
                user("前文摘要：说过一和二"),
                user("三（已清空）"),
                assistant("回三"),
                user("四"),
                assistant("回四"),
                user("五"),
            ]),
            &says("回五"),
        );

        let t = d.transcript();
        let restart: Vec<bool> = t.turns.iter().map(|x| x.restart).collect();
        assert_eq!(restart, [false, false, true, false, true]);
        assert_eq!(t.turns[1].input, [msg(R::User, vec![text("二")])]);
        // 连着的两条用户消息不并
        assert_eq!(
            t.turns[2].input,
            [
                msg(R::User, vec![text("前文摘要：说过一和二")]),
                msg(R::User, vec![text("三")])
            ]
        );
        assert_eq!(t.turns[3].input, [msg(R::User, vec![text("四")])]);
        // 整段历史，连同里面的助手消息
        assert_eq!(t.turns[4].input.len(), 6);
        assert_eq!(t.turns[4].input[2], msg(R::Assistant, vec![text("回三")]));
    }

    /// 系统提示变了：那一轮说出新的那一份；`system` 是第一轮的
    #[test]
    fn a_changed_system_prompt_is_said_on_the_turn_it_changed() {
        let mut d = Disk::new();
        let with = |system: &str, messages: &[Value]| json!({"model": "m", "system": [{"type": "text", "text": system}], "messages": messages});
        d.turn(
            "/v1/messages",
            &with("计划模式", &[user("一")]),
            &says("回一"),
        );
        d.turn(
            "/v1/messages",
            &with("执行模式", &[user("一"), assistant("回一"), user("开始改")]),
            &says("回二"),
        );
        d.turn(
            "/v1/messages",
            &with(
                "执行模式",
                &[
                    user("一"),
                    assistant("回一"),
                    user("开始改"),
                    assistant("回二"),
                    user("三"),
                ],
            ),
            &says("回三"),
        );
        let t = d.transcript();
        assert_eq!(t.system.as_deref(), Some("计划模式"));
        let changed: Vec<Option<&str>> = t
            .turns
            .iter()
            .map(|x| x.system_changed.as_deref())
            .collect();
        assert_eq!(changed, [None, Some("执行模式"), None]);
        // 系统提示变了不等于历史断了
        assert!(t.turns.iter().all(|x| !x.restart));
        assert_eq!(t.turns[1].input, [msg(R::User, vec![text("开始改")])]);
    }

    /// 数 token 的请求也是会话里的一轮，但不是对话里的一句：空的，不打断前后的比对
    #[test]
    fn a_count_tokens_call_is_an_empty_turn_that_does_not_break_the_chain() {
        let mut d = Disk::new();
        d.turn("/v1/messages", &anthropic(&[user("一")]), &says("回一"));
        let history = [user("一"), assistant("回一"), user("二")];
        d.turn(
            "/v1/messages/count_tokens",
            &json!({"model": "m", "messages": history}),
            br#"{"input_tokens": 12}"#,
        );
        d.turn("/v1/messages", &anthropic(&history), &says("回二"));
        let t = d.transcript();
        assert_eq!(t.turns.len(), 3);
        let side = &t.turns[1];
        assert!(
            side.input.is_empty()
                && side.output.is_empty()
                && side.gaps.is_empty()
                && !side.restart
        );
        assert!(!t.turns[2].restart);
        assert_eq!(t.turns[2].input, [msg(R::User, vec![text("二")])]);
        assert_eq!(t.turns[2].output, [text("回二")]);
    }

    /// 失败了的请求没有回答也不算缺；客户端照原样重发，没有新的东西
    #[test]
    fn a_failed_turn_has_no_answer_and_its_retry_adds_nothing() {
        let mut d = Disk::new();
        let history = [user("一")];
        d.put(
            "/v1/messages",
            Some(anthropic(&history).to_string().as_bytes()),
            Some(br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            |r| {
                r.status = Some(529);
                r.error = Some(crate::db::tests::upstream_failed("overloaded"));
            },
        );
        d.turn("/v1/messages", &anthropic(&history), &says("回一"));
        let t = d.transcript();
        assert!(t.turns[0].output.is_empty() && t.turns[0].gaps.is_empty());
        assert_eq!(t.turns[0].input, [msg(R::User, vec![text("一")])]);
        assert!(!t.turns[1].restart);
        assert!(t.turns[1].input.is_empty(), "{:?}", t.turns[1].input);
        assert_eq!(t.turns[1].output, [text("回一")]);
    }

    // ───────────────────────────────────────────────── 读不出来的地方

    /// 请求体没存下、只存了开头：那一轮没有 `input`，下一个读得懂的从头交出整段历史
    #[test]
    fn missing_and_truncated_requests_are_gaps_and_the_next_turn_restarts() {
        let mut d = Disk::new();
        d.turn("/v1/messages", &anthropic(&[user("一")]), &says("回一"));
        d.put("/v1/messages", None, Some(says("回二").as_slice()), |_| {});
        let h3 = [
            user("一"),
            assistant("回一"),
            user("二"),
            assistant("回二"),
            user("三"),
        ];
        d.turn("/v1/messages", &anthropic(&h3), &says("回三"));
        // 只存了开头：存下来的那一段解析不了
        let mut h4 = h3.to_vec();
        h4.extend([assistant("回三"), user("四")]);
        let full = anthropic(&h4).to_string();
        let id = d.put(
            "/v1/messages",
            Some(&full.as_bytes()[..full.len() / 2]),
            Some(says("回四").as_slice()),
            |_| {},
        );
        d.cut(id, Which::Request, full.len());
        let mut h5 = h4.clone();
        h5.extend([assistant("回四"), user("五")]);
        d.turn("/v1/messages", &anthropic(&h5), &says("回五"));

        let t = d.transcript();
        let gaps: Vec<&[Gap]> = t.turns.iter().map(|x| x.gaps.as_slice()).collect();
        assert_eq!(
            gaps,
            [
                &[][..],
                &[Gap::RequestMissing],
                &[],
                &[Gap::RequestTruncated],
                &[]
            ]
        );
        let restart: Vec<bool> = t.turns.iter().map(|x| x.restart).collect();
        assert_eq!(restart, [false, false, true, false, true]);
        // 读不懂的那一轮回答照样读
        assert!(t.turns[1].input.is_empty());
        assert_eq!(t.turns[1].output, [text("回二")]);
        assert_eq!(t.turns[2].input.len(), 5);
        assert_eq!(t.turns[3].output, [text("回四")]);
        assert_eq!(t.turns[4].input.len(), 9);
    }

    /// 回答没存下、只存了开头、读不懂：各是一种缺口。没交全的回答，下一轮里客户端记下的那条
    /// 助手消息留着 —— 它是那一轮说过什么的唯一记录
    #[test]
    fn missing_truncated_and_unreadable_answers_are_gaps() {
        let mut d = Disk::new();
        let mut h = vec![user("一")];
        // 回答没存下：上游回了 200，收到了字节
        d.put(
            "/v1/messages",
            Some(anthropic(&h).to_string().as_bytes()),
            None,
            |_| {},
        );
        h.extend([assistant("回一"), user("二")]);
        // 只存了开头：读得出第一块，第二块断在半路
        let long = anthropic_stream(&[
            json!({"type": "text", "text": "前一半"}),
            json!({"type": "text", "text": "后一半"}),
        ]);
        let cut = String::from_utf8(long.clone()).unwrap();
        let keep = cut.find("后").unwrap();
        let id = d.put(
            "/v1/messages",
            Some(anthropic(&h).to_string().as_bytes()),
            Some(&long[..keep]),
            |_| {},
        );
        d.cut(id, Which::Response, long.len());
        h.extend([assistant("前一半后一半"), user("三")]);
        // 读不懂：上游回了 200，正文却是一页 HTML
        d.put(
            "/v1/messages",
            Some(anthropic(&h).to_string().as_bytes()),
            Some(b"<html>502 Bad Gateway</html>"),
            |_| {},
        );
        h.extend([assistant("回三"), user("四")]);
        // 客户端在第一个字节之前就走了：没有回答，也不算缺
        d.put(
            "/v1/messages",
            Some(anthropic(&h).to_string().as_bytes()),
            None,
            |r| {
                r.cancelled = true;
                r.received_bytes = Some(0);
            },
        );

        let t = d.transcript();
        let gaps: Vec<&[Gap]> = t.turns.iter().map(|x| x.gaps.as_slice()).collect();
        assert_eq!(
            gaps,
            [
                &[Gap::ResponseMissing][..],
                &[Gap::ResponseTruncated],
                &[Gap::ResponseUnreadable],
                &[]
            ]
        );
        assert!(t.turns[0].output.is_empty());
        assert_eq!(t.turns[1].output, [text("前一半")]);
        assert!(t.turns[2].output.is_empty() && t.turns[3].output.is_empty());
        // 上一轮的回答没交全：客户端记下的那条助手消息留在新的这几条里
        assert!(t.turns.iter().all(|x| !x.restart));
        assert_eq!(
            t.turns[1].input,
            [
                msg(R::Assistant, vec![text("回一")]),
                msg(R::User, vec![text("二")])
            ]
        );
        assert_eq!(
            t.turns[2].input,
            [
                msg(R::Assistant, vec![text("前一半后一半")]),
                msg(R::User, vec![text("三")])
            ]
        );
        assert_eq!(t.turns[3].input[0].role, R::Assistant);
    }

    // ───────────────────────────────────────────────── 打码

    /// 密钥在工具结果里、在一行的开头（JSON 里它前面是 `\n` 的转义），交出去之前都打了码；
    /// 系统提示和回答里的也一样
    #[test]
    fn secrets_come_out_masked_wherever_they_are() {
        let mut d = Disk::new();
        let key = "sk-ant-api03-SECRETSECRETSECRET";
        let body = json!({"model": "m", "system": format!("备用的 key：{key}"), "messages": [
            user("读一下 .env"),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read",
                "input": {"file_path": ".env", "note": key}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                "content": format!("# 配置\n{key}\nGITHUB=ghp_bbbbbbbbbbbbbbbbbbbb")}]})
        ]});
        d.turn("/v1/messages", &body, &says(&format!("找到了 {key}")));
        let t = d.transcript();
        let wire = serde_json::to_string(&t).unwrap();
        assert!(!wire.contains("SECRETSECRET"), "{wire}");
        assert!(!wire.contains("bbbbbbbbbbbbbbbb"), "{wire}");
        assert!(wire.contains("sk-an…CRET"), "{wire}");
        let P::ToolResult { text: said, .. } = &t.turns[0].input[2].parts[0] else {
            panic!("{:?}", t.turns[0].input)
        };
        assert_eq!(said, "# 配置\nsk-an…CRET\nGITHUB=ghp_b…bbbb");
    }

    // ───────────────────────────────────────────────── 转换过格式的

    /// 客户端说 Anthropic、上游说 Chat 或 Responses：请求按客户端的格式读，回答按上游的
    #[test]
    fn a_translated_turn_reads_the_answer_in_the_upstream_format() {
        let mut d = Disk::new();
        let translated = |to: &str| {
            Some(
                json!({"provider": "relay", "from": "anthropic", "to": to, "dropped": []})
                    .to_string(),
            )
        };
        let chat = sse(&[
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"content": "我来查"}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                    "type": "function", "function": {"name": "Grep", "arguments": "{\"pattern\":\"TODO\"}"}}]}}]}),
            ),
            (
                "",
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ),
        ]);
        d.put(
            "/v1/messages",
            Some(anthropic(&[user("找 TODO")]).to_string().as_bytes()),
            Some(&chat),
            |r| r.translated = translated("openai-chat"),
        );
        // 下一轮里客户端记下的调用号，是网关转换时编的那个
        let said = json!({"role": "assistant", "content": [{"type": "text", "text": "我来查"},
            {"type": "tool_use", "id": "call_gateway", "name": "Grep", "input": {"pattern": "TODO"}}]});
        let found = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_gateway",
            "content": "src/a.rs:1: TODO"}]});
        let responses = sse(&[
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0,
                    "item": {"type": "message", "role": "assistant", "content": []}}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0,
                    "delta": "只有一处。"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ),
        ]);
        d.put(
            "/v1/messages",
            Some(
                anthropic(&[user("找 TODO"), said, found])
                    .to_string()
                    .as_bytes(),
            ),
            Some(&responses),
            |r| r.translated = translated("openai-responses"),
        );

        let t = d.transcript();
        assert!(t.turns.iter().all(|x| x.gaps.is_empty() && !x.restart));
        assert_eq!(
            t.turns[0].output,
            [
                text("我来查"),
                call("call_gateway", "Grep", r#"{"pattern":"TODO"}"#)
            ]
        );
        assert_eq!(
            t.turns[1].input,
            [msg(
                R::Tool,
                vec![result("call_gateway", "src/a.rs:1: TODO")]
            )]
        );
        assert_eq!(t.turns[1].output, [text("只有一处。")]);
    }

    /// Codex 走 Anthropic 的上游：自由格式的 `apply_patch` 被包成了 `{"input": …}`，拆回原文
    #[test]
    fn a_freeform_call_answered_by_another_format_is_unwrapped() {
        let mut d = Disk::new();
        let patch = "*** Begin Patch\n*** End Patch";
        d.put(
            "/v1/responses",
            Some(
                json!({"model": "claude-sonnet-4-5", "input": "改一下",
                    "tools": [{"type": "custom", "name": "apply_patch"}]})
                .to_string()
                .as_bytes(),
            ),
            Some(&anthropic_stream(&[json!({"type": "tool_use", "id": "toolu_1",
                "name": "apply_patch", "input": {"input": patch}})])),
            |r| {
                r.translated = Some(
                    json!({"provider": "anthropic", "from": "openai-responses", "to": "anthropic", "dropped": []})
                        .to_string(),
                )
            },
        );
        let t = d.transcript();
        assert_eq!(t.turns[0].input, [msg(R::User, vec![text("改一下")])]);
        assert_eq!(t.turns[0].output, [call("toolu_1", "apply_patch", patch)]);
    }

    /// Codex 的 Responses Lite：工具声明在 `input` 的 `additional_tools` 里，自由格式工具在
    /// `functions` 这个 namespace 里。声明不算对话；转给别家时 `apply_patch` 还叫这个名字，
    /// 包着的原文照样拆回来
    #[test]
    fn a_responses_lite_request_reads_its_tools_from_input() {
        let mut d = Disk::new();
        let patch = "*** Begin Patch\n*** End Patch";
        d.put(
            "/v1/responses",
            Some(
                json!({"model": "claude-sonnet-4-5", "input": [
                    {"type": "additional_tools", "role": "developer", "tools": [
                        {"type": "namespace", "name": "functions", "tools": [
                            {"type": "custom", "name": "apply_patch"},
                            {"type": "function", "name": "exec_command", "parameters": {}}
                        ]}
                    ]},
                    {"type": "message", "role": "developer", "content": "You are Codex."},
                    {"type": "message", "role": "user", "content": "改一下"}
                ]})
                .to_string()
                .as_bytes(),
            ),
            Some(&anthropic_stream(&[json!({"type": "tool_use", "id": "toolu_1",
                "name": "apply_patch", "input": {"input": patch}})])),
            |r| {
                r.translated = Some(
                    json!({"provider": "anthropic", "from": "openai-responses", "to": "anthropic", "dropped": []})
                        .to_string(),
                )
            },
        );
        let t = d.transcript();
        assert_eq!(t.system.as_deref(), Some("You are Codex."));
        assert_eq!(t.turns[0].input, [msg(R::User, vec![text("改一下")])]);
        assert_eq!(t.turns[0].output, [call("toolu_1", "apply_patch", patch)]);
    }

    /// Bedrock 的二进制帧在网关进门时转成了 SSE；整包的 Converse 也读得出来
    #[test]
    fn a_bedrock_answer_is_read_streamed_or_whole() {
        let mut d = Disk::new();
        let bedrock = || {
            Some(
                json!({"provider": "bedrock", "from": "anthropic", "to": "bedrock", "dropped": []})
                    .to_string(),
            )
        };
        let stream = sse(&[
            ("messageStart", json!({"role": "assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "想"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 0})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"text": "基岩"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 1})),
            (
                "contentBlockStart",
                json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "t1", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"command\":\"ls\"}"}}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 2})),
            ("messageStop", json!({"stopReason": "tool_use"})),
        ]);
        d.put(
            "/v1/messages",
            Some(anthropic(&[user("一")]).to_string().as_bytes()),
            Some(&stream),
            |r| r.translated = bedrock(),
        );
        let whole = json!({"output": {"message": {"role": "assistant", "content": [{"text": "整包"}]}},
            "stopReason": "end_turn"});
        d.put(
            "/v1/messages",
            Some(anthropic(&[user("另一段")]).to_string().as_bytes()),
            Some(whole.to_string().as_bytes()),
            |r| r.translated = bedrock(),
        );
        let t = d.transcript();
        assert_eq!(
            t.turns[0].output,
            [
                thinking("想"),
                text("基岩"),
                call("t1", "Bash", r#"{"command":"ls"}"#)
            ]
        );
        assert_eq!(t.turns[1].output, [text("整包")]);
        assert!(t.turns.iter().all(|x| x.gaps.is_empty()));
    }

    /// 不带 `alt=sse` 的 Gemini 流是一个 JSON 数组；截断了的读到最后一个完整的元素
    #[test]
    fn a_gemini_answer_written_as_a_json_array_is_read_element_by_element() {
        let mut d = Disk::new();
        let path = "/v1beta/models/gemini-2.5-pro:streamGenerateContent";
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "画一只猫"}]}]});
        let array = json!([
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "画好了"}]}}]},
            {"candidates": [{"content": {"role": "model", "parts": [
                {"inlineData": {"mimeType": "image/png", "data": "QUJD".repeat(4)}},
                {"executableCode": {"language": "PYTHON", "code": "print(1)"}}]}}]},
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "。"}]}}]}
        ])
        .to_string();
        d.turn(path, &body, array.as_bytes());
        let id = d.put(
            path,
            Some(body.to_string().as_bytes()),
            Some(&array.as_bytes()[..array.len() - 30]),
            |_| {},
        );
        d.cut(id, Which::Response, array.len());

        let t = d.transcript();
        let image = P::Image {
            media_type: Some("image/png".into()),
            bytes: Some(12),
        };
        let code = P::Other {
            label: "executableCode".into(),
        };
        assert_eq!(
            t.turns[0].output,
            [text("画好了"), image.clone(), code.clone(), text("。")]
        );
        assert!(t.turns[0].gaps.is_empty());
        assert_eq!(t.turns[1].output, [text("画好了"), image, code]);
        assert_eq!(t.turns[1].gaps, [Gap::ResponseTruncated]);
    }

    /// 和会话详情同样的请求、同样的顺序：本地应答的不在，同一刻的按请求号
    #[test]
    fn the_turns_are_the_same_requests_in_the_same_order_as_the_session_detail() {
        let mut d = Disk::new();
        d.turn("/v1/messages", &anthropic(&[user("一")]), &says("回一"));
        d.put("/v1/messages", None, None, |r| {
            r.local = true;
        });
        d.put("/v1/messages", None, None, |r| r.at_ms = NOW);
        d.put("/v1/messages", None, None, |r| r.at_ms = NOW);
        let t = d.transcript();
        let want: Vec<String> =
            d.db.turns("s")
                .unwrap()
                .iter()
                .map(|r| r.id.to_string())
                .collect();
        assert_eq!(want, ["3", "4", "1"]);
        assert_eq!(
            t.turns.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
            want
        );
    }

    /// 别家没有对应物的块不丢：文件、音频、服务端工具的调用和结果、压缩过的前文，按类型名
    /// 记成 `other`，位置不变。图片只说类型和大小，给的是地址的不知道大小
    #[test]
    fn blocks_without_a_counterpart_are_named_in_place() {
        let mut d = Disk::new();
        let jpeg = format!("data:image/jpeg;base64,{}", "QUJD".repeat(5));
        d.turn(
            "/v1/chat/completions",
            &json!({"model": "m", "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图听音"},
                {"type": "image_url", "image_url": {"url": jpeg}},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
                {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,JVBERi0="}}
            ]}]}),
            br#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let t = d.transcript();
        assert_eq!(
            t.turns[0].input,
            [msg(
                R::User,
                vec![
                    text("看图听音"),
                    P::Image {
                        media_type: Some("image/jpeg".into()),
                        bytes: Some(15)
                    },
                    P::Image {
                        media_type: None,
                        bytes: None
                    },
                    P::Other {
                        label: "input_audio".into()
                    },
                    P::Other {
                        label: "file".into()
                    }
                ]
            )]
        );
        let wire = serde_json::to_string(&t).unwrap();
        assert!(!wire.contains("QUJD") && !wire.contains("UklGRg") && !wire.contains("JVBERi0"));

        // Anthropic：网页搜索是服务端工具，调用和结果都是助手消息里的块；回答的流里也一样
        let mut d = Disk::new();
        let search = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
            "input": {"query": "rust 2024"}});
        let found = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
            "content": [{"type": "web_search_result", "url": "https://x", "title": "x", "encrypted_content": "e"}]});
        d.turn(
            "/v1/messages",
            &anthropic(&[json!({"role": "user", "content": [
                {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "合同全文"}},
                {"type": "text", "text": "查一下"}
            ]})]),
            &anthropic_stream(&[
                search.clone(),
                found.clone(),
                json!({"type": "text", "text": "查到了"}),
            ]),
        );
        let t = d.transcript();
        assert_eq!(
            t.turns[0].input,
            [msg(
                R::User,
                vec![
                    P::Other {
                        label: "document".into()
                    },
                    text("查一下")
                ]
            )]
        );
        assert_eq!(
            t.turns[0].output,
            [
                P::Other {
                    label: "server_tool_use".into()
                },
                P::Other {
                    label: "web_search_tool_result".into()
                },
                text("查到了")
            ]
        );

        // Responses：压缩过的前文像一条系统消息；引用服务端内容的项是 `other`
        let mut d = Disk::new();
        d.turn(
            "/v1/responses",
            &json!({"model": "m", "input": [
                {"type": "compaction", "encrypted_content": "gAAAA"},
                {"type": "item_reference", "id": "msg_1"},
                {"role": "user", "content": "接着来"}
            ]}),
            br#"{"output":[]}"#,
        );
        let t = d.transcript();
        assert_eq!(
            t.turns[0].input,
            [
                msg(
                    R::System,
                    vec![P::Other {
                        label: "compaction".into()
                    }]
                ),
                msg(
                    R::User,
                    vec![P::Other {
                        label: "item_reference".into()
                    }]
                ),
                msg(R::User, vec![text("接着来")])
            ]
        );
        assert!(t.turns[0].output.is_empty() && t.turns[0].gaps.is_empty());

        // 转换时压缩出的前文：摘要是上游写的，读得出来
        let mut d = Disk::new();
        d.turn(
            "/v1/responses",
            &json!({"model": "m", "input": [
                {"type": "compaction", "encrypted_content": tw_dialect::compaction::carry("改过 src/a.rs")},
                {"role": "user", "content": "接着来"}
            ]}),
            br#"{"output":[]}"#,
        );
        assert_eq!(
            d.transcript().turns[0].input[0],
            msg(
                R::System,
                vec![text(&tw_dialect::compaction::restored("改过 src/a.rs"))]
            )
        );
    }

    // ───────────────────────────────────────────────── 读过的不再读

    /// 很久以后的此刻：每一轮都早就结束了
    const LATER: i64 = NOW + 86_400_000;

    impl Disk {
        /// 用 `cache` 读：从 `from_turn` 起，此刻是 `now_ms`
        fn read(&self, cache: &Cache, from_turn: usize, now_ms: i64) -> Transcript {
            self.read_running(cache, from_turn, None, now_ms)
        }

        fn read_running(
            &self,
            cache: &Cache,
            from_turn: usize,
            running: Option<(i64, i64)>,
            now_ms: i64,
        ) -> Transcript {
            let rows = self.db.session_requests("s").unwrap();
            cache.read(
                &self.blobs,
                &Ask {
                    session: "s",
                    rows: &rows,
                    from_turn,
                    running,
                    now_ms,
                },
            )
        }

        /// 第 `id` 轮结束之后过了 `ms` 毫秒（`row` 给的每一轮都跑了 4 秒）
        fn after(&self, id: i64, ms: i64) -> i64 {
            NOW + id * 1000 + 4000 + ms
        }
    }

    /// 一段带工具调用的对话：第 `k` 轮的请求和回答。回答里工具调用的号和客户端记下的不一样
    /// （转换过格式的上游就是这样）：读下一轮时，上一轮回答里的号换成客户端的那个
    fn claude_turn(k: usize) -> (Value, Vec<u8>) {
        let mut messages = vec![user("把 bug 修了")];
        for i in 0..k {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("读 {i}")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "Read", "input": {"n": i}}]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": format!("文件 {i}")}]}));
        }
        let answer = anthropic_stream(&[
            json!({"type": "text", "text": format!("读 {k}")}),
            json!({"type": "tool_use", "id": format!("up{k}"), "name": "Read", "input": {"n": k}}),
        ]);
        (anthropic(&messages), answer)
    }

    /// **接着读和从头读是同一段对话**：每来一轮读一次，每一次都和整段从头读的一样；只要
    /// 后面几轮的，就是整段的那一截。中间夹着一个数 token 的调用
    #[test]
    fn reading_on_from_where_it_stopped_gives_what_reading_it_all_gives() {
        let mut d = Disk::new();
        let cache = Cache::default();
        for k in 0..8 {
            let (request, answer) = claude_turn(k);
            d.turn("/v1/messages", &request, &answer);
            if k == 3 {
                d.turn("/v1/messages/count_tokens", &request, b"{}");
            }
            let full = d.transcript();
            let got = d.read(&cache, 0, LATER);
            assert_eq!(got.turns, full.turns, "第 {k} 轮");
            assert_eq!(got.system, full.system);
            assert_eq!(got.total_turns as usize, full.turns.len());
            assert!(got.turns.iter().skip(1).all(|t| !t.restart), "{got:?}");
            // 上一轮回答里的号换成了客户端记下的，最后一轮的还是上游给的
            if k > 0 {
                let ids = |t: &TranscriptTurn| -> Vec<String> {
                    t.output
                        .iter()
                        .filter_map(|p| match p {
                            P::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect()
                };
                let answered: Vec<Vec<String>> = got
                    .turns
                    .iter()
                    .map(ids)
                    .filter(|x| !x.is_empty())
                    .collect();
                assert_eq!(answered[k - 1], [format!("t{}", k - 1)]);
                assert_eq!(answered[k], [format!("up{k}")]);
            }
            for from in [0, 1, full.turns.len() - 1, full.turns.len(), 99] {
                let part = d.read(&cache, from, LATER);
                assert_eq!(
                    part.turns,
                    full.turns[from.min(full.turns.len())..],
                    "{from}"
                );
                assert_eq!(part.total_turns, got.total_turns);
            }
        }
    }

    /// **读过的不再读，正文回收之后才重读。**读过一遍之后正文没了（这里直接删掉），记着的
    /// 照样交出去 —— 说明它真的没再去读；作废之后重读，那几轮说出缺了什么
    #[test]
    fn what_was_read_is_not_read_again_until_bodies_are_reclaimed() {
        let mut d = Disk::new();
        let cache = Cache::default();
        for k in 0..2 {
            let (request, answer) = claude_turn(k);
            d.turn("/v1/messages", &request, &answer);
        }
        let first = d.read(&cache, 0, LATER);
        assert!(first.turns.iter().all(|t| t.gaps.is_empty()), "{first:?}");
        // 整天的正文都删掉：两天之后、只留一天
        d.blobs.gc(NOW + 2 * 86_400_000, 1, u64::MAX);
        assert!(d.blobs.get(NOW + 1000, 1, Which::Request).is_none());
        assert_eq!(d.read(&cache, 0, LATER), first, "记着的没用上");

        cache.invalidate();
        let again = d.read(&cache, 0, LATER);
        for t in &again.turns {
            assert_eq!(
                t.gaps,
                [Gap::RequestMissing, Gap::ResponseMissing],
                "{again:?}"
            );
        }
    }

    /// 刚结束、回答还没落盘的那一轮**不记下**：回答一落盘，下一次就读得到，后面那一轮跟着
    /// 对得上（上一轮的回答交出去了，它的输入里不再重复那条助手消息）。结束得够久还缺着的，
    /// 就是真的缺了
    #[test]
    fn a_turn_whose_answer_is_still_on_its_way_is_read_again() {
        let mut d = Disk::new();
        let cache = Cache::default();
        let (r0, a0) = claude_turn(0);
        d.turn("/v1/messages", &r0, &a0);
        let (r1, a1) = claude_turn(1);
        let id = d.put(
            "/v1/messages",
            Some(r1.to_string().as_bytes()),
            None,
            |_| {},
        );
        let now = d.after(id, 500);

        let t = d.read(&cache, 0, now);
        assert_eq!(t.turns[1].gaps, [Gap::ResponseMissing]);
        assert_eq!(t.settled_turns, 1, "回答还在路上的那一轮算定下来了");
        // 下一轮来了，回答还没到
        let (r2, a2) = claude_turn(2);
        d.turn("/v1/messages", &r2, &a2);
        let t = d.read(&cache, 0, d.after(id + 1, 500));
        assert_eq!(t.turns[1].gaps, [Gap::ResponseMissing]);
        assert_eq!(t.settled_turns, 1);

        // 回答落盘了：重读那一轮，和从头读的一样
        assert!(d.blobs.put(NOW + id * 1000, id, Which::Response, &a1));
        let t = d.read(&cache, 0, d.after(id + 1, 600));
        assert_eq!(t.turns, d.transcript().turns);
        assert!(t.turns.iter().all(|x| x.gaps.is_empty()), "{t:?}");
        assert_eq!(t.total_turns, 3);
        // 最后一轮的回答下一轮还会改写（工具调用的号），它还不算定下来
        assert_eq!(t.settled_turns, 2);

        // 回答一直没来：结束得够久之后它就是缺了，算定下来
        let (r3, _) = claude_turn(3);
        let late = d.put(
            "/v1/messages",
            Some(r3.to_string().as_bytes()),
            None,
            |_| {},
        );
        let t = d.read(&cache, 0, d.after(late, 1000));
        assert_eq!(t.settled_turns, 3);
        let t = d.read(&cache, 0, d.after(late, SETTLE_MS));
        assert_eq!(t.turns[3].gaps, [Gap::ResponseMissing]);
        assert_eq!(t.settled_turns, 4, "缺着回答的最后一轮不会再被改写");
    }

    /// 定下来的几轮也要排在还在跑的请求前面：一个开始得早、结束得晚的请求落库时插在它开始的
    /// 那一刻，它后面的几轮都要往后挪一格
    #[test]
    fn turns_after_a_request_still_running_are_not_settled() {
        let mut d = Disk::new();
        let cache = Cache::default();
        for k in 0..3 {
            let (request, answer) = claude_turn(k);
            d.turn("/v1/messages", &request, &answer);
        }
        let t = d.read(&cache, 0, LATER);
        assert_eq!((t.total_turns, t.settled_turns), (3, 2));
        // 第二轮之前开始的一个请求还在跑
        let t = d.read_running(&cache, 0, Some((NOW + 2 * 1000 - 1, 99)), LATER);
        assert_eq!(t.settled_turns, 1);
        // 在最后一轮之后开始的不碍事
        let t = d.read_running(&cache, 0, Some((NOW + 10_000, 99)), LATER);
        assert_eq!(t.settled_turns, 2);
    }

    /// 一个请求插进了读过的几轮中间（开始得早、结束得晚）：记着的那一截对不上了，从头读
    #[test]
    fn a_request_landing_between_turns_already_read_is_read_from_the_start() {
        let mut d = Disk::new();
        let cache = Cache::default();
        let (r0, a0) = claude_turn(0);
        let (r1, a1) = claude_turn(1);
        let (r2, a2) = claude_turn(2);
        d.turn("/v1/messages", &r0, &a0);
        d.turn("/v1/messages", &r2, &a2);
        d.read(&cache, 0, LATER);
        // 第二轮开始在前两轮之间，结束在它们之后
        d.put(
            "/v1/messages",
            Some(r1.to_string().as_bytes()),
            Some(&a1),
            |r| r.at_ms = NOW + 1500,
        );
        let got = d.read(&cache, 0, LATER);
        let full = d.transcript();
        assert_eq!(got.turns, full.turns);
        assert_eq!(
            got.turns.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["1", "3", "2"]
        );
        assert!(got.turns.iter().all(|t| !t.restart), "{got:?}");
    }

    // ───────────────────────────────────────────────── 量一量

    /// 读一次几百轮的会话要多久：照 Claude Code 的样子造一段 300 轮的对话，每一轮的请求带着
    /// 整段历史（平均 1 MB 上下，工具结果每条 2.5 KB 上下），流式的回答里有推理、文字和工具调用。
    ///
    /// 默认不跑：要写三百多 MB 的正文。发布构建下跑：
    /// `cargo test --release -p tw-store transcript_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn transcript_cost() {
        let mut d = Disk::new();
        let system = "You are Claude Code. ".repeat(1000);
        let tools: Vec<Value> = (0..40)
            .map(|i| {
                json!({"name": format!("Tool{i}"), "description": "x".repeat(600),
                    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}})
            })
            .collect();
        let file = "fn main() { println!(\"hello\"); }\n".repeat(75);
        let pair = |k: usize| {
            [
                json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": format!("第 {k} 步先读一下"), "signature": "s".repeat(300)},
                    {"type": "text", "text": format!("第 {k} 步我来读 src/{k}.rs")},
                    {"type": "tool_use", "id": format!("toolu_{k}"), "name": "Read",
                        "input": {"file_path": format!("src/{k}.rs")}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("toolu_{k}"),
                    "content": format!("{file}// 第 {k} 个文件")}]}),
            ]
        };
        // 开头已经聊了两百轮
        let mut messages = vec![json!({"role": "user", "content": "把这个仓库的 bug 都修了"})];
        for k in 0..200 {
            messages.extend(pair(k));
        }
        let mut bytes = 0usize;
        for turn in 0..300 {
            let mut sent = messages.clone();
            if let Some(last) = sent.last_mut() {
                *last = with_cache(last.clone());
            }
            let body = json!({"model": "claude-sonnet-4-5", "system": system, "tools": tools,
                "max_tokens": 32000, "stream": true, "messages": sent})
            .to_string();
            let k = 200 + turn;
            let answer = anthropic_stream(&[
                json!({"type": "thinking", "thinking": format!("第 {k} 步先读一下"), "signature": "s".repeat(300)}),
                json!({"type": "text", "text": format!("第 {k} 步我来读 src/{k}.rs")}),
                json!({"type": "tool_use", "id": format!("toolu_{k}"), "name": "Read",
                    "input": {"file_path": format!("src/{k}.rs")}}),
            ]);
            bytes += body.len() + answer.len();
            d.put("/v1/messages", Some(body.as_bytes()), Some(&answer), |_| {});
            messages.extend(pair(k));
        }
        let rows = d.db.session_requests("s").unwrap();
        eprintln!(
            "{} requests, {} MB of bodies ({} KB per request on average)",
            rows.len(),
            bytes / 1_000_000,
            bytes / rows.len() / 1000
        );
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            let t = build("s", &rows, &d.blobs);
            let took = t0.elapsed();
            let wire = serde_json::to_string(&t).unwrap();
            eprintln!(
                "built in {took:?} ({:?} per request), {} KB of JSON",
                took / rows.len() as u32,
                wire.len() / 1000
            );
            assert!(
                t.turns
                    .iter()
                    .skip(1)
                    .all(|x| !x.restart && x.input.len() == 1)
            );
            assert!(
                t.turns
                    .iter()
                    .all(|x| x.gaps.is_empty() && x.output.len() == 3)
            );
        }

        // 界面开着这次会话：读过前 299 轮，第 300 轮来了
        let cache = Cache::default();
        let read = |rows: &[RequestRow], from_turn| {
            let t0 = std::time::Instant::now();
            let t = cache.read(
                &d.blobs,
                &Ask {
                    session: "s",
                    rows,
                    from_turn,
                    running: None,
                    now_ms: i64::MAX,
                },
            );
            (t0.elapsed(), t)
        };
        let (took, before) = read(&rows[..rows.len() - 1], 0);
        eprintln!("first read of {} turns: {took:?}", before.total_turns);
        let (took, whole) = read(&rows, 0);
        eprintln!("one more turn, whole transcript again: {took:?}");
        assert_eq!(whole.turns, build("s", &rows, &d.blobs).turns);
        let (took, tail) = read(&rows, before.settled_turns as usize);
        eprintln!(
            "one more turn, from settled_turns ({}): {took:?}, {} turns, {} KB of JSON",
            before.settled_turns,
            tail.turns.len(),
            serde_json::to_string(&tail).unwrap().len() / 1000
        );
    }
}

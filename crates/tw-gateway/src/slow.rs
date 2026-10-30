//! 生成过慢时中断（配置的 `providers[].min_speed`）。
//!
//! 自部署的模型偶尔会掉到每秒几个 token：一直有内容，无响应超时（见
//! `server::pipeline::idle`）永远不到点，用户只能干等或者手动中止。设了最低生成速度的上游，
//! 流式回答从**第一个 token** 起每秒看一次：最近 `window_secs` 秒的平均速度低于它，就以一条
//! 客户端可以重试的错误（503）结束回答，断开上游，这家记一次失败（见 `relay`）。
//!
//! 速度怎么算：
//!
//! - **token 和实时速度同一个估法**（`/live`、`RunningView::tokens_per_sec`，见
//!   [`crate::ending`]）：正文、推理、工具参数按字数估，乘上这家这个模型校准过的比例
//!   （[`crate::pace`]）。界面上看到的数就是判断的依据。
//! - **长静默不算**：连续 [`SILENCE_SECS`] 秒以上（判定窗口不到 20 秒的，窗口的四分之一以上）
//!   没有内容的那一段，时长和其中的 token（0 个）都从窗口里扣掉 —— 那是无响应超时的事，回答
//!   中途想一会儿、等工具参数这类正常的停顿不该被当成生成太慢。扣完之后剩下的不到窗口的一半，
//!   这一秒不下结论：几秒的数据说明不了什么。
//! - **停着的时候不判断**：上一段内容到了一秒多还没有下一段，就等它接上了再说。工具参数、整个
//!   到的 Gemini 函数调用常常是停一下再一下子到一大段，在停着的那一秒下结论，量到的只是停顿。
//! - **摘要式的推理不算**：Anthropic、Gemini、Bedrock 的推理和 Responses 的推理摘要是隔几秒
//!   一段段到的，模型其实一直在快速地想（见 `crate::ending`）。推理进行着的那段时间不判断，
//!   之后从窗口里扣掉，和长静默一样。原样流出来的推理（Chat 的 `reasoning_content`、Responses
//!   的 `reasoning_text`）照常算。
//! - **第一个 token 之后不到一个窗口不判断**：开头几秒的波动不算数。第一个 token 之前的
//!   等待归无响应超时管。
//! - **回答说完了就不再判断**（上游报了停止原因）：有的上游说完之后过一阵才关连接，那段时间
//!   不该让一个完整的回答以错误收尾。
//!
//! 只管流式的回答（SSE、Bedrock 的 eventstream）：整包的回答在结束之前没有可测的速度，
//! WebSocket 上的一轮也不管，不带 `alt=sse` 的 Gemini 流（一个 JSON 数组）也不管。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tw_types::msg;

/// 连续多少秒没有内容算长静默（从窗口里扣掉）。判定窗口短的按窗口的四分之一算（见
/// [`Gauge::new`]）
pub const SILENCE_SECS: u32 = 5;

/// 一条流最近一段时间里生成了多少：每来一段内容记一笔，判断时看最近一个窗口。
///
/// **按格记，不按笔记**：一条流一秒几十笔，相隔不到四分之一秒的并成一笔。窗口最长两分钟，
/// 留下的最多几百笔。
#[derive(Debug)]
pub struct Gauge {
    /// 「一秒」有多长。**测试会把它调短**（`AppState::speed_tick`），速度按这个「秒」算
    tick: Duration,
    /// 判定窗口
    window: Duration,
    /// 长静默的门限
    silence: Duration,
    /// 第一段内容到的时刻
    first: Option<Instant>,
    /// 上一段内容到的时刻
    last: Option<Instant>,
    /// （时刻，这一笔估出来的 token），按时间排，只留窗口里的
    samples: VecDeque<(Instant, u64)>,
    /// 扣掉的时段：长静默和摘要式推理进行着的那段。（开始，结束），按时间排，只留和窗口有交集的
    gaps: VecDeque<(Instant, Instant)>,
    /// 摘要式的推理从这一刻开始，还在进行（见 [`Gauge::pause`]）
    paused: Option<Instant>,
    /// 回答说完了（见 [`Gauge::end`]）
    ended: bool,
}

impl Gauge {
    pub fn new(window_secs: u32, tick: Duration) -> Self {
        Self {
            tick,
            window: tick.saturating_mul(window_secs),
            // 窗口短的，五秒的停顿就是窗口的一大块：按窗口的四分之一算
            silence: tick
                .saturating_mul(SILENCE_SECS)
                .min(tick.saturating_mul(window_secs) / 4),
            first: None,
            last: None,
            samples: VecDeque::new(),
            gaps: VecDeque::new(),
            paused: None,
            ended: false,
        }
    }

    /// 摘要式的推理在 `at` 开始了：进行着的时候不判断，之后这段时间从窗口里扣掉
    pub fn pause(&mut self, at: Instant) {
        self.first.get_or_insert(at);
        self.paused.get_or_insert(at);
    }

    /// 推理在 `at` 结束了（回答接着出来了）
    pub fn resume(&mut self, at: Instant) {
        if let Some(from) = self.paused.take() {
            self.gaps.push_back((from, at));
            self.last = Some(self.last.map_or(at, |l| l.max(at)));
        }
    }

    /// 回答说完了：之后不再判断
    pub fn end(&mut self) {
        self.ended = true;
    }

    /// 在 `at` 来了一段内容，估出 `tokens` 个（可以是 0：一个字估不出一个 token）。摘要式的
    /// 推理进行着的时候来的不算
    pub fn content(&mut self, at: Instant, tokens: u64) {
        self.first.get_or_insert(at);
        if self.paused.is_some() {
            return;
        }
        if let Some(last) = self.last
            && at.saturating_duration_since(last) >= self.silence
        {
            self.gaps.push_back((last, at));
        }
        self.last = Some(at);
        match self.samples.back_mut() {
            Some((t, n)) if at.saturating_duration_since(*t) < self.tick / 4 => {
                *n = n.saturating_add(tokens)
            }
            _ => self.samples.push_back((at, tokens)),
        }
    }

    /// 此刻最近一个窗口的平均速度，每「秒」多少 token。说不出的时候是 `None`：还没开口、
    /// 开口不到一个窗口、停着（上一段内容是一秒多以前的）、摘要式的推理进行着、回答说完了、
    /// 扣掉长静默和推理之后剩下的不到窗口的一半
    pub fn rate(&mut self, now: Instant) -> Option<f64> {
        let first = self.first?;
        if self.ended
            || self.paused.is_some()
            || now.saturating_duration_since(self.last?) > self.tick
        {
            return None;
        }
        let start = now.checked_sub(self.window)?;
        if start < first {
            return None;
        }
        while self.samples.front().is_some_and(|(t, _)| *t <= start) {
            self.samples.pop_front();
        }
        while self.gaps.front().is_some_and(|(_, end)| *end <= start) {
            self.gaps.pop_front();
        }
        let tokens: u64 = self.samples.iter().map(|(_, n)| n).sum();
        let overlap =
            |(from, to): (Instant, Instant)| to.min(now).saturating_duration_since(from.max(start));
        // 停着的时候不判断（见上），到此刻为止的那一小段不会是长静默
        let quiet: Duration = self.gaps.iter().map(|g| overlap(*g)).sum();
        let spoken = self.window.saturating_sub(quiet);
        if spoken < self.window / 2 || spoken.is_zero() {
            return None;
        }
        Some(tokens as f64 * self.tick.as_secs_f64() / spoken.as_secs_f64())
    }
}

/// 这一家设了最低生成速度时，读流的循环怎么看着它（见 `relay`）。
#[derive(Debug, Clone, Copy)]
pub struct Watch {
    /// 每秒至少多少 token
    pub min: u32,
    /// 判定窗口，秒（配置写的，报给人看的句子用它）
    pub secs: u32,
    /// 「一秒」有多长
    pub tick: Duration,
}

impl Watch {
    pub fn of(m: &tw_config::MinSpeed, tick: Duration) -> Self {
        Self {
            min: m.tokens_per_sec,
            secs: m.window_secs,
            tick,
        }
    }

    pub fn gauge(&self) -> Gauge {
        Gauge::new(self.secs, self.tick)
    }

    /// 这个速度够不够：不够的话交回报给人看的那个数（取整，往下）
    pub fn judge(&self, rate: Option<f64>) -> Option<u32> {
        let r = rate?;
        (r < f64::from(self.min)).then(|| r.max(0.0).floor() as u32)
    }
}

/// 回答因为生成太慢被结束了：请求记录里的原因，也是交给客户端的那句话
pub fn too_slow(upstream: &str, rate: u32, min: u32, secs: u32) -> tw_types::Msg {
    msg!(
        "gw.upstream.too_slow", upstream = upstream, rate = rate, min = min, secs = secs =>
        "Upstream `{upstream}` generated {rate} tokens per second over the last {secs} seconds, \
         below the minimum of {min}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn at(t: Instant, secs: f64) -> Instant {
        t + Duration::from_secs_f64(secs)
    }

    /// 每 `every` 秒来一段、每段 `n` 个，从 `from` 到 `to`
    fn steady(g: &mut Gauge, t: Instant, from: f64, to: f64, every: f64, n: u64) {
        let mut s = from;
        while s < to {
            g.content(at(t, s), n);
            s += every;
        }
    }

    #[test]
    fn nothing_is_said_before_a_whole_window_after_the_first_token() {
        let t = Instant::now();
        let mut g = Gauge::new(20, S);
        assert_eq!(g.rate(at(t, 30.0)), None, "还没开口");
        steady(&mut g, t, 10.0, 29.5, 0.5, 1);
        assert_eq!(g.rate(at(t, 29.5)), None, "开口不到 20 秒");
        let r = g.rate(at(t, 30.0)).unwrap();
        assert!((r - 2.0).abs() < 0.11, "{r}");
    }

    #[test]
    fn the_window_only_counts_the_last_seconds() {
        let t = Instant::now();
        let mut g = Gauge::new(10, S);
        // 前十秒每秒 100 个，之后每秒 5 个
        steady(&mut g, t, 0.0, 10.0, 0.5, 50);
        steady(&mut g, t, 10.0, 31.0, 1.0, 5);
        let r = g.rate(at(t, 30.0)).unwrap();
        assert!((r - 5.0).abs() < 0.01, "{r}");
    }

    /// 中途停了 10 秒（长静默）：那段时间不算，速度按说话的时间算
    #[test]
    fn a_long_silence_is_taken_out_of_the_window() {
        let t = Instant::now();
        let mut g = Gauge::new(20, S);
        steady(&mut g, t, 0.0, 20.0, 0.25, 5);
        // 20 秒到 30 秒一个字都没有，之后照常
        steady(&mut g, t, 30.0, 40.0, 0.25, 5);
        let r = g.rate(at(t, 40.0)).unwrap();
        assert!((r - 20.0).abs() < 1.1, "{r}");
        // 接上之后，扣掉之后不到窗口的一半就不说
        let mut g = Gauge::new(20, S);
        steady(&mut g, t, 0.0, 20.0, 0.25, 5);
        steady(&mut g, t, 31.0, 32.0, 0.25, 5);
        assert_eq!(g.rate(at(t, 32.0)), None, "静了 11 秒，只剩 9 秒");
    }

    /// 停着的时候不判断：等内容接上了再说。窗口短的，停一会儿就算长静默
    #[test]
    fn a_pause_is_not_judged_until_the_content_resumes() {
        let t = Instant::now();
        let mut g = Gauge::new(5, S);
        steady(&mut g, t, 0.0, 10.0, 0.25, 5);
        assert!(g.rate(at(t, 10.0)).is_some());
        // 9.75 秒之后停了 2.5 秒：停着的时候一直不说
        for s in [10.8, 11.5, 12.0] {
            assert_eq!(g.rate(at(t, s)), None, "{s}");
        }
        // 一下子补上一大段（工具参数那样）：那 2.5 秒是长静默（窗口 5 秒的门限是 1.25 秒），扣掉
        g.content(at(t, 12.25), 50);
        steady(&mut g, t, 12.5, 15.0, 0.25, 5);
        let r = g.rate(at(t, 15.0)).unwrap();
        assert!(r > 20.0, "{r}");
    }

    /// 摘要式的推理进行着的时候不判断，那段时间和其中的字都不算
    #[test]
    fn summarised_reasoning_is_left_out() {
        let t = Instant::now();
        let mut g = Gauge::new(20, S);
        steady(&mut g, t, 0.0, 20.0, 0.25, 5);
        g.pause(at(t, 20.0));
        // 隔四秒一段摘要
        for s in [20.0, 24.0, 28.0, 32.0] {
            g.content(at(t, s), 40);
            assert_eq!(g.rate(at(t, s + 0.1)), None, "推理进行着：{s}");
        }
        g.resume(at(t, 34.0));
        steady(&mut g, t, 34.0, 50.0, 0.25, 5);
        // 窗口 30 到 50：扣掉 30 到 34，剩 16 秒，每秒 20 个
        let r = g.rate(at(t, 50.0)).unwrap();
        assert!((r - 20.0).abs() < 1.5, "{r}");
    }

    #[test]
    fn nothing_is_judged_once_the_answer_is_over() {
        let t = Instant::now();
        let mut g = Gauge::new(5, S);
        steady(&mut g, t, 0.0, 10.0, 1.0, 1);
        assert!(g.rate(at(t, 9.5)).is_some());
        g.end();
        assert_eq!(g.rate(at(t, 9.5)), None);
    }

    /// 短于门限的停顿照算：每 4 秒来一段，算的是平均下来的慢
    #[test]
    fn short_pauses_count_as_slow() {
        let t = Instant::now();
        let mut g = Gauge::new(20, S);
        steady(&mut g, t, 0.0, 40.0, 4.0, 8);
        let r = g.rate(at(t, 36.5)).unwrap();
        assert!((r - 2.0).abs() < 0.5, "{r}");
    }

    #[test]
    fn the_watch_reports_the_rate_rounded_down_and_only_when_below() {
        let w = Watch {
            min: 15,
            secs: 20,
            tick: S,
        };
        assert_eq!(w.judge(None), None);
        assert_eq!(w.judge(Some(15.0)), None);
        assert_eq!(w.judge(Some(14.9)), Some(14));
        assert_eq!(w.judge(Some(0.0)), Some(0));
    }

    #[test]
    fn the_sentence_carries_every_argument() {
        let m = too_slow("local", 3, 15, 20);
        assert_eq!(m.code, "gw.upstream.too_slow");
        assert_eq!(
            (
                m.arg("upstream"),
                m.arg("rate"),
                m.arg("min"),
                m.arg("secs")
            ),
            ("local", "3", "15", "20")
        );
        assert_eq!(
            m.text,
            "Upstream `local` generated 3 tokens per second over the last 20 seconds, below the \
             minimum of 15."
        );
    }
}

//! 每家上游的典型首字节时间（`url-test`）。
//!
//! # 为什么是 TTFB 而不是总时长
//!
//! 总时长里最大的一项是「模型说了多少字」，那和上游快不快无关 ——
//! 一个回答长的请求会让最快的上游看起来最慢。**TTFB 才是这一层能控制、
//! 用户也真的在等的那段**（L3 也是这个判据）。
//!
//! # 为什么是中位数而不是 EWMA
//!
//! EWMA 的问题是**说不清**：用户问「为什么切到乙了」，我们只能给一个
//! 被历史加权过的数，而它对应不到任何一次真实的请求。中位数返回的
//! 永远是一个**真实发生过的值**，用户能在请求列表里找到它 —— 和分位数
//! 用最近秩法是同一条理由。
//!
//! 这也是为什么当初说「EWMA 有意不做」：那时它没有消费者。
//! 现在 `url-test` 就是它的消费者，而消费者想要的是能解释的数。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// 每家留多少个样本。
///
/// **小是有意的**：上游的快慢是会变的（换了机房、限流开始了），而一个
/// 长窗口会让「它现在很慢」被三小时前的好成绩压住。
const WINDOW: usize = 32;

/// 少于这么多样本就当作「没测过」。
///
/// **一个样本不能决定路由**：第一次请求可能撞上冷启动、TLS 全握手、
/// DNS 没缓存 —— 那个数字比没有更误导。
const MIN_SAMPLES: usize = 3;

#[derive(Default)]
pub struct Latency {
    inner: Mutex<HashMap<String, VecDeque<u32>>>,
}

impl Latency {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次。**转发路径上调，所以必须便宜**：一次哈希加一次 push。
    pub fn record(&self, provider: &str, ttfb_ms: u32) {
        let mut g = self.inner.lock().expect("lock not poisoned");
        let w = g.entry(provider.to_string()).or_default();
        if w.len() == WINDOW {
            w.pop_front();
        }
        w.push_back(ttfb_ms);
    }

    /// 启动时用 L1 握手的结果垫一个底（样本不够时用零成本的 L1 补）。
    ///
    /// **只在完全没有真实样本时垫**。真实流量一到就该由它说了算 ——
    /// L1 量的是建连，不含上游排队和推理，天生偏乐观。
    pub fn seed(&self, provider: &str, ttfb_ms: u32) {
        let mut g = self.inner.lock().expect("lock not poisoned");
        let w = g.entry(provider.to_string()).or_default();
        if w.is_empty() {
            // 垫满 `MIN_SAMPLES` 才算数 —— 否则它自己也是「样本不够」
            for _ in 0..MIN_SAMPLES {
                w.push_back(ttfb_ms);
            }
        }
    }

    /// 这家的典型 TTFB。`None` = 样本不够，**不是「很快」**。
    pub fn typical(&self, provider: &str) -> Option<u32> {
        let g = self.inner.lock().expect("lock not poisoned");
        let w = g.get(provider)?;
        if w.len() < MIN_SAMPLES {
            return None;
        }
        let mut v: Vec<u32> = w.iter().copied().collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    /// 一次取一批 —— 排序时要用到，逐个取会连着锁好几次。
    pub fn snapshot(&self, names: &[String]) -> HashMap<String, u32> {
        let g = self.inner.lock().expect("lock not poisoned");
        let mut out = HashMap::new();
        for n in names {
            if let Some(w) = g.get(n)
                && w.len() >= MIN_SAMPLES
            {
                let mut v: Vec<u32> = w.iter().copied().collect();
                v.sort_unstable();
                out.insert(n.clone(), v[v.len() / 2]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_sample_is_not_enough_to_decide_a_route() {
        // 第一次请求可能撞上冷启动、全握手、DNS 没缓存 —— 那个数字
        // 比没有更误导
        let l = Latency::new();
        l.record("甲", 900);
        assert_eq!(l.typical("甲"), None);
        l.record("甲", 100);
        l.record("甲", 110);
        assert_eq!(l.typical("甲"), Some(110), "三个样本的中位数");
    }

    #[test]
    fn the_median_is_always_a_number_that_really_happened() {
        // 用户问「为什么切到乙了」时，这个数要能在请求列表里找得到
        let l = Latency::new();
        for ms in [100, 105, 110, 5000, 108] {
            l.record("甲", ms);
        }
        let t = l.typical("甲").unwrap();
        assert!(
            [100, 105, 110, 5000, 108].contains(&t),
            "{t} 不是真实发生过的值"
        );
        assert_eq!(t, 108, "一次抖动不该带偏中位数");
    }

    #[test]
    fn a_long_history_does_not_hide_that_it_is_slow_now() {
        // 上游的快慢是会变的。**窗口小是有意的**
        let l = Latency::new();
        for _ in 0..WINDOW {
            l.record("甲", 100);
        }
        for _ in 0..WINDOW {
            l.record("甲", 900);
        }
        assert_eq!(l.typical("甲"), Some(900), "三小时前的好成绩压住了现在");
    }

    #[test]
    fn an_l1_seed_yields_to_real_traffic() {
        // L1 量的是建连，不含上游排队和推理，天生偏乐观
        let l = Latency::new();
        l.seed("甲", 20);
        assert_eq!(l.typical("甲"), Some(20));
        for _ in 0..WINDOW {
            l.record("甲", 300);
        }
        assert_eq!(l.typical("甲"), Some(300));
        // 已经有真实样本之后，再 seed 一次不该把它顶回去
        l.seed("甲", 20);
        assert_eq!(l.typical("甲"), Some(300));
    }

    #[test]
    fn a_snapshot_leaves_out_the_ones_with_too_few_samples() {
        let l = Latency::new();
        l.record("甲", 100);
        for _ in 0..3 {
            l.record("乙", 200);
        }
        let names = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
        let s = l.snapshot(&names);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s.get("乙"), Some(&200));
    }
}

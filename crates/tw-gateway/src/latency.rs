//! 每家上游典型的快慢：**从这一跳发出去，到回答的第一段内容**。`url-test` 按它挑最快的，
//! `load-balance` 按快慢分时（`balance_by: latency`）按它算系数 —— **只认真实样本**（见
//! [`Latency::measured`]）。
//!
//! # 量的是哪一段
//!
//! - **起点是这一跳发出去的那一刻**，不是请求进网关的那一刻：等密钥的上限、等上游的空位、
//!   跑插件、前面失败了的几跳，都不是这一家慢。记到它头上的话，排在第二位的那家永远替
//!   第一家背着那段时间。
//! - **终点是第一段内容**（正文、推理的字、工具调用的开头 —— 和首 token 同一个认法，见
//!   [`crate::ending`]），不是响应头：不少中转收下请求马上回 200，然后在流里排队，响应头
//!   来得快什么都说明不了。也不是总时长：总时长里最大的一项是「模型说了多少字」，那和
//!   上游快不快无关 —— 一个回答长的请求会让最快的上游看起来最慢。
//! - **和这一跳排在第几家无关**：排在前面的那几家，开头要先看一眼有没有报错（见
//!   `server::pipeline::opening`），最后一家直接转发 —— 量的都是同一段。
//! - **只有流式回答有样本**：整包的回答只有「全到了」那一个时刻，分不出哪段在排队、哪段
//!   在说话。
//! - **开头慢被放弃的那一家**（`failover.next_on_slow_start`）记它被给的那段时间：它至少
//!   这么慢，记成这个数，它就排到慢的那一头去。什么都不记的话，它留着的还是从前快的
//!   样本，下一个请求还先发给它。
//!
//! # 为什么是中位数而不是 EWMA
//!
//! EWMA 的问题是**说不清**：用户问「为什么切到乙了」，我们只能给一个
//! 被历史加权过的数，而它对应不到任何一次真实的请求。中位数返回的
//! 永远是一个**真实发生过的值** —— 和分位数用最近秩法是同一条理由。
//!
//! 这也是为什么当初说「EWMA 有意不做」：那时它没有消费者。
//! 现在 `url-test` 就是它的消费者，而消费者想要的是能解释的数。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

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

/// 一家的样本。
#[derive(Default)]
struct Window {
    /// 真实请求量到的，最近的在后面
    real: VecDeque<u32>,
    /// 启动时 L1 握手垫的底（见 [`seed_url_test`]）。**真实样本够数之前顶着用，够数就扔掉**，
    /// 从不和真实样本放在一起取中位数
    seed: Option<u32>,
}

impl Window {
    /// 这家的典型值：真实样本够数就是它们的中位数，不够时有垫底的用垫底的，都没有是 `None`
    fn typical(&self) -> Option<u32> {
        self.measured().or(self.seed)
    }

    /// 真实样本够数时它们的中位数。垫的底不算
    fn measured(&self) -> Option<u32> {
        if self.real.len() < MIN_SAMPLES {
            return None;
        }
        let mut v: Vec<u32> = self.real.iter().copied().collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }
}

/// 一段时间写成样本的毫秒数
pub fn ms(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

#[derive(Default)]
pub struct Latency {
    inner: Mutex<HashMap<String, Window>>,
}

impl Latency {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一个真实样本，毫秒。**转发路径上调，所以必须便宜**：一次哈希加一次 push。
    pub fn record(&self, provider: &str, ms: u32) {
        let mut g = self.inner.lock().expect("lock not poisoned");
        let w = g.entry(provider.to_string()).or_default();
        if w.real.len() == WINDOW {
            w.real.pop_front();
        }
        w.real.push_back(ms);
        // 真实样本够数了：垫的底用不着了。**扔掉，不是混进去** —— L1 量的是建连，不含上游
        // 排队和推理，混进中位数会把一家慢的上游拉成快的
        if w.real.len() >= MIN_SAMPLES {
            w.seed = None;
        }
    }

    /// 用 L1 握手的结果垫一个底：真实样本还不够数时，[`Self::typical`] 先用它。
    ///
    /// **真实样本够数之后不再垫**：真实流量一到就该由它说了算。够数之前再垫一次（每天跟着
    /// 模型清单刷新一次）换成新测的那个数。
    ///
    /// 不够数时用垫底的而不是当作没测过：`url-test` 把没测过的排在最后，一家刚收到第一个
    /// 请求的上游要是因此沉到最后，就再也轮不到它攒够样本了。
    pub fn seed(&self, provider: &str, ms: u32) {
        let mut g = self.inner.lock().expect("lock not poisoned");
        let w = g.entry(provider.to_string()).or_default();
        if w.real.len() < MIN_SAMPLES {
            w.seed = Some(ms);
        }
    }

    /// 这家的典型值，毫秒。`None` = 样本不够、也没垫过底，**不是「很快」**。
    pub fn typical(&self, provider: &str) -> Option<u32> {
        let g = self.inner.lock().expect("lock not poisoned");
        g.get(provider)?.typical()
    }

    /// 一次取一批 —— 排序时要用到，逐个取会连着锁好几次。`url-test` 用：垫过底的算测过
    pub fn snapshot(&self, names: &[String]) -> HashMap<String, u32> {
        self.batch(names, Window::typical)
    }

    /// 一批里**真实样本够数的**那几家，`load-balance` 按快慢分时用。**垫的底不算**：L1 握手
    /// 量的是建连（几十毫秒），真实样本量到第一段内容（几秒），两样一比，只垫过底的那一家
    /// 被算成快几十倍、拿到十倍的份额。没测过的不在里面，系数算 1（中等）—— 和 `url-test`
    /// 不同，这里没测过的照样分到请求，攒得到真实样本
    pub fn measured(&self, names: &[String]) -> HashMap<String, u32> {
        self.batch(names, Window::measured)
    }

    fn batch(&self, names: &[String], of: fn(&Window) -> Option<u32>) -> HashMap<String, u32> {
        let g = self.inner.lock().expect("lock not poisoned");
        names
            .iter()
            .filter_map(|n| Some((n.clone(), of(g.get(n)?)?)))
            .collect()
    }
}

/// 给 `url-test` 组的成员垫一个底（样本不够时用零成本的 L1 补）。
///
/// **只测 `url-test` 组里的那些，启动时和之后每天各测一次**（跟着模型清单
/// 的刷新一起跑，见 [`crate::models::spawn`]）。
/// 没有这一步的话，`url-test` 在攒够真实样本之前完全等同于 `fallback`
/// —— 用户配了「选最快的」，而头几十个请求全落在配置里排第一那家。
///
/// L1 是握手计时，不发一个 API 请求、不花一分钱。**真实样本够数之后它就不算了**
/// （见 [`Latency::seed`]）—— 真实流量一到就该由它说了算。
pub async fn seed_url_test(state: &crate::state::AppState) {
    let rt = state.runtime();
    let mut want: Vec<String> = Vec::new();
    for g in rt.engine.groups() {
        if g.kind == tw_engine::GroupType::UrlTest {
            want.extend(g.names());
        }
    }
    want.sort();
    want.dedup();
    if want.is_empty() {
        return;
    }
    for name in want {
        // 停用的不参与路由，用不着垫
        let Some(p) = rt
            .config
            .providers
            .iter()
            .find(|p| p.name == name && !p.disabled)
        else {
            continue;
        };
        // 走代理的那家要测它真正会走的那条路。`system` 测不了，
        // 那时不垫底 —— 假装直连测一遍给的数字，测的根本不是那条路
        let hop = match crate::l1::hop_for(&rt.config, p) {
            Ok(h) => h,
            Err(why) => {
                tracing::debug!(provider = %name, %why, "this upstream cannot be link-tested, so no latency sample is seeded");
                continue;
            }
        };
        let r = crate::l1::l1(&p.base_url, hop.as_ref()).await;
        if r.ok {
            tracing::debug!(provider = %name, ms = r.total_ms, "seeding a latency sample from the link test");
            state
                .latency
                .seed(&name, r.total_ms.min(u32::MAX as u64) as u32);
        } else {
            // **连都连不上的那家不垫。**它会因为「没样本」排在最后，
            // 而那正是对的
            tracing::debug!(provider = %name, "the link test could not connect, so no latency sample is seeded");
        }
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
        // 用户问「为什么切到乙了」时，这个数要是某一次请求真的量到的
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
    fn a_seed_stands_in_until_real_samples_are_enough_and_is_never_mixed_in() {
        let l = Latency::new();
        l.seed("甲", 20);
        // 真实样本还不够数：顶着用垫的底。当作没测过的话，`url-test` 把它排到最后，它就再也
        // 攒不够样本了
        l.record("甲", 900);
        l.record("甲", 50);
        assert_eq!(l.typical("甲"), Some(20));
        // 够数了：只看真实的。混在一起取中位数是 [20, 20, 20, 50, 900, 900] 里的 50
        l.record("甲", 900);
        assert_eq!(l.typical("甲"), Some(900));
        let names = vec!["甲".to_string()];
        assert_eq!(l.snapshot(&names).get("甲"), Some(&900));
    }

    #[test]
    fn a_new_seed_replaces_the_old_one_while_real_samples_are_too_few() {
        // 每天跟着模型清单再测一次：还没攒够真实样本的，换成新测的数
        let l = Latency::new();
        l.seed("甲", 20);
        l.record("甲", 500);
        l.seed("甲", 80);
        assert_eq!(l.typical("甲"), Some(80));
    }

    #[test]
    fn a_duration_is_written_in_milliseconds() {
        assert_eq!(ms(Duration::from_millis(1_234)), 1_234);
        assert_eq!(ms(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    #[test]
    fn a_snapshot_leaves_out_the_ones_with_too_few_samples() {
        let l = Latency::new();
        l.record("甲", 100);
        for _ in 0..3 {
            l.record("乙", 200);
        }
        l.seed("丁", 30);
        let names = vec![
            "甲".to_string(),
            "乙".to_string(),
            "丙".to_string(),
            "丁".to_string(),
        ];
        // `url-test` 看的那一份：垫过底的算测过
        let s = l.snapshot(&names);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s.get("乙"), Some(&200));
        assert_eq!(s.get("丁"), Some(&30), "垫过底的算测过");
        // 按快慢分的 `load-balance` 看的那一份：只有真实样本够数的
        let m = l.measured(&names);
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!(m.get("乙"), Some(&200));
    }
}

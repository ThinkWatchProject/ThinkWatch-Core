//! 流里估出来的 token 和上游报的实数差多少，按上游 + 模型记。
//!
//! `/live` 的速度是读着回答的流现估的（见 [`crate::ending::Ending::streaming`]）：每段正文按
//! 字数估 token，和估输入同一个算法（ASCII 四个字节一个，其余一个字一个）。各家分词不同，
//! 中文差得尤其多。**每个流式请求结束时拿上游报的输出数校准一次**：实数除以估数，按
//! 滑动平均记下，之后同一家同一个模型的估数乘上它。重启之后从头校准，第一个请求用的是
//! 没校准过的估法。
//!
//! 只管 `/live` 的那个数。请求结束后记下的速度按上游报的实数算，用不着它。

use std::collections::HashMap;
use std::sync::Mutex;

/// 新的一个样本占多少分量
const WEIGHT: f64 = 0.3;
/// 比例限在这个范围里：估法再差也差不出两倍，超出的多半是这一次估漏了什么（隐藏的推理）
const RANGE: (f64, f64) = (0.5, 2.0);
/// 估出来不到这么多个的不拿来校准：几个 token 的回答，差一两个就是几成
const MIN_SAMPLE: u64 = 50;

#[derive(Debug, Default)]
pub struct Pace {
    ratios: Mutex<HashMap<(String, String), f64>>,
}

impl Pace {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这家上游这个模型，估数该乘多少。没校准过是 1
    pub fn ratio(&self, provider: &str, model: &str) -> f64 {
        let r = self.ratios.lock().unwrap_or_else(|p| p.into_inner());
        r.get(&(provider.to_string(), model.to_string()))
            .copied()
            .unwrap_or(1.0)
    }

    /// 一个流式请求结束了：估出来 `estimated` 个（没乘比例的），上游说是 `actual` 个
    pub fn learn(&self, provider: &str, model: &str, estimated: u64, actual: u64) {
        if estimated < MIN_SAMPLE || actual == 0 {
            return;
        }
        let sample = (actual as f64 / estimated as f64).clamp(RANGE.0, RANGE.1);
        let mut r = self.ratios.lock().unwrap_or_else(|p| p.into_inner());
        let entry = r.entry((provider.to_string(), model.to_string()));
        entry
            .and_modify(|old| *old = *old * (1.0 - WEIGHT) + sample * WEIGHT)
            .or_insert(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sample_is_taken_as_it_is_and_later_ones_move_it_gradually() {
        let p = Pace::new();
        assert_eq!(p.ratio("a", "m"), 1.0);
        p.learn("a", "m", 100, 150);
        assert_eq!(p.ratio("a", "m"), 1.5);
        p.learn("a", "m", 100, 100);
        assert!((p.ratio("a", "m") - 1.35).abs() < 1e-9);
        // 别家、别的模型不受影响
        assert_eq!(p.ratio("a", "n"), 1.0);
        assert_eq!(p.ratio("b", "m"), 1.0);
    }

    #[test]
    fn small_and_wild_samples_are_kept_in_bounds() {
        let p = Pace::new();
        p.learn("a", "m", 10, 100);
        assert_eq!(p.ratio("a", "m"), 1.0, "too small to judge");
        p.learn("a", "m", 100, 1_000);
        assert_eq!(p.ratio("a", "m"), 2.0);
        p.learn("b", "m", 100, 0);
        assert_eq!(p.ratio("b", "m"), 1.0);
    }
}

//! 数值比较：`">200k"`、`"<4k"`、`">=$2.50"`。
//!
//! 写成 `">200k"` 而不是 `TOKENS-GT,200000`：**`200k`
//! 本来就是大家在说 token 数时的说法**，而规则是给人读的。

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
}

/// 一个比较式。数值统一按 f64 存 —— token 数是整数，成本是小数，
/// 用一个类型省掉两套解析。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Compare {
    pub op: Op,
    pub value: f64,
}

impl Compare {
    pub fn matches(&self, x: f64) -> bool {
        match self.op {
            Op::Gt => x > self.value,
            Op::Gte => x >= self.value,
            Op::Lt => x < self.value,
            Op::Lte => x <= self.value,
            // 浮点相等在这里是有意义的：用户写 `= 0` 是想匹配「一个都没有」。
            // 但别的值上它很脆，所以给一个小容差。
            Op::Eq => (x - self.value).abs() < 1e-9,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("比较式是空的")]
    Empty,
    #[error("`{0}` 没有比较符。要写成 \">200k\" 这样，前面得有 > < >= <= = 之一")]
    NoOperator(String),
    #[error("`{0}` 里的数字读不出来")]
    BadNumber(String),
    #[error("`{0}` 的单位不认识。支持 k / m（千 / 百万），钱写成 $2.5")]
    BadUnit(String),
}

impl FromStr for Compare {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        // 两字符的操作符先试，否则 `>=` 会被读成 `>`
        let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
            (Op::Gte, r)
        } else if let Some(r) = s.strip_prefix("<=") {
            (Op::Lte, r)
        } else if let Some(r) = s.strip_prefix('>') {
            (Op::Gt, r)
        } else if let Some(r) = s.strip_prefix('<') {
            (Op::Lt, r)
        } else if let Some(r) = s.strip_prefix("==") {
            (Op::Eq, r)
        } else if let Some(r) = s.strip_prefix('=') {
            (Op::Eq, r)
        } else {
            // **不要默认成相等**。用户写 `input_tokens: "200k"` 十有八九
            // 是想写 `">200k"`，而静默当成相等匹配会让规则永远不命中，
            // 且完全看不出原因。
            return Err(ParseError::NoOperator(s.to_string()));
        };
        Ok(Compare {
            op,
            value: parse_value(rest.trim())?,
        })
    }
}

/// 解析 `200k` / `4.5m` / `$2.50` / `1200`。
fn parse_value(s: &str) -> Result<f64, ParseError> {
    let s = s.trim();
    // 钱的 `$` 只是给人看的标记 —— 单位由字段本身决定，不由这里决定
    let s = s.strip_prefix('$').unwrap_or(s);
    if s.is_empty() {
        return Err(ParseError::BadNumber(s.to_string()));
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1_000.0),
        'm' => (&s[..s.len() - 1], 1_000_000.0),
        c if c.is_ascii_digit() || c == '.' => (s, 1.0),
        _ => return Err(ParseError::BadUnit(s.to_string())),
    };
    num.trim()
        .parse::<f64>()
        .map(|v| v * mult)
        .map_err(|_| ParseError::BadNumber(s.to_string()))
}

impl fmt::Display for Compare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self.op {
            Op::Gt => ">",
            Op::Gte => ">=",
            Op::Lt => "<",
            Op::Lte => "<=",
            Op::Eq => "=",
        };
        write!(f, "{op}{}", self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Compare {
        s.parse().unwrap()
    }

    #[test]
    fn reads_the_way_people_actually_write_token_counts() {
        assert_eq!(c(">200k").value, 200_000.0);
        assert_eq!(c("<4k").value, 4_000.0);
        assert_eq!(c(">1.5m").value, 1_500_000.0);
        assert_eq!(c(">=8000").value, 8_000.0);
    }

    #[test]
    fn two_character_operators_win_over_one() {
        // `>=` 被读成 `>` 的话，`>=200k` 会漏掉正好 200k 的那次请求 ——
        // 而边界值恰恰是用户写这条规则时想到的那个数。
        assert_eq!(c(">=100").op, Op::Gte);
        assert_eq!(c("<=100").op, Op::Lte);
        assert_eq!(c(">100").op, Op::Gt);
    }

    #[test]
    fn the_dollar_sign_is_decoration() {
        // 单位由字段决定（session_cost 是钱，input_tokens 是数量），
        // 不由这个符号决定。
        assert_eq!(c(">$2.50").value, 2.5);
        assert_eq!(c(">2.50").value, 2.5);
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_assumed_to_mean_equals() {
        // `input_tokens: "200k"` 十有八九是想写 `">200k"`。静默当成
        // 相等匹配会让规则永远不命中，而且完全看不出原因。
        assert_eq!(
            "200k".parse::<Compare>().unwrap_err(),
            ParseError::NoOperator("200k".into())
        );
    }

    #[test]
    fn nonsense_is_refused_with_a_message_that_says_the_shape() {
        assert!(matches!("".parse::<Compare>(), Err(ParseError::Empty)));
        assert!(matches!(
            ">abc".parse::<Compare>(),
            Err(ParseError::BadUnit(_))
        ));
        assert!(matches!(
            ">".parse::<Compare>(),
            Err(ParseError::BadNumber(_))
        ));
        let e = "200k".parse::<Compare>().unwrap_err().to_string();
        assert!(e.contains(">200k"), "错误信息要给出正确写法：{e}");
    }

    #[test]
    fn comparison_actually_compares() {
        assert!(c(">200k").matches(300_000.0));
        assert!(!c(">200k").matches(200_000.0));
        assert!(c(">=200k").matches(200_000.0));
        assert!(c("<4k").matches(3_999.0));
        assert!(!c("<4k").matches(4_000.0));
        assert!(c("=0").matches(0.0));
    }

    #[test]
    fn whitespace_around_the_value_is_tolerated() {
        // YAML 里 `input_tokens: "> 200k"` 是很自然的写法。
        assert_eq!(c("> 200k").value, 200_000.0);
        assert_eq!(c("  >200k  ").value, 200_000.0);
    }
}

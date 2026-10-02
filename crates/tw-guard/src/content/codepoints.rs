//! 码位：内容规则的第三种匹配，按字符本身认，不按写出来的样子。
//!
//! 看不见的字符没法写成关键词，也不该写成正则（`[\u{E0000}-\u{E007F}]` 能用，但没人
//! 读得懂、更没人写得对）。码位就是 Unicode 给每个字符的编号，写成 `U+200B`，一段写成
//! `U+E0000–U+E007F` —— 和 Unicode 码表、各种字符查询工具上写的一样。
//!
//! # 写法
//!
//! 若干项，用逗号（`,` 或 `，`）、顿号或空白隔开。每一项是 `U+十六进制`，或者
//! `U+十六进制-U+十六进制`（中间用短横线 `-` 或 en dash `–`，两边可以有空格）。`u+`
//! 不分大小写，十六进制 1 到 6 位、不分大小写。码位在 0 到 10FFFF 之间、不是代理区
//! （D800–DFFF：它们不是字符，只出现在 UTF-16 的编码里），一段的起点不大于终点。
//! 最多 [`MAX_ITEMS`] 项。
//!
//! 内置规则显示时写成规范写法（[`Codepoints::canonical`]）：大写、至少四位、一段用 en
//! dash，和码表上的写法一样。用户自己写的规则显示的是它写的样子（[`Codepoints::written`]）：
//! 管理界面按显示出来的规则拼回整份策略再存，显示的必须就是存着的那一份。

use std::fmt;

/// 一条规则最多写多少项。**再多就不是一条规则了**：真要排除一大片的，用一段范围写。
pub const MAX_ITEMS: usize = 32;

/// 解析好的一组码位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codepoints {
    /// 按写的顺序，每项是含两头的一段（单个码位两头相同）。显示用
    items: Vec<(u32, u32)>,
    /// 每一项原来写的样子（`u+200b`、`U+202A - U+202E`）
    written: Vec<String>,
    /// 排好序、合并过的。匹配用
    merged: Vec<(u32, u32)>,
}

/// 码位写得不对。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodepointError {
    #[error("no code point is written")]
    Empty,
    #[error("there are more than {max} items")]
    TooMany { max: usize },
    #[error(
        "`{item}` is not written as U+ followed by 1 to 6 hexadecimal digits, or two of them joined by a dash"
    )]
    Syntax { item: String },
    #[error("`{item}` is beyond U+10FFFF, the last code point")]
    OutOfRange { item: String },
    #[error("`{item}` is a surrogate (U+D800 to U+DFFF), which is not a character")]
    Surrogate { item: String },
    #[error("`{item}` starts after it ends")]
    Reversed { item: String },
}

impl CodepointError {
    /// 稳定的错误码，给界面挑一句自己的话
    pub fn code(&self) -> &'static str {
        match self {
            CodepointError::Empty => "codepoints_empty",
            CodepointError::TooMany { .. } => "codepoints_too_many",
            CodepointError::Syntax { .. } => "codepoints_syntax",
            CodepointError::OutOfRange { .. } => "codepoints_out_of_range",
            CodepointError::Surrogate { .. } => "codepoints_surrogate",
            CodepointError::Reversed { .. } => "codepoints_reversed",
        }
    }
}

/// 写法里的一个记号
#[derive(Debug, PartialEq)]
enum Token<'a> {
    /// `U+…`，原样
    Point(&'a str),
    Dash,
}

fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '，' | '、')
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '–')
}

/// 切成记号，各带它在原文里的位置
fn tokens(s: &str) -> Vec<(Token<'_>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(c) = s[at..].chars().next() {
        if is_separator(c) {
            at += c.len_utf8();
        } else if is_dash(c) {
            out.push((Token::Dash, at..at + c.len_utf8()));
            at += c.len_utf8();
        } else {
            let end = s[at..]
                .find(|c: char| is_separator(c) || is_dash(c))
                .map_or(s.len(), |i| at + i);
            out.push((Token::Point(&s[at..end]), at..end));
            at = end;
        }
    }
    out
}

/// `U+200B` → 0x200B
fn point(item: &str) -> Result<u32, CodepointError> {
    let syntax = || CodepointError::Syntax {
        item: item.to_string(),
    };
    let hex = item
        .strip_prefix("U+")
        .or_else(|| item.strip_prefix("u+"))
        .ok_or_else(syntax)?;
    if hex.is_empty() || hex.len() > 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(syntax());
    }
    let n = u32::from_str_radix(hex, 16).map_err(|_| syntax())?;
    if n > 0x10FFFF {
        return Err(CodepointError::OutOfRange {
            item: item.to_string(),
        });
    }
    if (0xD800..=0xDFFF).contains(&n) {
        return Err(CodepointError::Surrogate {
            item: item.to_string(),
        });
    }
    Ok(n)
}

impl Codepoints {
    /// 按上面的写法读一段文字。
    pub fn parse(s: &str) -> Result<Codepoints, CodepointError> {
        let toks = tokens(s);
        let mut items = Vec::new();
        let mut written = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            let (Token::Point(a), ref span) = toks[i] else {
                return Err(CodepointError::Syntax {
                    item: "–".to_string(),
                });
            };
            let from = point(a)?;
            if matches!(toks.get(i + 1), Some((Token::Dash, _))) {
                let Some((Token::Point(b), end)) = toks.get(i + 2) else {
                    return Err(CodepointError::Syntax {
                        item: format!("{a}–"),
                    });
                };
                let to = point(b)?;
                if from > to {
                    return Err(CodepointError::Reversed {
                        item: format!("{a}–{b}"),
                    });
                }
                items.push((from, to));
                written.push(s[span.start..end.end].to_string());
                i += 3;
            } else {
                items.push((from, from));
                written.push(a.to_string());
                i += 1;
            }
            if items.len() > MAX_ITEMS {
                return Err(CodepointError::TooMany { max: MAX_ITEMS });
            }
        }
        if items.is_empty() {
            return Err(CodepointError::Empty);
        }
        let mut merged = items.clone();
        merged.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(merged.len());
        for (a, b) in merged {
            match out.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        Ok(Codepoints {
            items,
            written,
            merged: out,
        })
    }

    /// 这个字符在不在里面
    pub fn contains(&self, c: char) -> bool {
        let n = c as u32;
        let i = self.merged.partition_point(|(_, b)| *b < n);
        self.merged.get(i).is_some_and(|(a, _)| *a <= n)
    }

    /// 最小的那个码位。都在 ASCII 之外的，一段纯 ASCII 的正文可以整段跳过
    pub fn min(&self) -> u32 {
        self.merged.first().map_or(0, |(a, _)| *a)
    }

    /// 每一项原来写的样子，按写的顺序。各项用 `, ` 连起来就是一份意思不变的写法
    pub fn written(&self) -> &[String] {
        &self.written
    }

    /// 规范写法，一项一个：`U+200B`、`U+E0000–U+E007F`。按写的顺序
    pub fn canonical(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    format!("U+{a:04X}")
                } else {
                    format!("U+{a:04X}–U+{b:04X}")
                }
            })
            .collect()
    }
}

/// 规范写法，各项之间用 `, `
impl fmt::Display for Codepoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical().join(", "))
    }
}

/// 一个字符写成看得见的样子：`‹U+200B›`
pub fn visible(c: char) -> String {
    format!("‹U+{:04X}›", c as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_forms_people_actually_type_are_read() {
        let p = |s: &str| Codepoints::parse(s).unwrap().canonical();
        assert_eq!(p("U+200B"), ["U+200B"]);
        assert_eq!(p("u+e0000-u+e007f"), ["U+E0000–U+E007F"]);
        assert_eq!(
            p("U+200B–U+200D, U+2060，U+FEFF"),
            ["U+200B–U+200D", "U+2060", "U+FEFF"]
        );
        assert_eq!(
            p("U+202A - U+202E、U+2066 – U+2069"),
            ["U+202A–U+202E", "U+2066–U+2069"]
        );
        assert_eq!(p("  U+0  \n U+10FFFF "), ["U+0000", "U+10FFFF"]);
        assert_eq!(p("U+1F600"), ["U+1F600"], "超过四位的照原样写");
        let w = Codepoints::parse("u+200b-u+200d，U+202A - U+202E、 U+FEFF").unwrap();
        assert_eq!(
            w.written(),
            ["u+200b-u+200d", "U+202A - U+202E", "U+FEFF"],
            "写的样子原样留着"
        );
        assert_eq!(
            Codepoints::parse(&w.written().join(", "))
                .unwrap()
                .canonical(),
            w.canonical(),
            "连起来意思不变"
        );
        assert_eq!(
            Codepoints::parse("u+200b,u+e0000-u+e007f")
                .unwrap()
                .to_string(),
            "U+200B, U+E0000–U+E007F"
        );
    }

    #[test]
    fn what_is_written_wrong_says_which_item_and_why() {
        let e = |s: &str| Codepoints::parse(s).unwrap_err();
        assert_eq!(e(""), CodepointError::Empty);
        assert_eq!(e(" ,、 "), CodepointError::Empty);
        for bad in [
            "200B",
            "U+",
            "U+12345678",
            "U+GG",
            "0x200B",
            "U+200B-",
            "-U+200B",
        ] {
            assert_eq!(e(bad).code(), "codepoints_syntax", "{bad}");
        }
        assert_eq!(
            e("U+200B-200D"),
            CodepointError::Syntax {
                item: "200D".into()
            },
            "范围的两头都要写 U+"
        );
        assert_eq!(e("U+110000").code(), "codepoints_out_of_range");
        assert_eq!(e("U+D800").code(), "codepoints_surrogate");
        assert_eq!(e("U+DFFF-U+E000").code(), "codepoints_surrogate");
        assert_eq!(
            e("U+200D-U+200B"),
            CodepointError::Reversed {
                item: "U+200D–U+200B".into()
            }
        );
        let many: Vec<String> = (0..=MAX_ITEMS)
            .map(|i| format!("U+{:X}", 0x100 + i))
            .collect();
        assert_eq!(
            e(&many.join(",")),
            CodepointError::TooMany { max: MAX_ITEMS }
        );
        assert!(Codepoints::parse(&many[..MAX_ITEMS].join(",")).is_ok());
        // 一段跨过代理区没关系：两头都是字符，代理区里的码位不会出现在文字里
        assert!(Codepoints::parse("U+D000-U+E000").is_ok());
    }

    #[test]
    fn matching_is_by_the_character_itself() {
        let cp = Codepoints::parse("U+200B–U+200D, U+E0000–U+E007F, U+FEFF").unwrap();
        for c in ['\u{200B}', '\u{200C}', '\u{200D}', '\u{E0041}', '\u{FEFF}'] {
            assert!(cp.contains(c), "{c:?}");
        }
        for c in ['a', '\u{200A}', '\u{200E}', '\u{E0080}', '中'] {
            assert!(!cp.contains(c), "{c:?}");
        }
        assert_eq!(cp.min(), 0x200B);
        // 重叠、相邻的段合并之后照样认
        let cp = Codepoints::parse("U+10-U+20, U+15-U+30, U+31").unwrap();
        assert!(cp.contains('\u{31}') && cp.contains('\u{10}') && !cp.contains('\u{32}'));
    }
}

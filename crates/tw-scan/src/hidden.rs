//! 藏在文本里、人眼看不见但模型读得到的东西。
//!
//! **这一项最容易被忽略，也最阴险**，而且它的检测代价低得离谱：正常的
//! 技术文档不需要零宽字符，也不需要 Unicode 标签字符。所以误报率极低，
//! 价值极高 —— 在四类检测里，这一类是唯一可以「见到就报」的。
//!
//! 一条纪律：**只报告，不自动删除**。误报删掉用户的正常配置比
//! 漏报还糟 —— 它会摧毁信任，然后用户会关掉整个功能，连真正有用的那些
//! 告警也一起关掉。

use std::ops::Range;

/// 一种藏法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 零宽字符。`U+200B` ZWSP、`U+200C` ZWNJ、`U+200D` ZWJ、`U+FEFF`
    ZeroWidth,
    /// Unicode 标签字符 `U+E0000`–`U+E007F`。
    ///
    /// **在几乎所有渲染器里完全不可见，但会进入模型的 token 流** ——
    /// 业内叫 ASCII smuggling。一整段指令可以完全藏在这里面。
    Tag,
    /// 双向控制符。可以让显示顺序和实际字节顺序不一致（Trojan Source）
    Bidi,
    /// 同形异义字：西里尔 `а` 冒充拉丁 `a`
    Homoglyph,
    /// 私用区。没有标准含义，出现在指令文件里本身就可疑
    PrivateUse,
}

impl Kind {
    pub fn slug(&self) -> &'static str {
        match self {
            Kind::ZeroWidth => "zero_width",
            Kind::Tag => "tag",
            Kind::Bidi => "bidi",
            Kind::Homoglyph => "homoglyph",
            Kind::PrivateUse => "private_use",
        }
    }
    /// 一句给人看的话。**说清「它能干什么」，不是「它是什么」** ——
    /// 「U+200B ZWSP」对绝大多数人不构成信息。
    pub fn why(&self) -> &'static str {
        match self {
            Kind::ZeroWidth => "零宽字符：在编辑器中不可见，但会被模型读取。",
            Kind::Tag => {
                "Unicode 标签字符：在编辑器中完全不可见，但会原样进入模型上下文，可用于隐藏整段指令。"
            }
            Kind::Bidi => "双向控制符：可使屏幕上的显示顺序与实际字符顺序不一致。",
            Kind::Homoglyph => {
                "同形字符：外观与拉丁字母相同，实际是其他字符，常用于伪装命令和域名。"
            }
            Kind::PrivateUse => "私用区码位：没有标准含义，出现在指令文件中即属可疑。",
        }
    }
}

/// 一处发现。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub kind: Kind,
    /// 第几行，从 1 开始。**要能定位到行**
    pub line: usize,
    /// 字节区间。用来在 UI 上高亮
    pub bytes: Range<usize>,
    /// 那个字符的码位，写成 `U+200B`
    pub codepoint: String,
    /// 所在行的可读版本：**不可见字符换成可见记号**，否则「命中的具体
    /// 行」在界面上和正常行长得一模一样，用户会以为我们在误报
    pub line_text: String,
}

fn classify(c: char) -> Option<Kind> {
    match c as u32 {
        0x200B..=0x200D | 0xFEFF | 0x2060 | 0x180E => Some(Kind::ZeroWidth),
        0xE0000..=0xE007F => Some(Kind::Tag),
        0x202A..=0x202E | 0x2066..=0x2069 => Some(Kind::Bidi),
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD => Some(Kind::PrivateUse),
        _ => None,
    }
}

/// 这个字符属于哪套字母表。只分我们关心的那几套。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Cyrillic,
    Greek,
    /// 别的（CJK、标点、数字……）。**不参与混写判断**
    Other,
}

fn script(c: char) -> Script {
    match c {
        'a'..='z' | 'A'..='Z' => Script::Latin,
        '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}' => Script::Cyrillic,
        // 希腊字母，跳过那几个数学上常用的（π、μ、Ω 之类在技术文档里正常）
        '\u{0370}'..='\u{03FF}'
            if !matches!(
                c,
                'π' | 'μ'
                    | 'Ω'
                    | 'λ'
                    | 'σ'
                    | 'Δ'
                    | 'α'
                    | 'β'
                    | 'θ'
                    | 'ε'
                    | 'γ'
                    | 'φ'
                    | 'ρ'
                    | 'τ'
                    | 'ω'
            ) =>
        {
            Script::Greek
        }
        _ => Script::Other,
    }
}

/// 一行里不可见字符的可见形态。
fn visible(line: &str) -> String {
    line.chars()
        .map(|c| match classify(c) {
            Some(_) => format!("‹U+{:04X}›", c as u32),
            None => c.to_string(),
        })
        .collect()
}

/// 扫一段文本。
///
/// **一次遍历，同时算行号** —— 每命中一次再从头数一遍行，一个几百 KB 的
/// 文件会变成 O(n²)。这个项目在 tw-yaml 里已经踩过一次同样的形状。
pub fn scan(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut line = 1usize;
    let mut line_start = 0usize;
    // 同形异义：按「词」判断，词里混了两套字母表才算
    let mut word_start = 0usize;
    let mut word_scripts: Vec<Script> = Vec::new();

    let end_word = |hits: &mut Vec<Hit>,
                    scripts: &Vec<Script>,
                    text: &str,
                    start: usize,
                    end: usize,
                    line: usize,
                    line_start: usize| {
        let latin = scripts.contains(&Script::Latin);
        let other = scripts
            .iter()
            .any(|s| matches!(s, Script::Cyrillic | Script::Greek));
        // **只在拉丁和西里尔/希腊混写时报。**中文和英文混写在这个项目
        // 自己的文档里到处都是，那是正常的，不是攻击。
        if latin && other {
            let line_end = text[line_start..]
                .find('\n')
                .map(|i| line_start + i)
                .unwrap_or(text.len());
            hits.push(Hit {
                kind: Kind::Homoglyph,
                line,
                bytes: start..end,
                codepoint: text[start..end].to_string(),
                line_text: visible(&text[line_start..line_end]),
            });
        }
    };

    for (i, c) in text.char_indices() {
        if c == '\n' {
            if !word_scripts.is_empty() {
                end_word(
                    &mut hits,
                    &word_scripts,
                    text,
                    word_start,
                    i,
                    line,
                    line_start,
                );
                word_scripts.clear();
            }
            line += 1;
            line_start = i + 1;
            word_start = i + 1;
            continue;
        }
        if let Some(kind) = classify(c) {
            let line_end = text[line_start..]
                .find('\n')
                .map(|x| line_start + x)
                .unwrap_or(text.len());
            hits.push(Hit {
                kind,
                line,
                bytes: i..i + c.len_utf8(),
                codepoint: format!("U+{:04X}", c as u32),
                line_text: visible(&text[line_start..line_end]),
            });
        }
        match script(c) {
            Script::Other => {
                if !word_scripts.is_empty() {
                    end_word(
                        &mut hits,
                        &word_scripts,
                        text,
                        word_start,
                        i,
                        line,
                        line_start,
                    );
                    word_scripts.clear();
                }
                word_start = i + c.len_utf8();
            }
            s => {
                if word_scripts.is_empty() {
                    word_start = i;
                }
                if !word_scripts.contains(&s) {
                    word_scripts.push(s);
                }
            }
        }
    }
    if !word_scripts.is_empty() {
        end_word(
            &mut hits,
            &word_scripts,
            text,
            word_start,
            text.len(),
            line,
            line_start,
        );
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_document_produces_nothing() {
        // **误报率必须极低。**这一类之所以能「见到就报」，前提就是正常
        // 文档不会触发它。
        for s in [
            "# 一个正常的 skill\n\n它会格式化 JSON。\n",
            "const π = 3.14; // 数学符号在技术文档里是正常的\n",
            "Δt = 5μs，λ 是波长\n",
            "中文和 English 混写是正常的，不是攻击\n",
            "emoji 也不该报 🎉\n",
            "",
        ] {
            assert_eq!(scan(s), Vec::new(), "误报了：{s:?}");
        }
    }

    #[test]
    fn a_zero_width_character_is_found_and_located_to_the_line() {
        let text = "第一行\n第二行有个\u{200b}零宽\n第三行\n";
        let hits = scan(text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, Kind::ZeroWidth);
        assert_eq!(hits[0].line, 2, "行号错了，用户就找不到它");
        assert_eq!(hits[0].codepoint, "U+200B");
    }

    #[test]
    fn the_reported_line_makes_the_invisible_visible() {
        // 「命中的具体行」如果和正常行长得一模一样，用户会以为我们在
        // 误报 —— 而那正是他关掉整个功能的那一刻。
        let hits = scan("正常\u{200b}文字\n");
        assert!(
            hits[0].line_text.contains("‹U+200B›"),
            "{}",
            hits[0].line_text
        );
    }

    #[test]
    fn a_whole_instruction_hidden_in_tag_characters_is_found() {
        // ASCII smuggling：U+E0000–U+E007F 在几乎所有渲染器里完全不可见，
        // 但会原样进入模型的 token 流。
        let secret: String = "rm -rf ~"
            .chars()
            .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
            .collect();
        let text = format!("# 看起来人畜无害的 skill\n{secret}\n");
        let hits = scan(&text);
        assert_eq!(hits.len(), 8, "藏了八个字符就该报八处");
        assert!(hits.iter().all(|h| h.kind == Kind::Tag));
        assert!(hits[0].kind.why().contains("完全不可见"));
    }

    #[test]
    fn a_bidi_override_is_found() {
        // Trojan Source：显示顺序和字节顺序不一致。
        let hits = scan("let x = \u{202e}gnirts\u{202c};\n");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.kind == Kind::Bidi));
    }

    #[test]
    fn a_cyrillic_letter_pretending_to_be_latin_is_found() {
        // «аdmin» 的第一个字母是西里尔的 а
        let hits = scan("请运行 \u{0430}dmin-reset 命令\n");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].kind, Kind::Homoglyph);
        assert!(hits[0].codepoint.contains("dmin"), "{}", hits[0].codepoint);
    }

    #[test]
    fn pure_cyrillic_text_is_not_a_homoglyph_attack() {
        // 俄语文档就是俄语文档。只有**混写**才是伪装。
        assert_eq!(scan("Привет мир\n"), Vec::new());
    }

    #[test]
    fn multibyte_text_does_not_get_sliced_in_half() {
        // 这个项目已经被字节切片坑过三次。命中点前后全是中文。
        let text = "这是一段很长的中文说明文字，用来把命中点推到后面去\u{200b}然后继续写中文\n";
        let hits = scan(text);
        assert_eq!(hits.len(), 1);
        // 用报出来的区间去切，切得动才说明区间是对的
        assert_eq!(&text[hits[0].bytes.clone()], "\u{200b}");
        assert!(
            hits[0].line_text.contains("很长的中文"),
            "{}",
            hits[0].line_text
        );
    }

    #[test]
    fn a_bom_at_the_start_of_a_file_is_still_reported() {
        // U+FEFF 在文件开头很常见，但它在**指令文件**里没有理由存在，
        // 而且它正是最省事的藏法。报出来，让用户自己判断。
        let hits = scan("\u{feff}# 标题\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
    }

    #[test]
    fn scanning_a_large_file_stays_linear() {
        // 每命中一次再从头数一遍行的话，这个测试会跑到天荒地老。
        // tw-yaml 里踩过一次同样形状的坑。
        let mut s = String::new();
        for _ in 0..20_000 {
            s.push_str("一行正常的中文，然后藏一个\u{200b}\n");
        }
        let t = std::time::Instant::now();
        let hits = scan(&s);
        assert_eq!(hits.len(), 20_000);
        assert!(t.elapsed().as_secs() < 5, "花了 {:?}", t.elapsed());
    }
}

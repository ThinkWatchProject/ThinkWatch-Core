//! 对照测试：两段式扫描和逐条扫描（[`super`]）在同一份语料上的结果一模一样。
//!
//! 语料三份：
//!
//! - 各条规则测试里的样本（凭据、个人信息各自的 `sample`，前缀规则按规则表拼的），每个单独
//!   扫、编成请求体再扫；
//! - 这个仓库自己的源码（测试运行时现读），每个文件一条消息拼成请求体，也当纯文本扫；
//! - 随机拼的：样本、像词的、数字分组、JSON 转义（`\n` 开头的行、`\uXXXX`）、字符串外面的
//!   数值，拼成纯文本、请求体、正文里又一层 JSON。
//!
//! 规则集：出厂、全开、全开加几条自定义规则（包含 / 正则、分不分大小写、`^` `$`、能匹配
//! 空串的）、只开一条（每条各一遍，跨 token 接起来的几种互相不再干扰）。比两样：合并之后的
//! 命中（[`scan`]），和合并之前的命中（排序后逐条比，`flow` 用的就是它）。

use super::super::{BUILTINS, Matcher, Pattern, RuleSet, Tail, scan, scan_unresolved};
use crate::redact::secrets::tests::samples::{self, ALNUM, HEX, rnd};

fn all() -> RuleSet {
    RuleSet::only(&BUILTINS.iter().map(|b| b.id).collect::<Vec<_>>())
}

/// 几条自定义规则：按原样找的（分大小写、不分大小写、带 `k` `s` 的、中文的），正则（有固定
/// 文字的、没有的、`^` `$`、多行、能匹配空串的、`\b`）
fn with_custom(set: RuleSet) -> RuleSet {
    let contains: &[(&str, bool)] = &[
        ("ACME-INTERNAL", true),
        ("project-x", false),
        ("Secret", false),
        ("TOKEN", false),
        ("机密", true),
        ("ab", true),
        ("aa", false),
        ("\"", true),
        ("\\n", true),
        // 跨过字符串边界的：请求体里只在两个字符串之间出现，哪个字符串里都没有
        ("role\":\"user", true),
    ];
    let regexes: &[(&str, bool)] = &[
        (r"MYCORP-\d{6}", true),
        (r"\d{6}", true),
        (r"^#!.*$", true),
        (r"(?m)^\s*password\s*=.*$", true),
        (r"\bEMP\d{5}\b", true),
        (r"x*", true),
        (r"\S+@corp", true),
        (r"internal only", false),
        (r"(?x) proj_ [a-z]+ # 注释", true),
    ];
    let mut set = set;
    for (i, &(t, cs)) in contains.iter().enumerate() {
        let p = Pattern {
            text: t.to_string(),
            regex: false,
            case_sensitive: cs,
        };
        set = set.with_rule(&format!("c{i}"), p, None).unwrap();
    }
    for (i, &(t, cs)) in regexes.iter().enumerate() {
        let p = Pattern {
            text: t.to_string(),
            regex: true,
            case_sensitive: cs,
        };
        set = set.with_rule(&format!("r{i}"), p, Some("LBL")).unwrap();
    }
    set
}

fn sets() -> Vec<(&'static str, RuleSet)> {
    vec![
        ("defaults", RuleSet::defaults()),
        ("all", all()),
        ("all+custom", with_custom(all())),
        ("custom", with_custom(RuleSet::none())),
    ]
}

/// 两种扫法在 `text` 上一样
#[track_caller]
fn same(name: &str, text: &str, set: &RuleSet) {
    assert_eq!(
        super::json_string_spans(text),
        super::super::json_string_spans(text),
        "{text:?}"
    );
    if super::super::json_like(text) {
        assert_eq!(
            crate::redact::engine::string_spans(text),
            super::super::json_string_spans(text),
            "切 token 时记下的字符串区间不同：{text:?}"
        );
    }
    let want = super::scan(text, set);
    let got = scan(text, set);
    assert!(
        want == got,
        "[{name}] 合并后的命中不同\n文本: {:?}\n老: {want:?}\n新: {got:?}",
        text.chars().take(400).collect::<String>()
    );
    let key = |h: &super::Hit| (h.bytes.start, h.bytes.end, h.rule.clone(), h.label.clone());
    let mut want: Vec<_> = super::scan_unresolved(text, set).iter().map(key).collect();
    let mut got: Vec<_> = scan_unresolved(text, set).iter().map(key).collect();
    want.sort();
    got.sort();
    assert!(
        want == got,
        "[{name}] 合并前的命中不同\n文本: {:?}\n老: {want:?}\n新: {got:?}",
        text.chars().take(400).collect::<String>()
    );
}

/// 一段正文放进聊天请求体
fn body(text: &str) -> String {
    serde_json::json!({"model": "m", "messages": [{"role": "user", "content": text}]}).to_string()
}

/// 和 [`body`] 一样，只是非 ASCII 的字都写成 `\uXXXX`（Python 的客户端默认这么写）
fn ascii_body(text: &str) -> String {
    let mut out = String::new();
    for c in body(text).chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// 规则测试里的样本，和按规则表拼的前缀规则的样本
fn samples() -> Vec<String> {
    let mut out = Vec::new();
    for b in BUILTINS {
        if let Some(s) = samples::sample(b.id) {
            out.extend(s.hits.into_iter().map(|(t, _)| t));
            out.extend(s.misses);
        }
        if let Some(s) = crate::redact::personal::tests::sample(b.id) {
            out.push(s);
        }
        let mut prefixes = Vec::new();
        match b.matcher {
            Matcher::Prefix { prefix, min_tail } => prefixes.push((prefix, min_tail, Tail::Any)),
            Matcher::Prefixes { list } => {
                prefixes.extend(list.iter().map(|p| (p.prefix, p.min_tail, p.tail)));
            }
            _ => {}
        }
        for (i, (p, n, tail)) in prefixes.into_iter().enumerate() {
            let alphabet = match tail {
                Tail::Hex => HEX,
                _ => ALNUM,
            };
            let seed = i as u64 * 31 + b.id.len() as u64;
            out.push(format!("{p}{}", rnd(seed, n, alphabet)));
            out.push(format!(
                "key: {p}{} 和 {p}{}",
                rnd(seed + 1, n + 7, alphabet),
                rnd(seed + 2, n - 1, alphabet)
            ));
        }
    }
    // 跨 token 接起来的几种在什么地方断：字符串外面的 `true` 断开一对坐标，别的 token 不断；
    // 最后一个号码后面还有没有 token，决定它在按位置认的值让开别的规则之前还是之后交出去
    out.extend(
        [
            r#"{"a":"31.23045","lng":true,"b":"121.47370"}"#,
            r#"{"a":"31.23045","lng":"x","b":"121.47370"}"#,
            "https://h/?access_token=00:1a:2b:3c:4d:5e",
            "https://h/?access_token=00:1a:2b:3c:4d:5e x",
            "https://h/?access_token=00:1a:2b:3c:4d:5e ...",
            "https://h/?access_token=138 1234 5678",
            "https://h/?access_token=138 1234 5678 y",
            "lat=31.23045&lng=121.47370",
            "lat=31.23045&lng=121.47370 z",
            r#"{"content":"Authorization: abc/31.23045, 121.47370","n":1}"#,
            r#"{"content":"Authorization: abc/31.23045, 121.47370"}"#,
            r#"{"content":"Authorization: abc/31.23045, 121.47370","n":"x"}"#,
        ]
        .map(String::from),
    );
    // 中文助记词：词之间隔着宽窄不一的分隔（至多 12 个字节才接得上）
    let entropy: Vec<u8> = (0u8..16)
        .map(|i| i.wrapping_mul(37).wrapping_add(11))
        .collect();
    for sep in [
        " ",
        ", ",
        ",\t \n  ",
        " ,\n\n \t ",
        "          ",
        ",           ",
        "\n1) ",
    ] {
        out.push(samples::chinese_phrase(&entropy, sep));
    }
    // OpenAI 老式密钥、各家 `sk-` 前缀互相让的那几种
    out.push(format!("sk-{}", rnd(9, 45, ALNUM)));
    out.push(format!("sk-ant-{}", rnd(10, 12, ALNUM)));
    out
}

/// 这个仓库的源码，测试运行时现读。只读 `crates` 下的，总量有个上限（调试构建慢）
fn sources(limit: usize) -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut stack = vec![root];
    let mut files = Vec::new();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut kids: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        kids.sort();
        for p in kids {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target") {
                    stack.push(p);
                }
            } else if p
                .extension()
                .is_some_and(|e| e == "rs" || e == "md" || e == "toml" || e == "txt" || e == "yaml")
            {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    let mut size = 0;
    for f in files {
        let Ok(s) = std::fs::read_to_string(&f) else {
            continue;
        };
        size += s.len();
        out.push(s);
        if size > limit {
            break;
        }
    }
    out
}

#[test]
fn every_rule_sample_scans_the_same_both_ways() {
    let samples = samples();
    let mut sets = sets();
    for b in BUILTINS {
        sets.push((b.id, RuleSet::only(&[b.id])));
    }
    for (name, set) in &sets {
        for s in &samples {
            same(name, s, set);
            same(name, &body(s), set);
            if !s.is_ascii() {
                same(name, &ascii_body(s), set);
            }
        }
        // 全部样本拼成一段：彼此挨着
        let joined = samples.join("\n");
        same(name, &joined, set);
        same(name, &body(&joined), set);
    }
}

#[test]
fn this_repositorys_sources_scan_the_same_both_ways() {
    let files = sources(1 << 20);
    assert!(files.len() > 20, "没读到源码");
    let msgs: Vec<_> = files
        .iter()
        .map(|f| serde_json::json!({"role": "user", "content": f}))
        .collect();
    let req = serde_json::json!({"model": "m", "messages": msgs}).to_string();
    for (name, set) in sets() {
        same(name, &req, &set);
    }
    // 一个文件当一段纯文本：不是 JSON，不分里外
    let set = all();
    for f in files.iter().take(40) {
        same("all", f, &set);
    }
}

/// 确定的伪随机数
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

/// 一段随机拼的正文：样本插在噪声里，噪声专挑会让跨 token 的判据接上、断开的东西
fn noise(rng: &mut Rng, samples: &[String], words: &[&str]) -> String {
    const SEPS: &[&str] = &[
        " ", " ", " ", "  ", "\n", "\t", ", ", ",", ": ", "=", " = ", "\"", "'", "(", ")", "；",
        "，", "。", "：", "\\n", "\\", "\\u4e2d", "/", "?", "&", "#", "@", "+", "-", ".", "<<",
        "~",
    ];
    const BITS: &[&str] = &[
        "6222",
        "0212",
        "3456",
        "7894",
        "123",
        "12",
        "1",
        "86",
        "138",
        "1234",
        "5678",
        "0755",
        "(010)",
        "62345678",
        "00",
        "1A",
        "2b",
        "DE89",
        "3704",
        "0044",
        "0532",
        "0130",
        "AB",
        "C",
        "31.23045",
        "121.47370",
        "-33.86785",
        "\"lng\":",
        "lat",
        "京",
        "A",
        "·",
        "12345",
        "D12345",
        "version",
        "护照",
        "passport",
        "E12345678",
        "IMEI",
        "authorization",
        "Bearer",
        "curl",
        "-u",
        "user:pa55w0rd!",
        "password",
        "API_KEY",
        "token",
        "?access_token=",
        "mysql",
        "-pS3cr3t!x",
        "Q~",
        "AA",
        "xprv",
        "-----BEGIN",
        "PRIVATE",
        "KEY-----",
        "postgres://",
        "北京市朝阳区建国路88号",
        "1.",
        "2)",
        "...",
        "eyJhbGciOiJIUzI1NiJ9",
        "10.0.0.1",
        "corp.internal",
        "fc00::1",
        "52.12.34.56",
        "00:1a:2b:3c:4d:5e",
        "+44 20 7946 0958",
        "a@b.co",
        "机密",
        "ACME-INTERNAL",
        "MYCORP-123456",
        "EMP12345",
        "k",
        "ſecret",
    ];
    let mut s = String::new();
    let n = 5 + rng.below(60);
    for _ in 0..n {
        match rng.below(10) {
            0 | 1 => s.push_str(rng.pick(samples)),
            2..=4 => s.push_str(rng.pick(words)),
            5 | 6 => s.push_str(rng.pick(BITS)),
            _ => {
                // 一串像助记词的：十几个词，隔着空格、换行或者编号
                if rng.below(4) == 0 {
                    let k = 10 + rng.below(16);
                    for j in 0..k {
                        if rng.below(8) == 0 {
                            s.push_str(&format!("{}. ", j + 1));
                        }
                        s.push_str(rng.pick(words));
                        s.push_str(rng.pick(&[" ", " ", "\n", ", ", "\\n"]));
                    }
                } else {
                    s.push_str(&rnd(rng.next(), 1 + rng.below(40), ALNUM));
                }
            }
        }
        s.push_str(rng.pick(SEPS));
    }
    s
}

#[test]
fn random_bodies_scan_the_same_both_ways() {
    let samples = samples();
    let english: Vec<&str> = crate::redact::secrets::crypto::english().to_vec();
    let mut words: Vec<&str> = english.iter().step_by(7).copied().collect();
    words.extend([
        "the", "is", "a", "self", "let", "token", "value", "abandon", "about", "zoo",
    ]);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let sets = sets();
    let rounds = std::env::var("TW_ORACLE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    for round in 0..rounds {
        let text = noise(&mut rng, &samples, &words);
        let (name, set) = &sets[round % sets.len()];
        same(name, &text, set);
        same(name, &body(&text), set);
        // 正文里又一层 JSON：引号、换行都转义了两次
        same(name, &body(&body(&text)), set);
        same(name, &ascii_body(&text), set);
        // 一个对象，值有字符串也有字符串外面的数值
        let mut obj = String::from("{");
        for k in 0..rng.below(12) {
            if k > 0 {
                obj.push(',');
            }
            obj.push_str(&format!("\"k{k}\":"));
            match rng.below(3) {
                0 => obj.push_str(rng.pick(&[
                    "6222021234567894",
                    "31.23045",
                    "121.47370",
                    "13812345678",
                    "1",
                    "-2.5",
                ])),
                1 => obj
                    .push_str(&serde_json::to_string(&noise(&mut rng, &samples, &words)).unwrap()),
                _ => obj.push_str(&serde_json::to_string(rng.pick(&samples)).unwrap()),
            }
        }
        obj.push('}');
        same(name, &obj, set);
    }
}

/// 不分大小写按原样找的规则能并进自动机的条件：正则的不分大小写里，只有 `k`、`s` 两个 ASCII
/// 字母还认别的（非 ASCII）字符
#[test]
fn only_k_and_s_fold_to_non_ascii_letters() {
    let mut extra = Vec::new();
    for c in b'a'..=b'z' {
        let re = regex::RegexBuilder::new(&regex::escape(&(c as char).to_string()))
            .case_insensitive(true)
            .build()
            .unwrap();
        let mut buf = [0u8; 4];
        if (0x80..=0x10FFFFu32)
            .filter_map(char::from_u32)
            .any(|u| re.is_match(u.encode_utf8(&mut buf)))
        {
            extra.push(c as char);
        }
    }
    assert_eq!(extra, ['k', 's']);
}

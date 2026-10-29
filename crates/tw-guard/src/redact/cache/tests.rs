use std::sync::Arc;

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::*;
use crate::policy::Mode;
use crate::redact::flow::{self, Scope, Shape};
use crate::redact::replace::{Ledger, Scheme};
use crate::redact::rules::{BUILTINS, Pattern, Rule, SavedSecret, SavedSource, SavedSourceKind};

// ---------------------------------------------------------------- 造数据

/// 可复现的伪随机数
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.next(xs.len())]
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.next(one_in) == 0
    }
}

/// 补上 Luhn 校验位
fn luhn_done(body: &str) -> String {
    let sum: u32 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let x = u32::from(c - b'0');
            if i % 2 == 0 {
                (x * 2) / 10 + (x * 2) % 10
            } else {
                x
            }
        })
        .sum();
    format!("{body}{}", (10 - sum % 10) % 10)
}

/// 补上身份证号的校验位
fn id_done(first17: &str) -> String {
    let sum: u32 = first17
        .bytes()
        .enumerate()
        .map(|(i, c)| u32::from(c - b'0') * (2u32.pow(17 - i as u32) % 11))
        .sum();
    let c = (12 - sum % 11) % 11;
    let check = if c == 10 {
        'X'
    } else {
        char::from(b'0' + c as u8)
    };
    format!("{first17}{check}")
}

const SAVED: &str = "q7Rk2LmN8vXc4TpW9zYb";
const MNEMONIC: &str =
    "legal winner thank year wave sausage worth useful legal winner thank yellow";

/// 一小段：有的是各条规则认得的值，有的专挑段与段的接缝（消息开头、结尾）放 —— 半串
/// 助记词、一组卡号、只有前文才说得清的号码、结尾的转义
fn fragment(r: &mut Rng) -> String {
    let card = luhn_done(&format!("62220212345{:04}", r.next(10_000)));
    let id = id_done(&format!("11010119900307{:03}", r.next(1000)));
    let pool: Vec<String> = vec![
        format!("sk-ant-api03-{}", "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789".repeat(1)),
        "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789".into(),
        format!("AKIA{}", ["QRSTUVWXYZ234567", "ABCDEFGHIJKLMNOP"][r.next(2)]),
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7\n-----END PRIVATE KEY-----".into(),
        "postgres://app:hunter2xyz9@db.internal:5432/orders".into(),
        "mail alice@corp.example.com now".into(),
        card.clone(),
        format!("{} {} {} {}", &card[..4], &card[4..8], &card[8..12], &card[12..]),
        format!("{}-{}-{}-{}", &card[..4], &card[4..8], &card[8..12], &card[12..]),
        card[..4].to_string(),
        card[12..].to_string(),
        id,
        "call 13800138000".into(),
        MNEMONIC.into(),
        "legal winner thank year wave sausage".into(),
        "worth useful legal winner thank yellow".into(),
        "API_TOKEN=abc123XYZ789qq".into(),
        "DB_PASSWORD=\"s3cr3t-Pa55w0rd\"".into(),
        "password: hunter2hunter2".into(),
        "export SECRET_KEY='Zx9-qq81_ab77'".into(),
        "TOKEN=".into(),
        "api_key".into(),
        "Authorization: Bearer abcDEF123456ghiJKL789mno".into(),
        "Authorization".into(),
        "https://hooks.example.com/cb?token=abcDEF123456ghi&x=1".into(),
        "mysql -u root -pS3cretPass99 db".into(),
        "护照号码 E12345678".into(),
        "护照".into(),
        "curl -s https://api.example.com".into(),
        "-u admin:S3cretPass99 x".into(),
        "--password=S3cretPass99".into(),
        "-pS3cretPass99".into(),
        "E12345678".into(),
        "通行证 C12345678".into(),
        "39.9042, 116.4074".into(),
        "39.9042".into(),
        "116.4074".into(),
        "IBAN DE89370400440532013000".into(),
        "DE89".into(),
        "10.0.0.5 and 8.8.8.8 v1.2.3.4".into(),
        "MAC 00:1A:2B:3C:4D:5E".into(),
        "00:1A:2B".into(),
        "<<TW_SECRET_1>>".into(),
        "<<TW_PHONE_2>>".into(),
        SAVED.into(),
        format!("x{SAVED}"),
        "E-123456 and MYCORP-222222".into(),
        "mycorp-333333".into(),
        "你好，世界。这是一段中文。".into(),
        "姓名：张三，电话 13800138000".into(),
        "{\"real_name\":\"张三\",\"phone\":\"13800138000\"}".into(),
        "{\"name\":\"李四\"}".into(),
        "name".into(),
        "address: 北京市朝阳区建国路 88 号".into(),
        "C:\\Users\\dev\\".into(),
        "he said \"hi\"".into(),
        "\\".into(),
        "\\n".into(),
        "1.".into(),
        "12".into(),
        "the quick brown fox jumps over the lazy dog".into(),
        "fn main() { let x = vec![1, 2, 3]; }".into(),
        "abandon".into(),
        "zoo".into(),
        format!("data:image/png;base64,{}", "iVBORw0KGgo".repeat(30)),
        "VIN 1HGCM82633A004352".into(),
        "IMEI 490154203237518".into(),
        "SSN 123-45-6789".into(),
        "+1 415 555 2671".into(),
        "京A12345".into(),
    ];
    r.pick(&pool).clone()
}

const SEPS: &[&str] = &[" ", " ", "\n", ", ", "", "\t", "。", ": ", "\"", "\\"];

/// 一段正文：几个片段，用各种分隔连起来
fn prose(r: &mut Rng) -> String {
    let n = 1 + r.next(6);
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push_str(r.pick(SEPS));
        }
        s.push_str(&fragment(r));
    }
    // 有的长一点，过 [`MIN_BYTES`]
    if r.chance(2) {
        s.push(' ');
        s.push_str(&"lorem ipsum dolor sit amet ".repeat(1 + r.next(4)));
    }
    s
}

/// 第 `turn` 轮用户和模型各说一句，写成 `dialect` 的样子
fn messages(r: &mut Rng, dialect: Dialect) -> [Value; 2] {
    let user = prose(r);
    let reply = prose(r);
    let args = json!({"q": prose(r), "token": fragment(r)}).to_string();
    let blob = "iVBORw0KGgo".repeat(40);
    match dialect {
        Dialect::Anthropic => {
            let assistant = if r.chance(2) {
                json!({"role": "assistant", "content": reply})
            } else {
                json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": prose(r), "signature": blob},
                    {"type": "text", "text": reply},
                    {"type": "tool_use", "id": "toolu_1", "name": "run",
                     "input": {"cmd": prose(r), "api_key": fragment(r)}},
                ]})
            };
            let user = if r.chance(2) {
                json!({"content": user, "role": "user"})
            } else {
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": prose(r)},
                    {"type": "text", "text": user},
                ]})
            };
            [user, assistant]
        }
        Dialect::Chat => [
            json!({"role": "user", "content": user}),
            json!({"role": "assistant", "content": reply,
                   "tool_calls": [{"id": "call_1", "type": "function",
                                   "function": {"name": "f", "arguments": args}}]}),
        ],
        Dialect::Responses => [
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": user}]}),
            if r.chance(2) {
                json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": args})
            } else {
                json!({"type": "reasoning", "id": "rs_0a1b13800138000",
                       "summary": [{"type": "summary_text", "text": reply}],
                       "encrypted_content": blob})
            },
        ],
        _ => [
            json!({"role": "user", "parts": [{"text": user}]}),
            json!({"role": "model", "parts": [{"text": reply},
                   {"functionCall": {"name": "f", "args": {"q": prose(r)}}}]}),
        ],
    }
}

/// 一轮的请求体：顶层的几项加上到这一轮为止的全部历史
fn body(dialect: Dialect, system: &str, history: &[Value], extra: &Value) -> String {
    let tools = json!([{"name": "run", "description": format!("runs things {system}"),
                        "input_schema": {"type": "object"}}]);
    let mut b = match dialect {
        Dialect::Anthropic => json!({"model": "m", "system": system, "messages": history,
                                     "tools": tools, "max_tokens": 1024}),
        Dialect::Chat => {
            let mut all = vec![json!({"role": "system", "content": system})];
            all.extend(history.iter().cloned());
            json!({"model": "m", "messages": all, "stream": true})
        }
        Dialect::Responses => json!({"model": "m", "instructions": system, "input": history,
                                     "tools": [{"type": "function", "name": "f",
                                                "description": system}]}),
        _ => json!({"systemInstruction": {"parts": [{"text": system}]}, "contents": history}),
    };
    if let (Some(o), Some(x)) = (b.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    b.to_string()
}

/// 非 ASCII 的字都写成 `\uXXXX`（Python 写的客户端默认这样）
fn ascii_escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
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

/// 内置的全开，除了 `skip` 里的
fn all_but(skip: &[&str]) -> RuleSet {
    let ids: Vec<&str> = BUILTINS
        .iter()
        .map(|b| b.id)
        .filter(|id| !skip.contains(id))
        .collect();
    RuleSet::only(&ids)
}

fn saved(list: &[&str]) -> Vec<SavedSecret> {
    list.iter()
        .map(|v| SavedSecret {
            value: v.to_string(),
            source: SavedSource {
                kind: SavedSourceKind::UpstreamKey,
                name: "u".into(),
            },
            hosts: Vec::new(),
        })
        .collect()
}

fn with_customs(set: RuleSet) -> RuleSet {
    let rule = |text: &str, regex: bool, case_sensitive: bool| Pattern {
        text: text.into(),
        regex,
        case_sensitive,
    };
    set.with_rule("corp", rule("MYCORP-", false, false), None)
        .unwrap()
        .with_rule("emp", rule(r"E-\d{6}", true, true), Some("EMP"))
        .unwrap()
        .with_rule("lead", rule(r"^\w+", true, true), None)
        .unwrap()
        .with_rule("word", rule(r"\bzoo\b", true, false), None)
        .unwrap()
}

/// 对照用的几套规则：出厂、内置全开（除了看得到隔壁的那几条）、内置全开（缓存整个不用）、
/// 加自定义的、加保存的凭据的、有只记录的
fn rule_sets() -> Vec<(&'static str, RuleSet)> {
    vec![
        ("defaults", RuleSet::defaults()),
        ("all-local", all_but(CONTEXTUAL)),
        ("all", all_but(&[])),
        ("custom", with_customs(RuleSet::defaults())),
        (
            "saved",
            all_but(CONTEXTUAL).with_saved(saved(&[SAVED, "another-saved-value-123"])),
        ),
        (
            "record",
            with_customs(all_but(CONTEXTUAL))
                .recording(Rule::Builtin("email"))
                .recording(Rule::Builtin("bank-card"))
                .recording(Rule::Custom("lead".into())),
        ),
    ]
}

fn shape(d: Dialect) -> Shape {
    Shape::Generate(d)
}

/// 同一份请求体两条路各走一遍：找到的、报出去的、账本、换过的那一份都一样
fn agree(text: &str, rules: &RuleSet, d: Dialect, cache: &ScanCache, what: &str) {
    let s = shape(d);
    for scope in [Scope::Written(s), Scope::Whole] {
        let plain = flow::hits_in(text, rules, scope);
        let cached = flow::hits_in_with(text, rules, scope, Some(cache));
        assert_eq!(plain, cached, "{what} {scope:?}\n{text}");
        assert_eq!(
            flow::reported(text, rules, &plain, scope),
            flow::reported_with(text, rules, &plain, scope, Some(cache)),
            "{what} reported {scope:?}"
        );
    }
    let fresh = || Ledger::new(Scheme::SECRET).avoiding(text);
    let a = flow::look_hits(Mode::Enforce, rules, text.as_bytes(), s, fresh());
    let b = flow::look_hits_with(
        Mode::Enforce,
        rules,
        text.as_bytes(),
        s,
        fresh(),
        Some(cache),
    );
    assert_eq!(a.found, b.found, "{what} found");
    assert_eq!(a.hits, b.hits, "{what} hits");
    assert_eq!(a.ledger.table(), b.ledger.table(), "{what} ledger");
    let (x, _) = flow::replace(Mode::Enforce, rules, text.to_string().into(), &a.ledger, s);
    let (y, _) = flow::replace_with(
        Mode::Enforce,
        rules,
        text.to_string().into(),
        &a.ledger,
        s,
        Some(cache),
    );
    assert_eq!(x, y, "{what} replaced");
}

/// 按段扫，**不管规则看不看得到隔壁**：用来说明为什么那几条规则开着时不缓存
fn piecewise(text: &str, rules: &RuleSet) -> Vec<Hit> {
    let mut out = Vec::new();
    for p in split(text).expect("切得开") {
        let piece = p.text(text);
        for h in crate::redact::rules::scan_unresolved(&piece, rules) {
            out.push(Hit {
                bytes: p.to_body(h.bytes.start)..p.to_body(h.bytes.end),
                ..h
            });
        }
    }
    crate::redact::rules::resolve(out, rules)
}

// ---------------------------------------------------------------- 对照

/// 多轮对话（每轮加一句用户、一句模型）两条路各走一遍，每一轮都一样。缓存跨轮留着：
/// 第二轮起历史全是取的，新的那两句是扫的；同一轮再走一遍，全是取的
#[test]
fn multi_turn_conversations_find_exactly_what_a_full_scan_finds() {
    let mut r = Rng(0x5eed_cafe_f00d_1234);
    let sets = rule_sets();
    for conv in 0..36 {
        let d = [
            Dialect::Anthropic,
            Dialect::Chat,
            Dialect::Responses,
            Dialect::Gemini,
        ][conv % 4];
        let (name, rules) = &sets[r.next(sets.len())];
        let cache = ScanCache::new();
        let system = prose(&mut r);
        // 顶层另外几项：不是正文的 metadata、名字像口令的一个数组（拆开的数组的键不在任何
        // 一段里）
        let extra = match r.next(5) {
            0 => json!({"metadata": {"user_id": fragment(&mut r)}}),
            1 => json!({"api_token": [{"x": fragment(&mut r)}], "temperature": 0.5}),
            2 => json!({"passport": [{"no": "E12345678"}]}),
            // 挨着正文那几项（按键排序：`instructions`、`system` 后面，最前面）的几项
            3 => json!({"j1": fragment(&mut r), "j2": fragment(&mut r),
                        "system_a": fragment(&mut r), "a0": fragment(&mut r),
                        "contents_z": [{"t": fragment(&mut r)}]}),
            _ => json!({}),
        };
        let escaped = r.chance(4);
        let mut history = Vec::new();
        // 上一轮的请求够不够长、走没走缓存
        let mut warm = false;
        for turn in 0..7 {
            history.extend(messages(&mut r, d));
            let mut text = body(d, &system, &history, &extra);
            if escaped {
                text = ascii_escaped(&text);
            }
            let what = format!("conv {conv} ({name}, {d:?}) turn {turn}");
            let before = cache.misses();
            agree(&text, rules, d, &cache, &what);
            // 头一轮之后每一轮只扫新的那两句（只记录的规则另是一套、另存一份）
            let per_set = if rules.recording_only().is_some() {
                2
            } else {
                1
            };
            if warm {
                assert!(
                    cache.misses() - before <= 2 * per_set * 6,
                    "{what}: 扫了 {} 段",
                    cache.misses() - before
                );
            }
            warm = text.len() >= BODY_MIN;
            let before = cache.misses();
            agree(&text, rules, d, &cache, &format!("{what} again"));
            assert_eq!(cache.misses(), before, "{what}: 同一份再来一遍全是取的");
        }
        // 改了一条历史（客户端压缩过上下文、用户编辑了上一句）：改了的那条重扫，别的照取
        if history.len() > 2 {
            let at = r.next(history.len());
            history[at] = messages(&mut r, d)[0].clone();
            let text = body(d, &system, &history, &extra);
            agree(&text, rules, d, &cache, &format!("conv {conv} edited"));
        }
        if cacheable(rules) {
            assert!(
                cache.len() > 0,
                "conv {conv} ({name}) 一段都没存：没用上缓存"
            );
        }
    }
}

/// 换了规则、同一份缓存接着用：取出来的是新规则扫的，不是旧的
#[test]
fn changing_the_rules_or_the_saved_keys_never_reuses_old_results() {
    let mut r = Rng(0xfeed_beef_0000_0042);
    let cache = ScanCache::new();
    let mut history = Vec::new();
    for _ in 0..6 {
        history.extend(messages(&mut r, Dialect::Anthropic));
    }
    history.push(
        json!({"role": "user", "content": format!("{SAVED} and MYCORP-1 {}",
        "padding text ".repeat(10))}),
    );
    let text = body(Dialect::Anthropic, "sys", &history, &json!({}));
    let base = all_but(CONTEXTUAL);
    let variants = [
        base.clone(),
        base.clone().with_saved(saved(&[SAVED])),
        with_customs(base.clone()),
        base.clone().recording(Rule::Builtin("email")),
        RuleSet::defaults(),
        base.clone().with_saved(saved(&[SAVED])),
        base,
    ];
    for (i, rules) in variants.iter().enumerate() {
        agree(
            &text,
            rules,
            Dialect::Anthropic,
            &cache,
            &format!("variant {i}"),
        );
    }
    let saved_hits = |rules: &RuleSet| {
        flow::hits_in_with(
            &text,
            rules,
            Scope::Written(shape(Dialect::Anthropic)),
            Some(&cache),
        )
        .iter()
        .filter(|h| h.rule.id() == "thinkwatch-saved-secret")
        .count()
    };
    assert_eq!(saved_hits(&variants[0]), 0);
    assert!(saved_hits(&variants[1]) > 0, "加了保存的密钥，同一段要重扫");
}

/// 几个线程共用一份缓存、各扫各的对话：和不带缓存的一样
#[test]
fn concurrent_requests_share_one_cache_safely() {
    let cache = Arc::new(ScanCache::new());
    let rules = Arc::new(all_but(CONTEXTUAL).with_saved(saved(&[SAVED])));
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let (cache, rules) = (cache.clone(), rules.clone());
            std::thread::spawn(move || {
                let mut r = Rng(0x1234_5678 + t);
                let mut history = Vec::new();
                for turn in 0..8 {
                    history.extend(messages(&mut r, Dialect::Chat));
                    let text = body(Dialect::Chat, "sys prompt here", &history, &json!({}));
                    agree(
                        &text,
                        &rules,
                        Dialect::Chat,
                        &cache,
                        &format!("thread {t} turn {turn}"),
                    );
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert!(cache.len() > 0);
}

// ---------------------------------------------------------------- 为什么那几条不缓存

/// 只有前文写明了才算的号码：一条消息开头的护照号，前文是上一条消息末尾的「护照」。整段扫
/// 认得出，按段扫认不出 —— 所以这条开着时不缓存，结果照样和整段扫一样
#[test]
fn a_keyword_at_the_end_of_the_previous_message_is_why_some_rules_are_not_cached() {
    let rules = RuleSet::only(&["cn-passport"]);
    let pad = "padding ".repeat(20);
    let text = json!({"messages": [
        {"content": format!("{pad} 护照")},
        {"content": "E12345678"},
    ]})
    .to_string();
    let whole = crate::redact::rules::scan(&text, &rules);
    assert_eq!(whole.len(), 1, "{text}");
    assert!(piecewise(&text, &rules).is_empty(), "按段扫看不到上一条");
    assert!(!cacheable(&rules));
    agree(
        &text,
        &rules,
        Dialect::Anthropic,
        &ScanCache::new(),
        "passport",
    );
}

/// 坐标的纬度、经度隔着引号里的东西照样配对（`"lat":"…","lng":"…"`）：顶层挨着的几项合成
/// 一段，不拆开
#[test]
fn neighbouring_top_level_fields_stay_in_one_piece() {
    let rules = RuleSet::only(&["geo-coordinates"]);
    let pad = "padding ".repeat(20);
    let text = json!({
        "instructions": format!("{pad} 39.90421"),
        "lng": format!("116.40742 {pad}"),
        "input": [{"type": "message", "content": pad}],
    })
    .to_string();
    let whole = crate::redact::rules::scan(&text, &rules);
    assert_eq!(whole.len(), 2, "{text}");
    assert_eq!(piecewise(&text, &rules), whole);
    assert!(cacheable(&rules));
    agree(&text, &rules, Dialect::Responses, &ScanCache::new(), "geo");
}

/// 一条消息末尾一条没结束的命令，整段扫的时候接着认下一条消息开头的选项：这时候整段扫；
/// 后面头一个换行之前没有选项的话，照样按段取
#[test]
fn a_command_left_open_at_the_end_of_a_message_reaches_into_the_next() {
    let rules = RuleSet::defaults();
    let pad = "padding ".repeat(80);
    let msgs = |second: &str| {
        json!({"messages": [
            {"content": format!("{pad} run curl -s https://api.example.com")},
            {"content": second},
        ]})
        .to_string()
    };
    let text = msgs(&format!("-u admin:S3cretPass99 {pad}"));
    let whole = crate::redact::rules::scan(&text, &rules);
    assert!(
        whole
            .iter()
            .any(|h| &text[h.bytes.clone()] == "S3cretPass99"),
        "{whole:?}"
    );
    assert_ne!(piecewise(&text, &rules), whole, "单拿出来扫认不出");
    let cache = ScanCache::new();
    agree(&text, &rules, Dialect::Anthropic, &cache, "open command");
    assert!(text.len() > BODY_MIN);
    // 后面那一条头一个换行之前没有选项：按段取
    let text = msgs(&format!("ok\n-u admin:S3cretPass99 {pad}"));
    let cache = ScanCache::new();
    agree(
        &text,
        &rules,
        Dialect::Anthropic,
        &cache,
        "closed by a newline",
    );
    let before = cache.misses();
    agree(
        &text,
        &rules,
        Dialect::Anthropic,
        &cache,
        "closed by a newline, again",
    );
    assert_eq!(cache.misses(), before);
    assert!(cache.len() >= 2);
}

/// 每一条看得到隔壁的规则开着都不缓存；出厂的规则里一条都没有
#[test]
fn contextual_rules_exist_and_are_off_by_default() {
    for id in CONTEXTUAL {
        let b = crate::redact::rules::builtin(id).unwrap_or_else(|| panic!("{id} 不是内置规则"));
        assert!(!b.on_by_default, "{id}");
        assert!(!cacheable(&RuleSet::only(&[id])), "{id}");
    }
    assert!(cacheable(&RuleSet::defaults()));
    assert!(cacheable(&all_but(CONTEXTUAL)));
    // 保存的凭据本身带引号：按值找时能跨过一个字符串的结尾
    assert!(!cacheable(
        &RuleSet::defaults().with_saved(saved(&["abc\"def123456789"]))
    ));
    assert!(cacheable(&RuleSet::defaults().with_saved(saved(&[SAVED]))));
}

// ---------------------------------------------------------------- 切段

#[test]
fn a_body_splits_into_members_and_array_elements() {
    let text = r#" {"model":"m", "messages" : [ {"role":"user","content":"a"} ,{"x":[1,{"y":"]"}]}],"system":"s\"}","tools":[],"n":[1,2]} "#;
    let got: Vec<(&str, bool)> = split(text)
        .unwrap()
        .iter()
        .map(|p| (&text[p.range.clone()], p.wrapped))
        .collect();
    assert_eq!(
        got,
        [
            (r#""model":"m""#, true),
            (r#"{"role":"user","content":"a"}"#, false),
            (r#"{"x":[1,{"y":"]"}]}"#, false),
            (r#""system":"s\"}","tools":[],"n":[1,2]"#, true),
        ]
    );
}

#[test]
fn anything_unusual_is_scanned_whole() {
    for bad in [
        "",
        "[]",
        "{}",
        r#""text""#,
        r#"{"a":1,"a":2}"#,
        r#"{"a":1} x"#,
        r#"{"a":1,}"#,
        r#"{"a" 1}"#,
        r#"{"a":[{"b":1}"#,
        r#"{"a":"unterminated}"#,
        "\u{feff}{\"a\":1}",
    ] {
        assert!(split(bad).is_none(), "{bad:?}");
    }
    // 拆开的数组的键带转义：没有哪一段替它校验，整项算一段
    let p = split(r#"{"me\u0073sages":[{"a":1}]}"#).unwrap();
    assert_eq!(p.len(), 1);
    assert!(p[0].wrapped);
    // 太深
    let deep = format!(r#"{{"a":{}{}}}"#, "[".repeat(120), "]".repeat(120));
    assert!(split(&deep).is_none());
}

/// 坏掉的请求体（一段里面不是合规的 JSON）不按段扫，照旧整段扫
#[test]
fn a_broken_piece_falls_back_to_the_whole_scan() {
    let rules = RuleSet::defaults();
    let pad = "padding ".repeat(200);
    let text =
        format!(r#"{{"messages":[{{"content":"{pad} AKIAQRSTUVWXYZ234567"}},{{"content":01}}]}}"#);
    let cache = ScanCache::new();
    assert!(cache.kept(&text, &rules, Scope::Whole).is_none());
    assert_eq!(cache.len(), 0, "{text}");
    agree(&text, &rules, Dialect::Anthropic, &cache, "broken");
}

// ---------------------------------------------------------------- 容量

fn conversation(n: usize, each: usize) -> String {
    let history: Vec<Value> = (0..n)
        .map(|i| json!({"role": "user", "content": format!("{i:06} {}", "x".repeat(each))}))
        .collect();
    json!({"messages": history}).to_string()
}

#[test]
fn short_and_huge_pieces_are_scanned_but_not_kept() {
    let rules = RuleSet::defaults();
    let cache = ScanCache::new();
    // 每条消息不到 64 字节
    cache.kept(&conversation(50, 10), &rules, Scope::Whole);
    assert_eq!(cache.len(), 0);
    // 够长的存下来
    cache.kept(&conversation(50, 100), &rules, Scope::Whole);
    assert_eq!(cache.len(), 50);
    // 一条超过 1 MiB 的不存，旁边的照存
    let text = json!({"messages": [
        {"role": "user", "content": "y".repeat(MAX_BYTES + 1)},
        {"role": "user", "content": "z".repeat(100)},
    ]})
    .to_string();
    let cache = ScanCache::new();
    cache.kept(&text, &rules, Scope::Whole);
    assert_eq!(cache.len(), 1);
}

#[test]
fn the_cache_keeps_the_most_recent_entries_and_a_bounded_number_of_hits() {
    let rules = RuleSet::defaults();
    let cache = ScanCache::new();
    cache.kept(&conversation(CAPACITY + 300, 80), &rules, Scope::Whole);
    assert_eq!(cache.len(), CAPACITY);
    // 最近用过的留着：最后那 2000 条再来一遍全是取的
    let tail = {
        let history: Vec<Value> = (300..CAPACITY + 300)
            .map(|i| json!({"role": "user", "content": format!("{i:06} {}", "x".repeat(80))}))
            .collect();
        json!({"messages": history}).to_string()
    };
    let before = cache.misses();
    cache.kept(&tail, &rules, Scope::Whole);
    assert_eq!(cache.misses(), before, "最近的 2000 条都还在");
    // 最早的 300 条挤掉了
    let head = conversation(300, 80);
    cache.kept(&head, &rules, Scope::Whole);
    assert_eq!(cache.misses(), before + 300);
    assert_eq!(cache.len(), CAPACITY);
    // 一段的命中太多的不存；存着的命中加起来有上限
    let key = "AKIAQRSTUVWXYZ234567";
    let many = |n: usize| {
        let history: Vec<Value> = (0..n)
            .map(|i| json!({"role": "user", "content": format!("{i} {}", vec![key; 3000].join(" "))}))
            .collect();
        json!({"messages": history}).to_string()
    };
    let cache = ScanCache::new();
    let found = cache.kept(&many(30), &rules, Scope::Whole).unwrap();
    assert_eq!(found.len(), 90_000);
    assert!(
        cache.hits_stored() <= HITS_BUDGET,
        "{}",
        cache.hits_stored()
    );
    assert!(cache.len() < 30);
    let huge = json!({"messages": [{"role": "user",
        "content": vec![key; ENTRY_HITS_MAX + 1].join(" ")}]})
    .to_string();
    let cache = ScanCache::new();
    cache.kept(&huge, &rules, Scope::Whole);
    assert_eq!(cache.len(), 0);
}

// ---------------------------------------------------------------- 指纹

#[test]
fn the_fingerprint_changes_with_everything_that_changes_hits() {
    let base = RuleSet::defaults();
    let fp = base.fingerprint();
    // 同样的规则另建一份：HashSet 的顺序不同，指纹一样
    assert_eq!(fp, RuleSet::defaults().fingerprint());
    let ids: Vec<&str> = BUILTINS
        .iter()
        .filter(|b| b.on_by_default)
        .map(|b| b.id)
        .collect();
    let mut rev = ids.clone();
    rev.reverse();
    assert_eq!(
        RuleSet::only(&ids).fingerprint(),
        RuleSet::only(&rev).fingerprint()
    );
    let pattern = |text: &str| Pattern {
        text: text.into(),
        regex: false,
        case_sensitive: true,
    };
    let changed = [
        RuleSet::only(&ids[1..]),
        base.clone().with_saved(saved(&[SAVED])),
        base.clone().with_saved(saved(&["another-saved-value-123"])),
        base.clone().with_custom("a", "x+").unwrap(),
        base.clone().with_custom("b", "x+").unwrap(),
        base.clone().with_custom("a", "x*").unwrap(),
        base.clone().with_rule("a", pattern("x+"), None).unwrap(),
        base.clone()
            .with_rule(
                "a",
                Pattern {
                    case_sensitive: false,
                    ..pattern("x+")
                },
                None,
            )
            .unwrap(),
        base.clone()
            .with_rule("a", pattern("x+"), Some("L"))
            .unwrap(),
        base.clone()
            .with_rule("a", pattern("x+"), Some(""))
            .unwrap(),
        base.clone().recording(Rule::Builtin("jwt")),
        base.clone().recording(Rule::Custom("jwt".into())),
        base.clone()
            .with_custom("a", "1")
            .unwrap()
            .with_custom("b", "2")
            .unwrap(),
        base.clone()
            .with_custom("b", "2")
            .unwrap()
            .with_custom("a", "1")
            .unwrap(),
    ];
    let mut seen = vec![fp];
    for (i, s) in changed.iter().enumerate() {
        let f = s.fingerprint();
        assert!(!seen.contains(&f), "第 {i} 个和前面的撞了");
        seen.push(f);
    }
}

const KEYS: &[&str] = &[
    "content",
    "text",
    "c",
    "password",
    "api_key",
    "name",
    "phone",
    "lat",
    "lng",
    "token",
    "passport",
    "Authorization",
    "x",
    "real_name",
    "address",
    "imei",
    "护照",
    "url",
    "DB_PASSWORD",
    "role",
];

fn adv_string(r: &mut Rng) -> String {
    let mut s = String::new();
    let n = 1 + r.next(3);
    for i in 0..n {
        if i > 0 {
            s.push_str(r.pick(SEPS));
        }
        s.push_str(&fragment(r));
    }
    s
}

fn adv_object(r: &mut Rng, depth: usize) -> Value {
    let mut m = serde_json::Map::new();
    for _ in 0..1 + r.next(3) {
        let k = r.pick(KEYS).to_string();
        let v = if depth < 2 && r.chance(4) {
            adv_object(r, depth + 1)
        } else if r.chance(8) {
            json!(r.next(100000))
        } else {
            json!(adv_string(r))
        };
        m.insert(k, v);
    }
    Value::Object(m)
}

/// 每一条规则单独开着，在随手拼出来的请求体上（键专挑口令、字段名、坐标、护照这些会连起来
/// 看的，值专挑各条规则认得的片段）两条路各走一遍：一样。按段扫、不管看不看得到隔壁的话，
/// 有几条会不一样（命令行、护照、按字段名认的）—— 前者靠每段记着的「没结束的命令」，后几条
/// 靠 [`CONTEXTUAL`]
#[test]
fn every_rule_alone_agrees_on_adversarial_bodies() {
    let mut r = Rng(0x0dd_ba11_7777_1234);
    let mut piecewise_differs = std::collections::BTreeSet::new();
    let mut cached = 0;
    for _ in 0..120 {
        let mut top = serde_json::Map::new();
        for _ in 0..1 + r.next(4) {
            let k = r.pick(KEYS).to_string();
            let v = match r.next(3) {
                0 => json!(adv_string(&mut r)),
                1 => Value::Array((0..1 + r.next(4)).map(|_| adv_object(&mut r, 0)).collect()),
                _ => adv_object(&mut r, 0),
            };
            top.insert(k, v);
        }
        // 过了不走缓存的长度下限
        top.insert("zz_pad".into(), json!([{"p": "lorem ipsum ".repeat(100)}]));
        let mut text = Value::Object(top).to_string();
        if r.chance(4) {
            text = ascii_escaped(&text);
        }
        for b in BUILTINS {
            let rules = RuleSet::only(&[b.id]).with_saved(saved(&[SAVED]));
            let whole = crate::redact::rules::scan(&text, &rules);
            let cache = ScanCache::new();
            for scope in [Scope::Whole, Scope::Written(shape(Dialect::Anthropic))] {
                let got = flow::hits_in_with(&text, &rules, scope, Some(&cache));
                assert_eq!(flow::hits_in(&text, &rules, scope), got, "{}\n{text}", b.id);
            }
            cached += usize::from(cache.len() > 0);
            if piecewise(&text, &rules) != whole {
                piecewise_differs.insert(b.id);
            }
        }
    }
    assert!(cached > 0);
    for id in piecewise_differs {
        assert!(
            CONTEXTUAL.contains(&id) || id == "command-line-password",
            "{id} 单拿出来扫不一样，却没有防着"
        );
    }
}

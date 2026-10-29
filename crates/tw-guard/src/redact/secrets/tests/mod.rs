use crate::redact::replace::{Ledger, Scheme, redact, restore};
use crate::redact::rules::{
    BUILTINS, Kind, Matcher, Rule, RuleSet, SavedSecret, SavedSource, SavedSourceKind, Tail,
    findings_in, masked, scan, scan_plain,
};
pub(crate) mod samples;

use samples::{ALNUM, HEX, rnd, sample};

fn all() -> RuleSet {
    RuleSet::only(&BUILTINS.iter().map(|b| b.id).collect::<Vec<_>>())
}

/// 只开这一条，扫纯文本（和界面上的「试一试」一样，先编成 JSON 字符串）
fn values(id: &str, text: &str) -> Vec<String> {
    scan_plain(text, &RuleSet::only(&[id]))
        .into_iter()
        .filter(|h| h.rule.id() == id)
        .map(|h| text[h.bytes].to_string())
        .collect()
}

/// 这段文本放进一个聊天请求体，原样扫一遍：找到的值（请求体里的写法）
fn in_body(id: &str, text: &str) -> Vec<String> {
    let body = serde_json::json!({"messages": [
        {"role": "user", "content": format!("看看这个：\n{text}\n完")},
    ]})
    .to_string();
    scan(&body, &RuleSet::only(&[id]))
        .into_iter()
        .filter(|h| h.rule.id() == id)
        .map(|h| body[h.bytes].to_string())
        .collect()
}

fn code_rules() -> impl Iterator<Item = &'static str> {
    BUILTINS
        .iter()
        .filter(|b| {
            !b.kind.is_personal() && matches!(b.matcher, Matcher::Code | Matcher::Assignment { .. })
        })
        .map(|b| b.id)
}

#[test]
fn every_code_rule_has_at_least_three_hits_and_three_misses() {
    for id in code_rules() {
        let s = sample(id).unwrap_or_else(|| panic!("{id} 没有样本"));
        assert!(s.hits.len() >= 3 && s.misses.len() >= 3, "{id} 样本不够");
        for (text, value) in &s.hits {
            assert_eq!(values(id, text), vec![value.clone()], "{id} 没认出：{text}");
            // 请求体里：前面是 `\n`（JSON 转义），值照样一个字不差
            let escaped = serde_json::to_string(value).unwrap();
            let escaped = escaped[1..escaped.len() - 1].to_string();
            assert_eq!(
                in_body(id, text),
                vec![escaped],
                "{id} 在请求体里没认出：{text}"
            );
        }
        for text in &s.misses {
            assert!(
                values(id, text).is_empty(),
                "{id} 误报：{text} → {:?}",
                values(id, text)
            );
        }
    }
}

/// 每条多前缀规则：前缀加上够长、字符对的尾巴认得出（原样、请求体里、一句话中间），
/// 短一位、字符不对、前面粘着别的字的都不认
#[test]
fn every_prefix_family_rule_finds_its_shapes_and_nothing_near_them() {
    for b in BUILTINS {
        let Matcher::Prefixes { list } = b.matcher else {
            continue;
        };
        for (n, p) in list.iter().enumerate() {
            let alphabet = match p.tail {
                Tail::Hex => HEX,
                _ => ALNUM,
            };
            let tail = rnd(n as u64 + 7, p.min_tail, alphabet);
            let key = format!("{}{tail}", p.prefix);
            for text in [
                key.clone(),
                format!("my key is {key}, keep it safe"),
                format!("export K=\"{key}\""),
            ] {
                assert_eq!(
                    values(b.id, &text),
                    vec![key.clone()],
                    "{} 没认出 {text}",
                    b.id
                );
                assert_eq!(
                    in_body(b.id, &text),
                    vec![key.clone()],
                    "{} 请求体里没认出 {text}",
                    b.id
                );
            }
            // 别的规则一条都不该抢走它（G7：`sk-or-v1-` 不是 OpenAI 的）
            let got = scan(&key, &all());
            assert_eq!(got.len(), 1, "{key}");
            assert_eq!(got[0].rule.id(), b.id, "{key} 归错了规则");
            let short = format!("{}{}", p.prefix, &tail[..p.min_tail - 1]);
            assert!(
                values(b.id, &short).is_empty(),
                "{} 认了截断的 {short}",
                b.id
            );
            assert!(
                values(b.id, &format!("x{key}")).is_empty(),
                "{} 认了粘着的",
                b.id
            );
            if p.tail != Tail::Any {
                let bad = format!("{}{}!z", p.prefix, &tail[..p.min_tail - 1]).replace('!', "-");
                assert!(
                    values(b.id, &bad).is_empty(),
                    "{} 认了字符不对的 {bad}",
                    b.id
                );
            }
        }
    }
}

/// G7：别家的 `sk-` 前缀尾巴不够长、字符不对时也不归 OpenAI
#[test]
fn other_providers_sk_prefixes_never_become_openai_keys() {
    let set = RuleSet::only(&["openai-api-key", "sk-compat-key"]);
    for p in [
        "sk-or-v1-",
        "sk-lf-",
        "sk-svcacct-",
        "sk-admin-",
        "sk-api-",
        "sk-ant-",
        "sk-proj-",
    ] {
        let tok = format!("{p}{}", rnd(3, 40, ALNUM));
        assert!(
            scan(&tok, &set).is_empty(),
            "{tok} → {:?}",
            scan(&tok, &set)
        );
    }
    // 真的 OpenAI 老式密钥照样认
    let legacy = format!("sk-{}", rnd(9, 48, ALNUM));
    assert_eq!(scan(&legacy, &set)[0].rule.id(), "openai-api-key");
}

fn saved(value: &str, kind: SavedSourceKind, name: &str, hosts: &[&str]) -> SavedSecret {
    SavedSecret {
        value: value.to_string(),
        source: SavedSource {
            kind,
            name: name.to_string(),
        },
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
    }
}

#[test]
fn saved_secrets_are_found_by_value_and_reported_by_source() {
    let key = rnd(1, 32, HEX);
    let pass = "p\"ss\\word-2026!";
    let set = RuleSet::only(&["thinkwatch-saved-secret"]).with_saved(vec![
        saved(
            &key,
            SavedSourceKind::UpstreamKey,
            "DeepSeek",
            &["api.deepseek.com"],
        ),
        saved(pass, SavedSourceKind::ProxyPassword, "公司", &[]),
        // 太短的不收
        saved("short-1", SavedSourceKind::AccessKey, "x", &[]),
    ]);
    assert_eq!(set.saved().map(|s| s.len()), Some(2));
    for text in [
        key.clone(),
        format!("key={key}"),
        format!("line\\n{key}\\nnext"),
        format!("\"{key}\""),
        // 挨着的标点、分隔不算粘在一起：句末的句号、名字里的下划线、连字符
        format!("My key is {key}."),
        format!("see {key}... done"),
        format!("key={key}_old"),
        format!("MISTRAL_KEY_{key}"),
        format!("old-{key}-2"),
    ] {
        let got = scan(&text, &set);
        assert_eq!(got.len(), 1, "{text}");
        assert_eq!(&text[got[0].bytes.clone()], key);
    }
    // 粘着别的字的、截断的、大小写不同的不算
    for text in [
        format!("x{key}"),
        format!("{key}x"),
        key[..31].to_string(),
        key.to_uppercase(),
        "short-1".to_string(),
    ] {
        assert!(scan(&text, &set).is_empty(), "{text}");
    }
    // 带引号、反斜杠的值：请求体里是转义过的写法
    let body = serde_json::json!({"content": format!("proxy pass {pass} ok")}).to_string();
    let hits = scan(&body, &set);
    assert_eq!(hits.len(), 1, "{body}");
    let f = findings_in(&body, &hits, &set);
    assert_eq!(f[0].masked, "", "保存的凭据一个字都不报");
    assert_eq!(
        f[0].source,
        Some(SavedSource {
            kind: SavedSourceKind::ProxyPassword,
            name: "公司".into()
        })
    );
    // 两项不同的保存的凭据是两条
    let text = format!("{key} and {pass}");
    let body = serde_json::to_string(&text).unwrap();
    let f = findings_in(&body, &scan(&body, &set), &set);
    assert_eq!(f.len(), 2);
    // 规则关着就不找
    let off = RuleSet::none().with_saved(vec![saved(&key, SavedSourceKind::UpstreamKey, "a", &[])]);
    assert!(scan(&key, &off).is_empty());
}

#[test]
fn a_saved_secret_wins_over_a_format_rule_on_the_same_value() {
    let key = format!("sk-ant-api03-{}", rnd(5, 40, ALNUM));
    let set = RuleSet::defaults().with_saved(vec![saved(
        &key,
        SavedSourceKind::UpstreamKey,
        "Claude",
        &[],
    )]);
    let got = scan(&key, &set);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rule, Rule::Builtin("thinkwatch-saved-secret"));
}

#[test]
fn placed_rules_give_way_to_rules_that_say_what_the_value_is() {
    let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.abcdefghijk";
    let got = scan(&format!("Authorization: Bearer {jwt}"), &all());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rule.id(), "jwt");
    let key = format!("sk-ant-api03-{}", rnd(6, 30, ALNUM));
    let t = format!("ANTHROPIC_API_KEY={key}");
    let got = scan(&t, &all());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rule.id(), "anthropic-api-key");
}

/// 这段话放进一个请求体、按 `set` 换完之后发出去的样子（只记录的规则不换）。换完还是合法的
/// JSON、还原回来一字不差
fn sent(set: &RuleSet, text: &str) -> String {
    let body = serde_json::json!({"messages": [{"role": "user", "content": text}]}).to_string();
    let hits = scan(&body, set);
    let r =
        crate::redact::replace::apply(&body, &set.to_replace(&hits), Ledger::new(Scheme::SECRET));
    serde_json::from_str::<serde_json::Value>(&r.text).expect("换完不是合法 JSON");
    assert_eq!(restore(&r.text, &r.ledger), body);
    r.text
}

/// 按位置认的值只让给把它整段盖住、要换掉的命中：只盖住一截的（令牌中间像手机号、口令里
/// 像邮箱），或者那条规则只记录不换的，整段照样换掉
#[test]
fn placed_values_are_replaced_whole_unless_a_replacing_rule_covers_all_of_them() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    let set = RuleSet::defaults().recording(Rule::Builtin("jwt"));
    let out = sent(&set, &format!("Authorization: Bearer {jwt}"));
    assert!(!out.contains(jwt), "{out}");
    let with_phone: Vec<&str> = BUILTINS
        .iter()
        .filter(|b| b.on_by_default || b.id == "cn-mobile-phone")
        .map(|b| b.id)
        .collect();
    for set in [RuleSet::only(&with_phone), all()] {
        for (text, rest) in [
            (
                "Authorization: Bearer Zx9QkLmN13812345678kLmNoPqRsTuVw",
                ["Zx9QkLmN", "kLmNoPqRsTuVw"],
            ),
            ("DB_PASSWORD=Xk9_13812345678_qZ", ["Xk9_", "_qZ"]),
            (
                "SMTP_PASSWORD=\"ops@acme-mail.com#Q7x!\"",
                ["ops@", "#Q7x!"],
            ),
            (
                "https://h.example/x?access_token=ya29.a0AfB_byC@corp-sso.com.9xk2QzLp",
                ["ya29.a0AfB_byC", "9xk2QzLp"],
            ),
        ] {
            let out = sent(&set, text);
            for r in rest {
                assert!(!out.contains(r), "{text} → {out}");
            }
        }
    }
    // 整段盖住的照样让：报成 JWT
    let got = scan(
        &format!("Authorization: Bearer {jwt}"),
        &RuleSet::defaults(),
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rule.id(), "jwt");
}

/// 值最多看 4 KB：几 KB 的令牌（Azure AD 的访问令牌）照样认，再长的一段是数据，不是凭据
#[test]
fn placed_values_up_to_a_few_kilobytes_are_found_and_longer_runs_are_not() {
    let set = RuleSet::only(&["auth-header-credential", "url-token", "config-assignment"]);
    for n in [3000, 4096] {
        let tok = rnd(7, n, ALNUM);
        for text in [
            format!("Authorization: Bearer {tok}"),
            format!("https://x.example/cb?access_token={tok}"),
            format!("API_TOKEN={tok}"),
        ] {
            let got = scan(&text, &set);
            assert_eq!(got.len(), 1, "{n}");
            assert_eq!(&text[got[0].bytes.clone()], tok);
        }
    }
    let tok = rnd(7, 4097, ALNUM);
    assert!(scan(&format!("Authorization: Bearer {tok}"), &set).is_empty());
}

/// PHP 的 `json_encode` 缺省把 `/` 写成 `\/`：这样的请求体里，按路径认的几条（千帆、各家
/// Webhook）照样认得出，换掉的那一段带着转义
#[test]
fn path_rules_read_escaped_slashes() {
    for id in ["qianfan-api-key", "bot-webhook"] {
        for (text, value) in sample(id).unwrap().hits {
            let body = serde_json::json!({"messages": [{"role": "user", "content": text}]})
                .to_string()
                .replace('/', "\\/");
            let got: Vec<&str> = scan(&body, &RuleSet::only(&[id]))
                .into_iter()
                .map(|h| &body[h.bytes])
                .collect();
            assert_eq!(got, [value.replace('/', "\\/")], "{id}：{body}");
        }
    }
}

#[test]
fn placed_values_report_only_their_last_four_and_phrases_nothing() {
    let r = Rule::Builtin("config-assignment");
    assert_eq!(masked(&r, "hunter2hunter2"), "…");
    assert_eq!(masked(&r, "abcdefghijklmnop1234"), "…1234");
    assert_eq!(
        masked(&Rule::Builtin("crypto-mnemonic"), "abandon ability"),
        "…"
    );
    assert_eq!(
        masked(&Rule::Builtin("thinkwatch-saved-secret"), "whatever-value"),
        ""
    );
}

/// 每个样本换成占位符、请求体还是合法的 JSON，回答里一段一段地还原回来一字不差
#[test]
fn every_sample_is_replaced_and_restored_even_in_small_stream_chunks() {
    let mut texts = Vec::new();
    let mut secrets = Vec::new();
    for id in code_rules() {
        for (t, v) in sample(id).unwrap().hits {
            texts.push(t);
            secrets.push(v);
        }
    }
    let body = serde_json::json!({"messages": [{"role": "user", "content": texts.join("\n")}]})
        .to_string();
    let r = redact(&body, &all(), Ledger::new(Scheme::SECRET));
    serde_json::from_str::<serde_json::Value>(&r.text).expect("换完不是合法 JSON");
    for v in &secrets {
        assert!(!r.text.contains(v.as_str()), "没换掉：{v}");
    }
    assert_eq!(restore(&r.text, &r.ledger), body);
    let mut s = crate::redact::stream::Restorer::new(&r.ledger);
    let mut out = String::new();
    let chars: Vec<char> = r.text.chars().collect();
    for piece in chars.chunks(7) {
        out.push_str(&s.process(&piece.iter().collect::<String>()));
    }
    out.push_str(&s.flush());
    assert_eq!(out, body);
}

#[test]
fn the_wordlists_are_the_official_ones() {
    let en = super::crypto::english();
    assert_eq!(en.len(), 2048);
    assert!(en.windows(2).all(|w| w[0] < w[1]), "英文词表要排好、不重复");
    assert_eq!((en[0], en[2047]), ("abandon", "zoo"));
    let zh: Vec<&str> = super::crypto::CHINESE.lines().collect();
    assert_eq!(zh.len(), 2048);
    let set: std::collections::HashSet<&str> = zh.iter().copied().collect();
    assert_eq!(set.len(), 2048);
    assert!(zh.iter().all(|w| {
        let mut c = w.chars();
        matches!((c.next(), c.next()), (Some(ch), None) if ('\u{4E00}'..='\u{9FFF}').contains(&ch))
    }));
    assert_eq!((zh[0], zh[2047]), ("的", "歇"));
}

/// 第一段按字符类分流的「像不像词」和逐字节看的那一个（[`super::crypto::run_member`]）
/// 说法一样：词表里的词、它们改一个字母、截短加长、大小写、编号、带点带横线的都试一遍
#[test]
fn run_member_by_class_agrees_with_run_member() {
    use crate::redact::anchor::class;
    let check = |t: &str| {
        let b = t.as_bytes();
        let cls = b.iter().fold(0, |c, &x| c | class(x));
        assert_eq!(
            super::crypto::run_member_by_class(b, cls),
            super::crypto::run_member(b),
            "{t}"
        );
    };
    let mut toks: Vec<String> = Vec::new();
    for w in super::crypto::english() {
        toks.push(w.to_string());
        toks.push(w.to_ascii_uppercase());
        toks.push(format!("{w}s"));
        toks.push(format!("{w}."));
        toks.push(format!("{w}-x"));
        toks.push(w[..w.len() - 1].to_string());
        toks.push(format!("{}z", &w[..w.len() - 1]));
        toks.push(format!("q{w}"));
    }
    for t in [
        "1", "12", "123", "1.", "12.", "123.", "1..", ".1", "1.2", "..", ".", "a", "ab", "a1",
        "1a", "_", "-", "abc_", "lorem", "ipsum", "dolor", "sit", "amet", "the", "zoo", "zoos",
        "abandon", "abandons",
    ] {
        toks.push(t.to_string());
    }
    for t in &toks {
        check(t);
    }
}

/// BIP39 官方测试向量（entropy 9e885d952ad362caeb4efe34a8e91bd2）：校验和照标准算
#[test]
fn an_official_test_vector_checks_out() {
    let phrase = "ozone drill grab fiber curtain grace pudding thank cruise elder eight picnic";
    let en = super::crypto::english();
    let idx: Vec<u16> = phrase
        .split(' ')
        .map(|w| en.iter().position(|x| *x == w).unwrap() as u16)
        .collect();
    assert!(super::crypto::mnemonic(&idx));
    assert_eq!(values("crypto-mnemonic", phrase), vec![phrase.to_string()]);
    let entropy: Vec<u8> = (0..16)
        .map(|i| {
            u8::from_str_radix(&"9e885d952ad362caeb4efe34a8e91bd2"[i * 2..i * 2 + 2], 16).unwrap()
        })
        .collect();
    assert_eq!(samples::english_phrase(&entropy), phrase);
}

#[test]
fn assignment_suffixes_agree_with_the_table_shown_in_the_ui() {
    let mut shown: Vec<String> = super::ASSIGNMENT_SUFFIXES
        .iter()
        .map(|s| s.replace('_', "").to_ascii_lowercase())
        .collect();
    shown.dedup();
    let used: Vec<String> = super::assign::SUFFIXES
        .iter()
        .map(|s| String::from_utf8(s.to_vec()).unwrap())
        .collect();
    assert_eq!(shown, used);
    for name in [
        "DB_PASSWORD",
        "dbPassword",
        "db-password",
        "apiKey",
        "AWS_SECRET_ACCESS_KEY",
        "client_secret",
        "token",
    ] {
        assert!(super::assign::name_ends_right(name), "{name}");
    }
    for name in [
        "compassword",
        "mytokens",
        "PWD",
        "OLDPWD",
        "nextPageToken",
        "Tokenizer",
    ] {
        assert!(!super::assign::name_ends_right(name), "{name}");
    }
}

/// 所有新规则出厂都开着，都在凭据或按位置认的那几类里
#[test]
fn the_new_credential_rules_ship_on() {
    for b in BUILTINS.iter().filter(|b| {
        matches!(b.matcher, Matcher::Prefixes { .. } | Matcher::Saved)
            || (matches!(b.matcher, Matcher::Code | Matcher::Assignment { .. })
                && !b.kind.is_personal())
    }) {
        assert!(b.on_by_default, "{}", b.id);
        assert!(
            b.kind.is_credential() || b.kind == Kind::InContext,
            "{}",
            b.id
        );
    }
}

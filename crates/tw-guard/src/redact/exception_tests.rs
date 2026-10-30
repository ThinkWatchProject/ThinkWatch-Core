//! 0.86 的审查（扫描缓存 + 例外值）：报出去的哈希列为例外之后一定管用。
//!
//! 「哈希能不能从打码后的样子穷举回去」那一条不在这里：安全日志不再存哈希，只在内存里留最近
//! 的一批（见 `tw_store::Recorder`），配置里的例外表照旧是不加盐的 SHA-256（见配置手册的
//! 本机安全边界一节）。

use crate::policy::{Mode, RedactException, RedactPolicy};
use crate::redact::flow::{self, Shape};
use crate::redact::rules::{BUILTINS, RuleSet};

const SHAPE: Shape = Shape::Generate(tw_dialect::ir::Dialect::Anthropic);

fn body(text: &str) -> String {
    serde_json::json!({"messages": [{"role": "user", "content": text}]}).to_string()
}

fn except(mut p: RedactPolicy, hashes: &[String]) -> RedactPolicy {
    p.exceptions = hashes
        .iter()
        .map(|h| RedactException {
            sha256: h.clone(),
            masked: String::new(),
            rule: String::new(),
            added_at: 0,
        })
        .collect();
    p.check().unwrap();
    p
}

/// Every hash the security log records for a finding must be one that, once listed as an
/// exception, actually stops that value from being reported. Otherwise "List as exception"
/// writes an exception that never matches, and the log then shows the row as `excepted`.
fn assert_logged_hashes_take_effect(p: &RedactPolicy, text: &str) {
    let b = body(text);
    let found = flow::find(Mode::Enforce, &p.rules().unwrap(), b.as_bytes(), SHAPE);
    assert!(!found.is_empty(), "nothing found in {text:?}");
    for f in &found {
        let Some(h) = &f.sha256 else { continue };
        let ex = except(p.clone(), std::slice::from_ref(h));
        let again = flow::find(Mode::Enforce, &ex.rules().unwrap(), b.as_bytes(), SHAPE);
        assert!(
            !again.iter().any(|g| g.sha256.as_ref() == Some(h)),
            "excepting the logged hash of {:?} ({}) has no effect: {again:?}",
            f.masked,
            f.rule.id()
        );
    }
}

/// Two rules that overlap partially are merged into one hit (`united`); the finding's hash is
/// the hash of the merged span, which no single rule ever matched.
#[test]
fn excepting_a_merged_finding_takes_effect() {
    let p: RedactPolicy = serde_yaml_ng::from_str(
        "custom:\n  - { name: a, pattern: 'PRJ-[0-9]+', match: regex }\n  - { name: b, pattern: '[0-9]+-INTERNAL', match: regex }\n",
    )
    .unwrap();
    assert_logged_hashes_take_effect(&p, "ticket PRJ-123456-INTERNAL today");
    // 合成的那一处没有哪条规则认过：不给哈希，界面上也就没有「列为例外」
    let b = body("ticket PRJ-123456-INTERNAL today");
    let found = flow::find(Mode::Enforce, &p.rules().unwrap(), b.as_bytes(), SHAPE);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].sha256, None);
    // 单独一条规则认出来的照样有
    let b = body("ticket PRJ-123456 today");
    let found = flow::find(Mode::Enforce, &p.rules().unwrap(), b.as_bytes(), SHAPE);
    assert!(found[0].sha256.is_some(), "{found:?}");
}

/// A record-only rule's hit that overlaps a placeholder rule is cut into fragments
/// (`outside_of`); the finding's hash is the hash of a fragment.
#[test]
fn excepting_a_record_only_fragment_takes_effect() {
    let p: RedactPolicy = serde_yaml_ng::from_str(
        "enable: [email]\nactions: { email: record }\ncustom:\n  - { name: who, pattern: 'alice.ops' }\n",
    )
    .unwrap();
    assert_logged_hashes_take_effect(&p, "mail alice.ops@corp.example.com now");
}

/// Exploratory: built-in rules alone, realistic texts.
#[test]
fn excepting_builtin_findings_takes_effect() {
    let ids: Vec<&str> = BUILTINS.iter().map(|b| b.id).collect();
    let _ = RuleSet::only(&ids);
    let p: RedactPolicy = serde_yaml_ng::from_str(&format!(
        "enable: [{}]\n",
        ids.iter()
            .filter(|i| !crate::redact::cache::CONTEXTUAL.contains(i))
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .unwrap();
    let mut bad = Vec::new();
    for t in [
        "postgres://app:hunter2xyz9@db.internal:5432/orders",
        "https://u:p4ssw0rdXYZ@example.com/x",
        "mysql -u root -pS3cretPass99@corp.example.com db",
        "Authorization: Bearer sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        "API_KEY=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        "password: alice@corp.example.com",
        "https://hooks.example.com/cb?token=alice@corp.example.com&x=1",
    ] {
        let r = std::panic::catch_unwind(|| assert_logged_hashes_take_effect(&p, t));
        if r.is_err() {
            bad.push(t);
        }
    }
    assert!(bad.is_empty(), "dead exceptions for: {bad:?}");
}

/// Built-in rule pairs side by side, with every separator: no logged hash is a dead exception.
#[test]
fn builtin_overlaps_leave_no_dead_hash() {
    let ids: Vec<&str> = BUILTINS
        .iter()
        .map(|b| b.id)
        .filter(|i| !crate::redact::cache::CONTEXTUAL.contains(i))
        .collect();
    let p: RedactPolicy =
        serde_yaml_ng::from_str(&format!("enable: [{}]\n", ids.join(", "))).unwrap();
    let pool = [
        "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
        "AKIAQRSTUVWXYZ234567",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        "postgres://app:hunter2xyz9@db.internal:5432/orders",
        "alice@corp.example.com",
        "6222021234567890",
        "13800138000",
        "API_TOKEN=abc123XYZ789qq",
        "password: hunter2hunter2",
        "Authorization: Bearer abcDEF123456ghiJKL789mno",
        "https://hooks.example.com/cb?token=abcDEF123456ghi&x=1",
        "mysql -u root -pS3cretPass99 db",
        "39.9042, 116.4074",
        "DE89370400440532013000",
        "10.0.0.5",
        "00:1A:2B:3C:4D:5E",
        "110101199003071234",
        "SSN 123-45-6789",
        "+1 415 555 2671",
        "1HGCM82633A004352",
    ];
    let mut bad = Vec::new();
    for a in pool {
        for b in pool {
            for sep in ["", ":", "@", "/", "=", "-", "."] {
                let t = format!("x {a}{sep}{b} y");
                let body = body(&t);
                let rs = p.rules().unwrap();
                let found = flow::find(Mode::Enforce, &rs, body.as_bytes(), SHAPE);
                for f in &found {
                    let Some(h) = &f.sha256 else { continue };
                    let ex = except(p.clone(), std::slice::from_ref(h));
                    let again =
                        flow::find(Mode::Enforce, &ex.rules().unwrap(), body.as_bytes(), SHAPE);
                    if again.iter().any(|g| g.sha256.as_ref() == Some(h)) {
                        bad.push(format!("{t:?} -> {} {}", f.rule.id(), f.masked));
                    }
                }
            }
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{} dead:\n{}",
        bad.len(),
        bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
    );
}

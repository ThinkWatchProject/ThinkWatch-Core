//! Wallet encodings, opaque trading credentials, and blockchain identifiers.
//!
//! Format candidates are validated before use. UUIDs, raw hex and public addresses
//! deliberately cover identifiers as well as keys; they are not evidence of theft.
//! Context rules replace literal values, including recovery material without a
//! recognisable encoding. All spans stay inside JSON strings.

use std::ops::Range;
use std::sync::OnceLock;

use bech32::FromBase32;
use regex::Regex;
use sha2::{Digest, Sha256, Sha512};

use super::super::rules::{Hit, RuleSet, builtin_hit, json_string_spans};
use super::crypto::{base58, base58_alphabet, base58check};

pub(in crate::redact) const IDS: &[&str] = &[
    "uuid",
    "crypto-hex-key",
    "crypto-solana-key",
    "crypto-sui-key",
    "crypto-stellar-key",
    "crypto-xrp-seed",
    "crypto-address",
    "crypto-secret-field",
    "exchange-credential",
    "crypto-keystore",
    "crypto-rpc-key",
];

pub(in crate::redact) fn on(set: &RuleSet) -> bool {
    IDS.iter().any(|id| set.is_on(id))
}

fn shapes() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(concat!(
        r"(?i:[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}|(?:0x)?[0-9a-f]{64}|0x[0-9a-f]{40}|[0-9a-f]{32})",
        r"|(?i:suiprivkey1[023456789acdefghjklmnpqrstuvwxyz]{59}|(?:bc1|tb1)[023456789acdefghjklmnpqrstuvwxyz]{11,87})",
        r"|[SG][A-Z2-7]{55}|[123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz]{25,112}"
    )).expect("wallet candidate regex"))
}

fn names() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!("(?P<name>{})(?:$|[^a-zA-Z0-9_])", concat!(
        r"(?i:[a-z0-9_.-]*(?:private[_-]?key(?:[_-]?(?:bytes|hex))?|secret[_-]?key(?:[_-]?(?:bytes|hex))?|secret[_-]?uri|keypair|api[_-]?key|api[_-]?secret|access[_-]?key(?:[_-]?id)?|wallet[_-]?secret|key[_-]?secret|passphrase|mnemonic|seed(?:[_-]?(?:phrase|hex|bytes))?|recovery[_-]?(?:phrase|seed|codes?|shares?)|backup[_-]?(?:codes?|shares?)|mpc[_-]?share|totp[_-]?secret|signature|ciphertext|keystore|api[_-]?memo|api[_-]?sign|password|secret|otp|key|sign|memo|pk))",
        r"|(?i:x-mbx-apikey|ok-access-(?:key|passphrase|sign)|x-bapi-(?:api-key|sign)|cb-access-(?:key|passphrase|sign)|kc-api-(?:key|passphrase|sign)|access-(?:key|passphrase|sign)|x-mexc-apikey|x-gemini-(?:apikey|payload|signature)|x-bm-(?:key|sign)|x-phemex-access-token|x-auth(?:-signature)?|x-wallet-auth|poly_(?:api_key|passphrase|signature|builder_api_key|builder_passphrase|builder_signature)|relayer_api_key|api-sign)",
        r"|助记词密码|私钥|助记词|种子短语|恢复短语|恢复码|备份份额"
    ))).expect("credential field regex"))
}

/// Context parsing is also run on decoded JSON strings. This recognises escaped
/// labels and nested JSON without ever replacing a JSON number or delimiter.
/// Raw JSON field assignments are scanned separately and limited to string values.
pub(in crate::redact) fn scan(
    text: &str,
    set: &RuleSet,
    json: bool,
    spans: Option<&[Range<usize>]>,
    out: &mut Vec<Hit>,
) {
    if !on(set) {
        return;
    }
    let mut context_hits = Vec::new();
    if json {
        let owned;
        let ss = if let Some(spans) = spans {
            spans
        } else {
            owned = json_string_spans(text);
            &owned
        };
        // Real JSON fields. Their values can be strings; the textual scan below
        // also covers JSON/config documents pasted inside a chat string.
        context(text, set, Some(ss), &mut context_hits);
        for s in ss {
            let raw = &text[s.clone()];
            let mut hits = Vec::new();
            if !raw.contains('\\') {
                scan_shapes(raw, set, &mut hits);
                if set.is_on("crypto-solana-key") {
                    arrays(raw, None, &mut hits);
                }
                for mut h in hits.drain(..) {
                    h.bytes = s.start + h.bytes.start..s.start + h.bytes.end;
                    out.push(h);
                }
                context(raw, set, None, &mut hits);
                for mut h in hits {
                    h.bytes = s.start + h.bytes.start..s.start + h.bytes.end;
                    context_hits.push(h);
                }
            } else {
                let literal = crate::locate::Literal::new(text, s.clone());
                let decoded = crate::locate::unescape(raw);
                // A JSON key may itself use Unicode escapes. Match its decoded
                // spelling while keeping the value span in the original JSON.
                if text
                    .get(s.end + 1..)
                    .is_some_and(|tail| tail.trim_start().starts_with(':'))
                    && names()
                        .captures(&decoded)
                        .and_then(|c| c.name("name"))
                        .is_some_and(|m| m.start() == 0 && m.end() == decoded.len())
                    && let Some(id) = field_id(&decoded).filter(|id| set.is_on(id))
                    && let Some((r, _, true)) = value(text, s.end)
                    && inside(ss, r.clone())
                    && literal_value(&crate::locate::unescape(&text[r.clone()]))
                {
                    context_hits.push(builtin_hit(id, r));
                }
                scan_shapes(&decoded, set, &mut hits);
                if set.is_on("crypto-solana-key") {
                    arrays(&decoded, None, &mut hits);
                }
                for mut h in hits.drain(..) {
                    h.bytes = literal.raw_at(h.bytes.start)..literal.raw_at(h.bytes.end);
                    out.push(h);
                }
                context(&decoded, set, None, &mut hits);
                for mut h in hits {
                    h.bytes = literal.raw_at(h.bytes.start)..literal.raw_at(h.bytes.end);
                    context_hits.push(h);
                }
            }
        }
    } else {
        scan_shapes(text, set, out);
        if set.is_on("crypto-solana-key") {
            arrays(text, None, out);
        }
        context(text, set, None, &mut context_hits);
    }
    // Prefer a complete, replacing format/saved-secret hit over its field hit.
    // Partial matches and record-only rules must never expose the rest of a value.
    if context_hits.is_empty() {
        return;
    }
    let mut cover: Vec<Range<usize>> = out
        .iter()
        .filter(|h| !set.records(&h.rule))
        .map(|h| h.bytes.clone())
        .collect();
    cover.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in cover {
        if let Some(last) = merged.last_mut()
            && r.start <= last.end
        {
            last.end = last.end.max(r.end);
        } else {
            merged.push(r);
        }
    }
    for h in context_hits {
        let k = merged.partition_point(|r| r.start <= h.bytes.start);
        if k == 0 || merged[k - 1].end < h.bytes.end {
            out.push(h);
        }
    }
}

fn scan_shapes(text: &str, set: &RuleSet, out: &mut Vec<Hit>) {
    if !IDS[..7].iter().any(|id| set.is_on(id)) {
        return;
    }
    for m in shapes().find_iter(text) {
        if !boundary(text.as_bytes(), m.start(), m.end()) {
            continue;
        }
        let t = m.as_str();
        let id = if uuid(t) {
            "uuid"
        } else if hex_key(t) {
            "crypto-hex-key"
        } else if set.is_on("crypto-solana-key") && solana(t) {
            "crypto-solana-key"
        } else if set.is_on("crypto-sui-key")
            && t.get(..11)
                .is_some_and(|p| p.eq_ignore_ascii_case("suiprivkey1"))
            && sui(t)
        {
            "crypto-sui-key"
        } else if set.is_on("crypto-stellar-key") && stellar(t, 18 << 3) {
            "crypto-stellar-key"
        } else if set.is_on("crypto-xrp-seed") && xrp_seed(t) {
            "crypto-xrp-seed"
        } else if set.is_on("crypto-address") && address(t) {
            "crypto-address"
        } else {
            continue;
        };
        if set.is_on(id) {
            out.push(builtin_hit(id, m.range()));
        }
    }
}

fn inside(ss: &[Range<usize>], r: Range<usize>) -> bool {
    let k = ss.partition_point(|s| s.start <= r.start);
    k > 0 && r.end <= ss[k - 1].end
}

fn word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}
fn boundary(b: &[u8], s: usize, e: usize) -> bool {
    (s == 0 || !word(b[s - 1])) && (e == b.len() || !word(b[e]))
}

fn uuid(t: &str) -> bool {
    let b = t.as_bytes();
    (b.len() == 32 && b.iter().all(u8::is_ascii_hexdigit))
        || (b.len() == 36
            && b.iter().enumerate().all(|(i, c)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            }))
}
fn hex_key(t: &str) -> bool {
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit())
}

/// A Solana keypair is seed || public key. Signatures have the same length and
/// alphabet, so derive the Ed25519 public half instead of trusting length.
fn solana(t: &str) -> bool {
    if !(86..=88).contains(&t.len()) {
        return false;
    }
    let Some(v) = base58(t.as_bytes()).filter(|v| v.len() == 64) else {
        return false;
    };
    solana_bytes(&v)
}

fn solana_bytes(v: &[u8]) -> bool {
    if v.len() != 64 {
        return false;
    }
    let h = Sha512::digest(&v[..32]);
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&h[..32]);
    let public = curve25519_dalek::edwards::EdwardsPoint::mul_base_clamped(scalar).compress();
    public.as_bytes()[..] == v[32..]
}
fn arrays(text: &str, strings: Option<&[Range<usize>]>, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\[\s*[0-9]{1,3}(?:\s*,\s*[0-9]{1,3}){63}\s*\]").expect("Solana array regex")
    });
    for m in re.find_iter(text) {
        if strings.is_some_and(|ss| !inside(ss, m.range())) {
            continue;
        }
        if serde_json::from_str::<Vec<u8>>(m.as_str()).is_ok_and(|v| solana_bytes(&v)) {
            out.push(builtin_hit("crypto-solana-key", m.range()));
        }
    }
}

fn sui(t: &str) -> bool {
    let Ok((hrp, data, variant)) = bech32::decode(t) else {
        return false;
    };
    if hrp != "suiprivkey" || variant != bech32::Variant::Bech32 {
        return false;
    }
    Vec::<u8>::from_base32(&data).is_ok_and(|v| v.len() == 33 && matches!(v[0], 0..=2))
}
fn stellar(t: &str, version: u8) -> bool {
    if t.len() != 56 {
        return false;
    }
    let mut v = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in t.bytes() {
        let n = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return false,
        };
        acc = (acc << 5) | u32::from(n);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            v.push((acc >> bits) as u8);
        }
    }
    if v.len() != 35 || v[0] != version {
        return false;
    }
    let mut crc = 0u16;
    for &b in &v[..33] {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    v[33..] == crc.to_le_bytes()
}
const XRP: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
fn xrp(t: &str) -> Option<Vec<u8>> {
    let mut v = base58_alphabet(t.as_bytes(), XRP)?;
    if v.len() < 5 {
        return None;
    }
    let sum = v.split_off(v.len() - 4);
    (Sha256::digest(Sha256::digest(&v))[..4] == sum).then_some(v)
}
fn xrp_seed(t: &str) -> bool {
    (t.starts_with('s') && (29..=31).contains(&t.len()))
        && xrp(t).is_some_and(|v| {
            (v.len() == 17 && v[0] == 0x21) || (v.len() == 19 && v[..3] == [0x01, 0xe1, 0x4b])
        })
}
fn address(t: &str) -> bool {
    if t.len() == 42
        && (t.starts_with("0x") || t.starts_with("0X"))
        && t.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
    {
        return true;
    }
    if stellar(t, 6 << 3) {
        return true;
    }
    if t.starts_with('r')
        && (25..=35).contains(&t.len())
        && xrp(t).is_some_and(|v| v.len() == 21 && v[0] == 0)
    {
        return true;
    }
    if t.get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("bc1") || p.eq_ignore_ascii_case("tb1"))
        && let Ok((hrp, data, variant)) = bech32::decode(t)
        && (hrp == "bc" || hrp == "tb")
        && !data.is_empty()
    {
        let ver = data[0].to_u8();
        if let Ok(p) = Vec::<u8>::from_base32(&data[1..]) {
            return (ver == 0 && variant == bech32::Variant::Bech32 && matches!(p.len(), 20 | 32))
                || ((1..=16).contains(&ver)
                    && variant == bech32::Variant::Bech32m
                    && (2..=40).contains(&p.len()));
        }
    }
    if t.len() == 111
        && let Some(v) = base58check(t.as_bytes())
    {
        const V: [u32; 10] = [
            0x0488b21e, 0x043587cf, 0x049d7cb2, 0x0295b43f, 0x04b24746, 0x02aa7ed3, 0x044a5262,
            0x024289ef, 0x045f1cf6, 0x02575483,
        ];
        if v.len() == 78
            && V.contains(&u32::from_be_bytes(v[..4].try_into().unwrap()))
            && matches!(v[45], 2 | 3)
        {
            return true;
        }
    }
    if (26..=35).contains(&t.len())
        && base58check(t.as_bytes())
            .is_some_and(|v| v.len() == 21 && matches!(v[0], 0 | 5 | 0x6f | 0xc4 | 0x41))
    {
        return true;
    }
    // Solana/Sui legacy 32-byte public keys have no checksum. This is an
    // intentionally broad privacy shape, not a verified account or private key.
    (32..=44).contains(&t.len()) && base58(t.as_bytes()).is_some_and(|v| v.len() == 32)
}

fn normalized(t: &str) -> String {
    t.chars()
        .filter(|&c| c != '_' && c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn field_id(name: &str) -> Option<&'static str> {
    let n = normalized(name);
    let leaf = n.rsplit('.').next().unwrap_or(&n);
    if [
        "okaccess", "cbaccess", "kcapi", "access", "xbapi", "xgemini", "xbm", "poly",
    ]
    .iter()
    .any(|s| leaf.starts_with(s))
        || matches!(
            leaf,
            "xauth" | "xauthsignature" | "xwalletauth" | "xphemexaccesstoken"
        )
    {
        return Some("exchange-credential");
    }
    if leaf == "keystore" {
        return Some("crypto-keystore");
    }
    if leaf == "ciphertext" {
        return Some("crypto-keystore");
    }
    if [
        "privatekey",
        "privatekeybytes",
        "privatekeyhex",
        "secretkey",
        "secretkeybytes",
        "secretkeyhex",
        "secreturi",
        "keypair",
        "seed",
        "seedhex",
        "seedbytes",
        "pk",
        "walletsecret",
        "keysecret",
        "passphrase",
        "mnemonic",
        "seedphrase",
        "recoveryphrase",
        "recoverycode",
        "recoverycodes",
        "recoveryshare",
        "recoveryshares",
        "backupcode",
        "backupcodes",
        "backupshare",
        "backupshares",
        "mpcshare",
        "totpsecret",
    ]
    .iter()
    .any(|s| leaf.ends_with(s))
        || matches!(
            leaf,
            "pk" | "seed"
                | "私钥"
                | "助记词"
                | "种子短语"
                | "恢复短语"
                | "助记词密码"
                | "恢复码"
                | "备份份额"
        )
    {
        return Some("crypto-secret-field");
    }
    // Opaque API keys/secrets and signed authentication values have no reliable
    // universal length, prefix or alphabet. Literal assignment is the evidence.
    if [
        "apikey",
        "apisecret",
        "accesskey",
        "accesskeyid",
        "secret",
        "signature",
        "apisign",
        "apimemo",
    ]
    .iter()
    .any(|s| leaf.ends_with(s))
        || matches!(
            leaf,
            "key"
                | "sign"
                | "otp"
                | "memo"
                | "password"
                | "xauth"
                | "xauthsignature"
                | "xwalletauth"
        )
        || [
            "okaccess", "cbaccess", "kcapi", "access", "xbapi", "xgemini", "xbm", "poly",
        ]
        .iter()
        .any(|s| leaf.starts_with(s))
        || leaf == "xphemexaccesstoken"
    {
        return Some("exchange-credential");
    }
    None
}

fn context(text: &str, set: &RuleSet, strings: Option<&[Range<usize>]>, out: &mut Vec<Hit>) {
    if !IDS[7..10].iter().any(|id| set.is_on(id)) {
        if set.is_on("crypto-rpc-key") {
            rpc(text, strings, out);
        }
        return;
    }
    let b = text.as_bytes();
    let mut seen = 0;
    for cap in names().captures_iter(text) {
        let m = cap.name("name").expect("field capture");
        if m.start() < seen || !boundary(b, m.start(), m.end()) {
            continue;
        }
        let Some(id) = field_id(m.as_str()).filter(|id| set.is_on(id)) else {
            continue;
        };
        if m.start() > 0 && b[m.start() - 1] == b'$' {
            continue;
        }
        if let Some(ss) = strings {
            let k = ss.partition_point(|s| s.start <= m.start());
            if k == 0
                || ss[k - 1].start != m.start()
                || ss[k - 1].end != m.end()
                || b.get(m.end()) != Some(&b'"')
                || !text
                    .get(m.end() + 1..)
                    .is_some_and(|tail| tail.trim_start().starts_with(':'))
            {
                continue;
            }
        }
        let Some((mut r, end, quoted)) = value(text, m.end()) else {
            continue;
        };
        // Bitstamp's X-Auth value has a scheme followed by the API key.
        if normalized(m.as_str()) == "xauth"
            && !quoted
            && text[r.clone()].eq_ignore_ascii_case("BITSTAMP")
        {
            let from = b[r.end..]
                .iter()
                .position(|c| !c.is_ascii_whitespace())
                .map(|n| r.end + n);
            if let Some(from) = from {
                let to = b[from..]
                    .iter()
                    .position(|c| c.is_ascii_whitespace() || b"\"'`\\,;".contains(c))
                    .map_or(b.len(), |n| from + n);
                r = from..to;
            }
        }
        // Plain recovery phrases commonly appear without quotes.
        let n = normalized(m.as_str());
        if !quoted
            && !text[r.clone()].starts_with(['[', '{'])
            && (n.ends_with("mnemonic")
                || n.ends_with("phrase")
                || matches!(n.as_str(), "助记词" | "恢复短语" | "种子短语"))
        {
            r.end = text[r.start..]
                .find(['\n', '\r'])
                .map_or(text.len(), |e| r.start + e);
        }
        if strings.is_some_and(|ss| !inside(ss, r.clone())) {
            continue;
        }
        seen = end.max(r.end);
        let header = m.as_str().contains('-')
            || m.as_str().starts_with("POLY_")
            || m.as_str().starts_with("RELAYER_");
        if literal_value(&text[r.clone()]) && (quoted || header || bare_literal(&text[r.clone()])) {
            out.push(builtin_hit(id, r));
        }
    }
    if set.is_on("crypto-rpc-key") {
        rpc(text, strings, out);
    }
}

/// A literal following a field/header name. Quoted values may contain spaces,
/// Unicode and escaped characters. Arrays/objects are allowed only when the
/// caller proves the entire span is inside a JSON string (text pasted into chat).
fn value(text: &str, mut i: usize) -> Option<(Range<usize>, usize, bool)> {
    let b = text.as_bytes();
    if matches!(b.get(i), Some(b'"' | b'\'' | b'`')) {
        i += 1;
    }
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if b.get(i) == Some(&b']') {
        i += 1;
    }
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    match b.get(i) {
        _ if text[i..].starts_with('：') => i += "：".len(),
        Some(b':' | b'=' | b',') => i += 1,
        _ => return None,
    }
    if matches!(b.get(i), Some(b'=' | b'~' | b'>')) {
        return None;
    }
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if matches!(b.get(i), Some(b'|' | b'>')) {
        return yaml_block(text, i).map(|r| {
            let end = r.end;
            (r, end, true)
        });
    }
    let start;
    let end;
    let quoted = matches!(b.get(i), Some(b'"' | b'\'' | b'`'));
    if let Some(&q @ (b'"' | b'\'' | b'`')) = b.get(i) {
        start = i + 1;
        i = start;
        while i < b.len() && b[i] != q {
            if b[i] == b'\\' {
                i += 1;
            }
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        end = i;
        i += 1;
    } else if matches!(b.get(i), Some(b'[' | b'{')) {
        start = i;
        // serde verifies the structure and a bounded lexical walk finds the end.
        let mut stream =
            serde_json::Deserializer::from_slice(&b[i..]).into_iter::<serde_json::Value>();
        stream.next()?.ok()?;
        end = i + stream.byte_offset();
        i = end;
    } else {
        start = i;
        while b
            .get(i)
            .is_some_and(|c| !c.is_ascii_whitespace() && !b",;\"'`\\]}&#".contains(c))
        {
            i += 1;
        }
        end = i;
        if text[start..end].contains(['(', ')', '[', '{']) {
            return None;
        }
    }
    let mut after = i;
    while b.get(after).is_some_and(|c| matches!(c, b' ' | b'\t')) {
        after += 1;
    }
    // Concatenations, calls, templates, property expressions are code.
    if b.get(after).is_some_and(|c| b"+%([.$".contains(c)) {
        return None;
    }
    Some((start..end, i, quoted))
}

/// Preserve the first content line's indentation so a pasted YAML block remains
/// valid after replacement. Stop at the next field rather than hiding its value.
fn yaml_block(text: &str, marker: usize) -> Option<Range<usize>> {
    let line_start = text[..marker].rfind('\n').map_or(0, |i| i + 1);
    let indent = text[line_start..marker]
        .bytes()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    let newline = marker + text[marker..].find('\n')?;
    if !text[marker + 1..newline]
        .trim()
        .bytes()
        .all(|b| b"+-123456789".contains(&b))
    {
        return None;
    }
    let mut at = newline + 1;
    let mut first = None;
    let mut end = at;
    while at < text.len() {
        let next = text[at..].find('\n').map_or(text.len(), |n| at + n);
        let line = &text[at..next];
        let spaces = line
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
        if !line.trim().is_empty() {
            if spaces <= indent {
                break;
            }
            first.get_or_insert(at + spaces);
            end = next;
        }
        at = next.saturating_add(1);
    }
    first.map(|start| start..end)
}

fn placeholder(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "changeme"
            | "change_me"
            | "example"
            | "placeholder"
            | "redacted"
            | "dummy"
            | "sample"
            | "todo"
    ) || lower.starts_with("your_")
        || lower.starts_with("your-")
        || (v.len() >= 3 && v.bytes().all(|c| matches!(c, b'x' | b'X' | b'*')))
}

fn literal_value(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() || placeholder(v) {
        return false;
    }
    if v.starts_with('$')
        || (v.starts_with('<') && v.ends_with('>'))
        || v.contains("process.env")
        || v.contains("os.getenv")
        || v.contains("${")
        || v.contains("{{")
    {
        return false;
    }
    if matches!(
        v,
        "null" | "None" | "true" | "false" | "string" | "str" | "undefined"
    ) {
        return false;
    }
    // Numeric arrays/keystores are validated by `value`; literal phrases and
    // all-letter passphrases must be protected as well as random-looking keys.
    !v.is_empty() && !v.contains("env(") && !v.contains("getenv(")
}

fn bare_literal(v: &str) -> bool {
    // An unquoted identifier is a variable reference. Actual opaque keys normally
    // mix character classes; quoted passphrases have no alphabet restriction.
    v.starts_with(['[', '{'])
        || v.contains(' ')
        || !v.is_ascii()
        || (v.bytes().any(|c| c.is_ascii_digit()) && v.bytes().any(|c| c.is_ascii_alphabetic()))
        || v.bytes().any(|c| b"+/=!@#^".contains(&c))
}

fn rpc(text: &str, strings: Option<&[Range<usize>]>, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(concat!(
        r"(?i:https?://(?:[a-z0-9-]+\.)?(?:infura\.io/v3/|g\.alchemy\.com/v2/|quiknode\.pro/)[a-z0-9_%+~.-]+",
        r"|https?://(?:[a-z0-9-]+\.)?helius-rpc\.com/\?api-key=[a-z0-9_%+~.-]+)"
    )).expect("RPC key URL regex"));
    for m in re.find_iter(text) {
        let t = m.as_str();
        let at = if let Some(i) = t.find("api-key=") {
            i + 8
        } else {
            t.rfind('/').unwrap() + 1
        };
        let r = m.start() + at..m.end();
        if !r.is_empty()
            && strings.is_none_or(|ss| inside(ss, r.clone()))
            && literal_value(&text[r.clone()])
        {
            out.push(builtin_hit("crypto-rpc-key", r));
        }
    }
}

#[cfg(test)]
pub(in crate::redact) mod tests;

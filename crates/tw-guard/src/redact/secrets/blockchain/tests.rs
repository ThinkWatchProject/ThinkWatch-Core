use super::*;
use crate::redact::replace::{Ledger, Scheme, redact, restore};
use crate::redact::rules::{Rule, masked, scan, scan_plain};
use crate::redact::secrets::tests::samples::{HEX, Sample, base58_encode, check_encode, rnd};
use bech32::ToBase32;

fn pair(seed: u8) -> Vec<u8> {
    let seed: Vec<u8> = (0..32).map(|i| seed.wrapping_add(i * 7)).collect();
    let h = Sha512::digest(&seed);
    let public =
        curve25519_dalek::edwards::EdwardsPoint::mul_base_clamped(h[..32].try_into().unwrap())
            .compress();
    [seed, public.as_bytes().to_vec()].concat()
}
fn sui_key(n: u8) -> String {
    bech32::encode(
        "suiprivkey",
        [vec![n % 3], vec![n; 32]].concat().to_base32(),
        bech32::Variant::Bech32,
    )
    .unwrap()
}
fn stellar_key(n: u8, ver: u8) -> String {
    let mut v = [vec![ver], vec![n; 32]].concat();
    let mut crc = 0u16;
    for &b in &v {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    v.extend(crc.to_le_bytes());
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut acc, mut bits) = (0u32, 0);
    let mut s = String::new();
    for b in v {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            s.push(A[((acc >> bits) & 31) as usize] as char);
        }
    }
    s
}
fn xrp_key(n: u8, ed: bool) -> String {
    let p = [
        if ed { vec![1, 0xe1, 0x4b] } else { vec![0x21] },
        vec![n; 16],
    ]
    .concat();
    let s = check_encode(&p);
    const BTC: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    s.bytes()
        .map(|c| XRP[BTC.iter().position(|&b| b == c).unwrap()] as char)
        .collect()
}
fn same(s: String) -> (String, String) {
    (s.clone(), s)
}
fn assignments(names: &[&str]) -> Sample {
    Sample {
        hits: names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let v = format!("opaqueCredential{i}Alpha");
                (format!("\"{n}\": \"{v}\""), v)
            })
            .collect(),
        misses: vec![
            "privateKey = process.env.KEY".into(),
            "apiKey: string".into(),
            "key = '<your-key>'".into(),
            "key = getSecret()".into(),
        ],
    }
}
pub(in crate::redact) fn sample(id: &str) -> Option<Sample> {
    Some(match id {
        "uuid" => Sample {
            hits: vec![
                same("550e8400-e29b-41d4-a716-446655440000".into()),
                same(rnd(42, 32, HEX)),
                same("ABCDEF12-3456-7890-ABCD-EF1234567890".into()),
            ],
            misses: vec![
                "x550e8400-e29b-41d4-a716-446655440000".into(),
                "a".repeat(64),
                "abcdef1234".into(),
            ],
        },
        "crypto-hex-key" => Sample {
            hits: vec![
                same(rnd(4, 64, HEX)),
                same(format!("0x{}", rnd(5, 64, HEX))),
                same(format!("0X{}", rnd(6, 64, HEX).to_uppercase())),
            ],
            misses: vec![
                rnd(3, 63, HEX),
                rnd(4, 65, HEX),
                format!("x{}", rnd(5, 64, HEX)),
            ],
        },
        "crypto-solana-key" => Sample {
            hits: (1..=3).map(|n| same(base58_encode(&pair(n)))).collect(),
            misses: vec![
                base58_encode(&[7; 64]),
                base58_encode(&pair(4)[..32]),
                base58_encode(&[0; 64]),
            ],
        },
        "crypto-sui-key" => Sample {
            hits: (1..=3).map(|n| same(sui_key(n))).collect(),
            misses: vec![
                sui_key(1).replacen('1', "2", 1),
                bech32::encode(
                    "suiprivkey",
                    vec![3; 33].to_base32(),
                    bech32::Variant::Bech32,
                )
                .unwrap(),
                bech32::encode(
                    "suiprivkey",
                    vec![1; 32].to_base32(),
                    bech32::Variant::Bech32,
                )
                .unwrap(),
            ],
        },
        "crypto-stellar-key" => Sample {
            hits: (1..=3).map(|n| same(stellar_key(n, 18 << 3))).collect(),
            misses: vec![stellar_key(1, 6 << 3), "S".repeat(56), "S0".repeat(28)],
        },
        "crypto-xrp-seed" => Sample {
            hits: vec![
                same(xrp_key(1, false)),
                same(xrp_key(2, true)),
                same(xrp_key(3, false)),
            ],
            misses: vec![
                "s".repeat(29),
                xrp_key(4, true)[..29].into(),
                "s0".repeat(15),
            ],
        },
        "crypto-address" => Sample {
            hits: vec![
                same(format!("0x{}", rnd(13, 40, HEX))),
                same(stellar_key(4, 6 << 3)),
                same(base58_encode(&pair(4)[..32])),
            ],
            misses: vec![
                format!("0x{}", rnd(5, 39, HEX)),
                "G".repeat(56),
                "bc1badchecksum".into(),
            ],
        },
        "crypto-secret-field" => {
            assignments(&["PRIVY_WALLET_PRIVATE_KEY", "mnemonic", "passphrase"])
        }
        "exchange-credential" => {
            assignments(&["POLY_API_KEY", "PREDICT_API_KEY", "OK-ACCESS-PASSPHRASE"])
        }
        "crypto-keystore" => assignments(&["keystore", "ciphertext", "crypto.ciphertext"]),
        "crypto-rpc-key" => Sample {
            hits: vec![
                (
                    "https://mainnet.infura.io/v3/opaqueKeyAlpha".into(),
                    "opaqueKeyAlpha".into(),
                ),
                (
                    "https://eth-mainnet.g.alchemy.com/v2/opaqueKeyBeta".into(),
                    "opaqueKeyBeta".into(),
                ),
                (
                    "https://mainnet.helius-rpc.com/?api-key=opaqueKeyGamma".into(),
                    "opaqueKeyGamma".into(),
                ),
            ],
            misses: vec![
                "https://api.example/v3/opaqueKeyAlpha".into(),
                "https://infura.io.evil.example/v3/opaqueKeyAlpha".into(),
                "https://mainnet.infura.io/v3/<your-key>".into(),
            ],
        },
        _ => return None,
    })
}

#[test]
fn every_shape_and_context_round_trips_in_chat_json() {
    for id in IDS {
        let samples = sample(id).unwrap();
        for (text, value) in samples.hits {
            let body = serde_json::json!({"messages":[{"role":"user","content":text}]}).to_string();
            let set = RuleSet::only(&[id]);
            let hits = scan(&body, &set);
            assert_eq!(hits.len(), 1, "{id}: {text} {hits:?}");
            let encoded = serde_json::to_string(&value).unwrap();
            assert_eq!(
                &body[hits[0].bytes.clone()],
                &encoded[1..encoded.len() - 1],
                "{id}"
            );
            let r = redact(&body, &set, Ledger::new(Scheme::SECRET));
            serde_json::from_str::<serde_json::Value>(&r.text).unwrap();
            assert_eq!(restore(&r.text, &r.ledger), body, "{id}");
            assert!(!r.text.contains(&value), "{id}");
            let mut stream = crate::redact::stream::Restorer::new(&r.ledger);
            let chars: Vec<_> = r.text.chars().collect();
            let mut restored = String::new();
            for chunk in chars.chunks(7) {
                restored.push_str(&stream.process(&chunk.iter().collect::<String>()));
            }
            restored.push_str(&stream.flush());
            assert_eq!(restored, body, "{id}");
        }
    }
}

#[test]
fn trading_headers_and_sdk_fields_only_replace_values() {
    let fields = [
        "X-MBX-APIKEY",
        "OK-ACCESS-KEY",
        "OK-ACCESS-PASSPHRASE",
        "OK-ACCESS-SIGN",
        "X-BAPI-API-KEY",
        "X-BAPI-SIGN",
        "CB-ACCESS-KEY",
        "CB-ACCESS-PASSPHRASE",
        "API-Key",
        "API-Sign",
        "KC-API-KEY",
        "KC-API-PASSPHRASE",
        "ACCESS-KEY",
        "ACCESS-PASSPHRASE",
        "KEY",
        "SIGN",
        "X-MEXC-APIKEY",
        "X-GEMINI-APIKEY",
        "X-GEMINI-PAYLOAD",
        "X-GEMINI-SIGNATURE",
        "API-SECRET",
        "AccessKeyId",
        "X-BM-KEY",
        "X-BM-SIGN",
        "API_MEMO",
        "x-phemex-access-token",
        "X-Auth",
        "X-Auth-Signature",
        "X-Wallet-Auth",
        "POLY_API_KEY",
        "POLY_SECRET",
        "POLY_PASSPHRASE",
        "POLY_SIGNATURE",
        "POLY_BUILDER_API_KEY",
        "POLY_BUILDER_PASSPHRASE",
        "POLY_BUILDER_SIGNATURE",
        "POLYMARKET_BUILDER_SECRET",
        "RELAYER_API_KEY",
        "PREDICT_API_KEY",
        "x-api-key",
        "apiKey",
        "secret",
        "passphrase",
        "KEY_SECRET",
        "WALLET_SECRET",
        "WALLET_PRIVATE_KEY",
        "PRIVY_WALLET_PRIVATE_KEY",
        "SIGNER_PRIVATE_KEY",
        "pk",
    ];
    for name in fields {
        for text in [
            format!("{name}: opaqueValue37Alpha"),
            format!("\"{name}\": \"allletters\""),
            format!("{name}='a'"),
        ] {
            let hits = scan_plain(&text, &RuleSet::defaults());
            let vals: Vec<_> = hits.iter().map(|h| &text[h.bytes.clone()]).collect();
            assert_eq!(vals.len(), 1, "{name}: {text} {vals:?}");
            assert!(!vals[0].contains(name), "{text}");
        }
    }
}

#[test]
fn recovery_arrays_unicode_and_escaped_quotes_are_protected() {
    for text in [
        "mnemonic: abandon typo partial phrase",
        "私钥： '短口令'",
        "passphrase: \"correct horse battery staple\"",
        "secret: \"punctuation'and\\\"quotes\\nline\"",
        "seed = [7, 11, 13, 17]",
        "mnemonic = [\"abandon\",\"partial\",\"typo\"]",
        "keystore = {\"crypto\":{\"ciphertext\":\"encryptedAlpha37\"},\"version\":3}",
    ] {
        let body = serde_json::json!({"messages":[{"role":"user","content":text}]}).to_string();
        let r = redact(&body, &RuleSet::defaults(), Ledger::new(Scheme::SECRET));
        assert!(!r.ledger.is_empty(), "{text}");
        serde_json::from_str::<serde_json::Value>(&r.text).unwrap();
        assert_eq!(restore(&r.text, &r.ledger), body, "{text}");
    }
    let a = serde_json::to_string(&pair(9)).unwrap();
    let text = format!("Solana keypair: {a}");
    let hits = scan_plain(&text, &RuleSet::only(&["crypto-solana-key"]));
    assert_eq!(hits.len(), 1);
    assert_eq!(&text[hits[0].bytes.clone()], a);
}

#[test]
fn numeric_json_and_larger_tokens_are_left_structurally_valid() {
    let set = RuleSet::defaults();
    for text in [
        format!(
            r#"{{"seed":{},"n":{}}}"#,
            serde_json::to_string(&pair(2)).unwrap(),
            "1".repeat(32)
        ),
        format!("x{}y", rnd(42, 64, HEX)),
    ] {
        assert!(scan(&text, &set).is_empty(), "{text}");
    }
}

#[test]
fn all_slip132_versions_have_valid_private_payloads() {
    for version in [
        0x0488ade4u32,
        0x04358394,
        0x049d7878,
        0x0295b005,
        0x04b2430c,
        0x02aa7a99,
        0x044a4e28,
        0x024285b5,
        0x045f18bc,
        0x02575048,
    ] {
        let key = crate::redact::secrets::tests::samples::xprv_of(18, version.to_be_bytes());
        assert_eq!(
            scan_plain(&key, &RuleSet::only(&["crypto-extended-key"])).len(),
            1,
            "{key}"
        );
        let mut payload = base58check(key.as_bytes()).unwrap();
        payload[45] = 2;
        assert!(
            scan_plain(
                &check_encode(&payload),
                &RuleSet::only(&["crypto-extended-key"])
            )
            .is_empty()
        );
    }
}

#[test]
fn findings_never_include_wallet_fragments() {
    for id in IDS.iter().filter(|id| **id != "crypto-address") {
        let (_, v) = sample(id).unwrap().hits.remove(0);
        assert_eq!(masked(&Rule::Builtin(id), &v), "…", "{id}");
    }
}

#[test]
fn negative_samples_and_bitstamp_scheme_are_checked() {
    for id in IDS {
        for text in sample(id).unwrap().misses {
            assert!(
                scan_plain(&text, &RuleSet::only(&[id])).is_empty(),
                "{id}: {text}"
            );
        }
    }
    let text = "X-Auth: BITSTAMP opaqueValue37Alpha";
    let hits = scan_plain(text, &RuleSet::only(&["exchange-credential"]));
    assert_eq!(&text[hits[0].bytes.clone()], "opaqueValue37Alpha");
    let text = "X-MBX-APIKEY: pureletters";
    let hits = scan_plain(text, &RuleSet::only(&["exchange-credential"]));
    assert_eq!(&text[hits[0].bytes.clone()], "pureletters");
    let text = "助记词密码：'短口令'";
    assert_eq!(
        scan_plain(text, &RuleSet::only(&["crypto-secret-field"])).len(),
        1
    );
}

#[test]
fn disabled_recording_and_exceptions_keep_their_meaning() {
    let value = "550e8400-e29b-41d4-a716-446655440000";
    assert!(scan_plain(value, &RuleSet::none()).is_empty());
    let set = RuleSet::defaults().recording(Rule::Builtin("uuid"));
    assert_eq!(scan_plain(value, &set).len(), 1);
    let body = serde_json::json!({"messages":[{"role":"user","content":value}]}).to_string();
    let (r, ledger) = crate::redact::flow::replace(
        crate::policy::Mode::Enforce,
        &set,
        body.clone().into(),
        &Ledger::new(Scheme::SECRET),
        crate::redact::flow::Shape::Generate(tw_dialect::ir::Dialect::Anthropic),
    );
    assert_eq!(r.as_ref(), body.as_bytes());
    assert!(ledger.is_empty());
    let digest = Sha256::digest(value.as_bytes()).into();
    let set = RuleSet::defaults().with_exceptions(
        crate::redact::exceptions::Exceptions::new([digest]).map(std::sync::Arc::new),
    );
    assert!(scan_plain(value, &set).is_empty());
}

#[test]
fn a_non_string_secret_container_does_not_hide_its_nested_string_fields() {
    let body = r#"{"api_key":[{"api_key":"opaqueValue37Alpha"}]}"#;
    let hits = scan(body, &RuleSet::only(&["exchange-credential"]));
    assert_eq!(hits.len(), 1);
    assert_eq!(&body[hits[0].bytes.clone()], "opaqueValue37Alpha");
}

#[test]
fn base64_and_symbol_first_passphrases_are_literal_secrets() {
    for value in ["/base64material+==", "!@#$%", "~allletters", "短", "0000"] {
        let text = format!("passphrase: {}", serde_json::to_string(value).unwrap());
        let hits = scan_plain(&text, &RuleSet::only(&["crypto-secret-field"]));
        assert_eq!(hits.len(), 1, "{text}");
    }
    for text in ["{\"secret", "{\"passphrase", "{\"x-api-key"] {
        assert!(scan(text, &RuleSet::defaults()).is_empty(), "{text}");
    }
    assert!(
        scan_plain(
            "X-API-KEY: request.headers[\"key\"]",
            &RuleSet::only(&["exchange-credential"])
        )
        .is_empty()
    );
}

#[test]
fn escaped_json_names_and_uppercase_bech32_are_recognized() {
    let body = r#"{"api\u005fkey":"opaqueValue37Alpha"}"#;
    let hits = scan(body, &RuleSet::only(&["exchange-credential"]));
    assert_eq!(hits.len(), 1);
    assert_eq!(&body[hits[0].bytes.clone()], "opaqueValue37Alpha");
    let key = sui_key(1).to_uppercase();
    assert_eq!(
        scan_plain(&key, &RuleSet::only(&["crypto-sui-key"])).len(),
        1
    );
    let data = [
        vec![bech32::u5::try_from_u8(0).unwrap()],
        vec![7u8; 20].to_base32(),
    ]
    .concat();
    let addr = bech32::encode("bc", data, bech32::Variant::Bech32)
        .unwrap()
        .to_uppercase();
    assert_eq!(
        scan_plain(&addr, &RuleSet::only(&["crypto-address"])).len(),
        1
    );
}

#[test]
fn seed_aliases_and_yaml_blocks_keep_following_fields() {
    for name in [
        "WALLET_SEED",
        "SOLANA_KEYPAIR",
        "privateKeyBytes",
        "SUI_SECRET_KEY_HEX",
        "secretURI",
        "wallet_seed_hex",
        "recoverySeed",
        "WALLET_PK",
    ] {
        let text = format!("{name}: 'opaqueAlpha37'");
        let hits = scan_plain(&text, &RuleSet::only(&["crypto-secret-field"]));
        assert_eq!(hits.len(), 1, "{text}");
    }
    let text = "mnemonic: |\n  typo phrase\n  additional words\nnext: safe";
    let body = serde_json::to_string(text).unwrap();
    let r = redact(
        &body,
        &RuleSet::only(&["crypto-secret-field"]),
        Ledger::new(Scheme::SECRET),
    );
    let sent: String = serde_json::from_str(&r.text).unwrap();
    assert_eq!(sent, "mnemonic: |\n  <<TW_SECRET_1>>\nnext: safe");
    assert_eq!(restore(&r.text, &r.ledger), body);
}

#[test]
fn numeric_credentials_in_text_are_hidden_without_replacing_json_numbers() {
    for text in [
        "OTP: 927416",
        "privateKey = 123456789012345678901234567890123456789012345678901234567890123456789012345",
        "seed: 42",
    ] {
        let body = serde_json::json!({"messages":[{"role":"user","content":text}]}).to_string();
        let r = redact(&body, &RuleSet::defaults(), Ledger::new(Scheme::SECRET));
        assert_eq!(r.ledger.len(), 1, "{text}");
        assert_eq!(restore(&r.text, &r.ledger), body);
        serde_json::from_str::<serde_json::Value>(&r.text).unwrap();
    }
    let body = r#"{"seed":42,"OTP":927416}"#;
    assert!(scan(body, &RuleSet::defaults()).is_empty());
}

#[test]
fn code_arguments_and_type_annotations_are_not_credential_assignments() {
    for text in [
        "print(key, 'field_names', list(value))",
        "print(key, 'type', type(value).__name__)",
        "const labels = [key, 'time', 'event'];",
        "pub passphrase: String,",
        "pub passphrase: Option<String>,",
        "pub seed: Vec<u8>,",
        "pub secret: Option<String>,",
        "passphrase: String,\\n    next_field: value",
        "passphrase: Option<String>,\\n    next_field: value",
        "fn restore(passphrase: &str) -> String { value.to_string() }",
    ] {
        let set = RuleSet::defaults();
        assert!(scan_plain(text, &set).is_empty(), "{text}");
        let body = serde_json::json!({"input":[{"role":"user","content":text}]}).to_string();
        let (sent, ledger) = crate::redact::flow::replace(
            crate::policy::Mode::Enforce,
            &set,
            body.clone().into(),
            &Ledger::new(Scheme::SECRET),
            crate::redact::flow::Shape::Generate(tw_dialect::ir::Dialect::Responses),
        );
        assert_eq!(sent.as_ref(), body.as_bytes(), "{text}");
        assert!(ledger.is_empty(), "{text}");
    }
}

#[test]
fn quoted_header_pairs_still_protect_the_value() {
    for text in [
        "headers.insert(\"X-API-KEY\", \"opaqueAlpha37\")",
        "headers = [(\"POLY_API_KEY\", \"opaqueAlpha37\")]",
        "headers[\"api_key\"] = \"opaqueAlpha37\"",
    ] {
        let hits = scan_plain(text, &RuleSet::defaults());
        let values: Vec<_> = hits.iter().map(|h| &text[h.bytes.clone()]).collect();
        assert_eq!(values, ["opaqueAlpha37"], "{text}");
    }
}

#[test]
fn short_numeric_lookup_keys_are_not_secrets_but_explicit_credentials_are() {
    for text in [
        "[WsPool] new connection: key=0, 2 assets",
        "key=42",
        "pub seed: u64,",
        "let seed: u64 = get_seed();",
    ] {
        assert!(scan_plain(text, &RuleSet::defaults()).is_empty(), "{text}");
    }
    for text in ["KEY: '42'", "API_KEY=42", "OTP: 927416", "seed: 42"] {
        assert_eq!(scan_plain(text, &RuleSet::defaults()).len(), 1, "{text}");
    }
}

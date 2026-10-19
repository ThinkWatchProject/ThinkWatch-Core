//! 配置包的格式：写出去读得回来，读的时候不信任文件。

use std::io::Write;

use super::*;

fn fast() {
    // 口令加密的工作量调低：测试不用每次等一秒
    TEST_WORK_FACTOR.store(10, std::sync::atomic::Ordering::Relaxed);
}

fn sample() -> Pack {
    let mut plugins = BTreeMap::new();
    plugins.insert(
        "add-date".to_string(),
        PluginFiles {
            script: b"export const manifest = { name: \"x\", api: 1 };\n".to_vec(),
            approved: Some(b"export const manifest = { name: \"x\", api: 1 };\n".to_vec()),
        },
    );
    plugins.insert(
        "only-script".to_string(),
        PluginFiles {
            script: b"// no approved copy\n".to_vec(),
            approved: None,
        },
    );
    Pack {
        manifest: Manifest::new(PackKind::Full, PackOrigin::Export),
        config_yaml: "version: 1\n# 注释\nproviders:\n  - { name: a, base_url: https://a.example, key: sk-a }\nclients:\n  - { name: default, key: tw-k }\n".into(),
        plugins,
        client_prefs: Some(serde_json::json!({"language": "zh", "menubar": {"show": "tokens"}})),
    }
}

#[test]
fn a_pack_reads_back_exactly_as_written_with_each_seal() {
    fast();
    let pack = sample();
    let id = age::x25519::Identity::generate();
    let recipient = id.to_public();
    for (seal, unseal) in [
        (Seal::None, Unseal::None),
        (
            Seal::Passphrase("correct horse"),
            Unseal::Passphrase("correct horse"),
        ),
        (Seal::Recipient(&recipient), Unseal::Identity(&id)),
    ] {
        let encrypted = !matches!(seal, Seal::None);
        let bytes = write(&pack, seal).unwrap();
        assert_eq!(is_encrypted(&bytes), encrypted);
        let back = read(&bytes, unseal).unwrap();
        assert_eq!(back.config_yaml, pack.config_yaml);
        assert_eq!(back.plugins, pack.plugins);
        assert_eq!(back.client_prefs, pack.client_prefs);
        assert_eq!(back.manifest.encrypted, encrypted);
        assert_eq!(back.manifest.counts.providers, 1);
        assert_eq!(back.manifest.counts.clients, 1);
        assert_eq!(back.manifest.origin, PackOrigin::Export);
        assert_eq!(back.manifest.core_version, CORE_VERSION);
    }
}

/// 内容一样，字节就一样（只差创建时间）：备份去重、同步比较靠得住
#[test]
fn the_same_content_writes_the_same_bytes() {
    let mut a = sample();
    a.manifest.created_at_ms = 1;
    let b = a.clone();
    assert_eq!(
        write(&a, Seal::None).unwrap(),
        write(&b, Seal::None).unwrap()
    );
}

#[test]
fn opening_an_encrypted_pack_needs_the_right_secret() {
    fast();
    let pack = sample();
    let by_pass = write(&pack, Seal::Passphrase("right one")).unwrap();
    let id = age::x25519::Identity::generate();
    let by_key = write(&pack, Seal::Recipient(&id.to_public())).unwrap();
    let other = age::x25519::Identity::generate();
    let cases: Vec<(&[u8], Unseal, PackError)> = vec![
        (&by_pass, Unseal::None, PackError::NeedPassphrase),
        (
            &by_pass,
            Unseal::Passphrase("wrong one"),
            PackError::WrongPassphrase,
        ),
        (&by_pass, Unseal::Identity(&id), PackError::NeedPassphrase),
        (&by_key, Unseal::None, PackError::NeedKey),
        (&by_key, Unseal::Passphrase("right one"), PackError::NeedKey),
        (&by_key, Unseal::Identity(&other), PackError::WrongKey),
    ];
    for (bytes, unseal, want) in cases {
        assert_eq!(read(bytes, unseal).unwrap_err(), want);
    }
}

/// 手工做一个 ZIP：测「读的时候不信任文件」
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in entries {
        z.start_file(*name, opts).unwrap();
        z.write_all(bytes).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn manifest_json(format: u32) -> Vec<u8> {
    let mut m = Manifest::new(PackKind::Full, PackOrigin::Export);
    m.format = format;
    serde_json::to_vec(&m).unwrap()
}

#[test]
fn a_pack_with_anything_unexpected_inside_is_refused_whole() {
    let m = manifest_json(FORMAT);
    let cfg: &[u8] = b"version: 1\n";
    for (bad, why) in [
        ("../config.yaml", "climbs out"),
        ("/etc/passwd", "absolute"),
        ("plugins/../../x.js", "climbs out through plugins"),
        ("plugins/Bad_Id.js", "an id that is not one"),
        ("plugins/a.txt", "not a script"),
        (
            "plugins/.approved/../a.js",
            "climbs out of the approved copies",
        ),
        ("README.md", "a file a pack does not have"),
        ("plugins/sub/a.js", "a nested directory"),
    ] {
        let bytes = zip_of(&[("manifest.json", &m), ("config.yaml", cfg), (bad, b"x")]);
        match read(&bytes, Unseal::None) {
            Err(PackError::BadEntry { name }) => assert_eq!(name, bad, "{why}"),
            other => panic!("{why}: {other:?}"),
        }
    }
    // 只有底稿没有脚本
    let bytes = zip_of(&[
        ("manifest.json", &m),
        ("config.yaml", cfg),
        ("plugins/.approved/a.js", b"x"),
    ]);
    assert!(matches!(
        read(&bytes, Unseal::None),
        Err(PackError::BadEntry { .. })
    ));
    // 目录条目无害
    let bytes = zip_of(&[
        ("manifest.json", &m),
        ("config.yaml", cfg),
        ("plugins/", b""),
        ("plugins/.approved/", b""),
        ("plugins/a.js", b"x"),
    ]);
    assert_eq!(read(&bytes, Unseal::None).unwrap().plugins.len(), 1);
}

#[test]
fn a_pack_missing_its_manifest_or_config_says_which() {
    let m = manifest_json(FORMAT);
    for (entries, missing) in [
        (vec![("config.yaml", &b"version: 1\n"[..])], "manifest.json"),
        (vec![("manifest.json", &m[..])], "config.yaml"),
    ] {
        let e = read(&zip_of(&entries), Unseal::None).unwrap_err();
        assert_eq!(
            e,
            PackError::MissingEntry {
                name: missing.into()
            }
        );
        assert_eq!(e.msg().code, "pack.missing_entry");
    }
}

#[test]
fn a_newer_format_is_too_new_and_not_corrupt() {
    let m = manifest_json(FORMAT + 1);
    let bytes = zip_of(&[("manifest.json", &m), ("config.yaml", b"version: 1\n")]);
    match read(&bytes, Unseal::None) {
        Err(PackError::TooNew {
            format,
            core_version,
        }) => {
            assert_eq!(format, FORMAT + 1);
            assert_eq!(core_version, CORE_VERSION);
        }
        other => panic!("{other:?}"),
    }
    // 新的格式里清单多了不认识的字段，照样先说太新
    let raw = serde_json::json!({"format": FORMAT + 1, "whatever": [1, 2]});
    let bytes = zip_of(&[
        ("manifest.json", raw.to_string().as_bytes()),
        ("config.yaml", b"version: 1\n"),
    ]);
    assert!(matches!(
        read(&bytes, Unseal::None),
        Err(PackError::TooNew { .. })
    ));
    // 同一个格式里多出来的字段不碍事：新一点的 Core 写的、格式没变的包读得进来
    let mut raw: serde_json::Value = serde_json::from_slice(&manifest_json(FORMAT)).unwrap();
    raw["something_new"] = serde_json::json!(true);
    raw["origin"] = serde_json::json!("a_new_origin");
    let bytes = zip_of(&[
        ("manifest.json", raw.to_string().as_bytes()),
        ("config.yaml", b"version: 1\n"),
    ]);
    let p = read(&bytes, Unseal::None).unwrap();
    assert_eq!(p.manifest.origin, PackOrigin::Other);
}

#[test]
fn garbage_is_corrupt_and_says_so() {
    for bytes in [&b"not a zip at all"[..], b"PK\x03\x04broken", b""] {
        let e = read(bytes, Unseal::None).unwrap_err();
        assert!(matches!(e, PackError::Corrupt { .. }), "{e:?}");
        assert_eq!(e.msg().code, "pack.corrupt");
    }
    let m = manifest_json(FORMAT);
    let bytes = zip_of(&[("manifest.json", &m), ("config.yaml", &[0xff, 0xfe])]);
    assert!(matches!(
        read(&bytes, Unseal::None),
        Err(PackError::Corrupt { .. })
    ));
    let bytes = zip_of(&[
        ("manifest.json", b"{not json"),
        ("config.yaml", b"version: 1\n"),
    ]);
    assert!(matches!(
        read(&bytes, Unseal::None),
        Err(PackError::Corrupt { .. })
    ));
}

/// 解压之后的大小按真读到的算：一个压得很小、解开很大的包挡在门口
#[test]
fn a_pack_that_unpacks_to_more_than_the_limit_is_refused() {
    let m = manifest_json(FORMAT);
    let big = vec![b'a'; MAX_BYTES + 1];
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    z.start_file("manifest.json", opts).unwrap();
    z.write_all(&m).unwrap();
    z.start_file("config.yaml", opts).unwrap();
    z.write_all(&big).unwrap();
    let bytes = z.finish().unwrap().into_inner();
    assert!(bytes.len() < MAX_BYTES, "the compressed file is small");
    assert_eq!(
        read(&bytes, Unseal::None).unwrap_err(),
        PackError::TooLarge { limit: MAX_BYTES }
    );
    // 本身就太大的文件不打开
    assert_eq!(
        read(&vec![0u8; MAX_BYTES + 1], Unseal::None).unwrap_err(),
        PackError::TooLarge { limit: MAX_BYTES }
    );
    // 写的时候也挡
    let mut p = sample();
    p.config_yaml = "a".repeat(MAX_BYTES * 2);
    // 压得很小的内容照样按解压后的大小算：写得出去就得读得回来
    assert_eq!(
        write(&p, Seal::None).unwrap_err(),
        PackError::TooLarge { limit: MAX_BYTES }
    );
}

#[test]
fn every_error_speaks_with_its_own_code() {
    let all = [
        (PackError::NeedPassphrase, "pack.need_passphrase"),
        (PackError::WrongPassphrase, "pack.wrong_passphrase"),
        (PackError::NeedKey, "pack.need_key"),
        (PackError::WrongKey, "pack.wrong_key"),
        (
            PackError::TooNew {
                format: 2,
                core_version: "9.0.0".into(),
            },
            "pack.too_new",
        ),
        (PackError::Corrupt { detail: "x".into() }, "pack.corrupt"),
        (PackError::BadEntry { name: "x".into() }, "pack.bad_entry"),
        (
            PackError::MissingEntry { name: "x".into() },
            "pack.missing_entry",
        ),
        (PackError::TooLarge { limit: MAX_BYTES }, "pack.too_large"),
    ];
    for (e, code) in all {
        assert_eq!(e.msg().code, code);
        assert_eq!(e.to_string(), e.msg().text);
    }
    assert_eq!(
        PackError::TooLarge { limit: MAX_BYTES }.msg().arg("limit"),
        "16"
    );
}

#[test]
fn versions_compare_as_numbers() {
    assert!(!newer_than_us(CORE_VERSION));
    assert!(newer_than_us("999.0.0"));
    assert!(!newer_than_us("0.0.1"));
    assert!(!newer_than_us("not a version"));
    assert_eq!(parse_version("0.77.0-rc.1"), Some((0, 77, 0)));
    assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
}

#[test]
fn origins_keep_their_words() {
    for o in [
        PackOrigin::Export,
        PackOrigin::Share,
        PackOrigin::Ui,
        PackOrigin::Cli,
        PackOrigin::External,
        PackOrigin::Defaults,
        PackOrigin::Sync,
        PackOrigin::Import,
        PackOrigin::Restore,
        PackOrigin::Repair,
    ] {
        assert_eq!(serde_json::to_value(o).unwrap(), o.slug());
        assert_eq!(PackOrigin::from_slug(o.slug()), o);
    }
    assert_eq!(PackOrigin::from_slug("nope"), PackOrigin::Other);
}

/// 要求加密的地方：给了钥匙就不收明文；加密的照常解；没给钥匙时和 `read` 一样
#[test]
fn a_strict_read_refuses_a_plain_pack_when_a_key_is_given() {
    fast();
    let pack = sample();
    let plain = write(&pack, Seal::None).unwrap();
    let id = age::x25519::Identity::generate();
    let sealed = write(&pack, Seal::Recipient(&id.to_public())).unwrap();
    assert_eq!(
        read_strict(&plain, Unseal::Identity(&id)).unwrap_err(),
        PackError::NotEncrypted
    );
    assert_eq!(
        read_strict(&plain, Unseal::Passphrase("whatever it is")).unwrap_err(),
        PackError::NotEncrypted
    );
    assert_eq!(PackError::NotEncrypted.msg().code, "pack.not_encrypted");
    assert_eq!(
        read_strict(&sealed, Unseal::Identity(&id))
            .unwrap()
            .config_yaml,
        pack.config_yaml
    );
    assert_eq!(
        read_strict(&plain, Unseal::None).unwrap().config_yaml,
        pack.config_yaml
    );
    // `read` 照收：导入的是用户自己挑的文件
    assert!(read(&plain, Unseal::Identity(&id)).is_ok());
}

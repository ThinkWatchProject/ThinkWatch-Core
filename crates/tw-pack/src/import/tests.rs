//! 导入：5.4 节的每一种状态，和改名、跳过、覆盖之后的样子。

use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::model::PluginCodec;

/// 整份比较、只有带 `steer` 的要批准
struct WholeFile;

impl PluginCodec for WholeFile {
    fn split(&self, _: &str) -> Option<(String, serde_json::Value)> {
        None
    }
    fn join(&self, source: &str, data: &serde_json::Value) -> Option<String> {
        data.is_null().then(|| source.to_string())
    }
    fn shipped(&self, _: &str) -> Option<String> {
        None
    }
    fn needs_approval(&self, source: &str) -> bool {
        source.contains("steer")
    }
}
use crate::{Manifest, PackKind, PackOrigin, PluginFiles};

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

fn local_text(extra: &str) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-local\n  - name: codex\n    key: tw-codex\nproviders:\n  - name: 公司中转\n    base_url: https://relay.example\n    key: sk-old\n  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-mine\n      endpoint: https://auth.openai.com/oauth/token\n    headers:\n      ChatGPT-Account-Id: acct-mine\n{extra}"
    )
}

fn pack_of(text: &str, secrets: bool) -> Pack {
    let mut manifest = Manifest::new(PackKind::Full, PackOrigin::Export);
    manifest.secrets = secrets;
    Pack {
        manifest,
        config_yaml: text.to_string(),
        plugins: BTreeMap::new(),
        client_prefs: None,
    }
}

const INCOMING: &str = r#"version: 1
clients:
  - name: default
    key: tw-local
  - name: shared
    key: tw-shared
providers:
  - name: 公司中转
    base_url: https://relay.example/v2
    key: sk-new
  - name: OpenRouter
    base_url: https://openrouter.ai/api/v1
    key: sk-or
  - name: chatgpt
    base_url: https://chatgpt.com/backend-api/codex
  - name: 新的 ChatGPT
    base_url: https://chatgpt.com/backend-api/codex
  - name: Bedrock 东京
    base_url: https://bedrock-runtime.ap-northeast-1.amazonaws.com
    aws: { profile: tokyo }
routes:
  - name: 走中转
    rules:
      - { name: all, to: 公司中转 }
  - name: 缺上游
    rules:
      - { name: all, to: 不存在 }
failover:
  pause_secs: 30
"#;

fn ctx() -> Context<'static> {
    Context {
        has_aws_profile: &|p| p == "default",
        new_key: &|| "tw-regenerated".to_string(),
        with_prefs: true,
    }
}

fn rows(local: &Pack, incoming: &Pack) -> BTreeMap<String, Row> {
    preview_with(local, incoming, &ctx(), &WholeFile)
        .unwrap()
        .into_iter()
        .map(|r| (r.item.to_string(), r))
        .collect()
}

#[test]
fn every_item_gets_its_status() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(INCOMING, true);
    let r = rows(&local, &incoming);
    let status = |k: &str| {
        r.get(k)
            .unwrap_or_else(|| panic!("no row {k}: {:?}", r.keys()))
            .status
    };
    assert_eq!(status("provider `OpenRouter`"), Status::New);
    assert_eq!(status("key `shared`"), Status::New);
    assert_eq!(status("key `default`"), Status::Same);
    assert_eq!(status("provider `公司中转`"), Status::Conflict);
    assert_eq!(r["provider `公司中转`"].differs, ["conn"]);
    assert_eq!(
        r["provider `公司中转`"].suggested_name.as_deref(),
        Some("公司中转 (2)")
    );
    // 本机的全局设置还是默认值：直接采用
    assert_eq!(status("global `failover`"), Status::New);
    assert!(r["global `failover`"].global);
    // 顺序项目不是一行
    assert!(!r.keys().any(|k| k.contains("order")), "{:?}", r.keys());
    // 引用了两边都没有的上游
    assert_eq!(status("route `缺上游`"), Status::Error);
    let e = r["route `缺上游`"].error.as_ref().unwrap();
    assert_eq!(e.code, "import.missing_reference");
    assert_eq!(e.arg("target"), "不存在");
    // 本机没有它要的 AWS profile
    assert_eq!(status("provider `Bedrock 东京`"), Status::Error);
    assert_eq!(
        r["provider `Bedrock 东京`"].error.as_ref().unwrap().code,
        "import.no_aws_profile"
    );
    // ChatGPT：本机没有同名的要登录；有同名、登录着的不用
    assert_eq!(r["provider `新的 ChatGPT`"].flags, [Flag::NeedsLogin]);
    assert_eq!(r["provider `chatgpt`"].status, Status::Same);
    // 引用：路由引用的、包里也有的上游
    assert_eq!(
        r["route `走中转`"]
            .refs
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>(),
        ["provider `公司中转`"]
    );
}

#[test]
fn a_changed_global_section_is_a_conflict_that_only_overwrites_or_skips() {
    let local = pack_of(&local_text("failover:\n  pause_secs: 120\n"), true);
    let r = rows(&local, &pack_of(INCOMING, true));
    let f = &r["global `failover`"];
    assert_eq!(f.status, Status::Conflict);
    assert!(f.global);
    assert_eq!(f.differs, ["failover.pause_secs"]);
    assert_eq!(f.suggested_name, None);
}

#[test]
fn a_pack_without_secrets_imports_disabled_upstreams_and_new_keys() {
    let local = pack_of(&local_text(""), true);
    let mut full = pack_of(INCOMING, true);
    full.config_yaml = full.config_yaml.replace("  - name: Bedrock 东京\n    base_url: https://bedrock-runtime.ap-northeast-1.amazonaws.com\n    aws: { profile: tokyo }\n", "");
    let exported = export::export_with(
        &full,
        export::Options {
            scope: None,
            secrets: false,
            client_prefs: None,
            origin: PackOrigin::Export,
        },
        &WholeFile,
    )
    .unwrap();
    let incoming = crate::read(
        &crate::write(&exported.pack, crate::Seal::None).unwrap(),
        crate::Unseal::None,
    )
    .unwrap();
    assert!(!incoming.manifest.secrets);
    assert!(
        !incoming.config_yaml.contains("sk-or"),
        "{}",
        incoming.config_yaml
    );
    assert!(
        !incoming.config_yaml.contains("tw-shared"),
        "{}",
        incoming.config_yaml
    );
    let r = rows(&local, &incoming);
    assert_eq!(r["provider `OpenRouter`"].flags, [Flag::MissingKey]);
    assert_eq!(r["provider `OpenRouter`"].status, Status::New);
    assert_eq!(r["key `shared`"].flags, [Flag::MissingKey]);
    let applied = apply_with(
        &local,
        &incoming,
        &[
            Decision {
                item: ItemId::new(ItemKind::Provider, "OpenRouter"),
                action: Action::Import,
            },
            Decision {
                item: ItemId::new(ItemKind::Key, "shared"),
                action: Action::Import,
            },
        ],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml).unwrap();
    let or = cfg
        .providers
        .iter()
        .find(|p| p.name == "OpenRouter")
        .unwrap();
    assert!(or.disabled, "缺密钥的上游导入为停用");
    assert_eq!(or.key.as_ref().map(|k| k.raw()), Some(""));
    let shared = cfg.clients.iter().find(|c| c.name == "shared").unwrap();
    assert_eq!(shared.key, "tw-regenerated");
}

#[test]
fn import_overwrite_rename_and_skip_land_as_chosen() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(INCOMING, true);
    let d = |kind, name: &str, action| Decision {
        item: ItemId::new(kind, name),
        action,
    };
    let applied = apply_with(
        &local,
        &incoming,
        &[
            d(ItemKind::Provider, "OpenRouter", Action::Import),
            d(
                ItemKind::Provider,
                "公司中转",
                Action::Rename("公司中转 (2)".into()),
            ),
            d(ItemKind::Route, "走中转", Action::Import),
            d(ItemKind::Provider, "新的 ChatGPT", Action::Import),
            d(ItemKind::Global, "failover", Action::Import),
            d(ItemKind::Key, "shared", Action::Skip),
        ],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", applied.pack.config_yaml));
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    // 本机的在前，新加的按包里的先后接在后面
    assert_eq!(
        names,
        [
            "公司中转",
            "chatgpt",
            "公司中转 (2)",
            "OpenRouter",
            "新的 ChatGPT"
        ]
    );
    // 本机那一家没动
    let mine = &cfg.providers[0];
    assert_eq!(mine.base_url, "https://relay.example");
    // 改名导入的那一家，包里引用它的路由跟着改
    let route = cfg.routes.iter().find(|r| r.name == "走中转").unwrap();
    assert_eq!(
        route.rules[0].to.as_ref().and_then(|t| t.name()),
        Some("公司中转 (2)")
    );
    // 没登录的 ChatGPT 账号上游停用着
    let gpt = cfg
        .providers
        .iter()
        .find(|p| p.name == "新的 ChatGPT")
        .unwrap();
    assert!(gpt.disabled && gpt.oauth.is_none());
    assert_eq!(cfg.failover.pause_secs, 30);
    assert!(!cfg.clients.iter().any(|c| c.name == "shared"));
    assert!(applied.changed_keys.is_empty());
    // 本机设置和登录都在
    assert!(applied.pack.config_yaml.contains(KEY));
    assert!(applied.pack.config_yaml.contains("rt-mine"));
}

#[test]
fn overwriting_keeps_the_local_login_and_reports_changed_keys() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(
        &INCOMING
            .replace("    key: tw-local\n", "    key: tw-local-new\n")
            .replace(
                "  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n",
                "  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    max_concurrent: 3\n",
            ),
        true,
    );
    let r = rows(&local, &incoming);
    assert_eq!(r["provider `chatgpt`"].status, Status::Conflict);
    assert_eq!(r["provider `chatgpt`"].flags, Vec::<Flag>::new());
    let applied = apply_with(
        &local,
        &incoming,
        &[
            Decision {
                item: ItemId::new(ItemKind::Provider, "chatgpt"),
                action: Action::Overwrite,
            },
            Decision {
                item: ItemId::new(ItemKind::Key, "default"),
                action: Action::Overwrite,
            },
        ],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml).unwrap();
    let gpt = cfg.providers.iter().find(|p| p.name == "chatgpt").unwrap();
    assert_eq!(gpt.max_concurrent, Some(3));
    assert!(!gpt.disabled);
    assert_eq!(gpt.oauth.as_ref().unwrap().refresh, "rt-mine");
    assert_eq!(applied.changed_keys, ["default"]);
}

#[test]
fn a_reference_to_a_skipped_item_is_refused() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(&INCOMING.replace("to: 公司中转", "to: OpenRouter"), true);
    let e = apply_with(
        &local,
        &incoming,
        &[
            Decision {
                item: ItemId::new(ItemKind::Route, "走中转"),
                action: Action::Import,
            },
            Decision {
                item: ItemId::new(ItemKind::Provider, "OpenRouter"),
                action: Action::Skip,
            },
        ],
        &ctx(),
        &WholeFile,
    )
    .unwrap_err();
    assert_eq!(e.msg().code, "import.skipped_reference");
    // 选「导入」一个本机已有、内容不同的：要选覆盖或改名
    let e = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::Provider, "公司中转"),
            action: Action::Import,
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap_err();
    assert_eq!(e.msg().code, "import.exists");
    // 改名改成一个被占的名字
    let e = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::Provider, "公司中转"),
            action: Action::Rename("chatgpt".into()),
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap_err();
    assert_eq!(e.msg().code, "import.name_taken");
    // 导入一个出错的那一行
    let e = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::Route, "缺上游"),
            action: Action::Import,
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap_err();
    assert_eq!(e.msg().code, "import.missing_reference");
}

#[test]
fn plugins_are_checked_against_their_approved_hash_and_arrive_disabled() {
    let script = b"export const manifest = {};\n".to_vec();
    let sha = model::sha256_hex(&script);
    let text = format!(
        "version: 1\nplugins:\n  - id: good\n    file: plugins/good.js\n    sha256: {sha}\n  - id: tampered\n    file: plugins/tampered.js\n    sha256: {sha}\n  - id: absent\n    file: plugins/absent.js\n    sha256: {sha}\n"
    );
    let mut incoming = pack_of(&text, true);
    incoming.plugins.insert(
        "good".into(),
        PluginFiles {
            script: script.clone(),
            approved: Some(script.clone()),
        },
    );
    incoming.plugins.insert(
        "tampered".into(),
        PluginFiles {
            script: b"something else".to_vec(),
            approved: None,
        },
    );
    let local = pack_of(&local_text(""), true);
    let r = rows(&local, &incoming);
    assert_eq!(r["plugin `good`"].status, Status::New);
    assert_eq!(r["plugin `good`"].flags, [Flag::PluginDisabled]);
    assert_eq!(
        r["plugin `tampered`"].error.as_ref().unwrap().code,
        "import.plugin_mismatch"
    );
    assert_eq!(
        r["plugin `absent`"].error.as_ref().unwrap().code,
        "import.plugin_missing"
    );
    let applied = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::Plugin, "good"),
            action: Action::Import,
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml).unwrap();
    assert_eq!(cfg.plugins.len(), 1);
    assert!(!cfg.plugins[0].enabled);
    assert_eq!(cfg.plugins[0].sha256, sha);
    assert_eq!(applied.pack.plugins["good"].script, script);

    // 改得了工具调用的插件：停用着进来，也不批准 —— 要在插件页上点头
    let steer = b"export const manifest = {}; // steer\n".to_vec();
    let sha = model::sha256_hex(&steer);
    let mut incoming = pack_of(
        &format!("version: 1\nplugins:\n  - id: s\n    file: plugins/s.js\n    sha256: {sha}\n"),
        true,
    );
    incoming.plugins.insert(
        "s".into(),
        PluginFiles {
            script: steer.clone(),
            approved: Some(steer),
        },
    );
    let applied = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::Plugin, "s"),
            action: Action::Import,
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml).unwrap();
    assert_ne!(cfg.plugins[0].sha256, sha);
    assert!(!cfg.plugins[0].enabled);
}

#[test]
fn a_key_with_the_value_of_another_key_here_is_an_error() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(
        "version: 1\nclients:\n  - name: copy\n    key: tw-codex\n",
        true,
    );
    let r = rows(&local, &incoming);
    assert_eq!(
        r["key `copy`"].error.as_ref().unwrap().code,
        "import.key_value_taken"
    );
}

/// 导进来过不了校验的：那一行就是错误，带着校验的那句话
#[test]
fn an_item_that_would_not_validate_is_an_error_with_the_reason() {
    let local = pack_of(&local_text(""), true);
    let incoming = pack_of(
        "version: 1\nproviders:\n  - name: bad\n    base_url: ftp://nope\n",
        true,
    );
    let r = rows(&local, &incoming);
    assert_eq!(r["provider `bad`"].status, Status::Error);
    assert_eq!(
        r["provider `bad`"].error.as_ref().unwrap().code,
        "config.bad_base_url"
    );
}

#[test]
fn client_preferences_import_only_when_core_keeps_them() {
    let local = pack_of(&local_text(""), true);
    let mut incoming = pack_of("version: 1\n", true);
    incoming.client_prefs = Some(json!({"language": "en"}));
    let r = rows(&local, &incoming);
    assert_eq!(r["client_prefs"].status, Status::New);
    let remote = Context {
        with_prefs: false,
        ..ctx()
    };
    let r = preview_with(&local, &incoming, &remote, &WholeFile).unwrap();
    assert!(r.is_empty(), "{r:?}");
    let applied = apply_with(
        &local,
        &incoming,
        &[Decision {
            item: ItemId::new(ItemKind::ClientPrefs, ""),
            action: Action::Import,
        }],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    assert_eq!(applied.pack.client_prefs, Some(json!({"language": "en"})));
}

/// 第一行是 `// data: <JSON>` 的插件源码：数据单独拆出来（同 `model` 的测试）
struct DataLine;

impl PluginCodec for DataLine {
    fn split(&self, source: &str) -> Option<(String, serde_json::Value)> {
        let (first, rest) = source.split_once('\n')?;
        let data = serde_json::from_str(first.strip_prefix("// data: ")?).ok()?;
        Some((format!("// data: null\n{rest}"), data))
    }
    fn join(&self, source: &str, data: &serde_json::Value) -> Option<String> {
        let (_, rest) = source.split_once('\n')?;
        Some(format!("// data: {data}\n{rest}"))
    }
    fn shipped(&self, _: &str) -> Option<String> {
        None
    }
    fn needs_approval(&self, _: &str) -> bool {
        false
    }
}

/// OAuth 上游的令牌不随包带：新来的停用着、标成缺密钥；覆盖本机同一个令牌端点的留着本机的令牌
#[test]
fn an_oauth_upstream_arrives_without_its_tokens() {
    let oauth = "  - name: corp\n    base_url: https://corp.example\n    oauth:\n      refresh: rt-theirs\n      endpoint: https://auth.example/token\n";
    let incoming = pack_of(
        &format!(
            "version: 1\nproviders:\n{oauth}  - name: corp2\n    base_url: https://corp2.example\n    oauth:\n      refresh: rt-2\n      endpoint: https://auth.example/token\n    max_concurrent: 2\n"
        ),
        true,
    );
    let local = pack_of(
        &local_text("").replace(
            "providers:\n",
            "providers:\n  - name: corp2\n    base_url: https://corp2.example\n    oauth:\n      refresh: rt-mine\n      endpoint: https://auth.example/token\n",
        ),
        true,
    );
    let r = rows(&local, &incoming);
    assert_eq!(r["provider `corp`"].flags, [Flag::MissingKey]);
    assert_eq!(r["provider `corp2`"].flags, Vec::<Flag>::new());
    assert_eq!(r["provider `corp2`"].status, Status::Conflict);
    let applied = apply_with(
        &local,
        &incoming,
        &[
            Decision {
                item: ItemId::new(ItemKind::Provider, "corp"),
                action: Action::Import,
            },
            Decision {
                item: ItemId::new(ItemKind::Provider, "corp2"),
                action: Action::Overwrite,
            },
        ],
        &ctx(),
        &WholeFile,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&applied.pack.config_yaml).unwrap();
    let p = |n: &str| cfg.providers.iter().find(|p| p.name == n).unwrap();
    assert!(p("corp").disabled);
    assert_eq!(p("corp").oauth.as_ref().unwrap().refresh, "");
    assert!(!applied.pack.config_yaml.contains("rt-theirs"));
    assert_eq!(p("corp2").oauth.as_ref().unwrap().refresh, "rt-mine");
    assert_eq!(p("corp2").max_concurrent, Some(2));
    assert!(!p("corp2").disabled);
}

/// 插件范围里写明的上游：跳过它，插件就导入不了；本机和包里都没有的不碍事
#[test]
fn skipping_an_upstream_a_plugin_names_refuses_the_plugin() {
    let src = "// data: {\"match\":{\"upstreams\":[\"relay\",\"nowhere\"]}}\nscoped\n";
    let sha = model::sha256_hex(src.as_bytes());
    let mut incoming = pack_of(
        &format!(
            "version: 1\nproviders:\n  - name: relay\n    base_url: https://relay.example\nplugins:\n  - id: scoped\n    file: plugins/scoped.js\n    sha256: {sha}\n"
        ),
        true,
    );
    incoming.plugins.insert(
        "scoped".into(),
        PluginFiles {
            script: src.as_bytes().to_vec(),
            approved: Some(src.as_bytes().to_vec()),
        },
    );
    let local = pack_of(&local_text(""), true);
    let r: BTreeMap<String, Row> = preview_with(&local, &incoming, &ctx(), &DataLine)
        .unwrap()
        .into_iter()
        .map(|r| (r.item.to_string(), r))
        .collect();
    let plugin = &r["plugin `scoped`"];
    assert_eq!(plugin.status, Status::New, "{plugin:?}");
    assert_eq!(
        plugin
            .refs
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>(),
        ["provider `relay`"]
    );
    let d = |kind, name: &str, action| Decision {
        item: ItemId::new(kind, name),
        action,
    };
    let e = apply_with(
        &local,
        &incoming,
        &[
            d(ItemKind::Plugin, "scoped", Action::Import),
            d(ItemKind::Provider, "relay", Action::Skip),
        ],
        &ctx(),
        &DataLine,
    )
    .unwrap_err();
    assert_eq!(e.msg().code, "import.skipped_reference");
    // 一起导入就成；改名导入的上游，插件的范围跟着改
    let applied = apply_with(
        &local,
        &incoming,
        &[
            d(ItemKind::Plugin, "scoped", Action::Import),
            d(
                ItemKind::Provider,
                "relay",
                Action::Rename("relay-cn".into()),
            ),
        ],
        &ctx(),
        &DataLine,
    )
    .unwrap();
    let script = String::from_utf8(applied.pack.plugins["scoped"].script.clone()).unwrap();
    assert!(script.contains("relay-cn"), "{script}");
}

/// 较旧版本导出的、有这一版不认识的写法的包：修好再看；更新的版本写的说太新
#[test]
fn an_older_pack_is_repaired_and_a_newer_one_is_too_new() {
    let text =
        "version: 1\nproviders:\n  - name: a\n    base_url: https://a.example\n    retired: 1\n";
    let mut old = pack_of(text, true);
    old.manifest.core_version = "0.1.0".into();
    let (fixed, fixes) = prepare(old.clone()).unwrap();
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0].field, "providers[0].retired");
    assert!(!fixed.config_yaml.contains("retired"));
    let r = rows(&pack_of(&local_text(""), true), &fixed);
    assert_eq!(r["provider `a`"].status, Status::New);
    // 读得进来的不动
    let (same, none) = prepare(pack_of("version: 1\n", true)).unwrap();
    assert!(none.is_empty());
    assert_eq!(same.config_yaml, "version: 1\n");
    // 更新的 Core 写的
    let mut new = old.clone();
    new.manifest.core_version = "999.0.0".into();
    assert_eq!(prepare(new).unwrap_err().msg().code, "pack.too_new");
    // 修不了的照旧是读不进来的那句话
    let broken = pack_of("version: 1\nproviders: [\n", true);
    assert!(prepare(broken).is_err());
}

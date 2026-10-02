//! `PATCH /config` 写进去的字。
//!
//! 控制字符、YAML 1.1 当换行的那几个（LS、PS、NEL）、C1、BOM 和两个非字符：写成转义过的
//! 双引号，配置读回来一字不差，只有那一行变了。以前它们原样写进文件，整份配置被拒，报的是
//! 一个说不清的语法错误（或者值悄悄变了样：双引号里的 NEL 读回来是空格）。
//!
//! 换行只进得了能写多行的字段（插件的设置），和按名字改一项是同一份规矩；别处拒绝，
//! 说的是 `config.edit.multiline`，文件不动。

use std::sync::Arc;

use tw_config::history::Origin;
use tw_control::ConfigManager;

const HASH: &str = "6f1c000000000000000000000000000000000000000000000000000000000abc";

fn cfg() -> String {
    format!(
        "version: 1
listen:
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00
clients:
  # 默认那把
  - name: default
    key: tw-aaaa
providers:
  - name: plain
    base_url: https://a.example.com
    key: sk-plain   # 行尾注释
  - name: single
    base_url: https://b.example.com
    key: 'sk-single'
  - name: double
    base_url: https://c.example.com
    key: \"sk-double\"
plugins:
  - id: terms
    file: plugins/terms.js
    sha256: {HASH}
    enabled: false
    settings:
      terms: old
"
    )
}

fn setup() -> (tempfile::TempDir, Arc<ConfigManager>) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, cfg()).unwrap();
    let c: tw_config::Config = serde_yaml_ng::from_str(&cfg()).unwrap();
    let gw = tw_gateway::AppState::new(c).unwrap();
    let bus = gw.bus.clone();
    (d, Arc::new(ConfigManager::new(p, gw, bus)))
}

fn replace(path: &str, value: &str) -> Vec<tw_api::PatchOp> {
    vec![tw_api::PatchOp::Replace {
        path: path.into(),
        value: tw_api::PatchValue::Str(value.into()),
    }]
}

/// 要转义的那些，每种写法（纯量、单引号、双引号、还没写过的键）都试
const ESCAPED: &[&str] = &[
    "a\u{2028}b",
    "a\u{2029}b",
    "a\u{85}b",
    "a\u{9b}b",
    "\u{feff}a",
    "a\u{fffe}\u{ffff}",
    "a\tb",
    "a\u{0}b\u{1b}",
    "a\u{7f}b",
    "\u{2029}x\u{2028}",
];

#[tokio::test]
async fn control_characters_and_line_separators_go_in_escaped_and_read_back_exactly() {
    let before = cfg();
    for s in ESCAPED {
        for (path, line) in [
            (
                "/providers/plain/key",
                Some("    key: sk-plain   # 行尾注释"),
            ),
            ("/providers/single/key", Some("    key: 'sk-single'")),
            ("/providers/double/key", Some("    key: \"sk-double\"")),
            // 还没写过的键：新加一行
            ("/clients/default/client", None),
        ] {
            let (_d, mgr) = setup();
            mgr.patch(&replace(path, s), None, Origin::Ui)
                .await
                .unwrap_or_else(|e| panic!("{path} {s:?}: {e}"));
            let after = std::fs::read_to_string(mgr.path()).unwrap();
            assert!(
                !after.chars().any(|c| c != '\n' && tw_yaml::must_escape(c)),
                "{path} {s:?} was written raw: {after:?}"
            );
            let c = tw_config::try_parse(&after)
                .unwrap_or_else(|r| panic!("{path} {s:?}: the configuration does not load: {r}"));
            let got = match path {
                "/providers/plain/key" => c.providers[0].key.as_ref().map(|k| k.raw()),
                "/providers/single/key" => c.providers[1].key.as_ref().map(|k| k.raw()),
                "/providers/double/key" => c.providers[2].key.as_ref().map(|k| k.raw()),
                _ => c.clients[0].client.as_deref(),
            };
            assert_eq!(got, Some(*s), "{path}: read back something else\n{after}");

            let old: Vec<&str> = before.lines().collect();
            let new: Vec<&str> = after.lines().collect();
            match line {
                // 只有那一行变了，尾注释还在
                Some(line) => {
                    assert_eq!(old.len(), new.len(), "{path} {s:?}\n{after}");
                    let changed: Vec<&str> = old
                        .iter()
                        .zip(&new)
                        .filter(|(a, b)| a != b)
                        .map(|(a, _)| *a)
                        .collect();
                    assert_eq!(changed, [line], "{path} {s:?}\n{after}");
                    if line.contains('#') {
                        assert!(after.contains("\"   # 行尾注释\n"), "{after}");
                    }
                }
                // 多了一行，原来的每一行都在
                None => {
                    assert_eq!(old.len() + 1, new.len(), "{path} {s:?}\n{after}");
                    let mut rest = new.iter();
                    assert!(
                        old.iter().all(|l| rest.any(|n| n == l)),
                        "{path} {s:?}: a line was changed\n{after}"
                    );
                }
            }
        }
    }
}

/// 单行的字段：换行（`\n`、`\r`）拒绝，说的是同一句话，文件不动
#[tokio::test]
async fn a_line_break_is_refused_in_a_single_line_field() {
    for s in ["a\nb", "a\rb", "a\r\nb", "\n"] {
        for path in [
            "/providers/plain/key",
            "/providers/single/key",
            "/clients/default/name",
            "/clients/default/client",
            // 插件按 `id` 认，路径里写下标
            "/plugins/0/file",
        ] {
            let (_d, mgr) = setup();
            let e = mgr
                .patch(&replace(path, s), None, Origin::Ui)
                .await
                .unwrap_err();
            assert_eq!(e.msg().code, "config.edit.multiline", "{path} {s:?}: {e}");
            assert_eq!(
                std::fs::read_to_string(mgr.path()).unwrap(),
                cfg(),
                "{path} {s:?}"
            );
        }
    }
}

/// 插件的设置能写多行（「一行一条」的对照表）：写得进去，读回来一字不差
#[tokio::test]
async fn a_plugin_setting_takes_line_breaks() {
    for s in ["登陆=登录\n帐号=账号", "a=b\r\nc=d\n", "x\u{2028}y\nz"] {
        let (_d, mgr) = setup();
        mgr.patch(&replace("/plugins/0/settings/terms", s), None, Origin::Ui)
            .await
            .unwrap_or_else(|e| panic!("{s:?}: {e}"));
        let after = std::fs::read_to_string(mgr.path()).unwrap();
        let c = tw_config::try_parse(&after).unwrap_or_else(|r| panic!("{s:?}: {r}"));
        assert_eq!(
            c.plugins[0].settings["terms"].as_str(),
            Some(s),
            "{s:?}\n{after}"
        );
        assert_eq!(
            after.lines().count(),
            cfg().lines().count(),
            "{s:?}: the value spans lines in the file\n{after}"
        );
    }
}

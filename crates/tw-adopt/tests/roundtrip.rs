//! 接管一次，再还原一次，用户的文件必须**一个字节都没变**。
//!
//! 这是整个 tw-adopt 唯一真正要证明的事。cc-switch 那批最集中的 issue
//! （#6902 把 23 个顶层键写剩 1 个、#6901 擦掉第三方 hooks、#6871 删掉
//! `enabledPlugins`）全都可以被这一条测出来。

use std::path::{Path, PathBuf};

use tw_adopt::clients::{Gateway, adoptable};
use tw_adopt::plan::{apply, apply_restore, plan_adopt, plan_restore};

fn gw() -> Gateway {
    Gateway {
        base: "http://127.0.0.1:8080".into(),
        key: Some("tw-用户的专属密钥".into()),
    }
}

struct Bed {
    _dir: tempfile::TempDir,
    home: PathBuf,
    backups: PathBuf,
}

fn bed(client: &str, contents: &str) -> Bed {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let c = adoptable().into_iter().find(|c| c.id == client).unwrap();
    let p = c.config_path(&home);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    if !contents.is_empty() {
        std::fs::write(&p, contents).unwrap();
    }
    Bed {
        backups: dir.path().join("backups"),
        home,
        _dir: dir,
    }
}

fn client(id: &str) -> tw_adopt::clients::Client {
    adoptable().into_iter().find(|c| c.id == id).unwrap()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

// ---- Claude Code：JSON，装不下注释，靠旁文件 --------------------------

/// 一份**有生活痕迹**的 settings.json。测试用的假数据太干净的话，
/// 「其余字节原样不动」这句话就没被真正考验过。
const CLAUDE: &str = r#"{
  "model": "opusplan",
  "permissions": {
    "allow": ["Bash(git diff:*)", "Read(~/Dev/**)"],
    "deny": ["Bash(rm -rf:*)"]
  },
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit", "hooks": [{ "type": "command", "command": "prettier -w $FILE" }] }
    ]
  },
  "enabledPlugins": { "my-plugin@local": true },
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "sk-ant-我自己的密钥",
    "MY_OWN_VAR": "别动我"
  },
  "statusLine": { "type": "command", "command": "~/bin/statusline.sh" }
}
"#;

#[test]
fn adopting_claude_code_touches_only_the_fields_we_named() {
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();

    let after = read(&b.home.join(".claude/settings.json"));
    for keep in [
        "\"model\": \"opusplan\"",
        "Bash(git diff:*)",
        "prettier -w $FILE",
        "my-plugin@local",
        "\"MY_OWN_VAR\": \"别动我\"",
        "~/bin/statusline.sh",
    ] {
        assert!(after.contains(keep), "接管把 {keep} 弄丢了：\n{after}");
    }
    assert!(
        after.contains("\"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8080\""),
        "{after}"
    );
    assert!(after.contains("tw-用户的专属密钥"), "{after}");
    assert!(
        after.contains("\"CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY\": \"1\""),
        "{after}"
    );
    assert!(!after.contains("sk-ant-我自己的密钥"), "{after}");
}

#[test]
fn restoring_claude_code_puts_the_file_back_byte_for_byte() {
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();

    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();

    assert_eq!(
        read(&b.home.join(".claude/settings.json")),
        CLAUDE,
        "还原之后不是原来那份文件"
    );
    assert!(
        !b.home
            .join(".claude/settings.json.thinkwatch.json")
            .exists(),
        "还原之后还留着接管记录"
    );
}

#[test]
fn the_sidecar_never_holds_the_users_own_key() {
    // 旁文件是我们新造的一份文件，而那个目录常常被 dotfile 管理器
    // 提交进 git。原值是密钥的，只留一个指向全文备份的指针。
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let side = read(&b.home.join(".claude/settings.json.thinkwatch.json"));
    assert!(
        !side.contains("sk-ant-我自己的密钥"),
        "用户的密钥被抄进旁文件了：\n{side}"
    );
    assert!(side.contains("secret"), "{side}");
    assert!(side.contains("full_backup"), "{side}");
}

#[test]
fn a_secret_original_comes_back_from_the_full_backup() {
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert!(read(&b.home.join(".claude/settings.json")).contains("sk-ant-我自己的密钥"));
}

#[test]
fn losing_the_backup_removes_our_key_and_says_so_out_loud() {
    // **不能假装还原成功。**留着我们的密钥比删掉更糟 —— 那等于卸载
    // 之后还在替他发请求。
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    std::fs::remove_dir_all(&b.backups).unwrap();

    let r = plan_restore(&c, &b.home).unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.text.contains("filled in again by hand")),
        "没说清密钥拿不回来了：{:?}",
        r.notes
    );
    apply_restore(&c, &r, &b.backups).unwrap();
    let after = read(&b.home.join(".claude/settings.json"));
    assert!(
        !after.contains("tw-用户的专属密钥"),
        "我们的密钥还留在里面：{after}"
    );
    assert!(after.contains("\"MY_OWN_VAR\": \"别动我\""), "{after}");
}

#[test]
fn adopting_a_client_with_no_config_file_creates_one_and_restore_takes_it_away() {
    let b = bed("claude-code", "");
    let c = client("claude-code");
    let path = b.home.join(".claude/settings.json");
    assert!(!path.exists());

    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    assert!(path.exists());

    let r = plan_restore(&c, &b.home).unwrap();
    assert!(r.delete_file, "当初是我们建的，还原时该整个删掉");
    apply_restore(&c, &r, &b.backups).unwrap();
    assert!(!path.exists(), "还原之后留下了一个用户从来没有过的文件");
}

#[test]
fn a_file_the_user_has_since_written_into_is_never_deleted_by_a_restore() {
    // 文件是我们建的，但用户这三个月往里加了自己的东西 —— 那些必须留下。
    let b = bed("claude-code", "");
    let c = client("claude-code");
    let path = b.home.join(".claude/settings.json");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();

    let mine =
        tw_adopt::json::set(&read(&path), &["model"], &tw_adopt::json::Val::s("opus")).unwrap();
    std::fs::write(&path, mine).unwrap();

    let r = plan_restore(&c, &b.home).unwrap();
    assert!(!r.delete_file, "用户往里写过东西，不该删");
    apply_restore(&c, &r, &b.backups).unwrap();
    let after = read(&path);
    assert!(
        after.contains("\"model\": \"opus\""),
        "把用户后来加的东西抹掉了：{after}"
    );
    assert!(!after.contains("127.0.0.1"), "{after}");
}

// ---- Codex：TOML，有注释，几十条项目授权 -------------------------------

const CODEX: &str = r#"model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"

# 这台机器上每个仓库的信任记录，丢一条都要重新点一次
[projects."/path/to/my-app"]
trust_level = "trusted"

[projects."/path/to/another-app"]
trust_level = "trusted"

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/workspace"]
"#;

#[test]
fn adopting_codex_keeps_every_project_trust_and_mcp_server() {
    let b = bed("codex", CODEX);
    let c = client("codex");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();

    let after = read(&b.home.join(".codex/config.toml"));
    assert!(after.contains(r#"[projects."/path/to/my-app"]"#), "{after}");
    assert!(
        after.contains(r#"[projects."/path/to/another-app"]"#),
        "{after}"
    );
    assert!(after.contains("[mcp_servers.filesystem]"), "{after}");
    assert!(after.contains("丢一条都要重新点一次"), "注释没了：{after}");
    assert!(
        after.contains(r#"model_provider = "thinkwatch""#),
        "{after}"
    );
    assert!(after.contains(r#"wire_api = "responses""#), "{after}");
    assert!(after.contains("X-ThinkWatch-Client"), "{after}");
    // 实测：不配 env_key 它也照发请求；env_key 指向没 export 的变量反而起不来
    assert!(!after.contains("env_key"), "不该写 env_key：{after}");
}

#[test]
fn codex_gets_a_sentinel_comment_because_toml_can_hold_one() {
    // 用户打开文件就该看见「这是谁改的、原来是什么」。
    let b = bed("codex", CODEX);
    let c = client("codex");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let after = read(&b.home.join(".codex/config.toml"));
    assert!(after.contains(tw_adopt::sentinel::BEGIN), "{after}");
    assert!(after.contains("to restore by hand"), "{after}");
    assert!(after.contains("no model_provider originally"), "{after}");
}

#[test]
fn restoring_codex_puts_the_file_back_byte_for_byte() {
    let b = bed("codex", CODEX);
    let c = client("codex");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert_eq!(read(&b.home.join(".codex/config.toml")), CODEX);
}

#[test]
fn adopting_twice_does_not_stack_up_sentinels() {
    let b = bed("codex", CODEX);
    let c = client("codex");
    for _ in 0..3 {
        let p = plan_adopt(&c, &b.home, &gw()).unwrap();
        apply(&c, &p, &b.backups).unwrap();
    }
    let after = read(&b.home.join(".codex/config.toml"));
    assert_eq!(
        after.matches(tw_adopt::sentinel::BEGIN).count(),
        1,
        "{after}"
    );
    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert_eq!(read(&b.home.join(".codex/config.toml")), CODEX);
}

// ---- Aider：YAML ------------------------------------------------------

const AIDER: &str = "# 我的 aider 配置\nmodel: gpt-4o\ndark-mode: true\nauto-commits: false\n";

#[test]
fn aider_round_trips_too() {
    let b = bed("aider", AIDER);
    let c = client("aider");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let after = read(&b.home.join(".aider.conf.yml"));
    assert!(after.contains("# 我的 aider 配置"), "{after}");
    assert!(after.contains("auto-commits: false"), "{after}");
    assert!(after.contains("127.0.0.1:8080/v1"), "{after}");

    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert_eq!(read(&b.home.join(".aider.conf.yml")), AIDER);
}

// ---- 跨格式的规矩 ------------------------------------------------------

#[test]
fn every_adoptable_client_survives_a_round_trip() {
    // 每加一个客户端，这条就自动覆盖到它 —— 不用记得去补测试。
    for c in adoptable() {
        let seed = match c.format {
            tw_adopt::clients::Format::Json => "{\n  \"我自己的\": \"别动\"\n}\n",
            tw_adopt::clients::Format::Toml => "\"我自己的\" = \"别动\"\n",
            tw_adopt::clients::Format::Yaml => "我自己的: 别动\n",
        };
        let b = bed(c.id, seed);
        let p = plan_adopt(&c, &b.home, &gw()).unwrap();
        apply(&c, &p, &b.backups).unwrap();
        let after = read(&c.config_path(&b.home));
        assert!(
            after.contains("别动"),
            "{} 把用户原有的字段弄丢了：\n{after}",
            c.id
        );

        let r = plan_restore(&c, &b.home).unwrap();
        apply_restore(&c, &r, &b.backups).unwrap();
        assert_eq!(
            read(&c.config_path(&b.home)),
            seed,
            "{} 还原之后对不上",
            c.id
        );
    }
}

#[test]
fn a_gateway_without_a_key_writes_no_key_field() {
    // 为「一个 key 就够」的人设计。网关不要求鉴权时，不该往
    // 用户的配置里塞一个空密钥。
    let g = Gateway {
        base: "http://127.0.0.1:8080".into(),
        key: None,
    };
    for c in adoptable() {
        let seed = match c.format {
            tw_adopt::clients::Format::Json => "{}\n",
            _ => "",
        };
        let b = bed(c.id, seed);
        let p = plan_adopt(&c, &b.home, &g).unwrap();
        assert!(!p.carries_secret, "{} 说自己要写密钥，可是没有密钥", c.id);
        apply(&c, &p, &b.backups).unwrap();
        let after = read(&c.config_path(&b.home));
        assert!(
            !after.contains("\"apiKey\": \"\""),
            "{} 写了一个空密钥：{after}",
            c.id
        );
        assert!(
            !after.contains("_token = \"\""),
            "{} 写了一个空密钥：{after}",
            c.id
        );
    }
}

#[test]
fn a_plan_is_just_a_plan_until_it_is_applied() {
    // 「算改动」和「落盘」分成两步，是因为中间必须夹一次人的确认。
    let b = bed("codex", CODEX);
    let c = client("codex");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    assert_ne!(p.before.as_deref(), Some(p.after.as_str()));
    assert_eq!(
        read(&b.home.join(".codex/config.toml")),
        CODEX,
        "只是算了一下就把文件改了"
    );
    assert!(!p.notes.is_empty(), "接管的代价一条都没说");
}

#[test]
fn adopting_the_same_thing_twice_is_recognised_as_a_no_op() {
    let b = bed("codex", CODEX);
    let c = client("codex");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    apply(&c, &p, &b.backups).unwrap();
    let again = plan_adopt(&c, &b.home, &gw()).unwrap();
    assert!(again.is_noop(), "第二次接管应该是空操作");
}

#[test]
fn a_restore_without_a_record_refuses_instead_of_guessing() {
    // 没有接管记录就不知道该还原成什么。**猜一个「合理的默认值」
    // 写回去，是这里面最坏的做法。**
    let b = bed("codex", CODEX);
    let c = client("codex");
    let e = plan_restore(&c, &b.home).unwrap_err();
    assert!(e.to_string().contains("there is no record"), "{e}");
    assert_eq!(read(&b.home.join(".codex/config.toml")), CODEX);
}

#[test]
fn the_shadow_file_that_bit_cc_switch_is_reported_at_plan_time() {
    // cc-switch #6828：我们写了 settings.json，而 settings.local.json
    // 里的残留把它遮住了 —— 用户看到的是「接管了但没生效」。
    let b = bed("claude-code", CLAUDE);
    std::fs::write(
        b.home.join(".claude/settings.local.json"),
        "{\"env\": {\"ANTHROPIC_BASE_URL\": \"https://别的地方\"}}\n",
    )
    .unwrap();
    let c = client("claude-code");
    let p = plan_adopt(&c, &b.home, &gw()).unwrap();
    assert_eq!(p.shadows.len(), 1, "{:?}", p.shadows);
    assert!(
        p.notes
            .iter()
            .any(|n| n.text.contains("settings.local.json")),
        "没提醒它会盖住我们：{:?}",
        p.notes
    );
}

#[test]
fn re_adopting_does_not_overwrite_what_the_original_was() {
    // **最隐蔽的一种数据丢失：每一步看起来都成功了。**第二次接管时，
    // 文件里的值已经是我们写的了；照着现状记一遍「原值」，之后的还原
    // 就会把用户还原到我们这儿，而不是还原回他原来的样子。
    let b = bed("claude-code", CLAUDE);
    let c = client("claude-code");
    for base in ["http://127.0.0.1:8080", "http://127.0.0.1:9999"] {
        let g = Gateway {
            base: base.into(),
            key: Some("tw-新密钥".into()),
        };
        let p = plan_adopt(&c, &b.home, &g).unwrap();
        apply(&c, &p, &b.backups).unwrap();
    }
    let side = read(&b.home.join(".claude/settings.json.thinkwatch.json"));
    assert!(
        !side.contains("127.0.0.1"),
        "把我们自己写的地址记成了「原值」：\n{side}"
    );

    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert_eq!(
        read(&b.home.join(".claude/settings.json")),
        CLAUDE,
        "还原回到的是我们自己写的那一版"
    );
}

#[test]
fn re_adopting_codex_also_keeps_the_first_record() {
    let b = bed("codex", CODEX);
    let c = client("codex");
    for base in ["http://127.0.0.1:8080", "http://127.0.0.1:9999"] {
        let g = Gateway {
            base: base.into(),
            key: None,
        };
        let p = plan_adopt(&c, &b.home, &g).unwrap();
        apply(&c, &p, &b.backups).unwrap();
    }
    let after = read(&b.home.join(".codex/config.toml"));
    assert!(
        after.contains("no model_provider originally"),
        "哨兵注释记成了我们自己的值：{after}"
    );
    let r = plan_restore(&c, &b.home).unwrap();
    apply_restore(&c, &r, &b.backups).unwrap();
    assert_eq!(read(&b.home.join(".codex/config.toml")), CODEX);
}

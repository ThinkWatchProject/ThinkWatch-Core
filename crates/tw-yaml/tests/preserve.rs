//! 格式保留的经典翻车点，一项一项来：
//! 行内与行尾注释、锚点与别名、多行标量、流式与块式混排、中文、CRLF、
//! tab 缩进、极长的行。
//!
//! **断言不是「解析回来语义对」，而是「除了目标那一段，字节逐个相同」。**
//! 前者一个 round-trip 序列化器也能通过 —— 而那正是我们不敢用它的原因。

#[allow(unused_imports)]
use tw_yaml::{PatchError, Scalar, Step, path, set};

/// 改一处之后，除了那一处，原文一个字节都没变。
fn only_changed(before: &str, after: &str, from: &str, to: &str) {
    let idx = before
        .find(from)
        .unwrap_or_else(|| panic!("原文里没有 {from:?}"));
    let expect = format!("{}{}{}", &before[..idx], to, &before[idx + from.len()..]);
    assert_eq!(after, expect, "改动溢出了目标那一段");
}

const REAL: &str = r#"# ThinkWatch 配置
version: 1

listen:
  gateway:
    port: 8788        # 换端口记得同步改客户端

clients:
  - name: claude-code
    key: tw-abc123

providers:
  # 这家走代理
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-ant-xxx
    proxy: 代理一

  - name: 中转
    base_url: 'https://relay.example.com'   # 尾注释
    key: sk-relay
"#;

#[test]
fn a_comment_on_the_same_line_survives() {
    // 尾注释和值在同一行。**span 只要多吃一个字节，注释就没了。**
    let out = set(
        REAL,
        &path!["providers", 1usize, "base_url"],
        &Scalar::s("https://relay2.example.com"),
    )
    .unwrap();
    only_changed(
        REAL,
        &out,
        "'https://relay.example.com'",
        "'https://relay2.example.com'",
    );
    assert!(out.contains("# 尾注释"), "尾注释被吃掉了");
}

#[test]
fn every_other_comment_and_blank_line_survives() {
    let out = set(
        REAL,
        &path!["listen", "gateway", "port"],
        &Scalar::Int(9999),
    )
    .unwrap();
    only_changed(REAL, &out, "8788", "9999");
    for keep in [
        "# ThinkWatch 配置",
        "# 换端口记得同步改客户端",
        "# 这家走代理",
        "# 尾注释",
    ] {
        assert!(out.contains(keep), "丢了 {keep}");
    }
    assert_eq!(
        REAL.matches("\n\n").count(),
        out.matches("\n\n").count(),
        "空行被动过"
    );
}

#[test]
fn chinese_keys_and_values_do_not_shift_the_span() {
    // **saphyr 的 marker 是按 char 数的。**直接拿它当字节下标，在中文
    // 配置上会切到一个字符中间 —— 那是 cc-switch 栽过两次的
    // 那个 panic。
    let out = set(
        REAL,
        &path!["providers", 0usize, "name"],
        &Scalar::s("官方直连"),
    )
    .unwrap();
    only_changed(REAL, &out, "官方\n", "官方直连\n");
    assert!(out.contains("# 换端口记得同步改客户端"));
    assert!(out.contains("proxy: 代理一"));
}

#[test]
fn a_value_after_a_long_chinese_prefix_is_still_found_correctly() {
    // 目标越靠后，char/byte 的偏差累积得越多。这条专门盯住那个累积。
    let y = "a: 一二三四五六七八九十\nb: 甲乙丙丁\nc: target\n";
    let out = set(y, &path!["c"], &Scalar::s("changed")).unwrap();
    assert_eq!(out, "a: 一二三四五六七八九十\nb: 甲乙丙丁\nc: changed\n");
}

#[test]
fn crlf_line_endings_are_not_normalised() {
    // 换行符被统一掉的话，整个文件在 git 里会显示成全文改动。
    let y = "version: 1\r\nlisten:\r\n  gateway:\r\n    port: 8788\r\n";
    let out = set(y, &path!["listen", "gateway", "port"], &Scalar::Int(9000)).unwrap();
    assert_eq!(
        out,
        "version: 1\r\nlisten:\r\n  gateway:\r\n    port: 9000\r\n"
    );
    assert_eq!(out.matches("\r\n").count(), 4);
}

#[test]
fn a_literal_block_elsewhere_in_the_file_is_untouched() {
    let y = "note: |\n  第一行\n  第二行\n    缩进的第三行\nport: 8788\n";
    let out = set(y, &path!["port"], &Scalar::Int(1)).unwrap();
    only_changed(y, &out, "8788", "1");
    assert!(out.contains("    缩进的第三行"), "块标量的缩进被动了");
}

#[test]
fn flow_style_mixed_with_block_style_keeps_its_shape() {
    let y = "allow: [a, b, c]\nname: x\nnested: { k: v, j: w }\n";
    let out = set(y, &path!["name"], &Scalar::s("z")).unwrap();
    assert_eq!(out, "allow: [a, b, c]\nname: z\nnested: { k: v, j: w }\n");
    // 流式序列里的元素也要能定位
    let out = set(y, &path!["allow", 1usize], &Scalar::s("bb")).unwrap();
    assert_eq!(out, "allow: [a, bb, c]\nname: x\nnested: { k: v, j: w }\n");
}

#[test]
fn tabs_inside_a_block_scalar_survive() {
    // YAML 不允许 tab 做结构缩进，块标量的内容也不能**以** tab 开头
    // （解析器会直接报错）—— 但缩进之后的 tab 是内容的一部分。
    let y = "script: |\n  echo\thi\n  echo\tbye\nport: 1\n";
    let out = set(y, &path!["port"], &Scalar::Int(2)).unwrap();
    assert!(out.contains("echo\thi"), "tab 被换成空格了");
    assert_eq!(out, "script: |\n  echo\thi\n  echo\tbye\nport: 2\n");
}

#[test]
fn a_block_scalar_is_refused_rather_than_reflowed() {
    // **缩进在块标量里是内容的一部分**，改错一格就改了值。
    let y = "script: |\n  echo hi\nport: 1\n";
    let e = set(y, &path!["script"], &Scalar::s("echo bye")).unwrap_err();
    assert!(matches!(e, PatchError::BlockScalar(_)), "{e:?}");
    assert!(
        e.to_string()
            .contains("edit the configuration file directly"),
        "{e}"
    );
}

#[test]
fn a_very_long_line_does_not_get_wrapped() {
    let long = "x".repeat(20_000);
    let y = format!("blob: {long}\nport: 1\n");
    let out = set(&y, &path!["port"], &Scalar::Int(2)).unwrap();
    assert!(out.contains(&long), "长行被折了");
    assert_eq!(out.len(), y.len(), "长度都变了");
}

#[test]
fn an_anchor_is_refused_instead_of_silently_changing_every_alias() {
    // **改一个锚点会同时改掉引用它的每一处。**那不是「改一个字段」，
    // 而是一次用户完全没预期的批量修改。宁可不动。
    let y = "defaults: &d\n  timeout: 30\na:\n  <<: *d\nb:\n  <<: *d\n";
    let e = set(y, &path!["defaults"], &Scalar::s("x")).unwrap_err();
    assert!(matches!(e, PatchError::AnchorOrAlias(_)), "{e:?}");
    assert!(e.to_string().contains("anchor or alias"), "{e}");
}

#[test]
fn an_anchored_scalar_is_refused_too() {
    let y = "base: &b https://api.example.com\nother: *b\n";
    let e = set(y, &path!["base"], &Scalar::s("https://x")).unwrap_err();
    assert!(matches!(e, PatchError::AnchorOrAlias(_)), "{e:?}");
}

#[test]
fn a_path_that_points_at_a_map_or_list_says_so_rather_than_mangling_it() {
    let e = set(REAL, &path!["providers"], &Scalar::s("x")).unwrap_err();
    assert!(matches!(e, PatchError::NotScalar(_)), "{e:?}");
    assert!(e.to_string().contains("providers"), "{e}");
}

#[test]
fn a_missing_path_names_the_thing_it_could_not_find() {
    let e = set(REAL, &path!["providers", 9usize, "key"], &Scalar::s("x")).unwrap_err();
    assert_eq!(e, PatchError::NotFound("providers[9].key".into()));
}

#[test]
fn changing_a_value_keeps_the_file_parseable_and_semantically_right() {
    // 格式保住了但语义错了，比格式没保住严重得多。
    let out = set(
        REAL,
        &path!["providers", 0usize, "key"],
        &Scalar::s("sk-ant-new"),
    )
    .unwrap();
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).expect("改完解析不了");
    assert_eq!(v["providers"][0]["key"], "sk-ant-new");
    let before: serde_yaml_ng::Value = serde_yaml_ng::from_str(REAL).unwrap();
    assert_eq!(v["providers"][1], before["providers"][1]);
    assert_eq!(v["listen"], before["listen"]);
}

#[test]
fn a_value_that_would_change_its_type_gets_quoted() {
    // **一个叫 `no` 的 provider 不加引号会变成布尔 false。**
    let out = set(REAL, &path!["providers", 0usize, "name"], &Scalar::s("no")).unwrap();
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(v["providers"][0]["name"], "no", "读回来不是字符串了");
}

#[test]
fn a_value_with_a_colon_and_a_hash_survives_a_round_trip() {
    for tricky in [
        "sk-a:b#c",
        "# 不是注释",
        "a: b",
        "  前后有空格  ",
        "",
        "含\"双引号\"和'单引号'",
        "换行\n也要能放进去",
    ] {
        let out = set(REAL, &path!["providers", 0usize, "key"], &Scalar::s(tricky))
            .unwrap_or_else(|e| panic!("{tricky:?}: {e}"));
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(v["providers"][0]["key"], tricky, "{tricky:?} 读回来不对");
        assert_eq!(v["providers"][1]["name"], "中转", "{tricky:?} 之后的行坏了");
    }
}

#[test]
fn a_yaml_11_lookalike_is_quoted_even_though_our_own_parser_does_not_need_it() {
    // 实测过：saphyr 和 serde_yaml_ng 都是 YAML 1.2，`no` / `y` / `on`
    // 在它们眼里就是字符串。**但配置文件不止我们在读** —— 编辑器插件、
    // 别人的脚本、CI 里的 linter，有的还停在 1.1，那边 `no` 是 false。
    //
    // 代价是一对用户没写的引号，收益是这个文件在别处也说同一件事。
    for name in ["no", "y", "on", "off"] {
        let out = set(REAL, &path!["providers", 0usize, "name"], &Scalar::s(name)).unwrap();
        assert!(
            out.contains(&format!("name: \"{name}\"")),
            "{name} 该被引起来：{out}"
        );
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(v["providers"][0]["name"], name);
    }
}

#[test]
fn a_plain_value_with_a_hash_in_the_middle_is_not_cut_in_half() {
    // 纯量里的 `#` 只有前面是空白时才开启注释。`sk-a#b` 是一个完整的值。
    let y = "key: sk-a#b\nport: 1\n";
    let out = set(y, &path!["port"], &Scalar::Int(2)).unwrap();
    assert_eq!(out, "key: sk-a#b\nport: 2\n");
    // 改它自己时也不能切
    let out = set(y, &path!["key"], &Scalar::s("sk-c#d")).unwrap();
    assert_eq!(out, "key: sk-c#d\nport: 1\n");
}

#[test]
fn a_quote_inside_a_quoted_value_does_not_end_the_span_early() {
    let y = "a: 'it''s here'   # 注释\nb: \"say \\\"hi\\\" ok\"   # 也有注释\n";
    let out = set(y, &path!["a"], &Scalar::s("changed")).unwrap();
    assert!(out.contains("# 注释"), "{out}");
    assert_eq!(
        out,
        "a: 'changed'   # 注释\nb: \"say \\\"hi\\\" ok\"   # 也有注释\n"
    );
    let out = set(y, &path!["b"], &Scalar::s("x")).unwrap();
    assert!(out.contains("# 也有注释"), "{out}");
}

#[test]
fn the_last_value_in_a_file_without_a_trailing_newline_still_works() {
    let y = "a: 1\nb: two";
    let out = set(y, &path!["b"], &Scalar::s("three")).unwrap();
    assert_eq!(out, "a: 1\nb: three");
}

#[test]
fn a_duplicate_key_is_refused_rather_than_guessed() {
    // property test 抓到的。**同名键谁生效各家解析器并不一致** —— 我们
    // 自己的加载器（serde_yaml_ng）直接报错，而挑一个改的表现会是
    // 「改了却没生效」，用户会以为是我们没写进去，然后反复点保存。
    let y = "a:\n  k: 1\n  k: 2\n";
    let e = set(y, &path!["a", "k"], &Scalar::Int(9)).unwrap_err();
    assert!(matches!(e, PatchError::Duplicate(_)), "{e:?}");
    assert!(e.to_string().contains("appears more than once"), "{e}");
}

#[test]
fn the_self_check_is_what_stands_between_a_bug_and_a_broken_config_file() {
    // 这条不测某个具体 bug，测的是**那道闸在**：改完一定重新解析一遍，
    // 读回来的值必须就是写进去的值。它拦的是我们还没想到的引号规则。
    for v in [
        Scalar::s("正常"),
        Scalar::s("a: b"),
        Scalar::s("#"),
        Scalar::s("- x"),
        Scalar::s("*anchor"),
        Scalar::s("&anchor"),
        Scalar::s("!!str"),
        Scalar::s("[1,2]"),
        Scalar::s("{a: 1}"),
        Scalar::s("'"),
        Scalar::s("\\"),
        Scalar::s("\u{7f}"),
        Scalar::Int(-1),
        Scalar::Bool(true),
        Scalar::Null,
    ] {
        let out = set(REAL, &path!["providers", 0usize, "key"], &v)
            .unwrap_or_else(|e| panic!("{v:?}: {e}"));
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
        let got = &parsed["providers"][0]["key"];
        match &v {
            Scalar::Str(s) => assert_eq!(got.as_str(), Some(s.as_str()), "{v:?} → {out}"),
            Scalar::Int(i) => assert_eq!(got.as_i64(), Some(*i), "{v:?}"),
            Scalar::Bool(b) => assert_eq!(got.as_bool(), Some(*b), "{v:?}"),
            Scalar::Null => assert!(got.is_null(), "{v:?} → {got:?}"),
        }
    }
}

#[test]
fn scalar_paths_lists_what_a_form_could_offer_to_edit() {
    let ps = tw_yaml::scalar_paths(REAL).unwrap();
    let names: Vec<String> = ps
        .iter()
        .map(|p| {
            p.iter()
                .map(|s| match s {
                    Step::Key(k) => k.clone(),
                    Step::Index(i) => format!("[{i}]"),
                })
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    assert!(
        names.contains(&"listen.gateway.port".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"providers.[1].base_url".to_string()),
        "{names:?}"
    );
    // 容器不在里面 —— 它们不是能填的字段
    assert!(!names.contains(&"providers".to_string()), "{names:?}");
}

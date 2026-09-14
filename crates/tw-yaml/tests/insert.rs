//! `insert` 的验收：**往一个从来没写过的字段上写值**。
//!
//! 表单模式的死活就在这一条上 —— 配置里绝大多数字段是可选的、默认不
//! 写的（`protocol`、`billing`、`trust`、`session_affinity`…）。只能改
//! 「用户碰巧写过」的字段，等于界面在他最需要的时候是死的。

use tw_yaml::{Scalar, insert, path};

const CFG: &str = "version: 1\n# 用户的注释\nproviders:\n  - name: 官方\n    base_url: https://api.anthropic.com\n    key: sk-a\n  - name: 中转\n    base_url: https://relay.example.com\n    key: sk-b\ngroups:\n  - name: 池子\n    type: load-balance\n    providers: [官方, 中转]\n";

#[test]
fn a_field_that_was_never_written_can_still_be_set() {
    // **表单模式的死活就在这一条上。**配置里绝大多数字段是可选的、
    // 默认不写的，只能改「用户碰巧写过」的字段等于界面在他最需要
    // 的时候是死的
    let out = insert(
        CFG,
        &path!["providers", 1, "protocol"],
        &Scalar::s("openai-chat"),
    )
    .unwrap();
    assert!(out.contains("protocol: openai-chat"), "{out}");
    // 插在这一家的下面，不是另一家
    let idx_relay = out.find("name: 中转").unwrap();
    assert!(
        out.find("protocol: openai-chat").unwrap() > idx_relay,
        "{out}"
    );
    // 注释和别的字段一个字都没动
    assert!(out.contains("# 用户的注释"), "{out}");
    assert!(out.contains("key: sk-a"), "{out}");
    // **原来的每一行都要原样还在。**不能用 zip 逐行比 —— 插入之后后面
    // 整体错位，比出来的「差异」全是假的（第一版就是这么写的）
    let mut left: Vec<&str> = out.lines().collect();
    for line in CFG.lines() {
        let at = left
            .iter()
            .position(|x| *x == line)
            .unwrap_or_else(|| panic!("原来的这一行没了：{line:?}\n{out}"));
        left.remove(at);
    }
    assert_eq!(
        left,
        vec!["    protocol: openai-chat"],
        "多出来的不止那一行"
    );
}

#[test]
fn the_indent_is_copied_from_the_neighbouring_line_not_computed() {
    // 用户可能用 4 空格，也可能是列表项里那种对齐
    let four = "a:\n    b: 1\n";
    // 值用一个不像数字的 —— `"2"` 会被正确地加引号免得变成数字，那是
    // `render_scalar` 管的另一件事；这条测的是缩进
    let out = insert(four, &path!["a", "c"], &Scalar::s("cc")).unwrap();
    assert!(out.contains("\n    c: cc"), "{out:?}");
}

#[test]
fn inserting_a_field_that_already_exists_is_just_a_replace() {
    let out = insert(CFG, &path!["providers", 0, "key"], &Scalar::s("sk-new")).unwrap();
    assert!(out.contains("key: sk-new"), "{out}");
    assert!(!out.contains("key: sk-a"), "{out}");
    assert_eq!(out.lines().count(), CFG.lines().count(), "行数不该变");
}

#[test]
fn a_boolean_reads_back_as_a_boolean() {
    // `session_affinity: "false"` 和 `false` 是两回事 —— 前者在
    // serde 那边会直接报类型错
    let out = insert(
        CFG,
        &path!["groups", 0, "session_affinity"],
        &Scalar::Bool(false),
    )
    .unwrap();
    assert!(out.contains("session_affinity: false"), "{out}");
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(
        v["groups"][0]["session_affinity"],
        serde_yaml_ng::Value::Bool(false)
    );
}

#[test]
fn a_missing_parent_is_refused_rather_than_invented() {
    // 猜结构是不能接受的 —— 这个文件会去改用户唯一的配置
    assert!(insert(CFG, &path!["providers", 9, "protocol"], &Scalar::s("x")).is_err());
    assert!(insert(CFG, &path!["没有这一段", "x"], &Scalar::s("x")).is_err());
}

#[test]
fn inserting_into_a_multiline_child_still_lands_in_the_right_place() {
    // 最后一个子键自己是个列表时，`bytes.end` 会跑过头
    let out = insert(
        CFG,
        &path!["groups", 0, "session_affinity"],
        &Scalar::Bool(true),
    )
    .unwrap();
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(
        v["groups"][0]["session_affinity"],
        serde_yaml_ng::Value::Bool(true)
    );
    assert_eq!(
        v["groups"][0]["providers"][1],
        serde_yaml_ng::Value::String("中转".into())
    );
}

#[test]
fn a_flow_mapping_gets_the_new_key_inside_the_braces() {
    // `- { name: 甲, key: sk-a }` 是完全合法的写法，而格式保留
    // 语料里本来就列了「流式与块式混排」。**在下一行插会产出一份解析
    // 不了的 YAML** —— 护栏会拦住，但那时用户看到的是「这是个 bug，
    // 请贴到 issue 里」，而他只是用了一种正常写法。
    let cfg = "providers:\n  - { name: 甲, base_url: \"http://x\", key: sk-a }\n";
    let out = insert(
        cfg,
        &path!["providers", 0, "billing"],
        &Scalar::s("subscription"),
    )
    .unwrap();
    assert!(out.contains("billing: subscription"), "{out}");
    assert_eq!(out.lines().count(), cfg.lines().count(), "行数不该变");
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(
        v["providers"][0]["billing"],
        serde_yaml_ng::Value::String("subscription".into())
    );
    assert_eq!(
        v["providers"][0]["key"],
        serde_yaml_ng::Value::String("sk-a".into())
    );
}

#[test]
fn the_comma_style_follows_what_is_already_there() {
    // `{a: 1, b: 2}` 和 `{a: 1,b: 2}` 都有人写。跟着来，比统一成我们的
    // 偏好更不打扰 —— 这是他的文件
    let spaced = insert("m: { a: 1, b: 2 }\n", &path!["m", "c"], &Scalar::Int(3)).unwrap();
    assert!(spaced.contains("b: 2, c: 3"), "{spaced:?}");
    let tight = insert("m: {a: 1,b: 2}\n", &path!["m", "c"], &Scalar::Int(3)).unwrap();
    assert!(tight.contains("b: 2,c: 3"), "{tight:?}");
}

#[test]
fn a_brace_inside_a_quoted_value_is_not_the_closing_one() {
    let cfg = "m: { cmd: \"echo }\", a: 1 }\n";
    let out = insert(cfg, &path!["m", "b"], &Scalar::Int(2)).unwrap();
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out).unwrap();
    assert_eq!(v["m"]["cmd"], serde_yaml_ng::Value::String("echo }".into()));
    assert_eq!(v["m"]["b"], serde_yaml_ng::Value::Number(2.into()));
}

// 空的 `{}`：解析器根本不为它发出一个 Map 节点，所以这条路走不到。
// **不为一个退化写法去改解析层** —— 它报的是一句清楚的「没有这个位置」，
// 而配置里写一个空映射本来也没有意义。

#[test]
fn the_cursor_maps_to_the_thing_it_is_actually_inside() {
    // **猜错的表现是「我明明点在中转上，右边显示的是官方」** —— 那比
    // 没有这个功能更让人不信任这一页
    let cfg = "providers:\n  - name: 官方\n    base_url: https://a\n  - name: 中转\n    base_url: https://b\n";
    let at = |needle: &str| {
        let i = cfg.find(needle).unwrap();
        tw_yaml::path_at(cfg, i + 1).unwrap()
    };
    assert_eq!(at("官方")[..2], tw_yaml::path!["providers", 0][..]);
    assert_eq!(at("中转")[..2], tw_yaml::path!["providers", 1][..]);
    // 停在第二家的 base_url 上，仍然算在第二家里
    assert_eq!(at("https://b")[..2], tw_yaml::path!["providers", 1][..]);
}

//! manifest 字面量：找得准（注释、字符串、模板、正则里的不算）、读得对（每种写法的值）、
//! 不是纯数据的一律说清哪里不对；写回去的样子固定；改写只换那一段字节。
//!
//! 后半是性质测试和乱码测试：随机的 manifest 放在随机的代码中间照样找得到、读回来一样；
//! 随机的合规改写不碰字面量以外的任何一个字节，读回来就是改成的那样；什么样的输入都不会
//! panic。随机数用一个固定种子的 xorshift，失败了能复现。

use serde_json::{Value, json};

use super::*;

fn lit(src: &str) -> Literal {
    find(src).unwrap_or_else(|e| panic!("{e:?}\n{src}"))
}

fn data(src: &str) -> Value {
    lit(src).data.to_json()
}

/// 只有一个 manifest 的源码
fn module(manifest: &str) -> String {
    format!(
        "export const manifest = {manifest};\nexport function onRequest(req) {{ return req; }}\n"
    )
}

fn not_data(manifest: &str) -> NotData {
    let src = module(manifest);
    match find(&src) {
        Ok(l) => panic!("read as data: {:?}\n{src}", l.data),
        Err(e) => e,
    }
}

// ── 找 ───────────────────────────────────────────────────────────

#[test]
fn the_span_is_exactly_the_object_literal() {
    let src = "// 插件\nexport const manifest = { name: \"x\", api: 1 };\nexport function onRequest() {}\n";
    let l = lit(src);
    assert_eq!(&src[l.span.clone()], "{ name: \"x\", api: 1 }");
    assert_eq!(l.data.to_json(), json!({"name": "x", "api": 1}));
}

#[test]
fn every_kind_of_value_reads_as_javascript_would() {
    let src = r#"export const manifest = {
  // 注释
  name: 'single "quoted"',
  "quoted key": "double \"quoted\"",
  'single key': `template
line`,
  中文: "名字",
  $dollar_1: true,
  _: false,
  nothing: null,
  escapes: "\n\t\r\b\f\v\0\x41\u0042\u{43}\u{1F600}\uD83D\uDE00\'\"\\\a\
continued",
  numbers: [0, -1, 1.5, .5, 5., 1e3, 1E-3, -2.5e+2, 0x1F, 0o17, 0b101, 1_000, 0.000_1, -0x10],
  nested: { list: [[], {}, [1, [2, { deep: "yes" }]]], /* 块注释 */ empty: {} },
  trailing: [1, 2, 3,],
};
"#;
    let v = data(src);
    assert_eq!(v["name"], "single \"quoted\"");
    assert_eq!(v["quoted key"], "double \"quoted\"");
    assert_eq!(v["single key"], "template\nline");
    assert_eq!(v["中文"], "名字");
    assert_eq!(v["$dollar_1"], true);
    assert_eq!(v["_"], false);
    assert_eq!(v["nothing"], Value::Null);
    assert_eq!(
        v["escapes"],
        "\n\t\r\u{8}\u{c}\u{b}\0ABC\u{1F600}\u{1F600}'\"\\acontinued"
    );
    assert_eq!(
        v["numbers"],
        json!([
            0, -1, 1.5, 0.5, 5, 1000, 0.001, -250, 31, 15, 5, 1000, 0.0001, -16
        ])
    );
    assert_eq!(v["nested"]["list"][2][1][1]["deep"], "yes");
    assert_eq!(v["nested"]["empty"], json!({}));
    assert_eq!(v["trailing"], json!([1, 2, 3]));
}

/// 模板字符串里的换行照 JavaScript 读：`\r\n`、`\r` 都是 `\n`
#[test]
fn line_breaks_in_a_template_literal_read_as_javascript_does() {
    let src = "export const manifest = { a: `x\r\ny\rz`, b: `p\\\r\nq` };\n";
    let v = data(src);
    assert_eq!(v["a"], "x\ny\nz");
    assert_eq!(v["b"], "pq");
}

/// 键的先后就是源码里的先后
#[test]
fn keys_keep_their_order() {
    let l = lit(&module("{ zeta: 1, alpha: 2, mid: 3 }"));
    let Data::Object(m) = l.data else { panic!() };
    let keys: Vec<&str> = m.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["zeta", "alpha", "mid"]);
}

#[test]
fn what_is_not_data_is_refused_and_says_why() {
    let cases: &[(&str, &str)] = &[
        (r#"{ name: "a" + "b" }"#, "expression"),
        (r#"{ name: String(1) }"#, "function call"),
        (r#"{ name: NAME }"#, "refers to a variable"),
        (r#"{ name: undefined }"#, "`undefined`"),
        (r#"{ n: NaN }"#, "`NaN`"),
        (r#"{ n: Infinity }"#, "`Infinity`"),
        (r#"{ n: -Infinity }"#, "minus sign"),
        (r#"{ n: +1 }"#, "expression"),
        (r#"{ n: 10n }"#, "BigInt"),
        (r#"{ n: 017 }"#, "octal"),
        (r#"{ n: 08 }"#, "octal"),
        (r#"{ n: 1__0 }"#, "`_`"),
        (r#"{ n: 1_ }"#, "`_`"),
        (r#"{ n: 1e }"#, "exponent"),
        (r#"{ n: 1e999 }"#, "too large"),
        (r#"{ n: 1px }"#, "followed directly by a name"),
        (r#"{ n: (1) }"#, "parentheses"),
        (r#"{ n: 1 ? 2 : 3 }"#, "expression"),
        (r#"{ s: "\1" }"#, "octal"),
        (r#"{ s: "\08" }"#, "octal"),
        (r#"{ s: "\xZZ" }"#, "\\x"),
        (r#"{ s: "\u12" }"#, "\\u"),
        (r#"{ s: "\u{110000}" }"#, "U+10FFFF"),
        (r#"{ s: "\uD800" }"#, "lone surrogate"),
        (r#"{ s: "\uDE00\uD83D" }"#, "lone surrogate"),
        (r#"{ s: `a${b}c` }"#, "${"),
        (r#"{ s: /re/ }"#, "regular expression"),
        (r#"{ ...other }"#, "spread"),
        (r#"{ list: [...other] }"#, "spread"),
        (r#"{ [key]: 1 }"#, "computed key"),
        (r#"{ name }"#, "refers to a variable"),
        (r#"{ name() { return "x"; } }"#, "method"),
        (r#"{ get name() { return "x"; } }"#, "getter"),
        (r#"{ set name(v) {} }"#, "setter"),
        (r#"{ async f() {} }"#, "async"),
        (r#"{ *gen() {} }"#, "generator"),
        (r#"{ 1: "a" }"#, "number cannot be a key"),
        (r#"{ __proto__: { a: 1 } }"#, "__proto__"),
        (r#"{ "__proto__": { a: 1 } }"#, "__proto__"),
        (r#"{ a: 1, a: 2 }"#, "appears twice"),
        (r#"{ a: [1, , 2] }"#, "empty slot"),
        (r#"{ a: [,] }"#, "empty slot"),
        (r#"{ a: function () {} }"#, "code"),
        (r#"{ a: () => 1 }"#, "parentheses"),
        (r#"{ a: new Date() }"#, "code"),
        (r#"{ a: this }"#, "code"),
        (r#"{ a: 1 b: 2 }"#, "expression"),
        (r#"{ a = 1 }"#, "`:`"),
        (r#"{ \u0061: 1 }"#, "escapes"),
    ];
    for (manifest, says) in cases {
        let e = not_data(manifest);
        assert!(
            e.message.contains(says),
            "{manifest}: {:?} does not say {says:?}",
            e.message
        );
        assert!(e.line.is_some() && e.column.is_some(), "{manifest}: {e:?}");
    }
}

/// 位置从 1 起，列按字符数；`\r\n` 算一次换行
#[test]
fn an_error_says_where_it_is() {
    let src = "// 第一行\r\nexport const manifest = {\r\n  name: \"名字\",\n  api: one,\n};\n";
    let e = find(src).unwrap_err();
    assert_eq!((e.line, e.column), (Some(4), Some(8)), "{e:?}");
}

#[test]
fn a_manifest_that_is_not_an_object_literal_is_refused() {
    for src in [
        "export const manifest = new Proxy({}, {});\n",
        "export const manifest = ({ name: \"x\" });\n",
        "export const manifest = make();\n",
        "export const manifest = \"x\";\n",
    ] {
        let e = find(src).unwrap_err();
        assert!(e.message.contains("object literal"), "{src}: {e:?}");
    }
    // 别的写法认不出来：说清该怎么写
    for src in [
        "export let manifest = { name: \"x\" };\n",
        "export var manifest = { name: \"x\" };\n",
        "const manifest = { name: \"x\" };\nexport { manifest };\n",
        "export default { manifest: { name: \"x\" } };\n",
        "",
    ] {
        let e = find(src).unwrap_err();
        assert!(e.message.contains("export const manifest"), "{src}: {e:?}");
        assert_eq!(e.line, None);
    }
}

#[test]
fn a_manifest_declared_twice_is_refused() {
    let src = "export const manifest = { a: 1 };\nexport const manifest = { a: 2 };\n";
    let e = find(src).unwrap_err();
    assert!(e.message.contains("twice"), "{e:?}");
    assert_eq!(e.line, Some(2));
}

/// 注释、字符串、模板字符串（连同 `${…}` 里的代码）、正则里长得像 manifest 的都不算；
/// 函数里的也不算（`export` 只能在模块顶层）
#[test]
fn lookalikes_in_comments_strings_templates_and_regexes_do_not_count() {
    let src = r#"// export const manifest = { name: "line comment" };
/* export const manifest = { name: "block comment" }; */
const a = "export const manifest = { name: \"string\" }";
const b = 'export const manifest = { name: "single" }';
const c = `export const manifest = ${ `nested ${ { x: "}" }.x } export const manifest = {` } {`;
const d = /export const manifest = \{[^}]*\}/g;
const e = /[/"'`]/;
function f() { const manifest = { name: "inner" }; return manifest; }
export const manifest = { name: "real" };
const g = "export const manifest = { name: \"after\" }";
"#;
    let l = lit(src);
    assert_eq!(l.data.to_json(), json!({"name": "real"}));
    assert_eq!(&src[l.span.clone()], "{ name: \"real\" }");
}

/// 除号和正则按前一个记号分：分错了的话，下面这些里的引号和反引号会把后面的代码吞掉
#[test]
fn division_and_regular_expressions_are_told_apart() {
    let src = r#"const half = total / 2 / count;
let i = 0; i++ / 2; i-- / 3;
const ok = (a) / 2 + [1][0] / 3;
function check(s) { return /'"`/.test(s) ? s.replace(/"/g, "'") : typeof /`/; }
if (ready) { start(); } /'/.test(other);
const obj = { a: 1 }.a / 2;
const x = a
/c/g.exec(b);
const tpl = `${ a / 2 }` / 3;
const del = obj.delete / 2, ret = obj.return / "'";
export const manifest = { name: "found" };
"#;
    assert_eq!(data(src), json!({"name": "found"}));
}

/// manifest 后面的代码读不懂也不碍事：它已经读出来了
#[test]
fn code_after_the_manifest_does_not_matter() {
    let src = "export const manifest = { name: \"x\" };\nconst broken = \"never closed\n";
    assert_eq!(data(src), json!({"name": "x"}));
    // 之前读不懂就找不到，说清在哪儿
    let src = "const broken = \"never closed\nexport const manifest = { name: \"x\" };\n";
    let e = find(src).unwrap_err();
    assert_eq!(e.line, Some(1), "{e:?}");
}

#[test]
fn a_bom_and_a_hashbang_at_the_start_are_skipped() {
    let src = "\u{feff}#!/usr/bin/env node\nexport const manifest = { a: 1 };\n";
    let l = lit(src);
    assert_eq!(&src[l.span.clone()], "{ a: 1 }");
}

// ── 写 ───────────────────────────────────────────────────────────

#[test]
fn the_written_style_is_fixed() {
    let d = Data::from_json(&json!({
        "name": "Answer in a chosen language",
        "api": 1,
        "permissions": ["system", "messages"],
        "match": {"clients": [], "models": ["deepseek*"], "upstreams": []},
        "settings": {
            "language": {"type": "string", "label": "Answer language", "value": "简体中文"},
            "windows_client": {"type": "boolean", "label": "The client runs on Windows (otherwise WSL)", "value": false},
        },
    }))
    .unwrap();
    // from_json 的键按 JSON 的先后（字母序）；manifest 字段的先后由调用方决定，这里只看样子
    let out = write(&d, "", "\n");
    assert_eq!(
        out,
        r#"{
  api: 1,
  match: { clients: [], models: ["deepseek*"], upstreams: [] },
  name: "Answer in a chosen language",
  permissions: ["system", "messages"],
  settings: {
    language: { label: "Answer language", type: "string", value: "简体中文" },
    windows_client: {
      label: "The client runs on Windows (otherwise WSL)",
      type: "boolean",
      value: false,
    },
  },
}"#
    );
}

#[test]
fn keys_strings_and_numbers_are_written_so_they_read_back() {
    let d = Data::Object(vec![
        ("plain_$1".into(), Data::Number(1.0)),
        ("needs quotes".into(), Data::Number(-0.5)),
        (
            "中文".into(),
            Data::String("a\"b\\c\nd\te\u{1}f\u{2028}g\u{7f}".into()),
        ),
        ("1st".into(), Data::Number(1e21)),
        ("tiny".into(), Data::Number(1e-7)),
        ("neg_zero".into(), Data::Number(-0.0)),
    ]);
    let out = write(&d, "", "\n");
    assert_eq!(
        out,
        "{\n  plain_$1: 1,\n  \"needs quotes\": -0.5,\n  \"中文\": \"a\\\"b\\\\c\\nd\\te\\u0001f\\u2028g\\u007f\",\n  \"1st\": 1e+21,\n  tiny: 1e-7,\n  neg_zero: 0,\n}"
    );
    let back = lit(&format!("export const manifest = {out};")).data;
    let mut want = d.clone();
    if let Data::Object(m) = &mut want {
        m[5].1 = Data::Number(0.0);
    }
    assert_eq!(back, want);
}

/// 数字照 JavaScript 的 `String(x)` 写
#[test]
fn numbers_are_written_as_javascript_prints_them() {
    for (x, s) in [
        (0.0, "0"),
        (1.0, "1"),
        (-7.0, "-7"),
        (0.1, "0.1"),
        (1.5, "1.5"),
        (1234.5678, "1234.5678"),
        (100.0, "100"),
        (1e20, "100000000000000000000"),
        (1e21, "1e+21"),
        (1.5e300, "1.5e+300"),
        (0.000001, "0.000001"),
        (0.0000001, "1e-7"),
        (1.2345e-8, "1.2345e-8"),
        (5e-324, "5e-324"),
        (9007199254740993.0, "9007199254740992"),
        (123456789012345680000.0, "123456789012345680000"),
        (f64::MAX, "1.7976931348623157e+308"),
    ] {
        assert_eq!(js_number(x), s, "{x}");
    }
}

/// 缩进照字面量开头那一行，换行符照文件
#[test]
fn indentation_and_line_endings_follow_the_file() {
    let src =
        "if (true) {}\r\n    export const manifest = { a: { b: [1] }, list: [\"x\"] };\r\nrest\r\n";
    let l = lit(src);
    let out = replace(src, &l, &l.data);
    assert_eq!(
        out,
        "if (true) {}\r\n    export const manifest = {\r\n      a: { b: [1] },\r\n      list: [\"x\"],\r\n    };\r\nrest\r\n"
    );
}

/// 写出来的再写一遍还是它
#[test]
fn writing_is_stable() {
    let src = module(
        r#"{ name: "x", api: 1, permissions: ["system"], description: "a long description that goes on and on and on and on and on", settings: { a: { type: "string", label: "A", value: "line\nbreak" }, b: { type: "number", label: "a label long enough to push this object over the width", value: 1.5 } } }"#,
    );
    let l = lit(&src);
    let once = replace(&src, &l, &l.data);
    let l2 = lit(&once);
    assert_eq!(l2.data, l.data);
    let twice = replace(&once, &l2, &l2.data);
    assert_eq!(once, twice);
}

// ── 改数据 ───────────────────────────────────────────────────────

const PLUGIN: &str = r#"// 附加日期
//
// 顶上的注释、manifest 外面的代码，改写之后一个字节都不变。
const ZONE = "UTC"; /* export const manifest = { fake: true } */

export const manifest = {
  name: "Add date", // 名字
  api: 1,
  permissions: ["system"],
  settings: {
    note: { type: "string", label: "Note", value: "today" },
    days: { type: "number", label: "Days" },
    loud: { type: "boolean", label: "Loud", value: true },
  },
};

export function onRequest(req, ctx) {
  return { ...req, system: `${req.system} ${ctx.settings.note}` };
}
"#;

fn values(src: &str) -> Values {
    lit(src).values().expect("the values read")
}

fn edit(on_error: OnError, models: &[&str], settings: Value) -> Values {
    Values {
        on_error,
        scope: Scope {
            clients: Vec::new(),
            models: models.iter().map(|s| s.to_string()).collect(),
            upstreams: Vec::new(),
        },
        settings: settings
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    }
}

#[test]
fn values_are_read_with_defaults_for_what_is_not_written() {
    let v = values(PLUGIN);
    assert_eq!(v.on_error, OnError::Reject);
    assert_eq!(v.scope, Scope::default());
    assert_eq!(
        v.settings,
        [
            ("note".to_string(), json!("today")),
            ("days".to_string(), json!(0)),
            ("loud".to_string(), json!(true)),
        ]
    );
    let bad = module(r#"{ on_error: "ignore" }"#);
    assert!(lit(&bad).values().is_none());
    let bad = module(r#"{ match: { models: "x" } }"#);
    assert!(lit(&bad).values().is_none());
    let bad = module(r#"{ settings: { a: { type: "number", value: "x" } } }"#);
    assert!(lit(&bad).values().is_none());
}

#[test]
fn a_rewrite_changes_only_the_literal() {
    let out = rewrite(
        PLUGIN,
        &edit(
            OnError::Skip,
            &["claude-*"],
            json!({"days": 3, "note": "明天"}),
        ),
    )
    .unwrap();
    let before = lit(PLUGIN);
    let after = lit(&out);
    assert_eq!(out[..after.span.start], PLUGIN[..before.span.start]);
    assert_eq!(out[after.span.end..], PLUGIN[before.span.end..]);
    assert_eq!(
        &out[after.span.clone()],
        r#"{
  name: "Add date",
  api: 1,
  permissions: ["system"],
  match: { clients: [], models: ["claude-*"], upstreams: [] },
  on_error: "skip",
  settings: {
    note: { type: "string", label: "Note", value: "明天" },
    days: { type: "number", label: "Days", value: 3 },
    loud: { type: "boolean", label: "Loud", value: true },
  },
}"#
    );
    let v = values(&out);
    assert_eq!(v.on_error, OnError::Skip);
    assert_eq!(v.scope.models, ["claude-*"]);
    assert_eq!(v.settings[1], ("days".to_string(), json!(3)));
    assert!(same_code(PLUGIN, &out));
}

/// 什么都没改：一个字节都不动（字面量里的注释和排版都还在）
#[test]
fn a_rewrite_to_the_same_values_leaves_the_file_as_it_is() {
    let same = rewrite(
        PLUGIN,
        &edit(OnError::Reject, &[], json!({"note": "today"})),
    )
    .unwrap();
    assert_eq!(same, PLUGIN);
    // 没写 `value` 的设置改成它的空值：照旧不写
    let same = rewrite(PLUGIN, &edit(OnError::Reject, &[], json!({"days": 0}))).unwrap();
    assert_eq!(same, PLUGIN);
}

/// 写过的就留着：`on_error` 改回拒绝、范围清空，键都还在
#[test]
fn fields_once_written_stay_written() {
    let first = rewrite(PLUGIN, &edit(OnError::Skip, &["a*"], json!({}))).unwrap();
    let back = rewrite(&first, &edit(OnError::Reject, &[], json!({}))).unwrap();
    let v = lit(&back).data.to_json();
    assert_eq!(v["on_error"], "reject");
    assert_eq!(
        v["match"],
        json!({"clients": [], "models": [], "upstreams": []})
    );
}

/// 原来只写了一张单子的范围：别的单子空着就不加
#[test]
fn a_partial_match_gets_only_the_lists_it_needs() {
    let src = module(r#"{ name: "x", match: { models: ["a"] }, settings: {} }"#);
    let out = rewrite(
        &src,
        &Values {
            scope: Scope {
                clients: Vec::new(),
                models: vec!["b".into()],
                upstreams: vec!["relay".into()],
            },
            ..Values::default()
        },
    )
    .unwrap();
    assert_eq!(
        lit(&out).data.to_json()["match"],
        json!({"models": ["b"], "upstreams": ["relay"]})
    );
}

#[test]
fn a_rewrite_refuses_what_does_not_fit() {
    let e = rewrite(
        PLUGIN,
        &edit(OnError::Reject, &[], json!({"colour": "red"})),
    )
    .unwrap_err();
    assert_eq!(e, RewriteError::UnknownSetting("colour".into()));
    let e = rewrite(
        PLUGIN,
        &edit(OnError::Reject, &[], json!({"days": "three"})),
    )
    .unwrap_err();
    assert_eq!(
        e,
        RewriteError::SettingType {
            key: "days".into(),
            kind: SettingKind::Number
        }
    );
    let e = rewrite(PLUGIN, &edit(OnError::Reject, &[" "], json!({}))).unwrap_err();
    assert!(
        matches!(e, RewriteError::Invalid(ref m) if m.contains("empty")),
        "{e:?}"
    );
    let long = "x".repeat(201);
    let e = rewrite(PLUGIN, &edit(OnError::Reject, &[&long], json!({}))).unwrap_err();
    assert!(matches!(e, RewriteError::Invalid(_)), "{e:?}");
    let e = rewrite(
        PLUGIN,
        &edit(OnError::Reject, &[], json!({"note": "x".repeat(10_001)})),
    )
    .unwrap_err();
    assert!(
        matches!(e, RewriteError::Invalid(ref m) if m.contains("too long")),
        "{e:?}"
    );
    let e = rewrite("export const manifest = make();", &Values::default()).unwrap_err();
    assert!(matches!(e, RewriteError::NotData(_)), "{e:?}");
}

/// 只改了数据（出错时怎么办、范围、设置的值，连同字面量里的排版和注释）是同一份代码；
/// 字面量以外差一个字节、manifest 里别的字段差一点，都是改了代码
#[test]
fn only_data_edits_keep_the_same_code() {
    let data_only = [
        PLUGIN.replace(r#""today""#, r#""tomorrow""#),
        PLUGIN.replace("  api: 1,\n", "  api: 1,\n  on_error: \"skip\",\n"),
        PLUGIN.replace("  api: 1,\n", "  api: 1,\n  match: { models: [\"x\"] },\n"),
        PLUGIN.replace("label: \"Days\" }", "label: \"Days\", value: 7 }"),
        PLUGIN.replace(" // 名字", ""),
        PLUGIN.replace("name: \"Add date\",", "\"name\": 'Add date',"),
        PLUGIN.replace("api: 1,", "api: 1.0,"),
    ];
    for other in &data_only {
        assert_ne!(other, PLUGIN);
        assert!(same_code(PLUGIN, other), "{other}");
    }
    let code = [
        PLUGIN.replace("ctx.settings.note", "ctx.settings.note.toUpperCase()"),
        PLUGIN.replace("const ZONE", "const  ZONE"),
        PLUGIN.replace("\"Add date\"", "\"Add a date\""),
        PLUGIN.replace("[\"system\"]", "[\"system\", \"messages\"]"),
        PLUGIN.replace("label: \"Days\"", "label: \"Number of days\""),
        PLUGIN.replace("type: \"number\"", "type: \"string\""),
        PLUGIN.replace(
            "    loud:",
            "    extra: { type: \"string\", label: \"Extra\" },\n    loud:",
        ),
        PLUGIN.replace("  api: 1,\n", "  api: 1,\n  requests: [\"conversation\"],\n"),
        PLUGIN.replace("  api: 1,\n", "  api: 1,\n  reply: \"block\",\n"),
        // 设置的先后也是代码：界面上的先后
        PLUGIN.replace(
            "    days: { type: \"number\", label: \"Days\" },\n    loud: { type: \"boolean\", label: \"Loud\", value: true },\n",
            "    loud: { type: \"boolean\", label: \"Loud\", value: true },\n    days: { type: \"number\", label: \"Days\" },\n",
        ),
        "export const manifest = make();".to_string(),
    ];
    for other in &code {
        assert_ne!(other, PLUGIN);
        assert!(!same_code(PLUGIN, other), "{other}");
    }
}

// ── 性质测试 ─────────────────────────────────────────────────────

/// 一个够用的伪随机数：测试要能复现，不引新的依赖
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn text(&mut self) -> String {
        const PIECES: &[&str] = &[
            "a",
            "Z",
            "中文",
            " ",
            "\"",
            "'",
            "`",
            "\\",
            "\n",
            "\r\n",
            "\t",
            "${",
            "}",
            "{",
            "//",
            "/*",
            "*/",
            "\u{2028}",
            "\u{1}",
            "\u{1F600}",
            "export const manifest = {",
            "é",
            "]",
            ",",
            ":",
            "\u{feff}",
            "\u{7f}",
        ];
        (0..self.below(5)).map(|_| *self.pick(PIECES)).collect()
    }
    fn number(&mut self) -> f64 {
        match self.below(6) {
            0 => self.below(1000) as f64,
            1 => -(self.below(1000) as f64),
            2 => self.below(100_000) as f64 / 7.0,
            3 => f64::from_bits(self.next() & 0x7fef_ffff_ffff_ffff),
            4 => [0.0, -0.0, 1e21, 1e-7, 5e-324, 9007199254740993.0][self.below(6)],
            _ => (self.next() as f64) * if self.chance(50) { 1.0 } else { -1.0 },
        }
    }
    fn key(&mut self, taken: &[(String, Data)]) -> String {
        const KEYS: &[&str] = &[
            "name",
            "api",
            "value",
            "default",
            "get",
            "set",
            "async",
            "a b",
            "中文",
            "a-b",
            "$x",
            "_",
            "x1",
            "1x",
            "",
            "\"q\"",
            "new",
            "class",
            "constructor",
            "toString",
        ];
        loop {
            let k = if self.chance(70) {
                self.pick(KEYS).to_string()
            } else {
                self.text()
            };
            if k != "__proto__" && !taken.iter().any(|(t, _)| *t == k) {
                return k;
            }
        }
    }
    fn data(&mut self, depth: usize) -> Data {
        match self.below(if depth > 3 { 4 } else { 7 }) {
            0 => Data::Null,
            1 => Data::Bool(self.chance(50)),
            2 => Data::Number(self.number()),
            3 => Data::String(self.text()),
            4 | 5 => {
                let mut m = Vec::new();
                for _ in 0..self.below(5) {
                    let k = self.key(&m);
                    let v = self.data(depth + 1);
                    m.push((k, v));
                }
                Data::Object(m)
            }
            _ => Data::Array((0..self.below(5)).map(|_| self.data(depth + 1)).collect()),
        }
    }
    fn object(&mut self) -> Data {
        let mut m = Vec::new();
        for _ in 0..self.below(8) {
            let k = self.key(&m);
            let v = self.data(1);
            m.push((k, v));
        }
        Data::Object(m)
    }
}

/// manifest 前后的代码：每一段都夹着长得像 manifest、像字符串结尾、像注释的东西
const AROUND: &[&str] = &[
    "// export const manifest = { name: \"fake\" };\n",
    "/* export const manifest = { name: \"fake\" } */\n",
    "const s = \"export const manifest = { name: \\\"fake\\\" }\";\n",
    "const t = 'it\\'s } { \" ` ${';\n",
    "const tpl = `export const manifest = ${ `nested ${ { a: \"}\" }.a } }` } {`;\n",
    "const re = /export const manifest = \\{[^}]*\\}/g;\n",
    "const re2 = /[/\"'`]/u;\n",
    "const half = a / 2 / b;\n",
    "function f(x) { return /'\"/.test(x) ? x / 2 : { y: \"}\" }; }\n",
    "let i = 0; i++ / 2; --i;\n",
    "const obj = { a: { b: [1, 2, { c: \"}\" }] } };\n",
    "export function onRequest(req, ctx) { const m = { manifest: 1 }; return req; }\n",
    "if (x) { } /regex-after-block'/.test(y);\n",
    "const html = `<div class=\"${cls}\">${ items.map(i => `<li>${i}</li>`).join('') }</div>`;\n",
    "const n = .5 + 1e-3 + 0x1F + 1_000 + 10n;\n",
    "const u = \"中文 \\u2028 \u{2028}\";\n",
    "const a = b\n/c/g.exec(d);\n",
    "\n\n",
    "export function onReplyText(t) { return t.replace(/`/g, \"'\"); }\n",
    "class K { #p = 1; get v() { return this.#p / 2; } }\n",
    "const del = o.delete / 2, ret = o.return / \"'\";\n",
];

/// 几段随机的代码，换行符统一成 `newline`
fn around(rng: &mut Rng, newline: &str) -> String {
    (0..rng.below(4))
        .map(|_| *rng.pick(AROUND))
        .collect::<String>()
        .replace('\n', newline)
}

/// 随机的 manifest 放在随机的代码中间：找得到、读回来一模一样、范围正好是写进去的那一段
#[test]
fn random_manifests_are_found_in_any_surrounding_code() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for i in 0..3000 {
        let d = rng.object();
        let indent = *rng.pick(&["", "  ", "\t"]);
        let newline = *rng.pick(&["\n", "\r\n"]);
        let before = format!(
            "{}{indent}export const manifest = ",
            around(&mut rng, newline)
        );
        let text = write(&d, indent, newline);
        let src = format!("{before}{text};{newline}{}", around(&mut rng, newline));
        let l = find(&src).unwrap_or_else(|e| panic!("#{i}: {e:?}\n{src}"));
        assert_eq!(l.data, d, "#{i}\n{src}");
        assert_eq!(
            l.span,
            before.len()..before.len() + text.len(),
            "#{i}\n{src}"
        );
        // 再写一遍还是同样的字节
        assert_eq!(replace(&src, &l, &l.data), src, "#{i}");
    }
}

/// 一份随机的、合规矩的 manifest：名字、权限，随机几个设置项（有的写了值、有的没写），
/// 有时带着范围和出错时怎么办
fn random_manifest(rng: &mut Rng) -> Data {
    let mut m = vec![
        (
            "name".to_string(),
            Data::String(format!("p{}", rng.below(100))),
        ),
        ("api".to_string(), Data::Number(1.0)),
        (
            "permissions".to_string(),
            Data::Array(vec![Data::String("system".into())]),
        ),
    ];
    if rng.chance(30) {
        m.push((
            "match".into(),
            Data::Object(vec![(
                "models".into(),
                Data::Array(vec![Data::String("claude-*".into())]),
            )]),
        ));
    }
    if rng.chance(30) {
        m.push(("on_error".into(), Data::String("skip".into())));
    }
    let mut settings = Vec::new();
    for i in 0..rng.below(5) {
        let kind = *rng.pick(&["string", "number", "boolean"]);
        let mut spec = vec![
            ("type".to_string(), Data::String(kind.into())),
            ("label".to_string(), Data::String(rng.text())),
        ];
        if rng.chance(60) {
            spec.push(("value".into(), random_value(rng, kind)));
        }
        settings.push((format!("s{i}"), Data::Object(spec)));
    }
    m.push(("settings".into(), Data::Object(settings)));
    Data::Object(m)
}

fn random_value(rng: &mut Rng, kind: &str) -> Data {
    match kind {
        "string" => Data::String(rng.text()),
        "number" => Data::Number(rng.number()),
        _ => Data::Bool(rng.chance(50)),
    }
}

fn random_pattern(rng: &mut Rng) -> String {
    let p: String = (0..1 + rng.below(3))
        .map(|_| *rng.pick(&["claude", "*", "-", "gpt", "中文", "a b", "\"", "\\"]))
        .collect();
    if p.trim().is_empty() { "x".into() } else { p }
}

/// 随机的合规改写：字面量以外一个字节都不动；读回来就是改成的那样；和原来是同一份代码；
/// 同样的改写再做一遍什么都不变
#[test]
fn random_rewrites_touch_nothing_but_the_manifest() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for i in 0..2000 {
        let d = random_manifest(&mut rng);
        let newline = *rng.pick(&["\n", "\r\n"]);
        let before = format!("{}export const manifest = ", around(&mut rng, newline));
        let src = format!(
            "{before}{};{newline}{}",
            write(&d, "", newline),
            around(&mut rng, newline)
        );
        let original = lit(&src);
        let old = original.values().unwrap_or_else(|| panic!("#{i}\n{src}"));
        let mut want = old.clone();
        want.on_error = *rng.pick(&[OnError::Reject, OnError::Skip]);
        want.scope = Scope {
            clients: (0..rng.below(3))
                .map(|_| random_pattern(&mut rng))
                .collect(),
            models: (0..rng.below(3))
                .map(|_| random_pattern(&mut rng))
                .collect(),
            upstreams: (0..rng.below(3))
                .map(|_| random_pattern(&mut rng))
                .collect(),
        };
        let mut edits = Values {
            on_error: want.on_error,
            scope: want.scope.clone(),
            settings: Vec::new(),
        };
        for (k, v) in want.settings.iter_mut() {
            if rng.chance(50) {
                let kind = match v {
                    Value::String(_) => "string",
                    Value::Number(_) => "number",
                    _ => "boolean",
                };
                *v = random_value(&mut rng, kind).to_json();
                edits.settings.push((k.clone(), v.clone()));
            }
        }
        let out = rewrite(&src, &edits).unwrap_or_else(|e| panic!("#{i}: {e:?}\n{src}"));
        let after = lit(&out);
        assert_eq!(
            out[..after.span.start],
            src[..original.span.start],
            "#{i}\n{out}"
        );
        assert_eq!(
            out[after.span.end..],
            src[original.span.end..],
            "#{i}\n{out}"
        );
        let got = after.values().unwrap_or_else(|| panic!("#{i}\n{out}"));
        assert_eq!(got.on_error, want.on_error, "#{i}");
        assert_eq!(got.scope, want.scope, "#{i}");
        assert_eq!(got.settings.len(), want.settings.len(), "#{i}");
        for ((gk, gv), (wk, wv)) in got.settings.iter().zip(&want.settings) {
            assert_eq!(gk, wk, "#{i}");
            assert!(crate::js_equal(gv, wv), "#{i}: {gk}: {gv} != {wv}");
        }
        assert!(same_code(&src, &out), "#{i}\n{src}\n{out}");
        assert_eq!(rewrite(&out, &edits).unwrap(), out, "#{i}");
    }
}

/// 什么样的输入都不 panic：随机的字符拼起来的、有效的源码随机删改几个字符的
#[test]
fn garbage_never_panics() {
    const ALPHABET: &[&str] = &[
        "export",
        " ",
        "const",
        "manifest",
        "=",
        "{",
        "}",
        "[",
        "]",
        "(",
        ")",
        ":",
        ",",
        ";",
        "\"",
        "'",
        "`",
        "${",
        "/",
        "*",
        "\\",
        "\n",
        "\r",
        "-",
        "+",
        ".",
        "0",
        "1",
        "e",
        "x",
        "_",
        "n",
        "a",
        "中",
        "\u{2028}",
        "\u{feff}",
        "#",
        "!",
        "?",
        "...",
        "get",
        "__proto__",
        "\\u{",
        "\\uD83D",
        "\\x4",
        "true",
        "null",
    ];
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let values = Values {
        on_error: OnError::Skip,
        scope: Scope {
            clients: vec!["c".into()],
            models: Vec::new(),
            upstreams: Vec::new(),
        },
        settings: vec![("s0".into(), json!("v"))],
    };
    for _ in 0..20_000 {
        let s: String = (0..rng.below(40)).map(|_| *rng.pick(ALPHABET)).collect();
        let _ = find(&s);
        let _ = rewrite(&s, &values);
        let _ = same_code(&s, PLUGIN);
    }
    // 有效的源码，随机删、插、换几个字符
    for i in 0..5000 {
        let base = if i % 2 == 0 {
            PLUGIN.to_string()
        } else {
            let d = random_manifest(&mut rng);
            format!(
                "{}export const manifest = {};\n",
                around(&mut rng, "\n"),
                write(&d, "", "\n")
            )
        };
        let mut chars: Vec<char> = base.chars().collect();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(chars.len() + 1);
            match rng.below(3) {
                0 if at < chars.len() => {
                    chars.remove(at);
                }
                1 => {
                    let piece = *rng.pick(ALPHABET);
                    for (j, c) in piece.chars().enumerate() {
                        chars.insert(at + j, c);
                    }
                }
                _ if at < chars.len() => {
                    chars[at] = rng.pick(ALPHABET).chars().next().unwrap_or('x');
                }
                _ => {}
            }
        }
        let s: String = chars.into_iter().collect();
        if let Ok(l) = find(&s) {
            // 读得出来的，写回去再读还是它
            let again = replace(&s, &l, &l.data);
            assert_eq!(find(&again).map(|x| x.data), Ok(l.data.clone()), "{s}");
        }
        let _ = rewrite(&s, &values);
        let _ = same_code(&s, &base);
    }
    // 很深的嵌套：报错，不爆栈
    let deep = format!(
        "export const manifest = {{ a: {}1{} }};",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    let e = find(&deep).unwrap_err();
    assert!(e.message.contains("nested too deeply"), "{e:?}");
}

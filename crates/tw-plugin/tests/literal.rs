//! 改写过的插件照样在真的沙箱里加载（契约附录四）：随机的合规改写（出错时怎么办、范围、
//! 设置的值）写回去，编出来的 manifest 就是改成的那样、别的字段一样不差，文件里 manifest
//! 以外的字节一个不动。
//!
//! 改写本身的性质（随机的代码里找得准、什么输入都不 panic）在 `literal` 模块自己的测试里；
//! 这里补的是「读回来」用的是真的沙箱，不是同一个解析器自说自话。

mod common;

use common::*;
use serde_json::{Value, json};
use tw_plugin::OnError;
use tw_plugin::literal::{self, Values};

const SOURCE: &str = r#"// 附加一句话
//
// manifest 外面的代码和注释：改写之后一个字节都不变。
const PREFIX = "export const manifest = { name: \"fake\" }"; // 字符串里的不算
const DIVIDE = (a, b) => a / b / 2;

export const manifest = {
  name: "Append a line", // 名字
  api: 1,
  description: "Appends a line to the system prompt.",
  permissions: ["system"],
  /* 设置：三种类型各一个 */
  settings: {
    line: { type: "string", label: "Line", value: "Be brief." },
    times: { type: "number", label: "Times" },
    loud: { type: "boolean", label: "Loud", value: false },
  },
};

export function onRequest(req, ctx) {
  const line = ctx.settings.loud ? ctx.settings.line.toUpperCase() : ctx.settings.line;
  req.system = `${req.system}\n${line.repeat(Math.max(1, DIVIDE(ctx.settings.times, 0.5)))}`;
  return req;
}
"#;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn text(&mut self) -> String {
        const PIECES: &[&str] = &[
            "a",
            "中文",
            " ",
            "\"",
            "'",
            "`",
            "\\",
            "\n",
            "${x}",
            "}",
            "*/",
            "\u{2028}",
            "\u{1F600}",
            "export const manifest = {",
        ];
        (0..self.below(6)).map(|_| *self.pick(PIECES)).collect()
    }
    fn pattern(&mut self) -> String {
        let p: String = (0..1 + self.below(3))
            .map(|_| *self.pick(&["claude", "*", "-", "gpt", "中文", "\"", "\\"]))
            .collect();
        p
    }
    fn patterns(&mut self) -> Vec<String> {
        (0..self.below(3)).map(|_| self.pattern()).collect()
    }
}

#[test]
fn rewritten_plugins_load_in_the_sandbox_with_exactly_the_new_values() {
    let original = load_source(SOURCE);
    let base = original.manifest().clone();
    let at = literal::find(SOURCE).unwrap();
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut src = SOURCE.to_string();
    for i in 0..150 {
        let values = Values {
            on_error: *rng.pick(&[OnError::Reject, OnError::Skip]),
            scope: tw_plugin::Scope {
                clients: rng.patterns(),
                models: rng.patterns(),
                upstreams: rng.patterns(),
            },
            settings: vec![
                ("line".into(), json!(rng.text())),
                (
                    "times".into(),
                    json!(*rng.pick(&[0.0, 1.0, 2.5, -3.0, 1e-7, 123456.789])),
                ),
                ("loud".into(), json!(rng.below(2) == 0)),
            ],
        };
        // 每一轮在上一轮改写过的那一份上接着改：排版已经是写出来的样子了，照样只动该动的
        let next = literal::rewrite(&src, &values).unwrap_or_else(|e| panic!("#{i}: {e:?}"));
        let now = literal::find(&next).unwrap();
        assert_eq!(next[..now.span.start], SOURCE[..at.span.start], "#{i}");
        assert_eq!(next[now.span.end..], SOURCE[at.span.end..], "#{i}");

        let p = rt()
            .load(next.as_bytes())
            .unwrap_or_else(|e| panic!("#{i}: {e:?}\n{next}"));
        let m = p.manifest();
        assert_eq!(m.on_error, values.on_error, "#{i}");
        assert_eq!(m.scope, values.scope, "#{i}");
        let got: Vec<(String, Value)> = m
            .settings
            .iter()
            .map(|s| (s.key.clone(), s.value.clone()))
            .collect();
        assert_eq!(got.len(), values.settings.len(), "#{i}");
        for ((gk, gv), (wk, wv)) in got.iter().zip(&values.settings) {
            assert_eq!(gk, wk, "#{i}");
            assert!(tw_plugin::js_equal(gv, wv), "#{i}: {gk}: {gv} != {wv}");
        }
        // 别的一样不差
        assert_eq!(m.name, base.name, "#{i}");
        assert_eq!(m.description, base.description, "#{i}");
        assert_eq!(m.permissions, base.permissions, "#{i}");
        assert_eq!(m.hooks, base.hooks, "#{i}");
        assert_eq!(m.reply_mode, base.reply_mode, "#{i}");
        let labels: Vec<_> = m
            .settings
            .iter()
            .map(|s| (&s.key, s.kind, &s.label))
            .collect();
        let want: Vec<_> = base
            .settings
            .iter()
            .map(|s| (&s.key, s.kind, &s.label))
            .collect();
        assert_eq!(labels, want, "#{i}");
        assert!(literal::same_code(SOURCE, &next), "#{i}");
        src = next;
    }
}

/// 改写出来的设置值真的交到钩子手里
#[test]
fn a_rewritten_value_is_what_the_hook_sees() {
    let next = literal::rewrite(
        SOURCE,
        &Values {
            settings: vec![
                ("line".into(), json!("Answer in English.")),
                ("times".into(), json!(2)),
                ("loud".into(), json!(true)),
            ],
            ..Values::default()
        },
    )
    .unwrap();
    let p = load_source(&next);
    let settings: serde_json::Map<String, Value> = p
        .manifest()
        .settings
        .iter()
        .map(|s| (s.key.clone(), s.value.clone()))
        .collect();
    let inv = p.on_request(
        json!({"format": "anthropic", "model": "m", "system": "Hi."}),
        ctx(Value::Object(settings)),
    );
    match inv.result {
        Ok(tw_plugin::RequestOutcome::Changed(v)) => {
            assert_eq!(v["system"], "Hi.\nANSWER IN ENGLISH.ANSWER IN ENGLISH.")
        }
        other => panic!("{other:?}"),
    }
}

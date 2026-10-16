//! 插件辅助函数 `tw`，端到端：真的插件跑在真的沙箱里，请求从客户端到假上游走一整圈。
//!
//! - 修改传入的 `req` 即生效：不返回，上游收到的也是改过的那一份；什么都没改就记成没改。
//! - 逐段替换（`tw.streamReplace`）：假上游一个字一帧，客户端拿到的和整包时一样。
//! - 回答里的密钥：插件看到的是占位符，替换规则碰不到它、也不会把它拆开，客户端照样拿回
//!   原值。

mod plugin_harness;

use plugin_harness::*;
use serde_json::{Value, json};
use tw_config::{RedactPolicy, Security, SecurityMode};

/// 用户粘进对话里的那把 key（出站脱敏的 anthropic-api-key 规则认得它）
const USER_KEY: &str = "sk-ant-api03-USERSOWNKEYAAAAAAAAAAAAAA";

fn body(text: &str, stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-4-5", "max_tokens": 64, "stream": stream,
        "system": "你是助手。",
        "messages": [{ "role": "user", "content": text }]
    })
}

fn text_of(r: &Resp, stream: bool) -> String {
    if stream {
        sse_text(&r.body)
    } else {
        json_text(&r.body)
    }
}

const SPELLING: &str = r#"
export const manifest = {
  name: "American spelling",
  api: 1,
  permissions: ["system", "messages"],
};

export function onRequest(req) {
  tw.appendSystem(req, "Use American spelling.");
  tw.texts(req, (t, place) => (place.kind === "text" ? tw.replace(t, [["colour", "color"]]) : undefined));
}
"#;

#[tokio::test]
async fn request_edits_count_without_a_return() {
    let up = Upstream::start(vec![Answer::Text("ok".into())]).await;
    let gw = Gateway::start(
        config(&up, Security::default()),
        vec![Plug::new("spelling", SPELLING)],
    )
    .await;
    let r = gw.ask(body("what colour is it", false)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let sent = up.body(0);
    assert_eq!(sent["system"], "你是助手。\n\nUse American spelling.");
    assert_eq!(sent["messages"][0]["content"], "what color is it");
    assert_eq!(gw.outcomes("spelling"), ["changed"]);
}

const LOOK: &str = r#"
export const manifest = { name: "look", api: 1, permissions: ["system", "messages"] };

export function onRequest(req) {
  let n = 0;
  tw.texts(req, (t) => { n += t.length; });
  tw.toolCalls(req, () => {});
  console.log(String(n));
}
"#;

#[tokio::test]
async fn a_request_hook_that_only_reads_changes_nothing() {
    let up = Upstream::start(vec![Answer::Text("ok".into())]).await;
    let gw = Gateway::start(
        config(&up, Security::default()),
        vec![Plug::new("look", LOOK)],
    )
    .await;
    let raw = r#"{"model":"claude-sonnet-4-5","max_tokens":64,"temperature":1.0,"system":"你是助手。","messages":[{"role":"user","content":"hi"}]}"#;
    let r = gw.post_raw("/v1/messages", raw).await;
    assert_eq!(r.status, 200, "{}", r.body);
    // 一个字节都不动（连 1.0 的写法都在）
    assert_eq!(up.raw(0), raw);
    assert_eq!(gw.outcomes("look"), ["unchanged"]);
}

const TERMS: &str = r#"
export const manifest = {
  name: "Terms",
  api: 1,
  permissions: ["reply.text"],
  reply: "stream",
  settings: {
    terms: { type: "string", label: "Terms", value: "" },
  },
};

export const { onReplyText, onReplyTextEnd } = tw.streamReplace((ctx) => [
  ...tw.pairs(ctx.settings.terms),
  [/\d{3,}/, '#'],
]);
"#;

#[tokio::test]
async fn stream_replace_gives_the_same_answer_one_character_at_a_time() {
    for stream in [false, true] {
        let up = Upstream::start(vec![Answer::Text(
            "my favourite colour is 12345, the colour of 7 seas".into(),
        )])
        .await;
        let gw = Gateway::start(
            config(&up, Security::default()),
            vec![
                Plug::new("terms", TERMS)
                    .settings(json!({ "terms": "colour => color\nfavourite => favorite" })),
            ],
        )
        .await;
        let r = gw.ask(body("hi", stream)).await;
        assert_eq!(r.status, 200, "{stream}: {}", r.body);
        assert_eq!(
            text_of(&r, stream),
            "my favorite color is #, the color of 7 seas",
            "{stream}"
        );
    }
}

const SAFE: &str = r#"
export const manifest = { name: "safe", api: 1, permissions: ["reply.text"], reply: "stream" };

// 每一条都能咬住占位符里的字：占位符整个跳过，它们一个也咬不到
export const { onReplyText, onReplyTextEnd } = tw.streamReplace([
  ["SECRET", "x"],
  ["<<", "«"],
  ["TW_", "tw"],
  [/\d+>>/, "?"],
  ["key", "KEY"],
]);
"#;

#[tokio::test]
async fn stream_replace_leaves_secrets_whole_and_the_client_gets_its_key_back() {
    for mode in [SecurityMode::Enforce, SecurityMode::Observe] {
        for stream in [false, true] {
            // 上游把它看到的那串原样说一遍：拦截档下是占位符，观察档下是真 key
            let up = Upstream::start(vec![Answer::Echo]).await;
            let gw = Gateway::start(
                config(
                    &up,
                    Security {
                        redact: RedactPolicy {
                            mode,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ),
                vec![Plug::new("safe", SAFE)],
            )
            .await;
            let r = gw
                .ask(body(&format!("my key is {USER_KEY} << ok"), stream))
                .await;
            assert_eq!(r.status, 200, "{mode:?}/{stream}: {}", r.body);
            assert_eq!(
                text_of(&r, stream),
                format!("my KEY is {USER_KEY} « ok"),
                "{mode:?}/{stream}"
            );
            assert!(
                !r.body.contains("<<TW_"),
                "{mode:?}/{stream}: a placeholder reached the client: {}",
                r.body
            );
        }
    }
}

// ── 记录里的「改了文字」 ─────────────────────────────────────────
//
// 逐段插件扣字是常事：按块比，这一块里插件交出的全部文字拼起来和交给它的全部文字拼起来
// 不一样，才算改了文字

const NEVER: &str = r#"
export const manifest = { name: "never", api: 1, permissions: ["reply.text"], reply: "stream" };

// 有正则：每段都扣着末尾 maxMatch 个字，块结束才放出
export const { onReplyText, onReplyTextEnd } = tw.streamReplace([[/\d{3}/, '#'], ["zebra", "horse"]]);
"#;

const HOARD: &str = r#"
export const manifest = { name: "hoard", api: 1, permissions: ["reply.text"], reply: "stream" };

let held = "";
export function onReplyText(t) { held += t; return ""; }
export function onReplyTextEnd() { const out = held; held = ""; return out; }
"#;

const SHOUT_AT_THE_END: &str = r#"
export const manifest = { name: "shout", api: 1, permissions: ["reply.text"], reply: "stream" };

let held = "";
export function onReplyText(t) { held += t; return ""; }
export function onReplyTextEnd() { const out = held; held = ""; return out.toUpperCase(); }
"#;

const SAME_BLOCK: &str = r#"
export const manifest = { name: "same", api: 1, permissions: ["reply.text"] };

export function onReplyText(t) { return t.replace("zebra", "horse"); }
"#;

#[tokio::test]
async fn holding_text_back_without_changing_it_is_not_a_change() {
    for stream in [true, false] {
        for (id, source, text, want) in [
            // 规则一处都没命中
            ("never", NEVER, "plain words, 12 of them", "unchanged"),
            ("never", NEVER, "a zebra and 123", "changed"),
            // 整块扣到最后原样放出
            ("hoard", HOARD, "plain words", "unchanged"),
            ("shout", SHOUT_AT_THE_END, "plain words", "changed"),
            // 整块模式
            ("same", SAME_BLOCK, "plain words", "unchanged"),
            ("same", SAME_BLOCK, "a zebra", "changed"),
        ] {
            let up = Upstream::start(vec![Answer::Text(text.into())]).await;
            let gw = Gateway::start(
                config(&up, Security::default()),
                vec![Plug::new(id, source)],
            )
            .await;
            let r = gw.ask(body("hi", stream)).await;
            assert_eq!(r.status, 200, "{id}/{stream}: {}", r.body);
            assert_eq!(gw.outcomes(id), [want], "{id}/{stream}: {text}");
        }
    }
}

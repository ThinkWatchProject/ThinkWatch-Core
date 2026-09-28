//! 拆帧（这里）和读帧（`tw_dialect`）是分开写的，靠一条约定对上：`:event-type`
//! 进 `event:`，载荷进 `data:`。这个测试从线上的二进制字节一路走到客户端的格式 ——
//! 哪一边改了形状，它就断。

use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use tw_bedrock::eventstream::Transcoder;
use tw_dialect::convert::decode;
use tw_dialect::ir::{Dialect, Target};

fn event(kind: &str, payload: &str) -> Vec<u8> {
    let m = Message::new(payload.as_bytes().to_vec())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(kind.to_string().into()),
        ))
        .add_header(Header::new(
            ":content-type",
            HeaderValue::String("application/json".into()),
        ));
    let mut out = Vec::new();
    write_message_to(&m, &mut out).unwrap();
    out
}

#[test]
fn a_converse_stream_reaches_a_chat_client_with_its_text_and_usage() {
    let wire = [
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"sun"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"ny"}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
        event(
            "metadata",
            r#"{"usage":{"inputTokens":60,"cacheReadInputTokens":40,"outputTokens":20,"totalTokens":120}}"#,
        ),
    ]
    .concat();

    // 客户端说 Chat，被路由到 Bedrock
    let body = serde_json::json!({
        "model": "anthropic.claude",
        "stream": true,
        "messages": [{"role": "user", "content": "weather?"}],
    });
    let converted = decode(Dialect::Chat, &body, "/v1/chat/completions", None)
        .unwrap()
        .encode(&Target {
            dialect: Dialect::Bedrock,
            official: true,
            default_max_tokens: 4096,
        });
    let mut to_client = converted.session.stream();
    let mut sniffer = tw_dialect::usage::Sniffer::new();

    // 七个字节一块，帧的边界落在哪儿都有
    let mut transcoder = Transcoder::new();
    let mut chat = Vec::new();
    for chunk in wire.chunks(7) {
        let sse = transcoder.feed(chunk).unwrap();
        sniffer.feed(&sse);
        chat.extend(to_client.process(&sse));
    }
    chat.extend(to_client.finish());
    let chat = String::from_utf8(chat).unwrap();

    let text: String = chat
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(text, "sunny");
    assert!(chat.contains(r#""finish_reason":"stop""#), "{chat}");

    // 用量在转成客户端格式之前、拆完帧的 SSE 上就读得出来
    let usage = sniffer.finish().expect("usage");
    assert_eq!((usage.input, usage.cache_read, usage.output), (60, 40, 20));
}

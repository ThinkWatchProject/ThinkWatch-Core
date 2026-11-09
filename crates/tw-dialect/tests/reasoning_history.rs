//! Reasoning histories must survive real client carriers, not just a text decode.
use serde_json::{Value, json};
use tw_dialect::{
    convert,
    history::{ChatHistory, HistoryFormat as F, HistoryTarget},
    ir::*,
};

fn target(format: F, origin: &str) -> (Target, HistoryTarget) {
    (
        Target {
            dialect: Dialect::Chat,
            official: false,
            default_max_tokens: 8192,
        },
        HistoryTarget {
            format,
            origin: Some(origin.into()),
        },
    )
}

fn prepare(
    client: Dialect,
    body: &[u8],
    path: &str,
    query: Option<&str>,
    target: &(Target, HistoryTarget),
) -> Result<convert::Prepared, Rejection> {
    convert::prepare_with_history(client, body, path, query, &target.0, &target.1)
}

fn encode(request: &Request, target: &(Target, HistoryTarget)) -> convert::Prepared {
    convert::encode_with_history(request, &target.0, &target.1)
}

fn initial(client: Dialect, stream: bool) -> (Value, &'static str) {
    match client {
        Dialect::Anthropic => (
            json!({"model":"m","max_tokens":1024,"stream":stream,"messages":[{"role":"user","content":"hi"}]}),
            "/v1/messages",
        ),
        Dialect::Responses => (
            json!({"model":"m","stream":stream,"input":[{"role":"user","content":"hi"}]}),
            "/v1/responses",
        ),
        Dialect::Gemini => (
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
            if stream {
                "/v1beta/models/m:streamGenerateContent"
            } else {
                "/v1beta/models/m:generateContent"
            },
        ),
        Dialect::Bedrock => (
            json!({"messages":[{"role":"user","content":[{"text":"hi"}]}]}),
            if stream {
                "/model/m/converse-stream"
            } else {
                "/model/m/converse"
            },
        ),
        _ => unreachable!(),
    }
}

fn followup(client: Dialect, mut request: Value, reply: &Value) -> Value {
    match client {
        Dialect::Anthropic => {
            request["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":reply["content"]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"done"}]}),
            ]);
        }
        Dialect::Responses => {
            request["input"]
                .as_array_mut()
                .unwrap()
                .extend(reply["output"].as_array().unwrap().clone());
            request["input"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"function_call_output","call_id":"call_1","output":"done"}));
        }
        Dialect::Gemini => {
            request["contents"].as_array_mut().unwrap().extend([
                reply["candidates"][0]["content"].clone(),
                json!({"role":"user","parts":[{"functionResponse":{"id":"call_1","name":"f","response":{"result":"done"}}}]}),
            ]);
        }
        Dialect::Bedrock => {
            request["messages"].as_array_mut().unwrap().extend([
                reply["output"]["message"].clone(),
                json!({"role":"user","content":[{"toolResult":{"toolUseId":"call_1","content":[{"text":"done"}]}}]}),
            ]);
        }
        _ => unreachable!(),
    }
    request
}

fn content() -> Value {
    json!([
        {"type":"thinking","thinking":[{"type":"text","text":"first thought"}],"signature":"opaque.one","closed":true},
        {"type":"text","text":"first answer"},
        {"type":"thinking","thinking":[{"type":"text","text":"second thought"}],"signature":"opaque.two","closed":true},
        {"type":"text","text":"second answer"}
    ])
}

fn tool() -> Value {
    json!({"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}})
}
fn response(native: Value) -> Value {
    json!({"id":"r","model":"m","choices":[{"message":{"role":"assistant","content":native,"tool_calls":[tool()]},"finish_reason":"tool_calls"}]})
}

fn check_next(client: Dialect, request: Value, path: &str, reply: Value, wanted: Value) {
    let next = followup(client, request, &reply);
    let prepared = prepare(
        client,
        next.to_string().as_bytes(),
        path,
        Some("alt=sse"),
        &target(F::Auto, "source"),
    )
    .unwrap();
    assert!(!prepared.incompatible_history, "{client:?}: {next}");
    let sent: Value = serde_json::from_slice(&prepared.body).unwrap();
    let assistant = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], wanted, "{client:?}");
    assert!(assistant.get("reasoning_content").is_none(), "{assistant}");
    assert!(assistant.get("reasoning").is_none(), "{assistant}");
    assert_eq!(assistant["tool_calls"][0], tool());
}

#[test]
fn native_blocks_and_signatures_roundtrip_through_every_signed_client_carrier() {
    for client in [
        Dialect::Anthropic,
        Dialect::Responses,
        Dialect::Gemini,
        Dialect::Bedrock,
    ] {
        let (request, path) = initial(client, false);
        let prepared = prepare(
            client,
            request.to_string().as_bytes(),
            path,
            None,
            &target(F::Auto, "source"),
        )
        .unwrap();
        let reply: Value = serde_json::from_slice(
            &prepared
                .session
                .response(response(content()).to_string().as_bytes())
                .unwrap(),
        )
        .unwrap();
        check_next(client, request, path, reply, content());
    }
}

fn frame(delta: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"r","model":"m","choices":[{"delta":delta,"finish_reason":null}]})
    )
}

#[test]
fn streamed_blocks_keep_signature_only_fragments_without_buffering_the_answer() {
    for client in [
        Dialect::Anthropic,
        Dialect::Responses,
        Dialect::Gemini,
        Dialect::Bedrock,
    ] {
        for chunk in [1, 13, 4096] {
            let (request, path) = initial(client, true);
            let prepared = prepare(
                client,
                request.to_string().as_bytes(),
                path,
                Some("alt=sse"),
                &target(F::Auto, "source"),
            )
            .unwrap();
            let mut stream = prepared.session.stream();
            let mut output = Vec::new();
            let fragments = [
                json!({"content":[{"type":"thinking","thinking":[{"type":"text","text":"first "}]}]}),
                json!({"content":[{"type":"thinking","thinking":[{"type":"text","text":"thought"}]}]}),
                json!({"content":[{"type":"thinking","thinking":[],"signature":"opaque.one","closed":true}]}),
                json!({"content":[{"type":"text","text":"first answer"}]}),
                json!({"content":[{"type":"thinking","thinking":[{"type":"text","text":"second thought"}],"signature":"opaque.two","closed":true}]}),
                json!({"content":[{"type":"text","text":"second answer"}]}),
                json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]}),
            ];
            for (i, fragment) in fragments.into_iter().enumerate() {
                for bytes in frame(fragment).as_bytes().chunks(chunk) {
                    output.extend(stream.process(bytes));
                }
                if i == 3 || (i == 0 && client != Dialect::Gemini) {
                    let expected = if i == 3 { "first answer" } else { "first " };
                    assert!(
                        String::from_utf8_lossy(&output).contains(expected),
                        "{client:?} must emit actual content before the upstream finishes"
                    );
                }
            }
            output.extend(stream.process(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"));
            output.extend(stream.finish());
            // Consume exactly what a streaming client received, through its native parser.
            let mut own = target(F::Auto, "unused");
            own.0.dialect = client;
            let consumer = prepare(
                client,
                request.to_string().as_bytes(),
                path,
                Some("alt=sse"),
                &own,
            )
            .unwrap();
            let mut collector = consumer.session.collector();
            for bytes in output.chunks(chunk) {
                collector.process(bytes);
            }
            let reply: Value = serde_json::from_slice(&collector.finish().unwrap()).unwrap();
            check_next(client, request, path, reply, content());
        }
    }
}

#[test]
fn explicit_capabilities_repair_old_histories_and_preserve_distinct_field_names() {
    let request = json!({"model":"m","input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"old thought"}],"encrypted_content":"tw1.ch."},{"role":"assistant","content":"answer"}]});
    for (format, key) in [
        (F::ReasoningContent, "reasoning_content"),
        (F::Reasoning, "reasoning"),
        (F::ContentBlocks, "content"),
        (F::Disabled, "none"),
    ] {
        let prepared = prepare(
            Dialect::Responses,
            request.to_string().as_bytes(),
            "/v1/responses",
            None,
            &target(format, "source"),
        )
        .unwrap();
        let sent: Value = serde_json::from_slice(&prepared.body).unwrap();
        let message = &sent["messages"][0];
        if key == "content" {
            assert_eq!(message["content"][0]["type"], "thinking");
            assert_eq!(message["content"][0]["thinking"][0]["text"], "old thought");
        } else if key != "none" {
            assert_eq!(message[key], "old thought");
        } else {
            assert!(message.get("reasoning_content").is_none());
            assert!(!prepared.dropped.is_empty());
        }
    }
}

#[test]
fn signed_history_is_rejected_before_incompatible_replay() {
    let mut history = ChatHistory::content(&content()[0]);
    history.origin = Some("source".into());
    let mut request = Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::Assistant,
            parts: vec![
                Part::Thinking(Thinking {
                    text: "first thought".into(),
                    signature: Some(history.signature()),
                }),
                Part::Text("answer".into()),
            ],
        }],
        ..Default::default()
    };
    for target in [
        target(F::ContentBlocks, "other"),
        target(F::ReasoningContent, "source"),
        target(F::Disabled, "source"),
    ] {
        assert!(encode(&request, &target).incompatible_history);
    }
    assert!(!encode(&request, &target(F::ContentBlocks, "source")).incompatible_history);
    // Unsigned source history can be omitted with the normal dropped-feature diagnostic.
    history
        .block
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("signature");
    if let Part::Thinking(th) = &mut request.messages[0].parts[0] {
        th.signature = Some(history.signature());
    }
    let prepared = encode(&request, &target(F::Auto, "other"));
    assert!(!prepared.incompatible_history);
    assert!(!prepared.dropped.is_empty());
}

#[test]
fn native_requests_keep_unrelated_fields_and_avoid_unnecessary_reserialization() {
    let body=br#"{ "model":"m", "custom":{"keep":true}, "messages":[{"role":"assistant","content":"answer","reasoning_content":"thought","custom":42}] }"#;
    assert!(tw_dialect::history::shape_chat(body, F::Auto).is_none());
    assert!(tw_dialect::history::shape_chat(body, F::ReasoningContent).is_none());
    let native = tw_dialect::history::shape_chat(body, F::ContentBlocks).unwrap();
    let sent: Value = serde_json::from_slice(&native).unwrap();
    assert_eq!(sent["custom"]["keep"], true);
    assert_eq!(sent["messages"][0]["custom"], 42);
    assert_eq!(sent["messages"][0]["content"][0]["type"], "thinking");
    assert!(sent["messages"][0].get("reasoning_content").is_none());
}

#[test]
fn signature_only_native_blocks_remain_replayable() {
    let native =
        json!([{"type":"thinking","thinking":[],"signature":"opaque.empty","closed":true}]);
    for client in [
        Dialect::Anthropic,
        Dialect::Responses,
        Dialect::Gemini,
        Dialect::Bedrock,
    ] {
        let (request, path) = initial(client, false);
        let prepared = prepare(
            client,
            request.to_string().as_bytes(),
            path,
            None,
            &target(F::Auto, "source"),
        )
        .unwrap();
        let reply: Value = serde_json::from_slice(
            &prepared
                .session
                .response(response(native.clone()).to_string().as_bytes())
                .unwrap(),
        )
        .unwrap();
        check_next(client, request, path, reply, native.clone());
    }
}

#[test]
fn a_response_reasoning_field_does_not_guess_a_new_request_capability() {
    let decoded=convert::decode(Dialect::Chat,&json!({"model":"m","messages":[{"role":"assistant","content":"answer","reasoning":"thought"}]}),"/v1/chat/completions",None).unwrap();
    let sent: Value =
        serde_json::from_slice(&encode(&decoded.request, &target(F::Auto, "source")).body).unwrap();
    assert_eq!(sent["messages"][0]["reasoning_content"], "thought");
    assert!(sent["messages"][0].get("reasoning").is_none());
    let sent: Value =
        serde_json::from_slice(&encode(&decoded.request, &target(F::Reasoning, "source")).body)
            .unwrap();
    assert_eq!(sent["messages"][0]["reasoning"], "thought");
}

#[test]
fn disabling_native_history_never_leaves_an_empty_assistant_turn() {
    let native = json!({"model":"m","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":[{"type":"text","text":"only thought"}]}]},{"role":"assistant","content":null,"reasoning_content":"tool thought","tool_calls":[tool()]},{"role":"user","content":"next"}]});
    let sent: Value = serde_json::from_slice(
        &tw_dialect::history::shape_chat(native.to_string().as_bytes(), F::Disabled).unwrap(),
    )
    .unwrap();
    assert_eq!(sent["messages"].as_array().unwrap().len(), 2);
    assert!(sent["messages"][0]["content"].is_null());
    assert_eq!(sent["messages"][0]["tool_calls"][0], tool());
    assert_eq!(sent["messages"][1]["role"], "user");
}

#[test]
fn assistant_content_arrays_still_keep_refusals_and_legacy_text_chunks() {
    let request = json!({"model":"m","messages":[{"role":"assistant","content":[{"type":"text","text":"first"},{"type":"refusal","refusal":"refused"},{"text":"legacy"}]}]});
    let decoded = convert::decode(Dialect::Chat, &request, "/v1/chat/completions", None).unwrap();
    let text: Vec<_> = decoded.request.messages[0]
        .parts
        .iter()
        .filter_map(|p| {
            if let Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, ["first", "refused", "legacy"]);
}

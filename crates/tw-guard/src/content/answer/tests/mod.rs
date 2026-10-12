use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

use super::*;
use crate::content::{Action, Match, Outcome, RuleInput, Sides};

/// 码位用 `char::from_u32` 拼：测试里直接写出来的转义，经过某些编辑工具会变成真字符
fn ch(n: u32) -> char {
    char::from_u32(n).unwrap()
}

const ANSWER: Sides = Sides {
    request: false,
    response: true,
};

fn rule<'a>(id: &'a str, pattern: &'a str, matching: Match, action: Action) -> RuleInput<'a> {
    RuleInput {
        id,
        name: id,
        custom: true,
        pattern,
        matching,
        action,
        sides: ANSWER,
        ..RuleInput::default()
    }
}

fn answer(rules: Vec<RuleInput<'_>>) -> Arc<Answer> {
    Answer::build(&Rules::build(rules).unwrap()).expect("rules that check answers")
}

/// 一段正文切成几块喂进去，交回客户端收到的整段
fn streamed(a: &Arc<Answer>, acts: bool, pieces: &[&str]) -> String {
    let mut lane = a.lane(acts);
    let mut out = String::new();
    for p in pieces {
        out.push_str(&lane.push(p));
    }
    out.push_str(&lane.finish());
    out
}

fn whole_text(a: &Arc<Answer>, acts: bool, text: &str) -> String {
    a.lane(acts).whole(text)
}

#[test]
fn only_rules_that_check_answers_are_built_and_none_means_none() {
    let rs = Rules::build([
        RuleInput {
            id: "req",
            name: "req",
            pattern: "x",
            ..RuleInput::default()
        },
        RuleInput {
            id: "cp",
            name: "cp",
            pattern: "U+200B",
            matching: Match::Codepoints,
            sides: ANSWER,
            ..RuleInput::default()
        },
    ])
    .unwrap();
    assert!(
        Answer::build(&rs).is_none(),
        "只查请求的、码位规则都不查回答"
    );
    let a = answer(vec![rule("k", "北极星", Match::Contains, Action::Strip)]);
    assert_eq!(a.hold(), 2, "三个字的关键词扣两个字");
}

#[test]
fn contains_strips_replaces_and_records() {
    let a = answer(vec![
        rule("s", "secret", Match::Contains, Action::Strip),
        RuleInput {
            replacement: "[隐去]",
            ..rule("r", "北极星", Match::Contains, Action::Replace)
        },
        rule("n", "note", Match::Contains, Action::Record),
    ]);
    let text = "a SECRET note about 北极星 and Secret";
    assert_eq!(whole_text(&a, true, text), "a  note about [隐去] and ");
    // 观察档一个字都不改
    assert_eq!(whole_text(&a, false, text), text);
}

#[test]
fn a_match_split_at_every_position_is_still_caught() {
    let a = answer(vec![RuleInput {
        replacement: "***",
        ..rule("r", "北极星计划", Match::Contains, Action::Replace)
    }]);
    let text = "前文北极星计划后文，再来一次北极星计划";
    let want = "前文***后文，再来一次***";
    let cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    for &i in &cuts {
        for &j in cuts.iter().filter(|&&j| j >= i) {
            let out = streamed(&a, true, &[&text[..i], &text[i..j], &text[j..]]);
            assert_eq!(out, want, "切在 {i} 和 {j}");
        }
    }
    // 一个字一块
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(streamed(&a, true, &refs), want);
}

#[test]
fn case_variants_beyond_ascii_are_the_same_word() {
    // 开尔文符号 K 和 k、带重音的大小写、希腊字母的 Σ 和 σ
    let kelvin = ch(0x212A);
    let word = format!("{kelvin}ey Éclair ΣΟΦΙΑ");
    let a = answer(vec![
        rule("k", "key", Match::Contains, Action::Strip),
        rule("e", "éclair", Match::Contains, Action::Strip),
        rule("s", "σοφια", Match::Contains, Action::Strip),
    ]);
    assert_eq!(whole_text(&a, true, &word), "  ");
    // 分大小写的只认写的那样
    let exact = answer(vec![RuleInput {
        case_sensitive: true,
        ..rule("k", "Key", Match::Contains, Action::Strip)
    }]);
    assert_eq!(whole_text(&exact, true, "key Key KEY"), "key  KEY");
}

#[test]
fn regex_rules_hold_back_only_what_they_can_match() {
    let a = answer(vec![rule("t", r"TKT-\d{6}", Match::Regex, Action::Strip)]);
    assert_eq!(a.hold(), 9, "最长十个字，扣九个");
    let b = answer(vec![rule("w", r"\bcat\b", Match::Regex, Action::Strip)]);
    assert_eq!(b.hold(), 3, "要看下一个字的多扣一个");
    let c = answer(vec![rule("u", r"x+", Match::Regex, Action::Strip)]);
    assert_eq!(c.hold(), REGEX_HOLD_MAX, "没有上限的扣满");
    // `\b` 要看下一个字：`cat` 后面紧跟着字母就不算，切在哪儿都一样
    let text = "cat catalog cat. concat cat";
    for i in 0..=text.len() {
        assert_eq!(
            streamed(&b, true, &[&text[..i], &text[i..]]),
            " catalog . concat ",
            "切在 {i}"
        );
    }
}

#[test]
fn a_match_longer_than_the_cap_may_have_its_start_sent() {
    // 模块说明里明说的边界：没有上限的正则，命中比扣住的长时开头那一截照常发
    let a = answer(vec![rule("x", r"x+", Match::Regex, Action::Strip)]);
    let long = "x".repeat(REGEX_HOLD_MAX * 3);
    let text = format!("a{long}b");
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let out = streamed(&a, true, &refs);
    assert!(out.starts_with('a') && out.ends_with('b'), "{out}");
    assert!(out.len() < text.len(), "后面那一截照样删");
    // 整份的一次删干净
    assert_eq!(whole_text(&a, true, &text), "ab");
    // 有上限的正则流着和整份一样
    let d = answer(vec![rule("d", r"\d{2,4}", Match::Regex, Action::Strip)]);
    let text = "1 22 333 4444 55555 666666";
    let pieces: Vec<String> = text.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(streamed(&d, true, &refs), whole_text(&d, true, text));
}

#[test]
fn overlapping_rules_merge_and_the_replacement_wins() {
    let a = answer(vec![
        rule("s", "星计", Match::Contains, Action::Strip),
        RuleInput {
            replacement: "[X]",
            ..rule("r", "北极星", Match::Contains, Action::Replace)
        },
    ]);
    assert_eq!(whole_text(&a, true, "北极星计划"), "[X]划");
    // 只是挨着的各换各的
    let b = answer(vec![RuleInput {
        replacement: "*",
        ..rule("r", "ab", Match::Contains, Action::Replace)
    }]);
    assert_eq!(whole_text(&b, true, "ababab"), "***");
}

#[test]
fn observe_sends_everything_at_once_and_still_finds_matches_across_pieces() {
    let a = answer(vec![rule("k", "北极星", Match::Contains, Action::Strip)]);
    let mut lane = a.lane(false);
    assert_eq!(lane.push("请看北"), "请看北", "观察档不扣");
    assert_eq!(lane.push("极星。"), "极星。");
    assert_eq!(lane.finish(), "");
    let found = lane.take_found();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].at, "请看".len().."请看北极星".len());
    let p = found[0].place.as_ref().unwrap();
    assert_eq!(
        (p.before.as_str(), p.matched.as_str(), p.after.as_str()),
        ("请看", "北极星", "。")
    );
}

#[test]
fn the_held_tail_comes_out_when_the_lane_ends() {
    let a = answer(vec![rule("k", "secret", Match::Contains, Action::Strip)]);
    let mut lane = a.lane(true);
    assert_eq!(
        lane.push("this is sec"),
        "this i",
        "最后五个字可能是命中的开头：扣着"
    );
    assert_eq!(lane.finish(), "s sec", "到头了也没成命中：照发");
    assert_eq!(lane.push("more"), "more", "结束之后的原样");
}

#[test]
fn context_around_a_match_is_kept_for_the_log() {
    let a = answer(vec![rule("k", "secret", Match::Contains, Action::Strip)]);
    let before = "前".repeat(100);
    let after = "后".repeat(100);
    let text = format!("{before}secret{after}");
    let mut lane = a.lane(true);
    for c in text.chars() {
        lane.push(&c.to_string());
    }
    lane.finish();
    let found = lane.take_found();
    let p = found[0].place.as_ref().unwrap();
    assert_eq!(p.before, "前".repeat(80));
    assert_eq!(p.matched, "secret");
    assert_eq!(p.after, "后".repeat(80));
}

/// 随机的正文、随机的规则、随机地切：流着处理出来的和整份一次处理的一模一样，和直接按
/// 正则找出来再改的也一样；不 panic
#[test]
fn streaming_agrees_with_the_whole_text_for_random_text_rules_and_cuts() {
    let mut seed = 0x51f1_5eed_c0de_2026_u64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let alphabet: Vec<String> = [
        "a", "b", "A", "B", "é", "É", "中", "文", " ", "1", "2", "😀", "ß",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        ch(0x212A).to_string(),
        ch(0x3A3).to_string(),
        ch(0x3C3).to_string(),
    ])
    .collect();
    let regexes = [
        r"a[b1]{1,3}",
        r"\d{2}",
        r"中文?",
        r"(?:ab|ba)é",
        r"\bab\b",
        r"[aA]{2}",
    ];
    for round in 0..400 {
        let word = |next: &mut dyn FnMut(usize) -> usize| -> String {
            (0..1 + next(3))
                .map(|_| alphabet[next(alphabet.len())].as_str())
                .collect()
        };
        let mut patterns: Vec<(String, Match, Action, bool, String)> = Vec::new();
        for _ in 0..1 + next(3) {
            let (p, m) = if next(2) == 0 {
                // 只有空白的关键词编不起来
                let w = word(&mut next);
                (
                    if w.trim().is_empty() {
                        "a".to_string()
                    } else {
                        w
                    },
                    Match::Contains,
                )
            } else {
                (regexes[next(regexes.len())].to_string(), Match::Regex)
            };
            let action = [Action::Strip, Action::Replace, Action::Record][next(3)];
            let with = ["***", "", "x", "中"][next(4)].to_string();
            patterns.push((p, m, action, next(2) == 0, with));
        }
        let names: Vec<String> = (0..patterns.len()).map(|i| format!("r{i}")).collect();
        let inputs: Vec<RuleInput<'_>> = patterns
            .iter()
            .zip(&names)
            .map(|((p, m, action, cs, with), name)| RuleInput {
                case_sensitive: *cs,
                replacement: with,
                ..rule(name, p, *m, *action)
            })
            .collect();
        let rules = Rules::build(inputs).unwrap();
        let a = Answer::build(&rules).unwrap();
        let text: String = (0..next(60))
            .map(|_| alphabet[next(alphabet.len())].as_str())
            .collect();
        let acts = round % 5 != 0;
        let whole = whole_text(&a, acts, &text);
        // 直接按正则在整段上找、合并、改：和引擎认的一样
        if acts {
            let mut edits = Vec::new();
            for (ri, r) in a.rules.iter().enumerate() {
                for m in r.re.find_iter(&text).filter(|m| !m.is_empty()) {
                    if r.action.edits() {
                        edits.push((m.range(), ri));
                    }
                }
            }
            let merged = merge_edits(edits, |ri| a.rules[ri].edit_with());
            assert_eq!(whole, apply_edits(&text, &merged), "{text:?} {patterns:?}");
        } else {
            assert_eq!(whole, text);
        }
        // 随机切成几块（切在字符边界上：一路正文是解析好的字符串）
        let bounds: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        let mut cuts: Vec<usize> = (0..next(8)).map(|_| bounds[next(bounds.len())]).collect();
        cuts.sort_unstable();
        let mut pieces = Vec::new();
        let mut at = 0;
        for c in cuts {
            pieces.push(&text[at..c]);
            at = c;
        }
        pieces.push(&text[at..]);
        let mut lane = a.lane(acts);
        let mut out = String::new();
        for p in &pieces {
            out.push_str(&lane.push(p));
        }
        out.push_str(&lane.finish());
        assert_eq!(out, whole, "{text:?} {pieces:?} {patterns:?}");
        // 认出来的命中也一样多
        let streamed_found = lane.take_found().len();
        let mut once = a.lane(acts);
        once.whole(&text);
        assert_eq!(streamed_found, once.take_found().len(), "{text:?}");
    }
}

// ───────────────────────────────────────────────────────── 按格式

fn named(event: &str, v: Value) -> String {
    format!("event: {event}\ndata: {v}\n\n")
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

/// 一条 SSE 流随机切成小块（切在任意字节上，包括一个字的中间）喂进去，交回整条输出
fn through(body: &mut Body, stream: &str, seed: u64) -> String {
    let bytes = stream.as_bytes();
    let mut seed = seed | 1;
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let n = 1 + (seed % 7) as usize;
        let end = (at + n).min(bytes.len());
        out.extend(body.process(&bytes[at..end]));
        at = end;
    }
    out.extend(body.flush());
    String::from_utf8(out).expect("output stays UTF-8")
}

/// SSE 输出里每一帧的 JSON
fn frames_of(out: &str) -> Vec<Value> {
    out.split("\n\n")
        .filter_map(|f| {
            f.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect()
}

fn star(id: &'static str, pattern: &'static str) -> Arc<Answer> {
    answer(vec![RuleInput {
        replacement: "***",
        ..rule(id, pattern, Match::Contains, Action::Replace)
    }])
}

#[test]
fn anthropic_text_deltas_are_filtered_and_the_tail_comes_before_the_block_stops() {
    let a = star("k", "北极星");
    let delta = |i: u64, t: &str| {
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":t}}),
        )
    };
    let stream = [
        named("message_start", json!({"type":"message_start","message":{"content":[]}})),
        delta(0, "关于北"),
        delta(0, "极星的说明：北"),
        named("content_block_stop", json!({"type":"content_block_stop","index":0})),
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"北极星\"}"}}),
        ),
        named("content_block_stop", json!({"type":"content_block_stop","index":1})),
        named("message_stop", json!({"type":"message_stop"})),
    ]
    .concat();
    for seed in [1, 7, 42, 99] {
        let mut body = Body::new(a.clone(), true, Dialect::Anthropic, true);
        let out = through(&mut body, &stream, seed);
        let frames = frames_of(&out);
        let text: String = frames
            .iter()
            .filter(|f| f["delta"]["type"] == "text_delta")
            .map(|f| f["delta"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, "关于***的说明：北");
        // 扣着的「北」在块结束之前补出来
        let stop = frames
            .iter()
            .position(|f| f["type"] == "content_block_stop")
            .unwrap();
        let last_text = frames
            .iter()
            .rposition(|f| f["delta"]["type"] == "text_delta")
            .unwrap();
        assert!(last_text < stop);
        // 工具参数不碰
        assert!(out.contains(r#"{\"q\":\"北极星\"}"#), "{out}");
        let hits = body.take_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].count, hits[0].outcome), (1, Outcome::Replaced));
        assert_eq!(hits[0].places[0].0, "content[0].text");
    }
}

#[test]
fn chat_deltas_are_filtered_per_choice_and_flushed_before_done() {
    let a = star("k", "secret");
    let chunk = |content: Value, finish: Value| {
        data(
            json!({"id":"c1","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":content,"finish_reason":finish}]}),
        )
    };
    let stream = [
        chunk(json!({"role":"assistant","content":""}), Value::Null),
        chunk(json!({"content":"the sec"}), Value::Null),
        chunk(json!({"content":"ret is sec"}), Value::Null),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let mut body = Body::new(a, true, Dialect::Chat, true);
    let out = through(&mut body, &stream, 3);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "the *** is sec");
    // 扣着的尾巴补成一帧，抄着 id 和模型名，排在 `[DONE]` 前面
    let synth = frames.last().unwrap();
    assert_eq!(synth["id"], "c1");
    assert_eq!(synth["choices"][0]["delta"]["content"], "s sec");
    assert!(out.trim_end().ends_with("data: [DONE]"));
}

fn responses_event(kind: &str, mut v: Value) -> String {
    v["type"] = json!(kind);
    named(kind, v)
}

#[test]
fn responses_done_events_and_the_completed_output_say_what_was_sent() {
    let a = answer(vec![RuleInput {
        replacement: "[X]",
        ..rule("k", "北极星", Match::Contains, Action::Replace)
    }]);
    let full = "北极星项目，北极星。";
    let sent = "[X]项目，[X]。";
    let stream = [
        responses_event("response.created", json!({"response":{"id":"r","output":[]}})),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"北极"}),
        ),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"星项目，北"}),
        ),
        responses_event(
            "response.output_text.delta",
            json!({"item_id":"m1","output_index":0,"content_index":0,"delta":"极星。"}),
        ),
        responses_event(
            "response.output_text.done",
            json!({"item_id":"m1","output_index":0,"content_index":0,"text":full}),
        ),
        responses_event(
            "response.content_part.done",
            json!({"item_id":"m1","output_index":0,"content_index":0,"part":{"type":"output_text","text":full}}),
        ),
        responses_event(
            "response.output_item.done",
            json!({"output_index":0,"item":{"type":"message","id":"m1","content":[{"type":"output_text","text":full}]}}),
        ),
        // 自由格式工具的输入也是一路
        responses_event(
            "response.custom_tool_call_input.delta",
            json!({"item_id":"t1","output_index":1,"delta":"*** Add File: 北极"}),
        ),
        responses_event(
            "response.custom_tool_call_input.delta",
            json!({"item_id":"t1","output_index":1,"delta":"星.md"}),
        ),
        responses_event(
            "response.custom_tool_call_input.done",
            json!({"item_id":"t1","output_index":1,"input":"*** Add File: 北极星.md"}),
        ),
        responses_event(
            "response.output_item.done",
            json!({"output_index":1,"item":{"type":"custom_tool_call","id":"t1","name":"apply_patch","input":"*** Add File: 北极星.md"}}),
        ),
        // 一个增量都没有、只在收尾时出现的一段：在这里查一遍，只算一次
        responses_event(
            "response.completed",
            json!({"response":{"id":"r","output":[
                {"type":"message","id":"m1","content":[{"type":"output_text","text":full}]},
                {"type":"custom_tool_call","id":"t1","input":"*** Add File: 北极星.md"},
                {"type":"message","id":"m2","content":[{"type":"output_text","text":"又是北极星"}]},
            ]}}),
        ),
    ]
    .concat();
    for seed in [5, 11, 23] {
        let mut body = Body::new(a.clone(), true, Dialect::Responses, true);
        let out = through(&mut body, &stream, seed);
        let frames = frames_of(&out);
        let streamed: String = frames
            .iter()
            .filter(|f| f["type"] == "response.output_text.delta")
            .map(|f| f["delta"].as_str().unwrap())
            .collect();
        assert_eq!(streamed, sent);
        let by = |kind: &str| frames.iter().find(|f| f["type"] == kind).unwrap().clone();
        assert_eq!(by("response.output_text.done")["text"], sent);
        assert_eq!(by("response.content_part.done")["part"]["text"], sent);
        assert_eq!(
            by("response.output_item.done")["item"]["content"][0]["text"],
            sent
        );
        let input: String = frames
            .iter()
            .filter(|f| f["type"] == "response.custom_tool_call_input.delta")
            .map(|f| f["delta"].as_str().unwrap())
            .collect();
        assert_eq!(input, "*** Add File: [X].md");
        assert_eq!(
            by("response.custom_tool_call_input.done")["input"],
            "*** Add File: [X].md"
        );
        let done = by("response.completed");
        assert_eq!(done["response"]["output"][0]["content"][0]["text"], sent);
        assert_eq!(
            done["response"]["output"][1]["input"],
            "*** Add File: [X].md"
        );
        assert_eq!(
            done["response"]["output"][2]["content"][0]["text"],
            "又是[X]"
        );
        let hits = body.take_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].count, 4,
            "两处正文、一处工具输入、一处只在收尾里出现的"
        );
        let paths: Vec<&str> = hits[0].places.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "output[0].content[0].text",
                "output[0].content[0].text",
                "output[1].input",
                "output[2].content[0].text"
            ]
        );
    }
}

#[test]
fn gemini_streams_as_sse_and_as_a_json_array() {
    let a = star("k", "secret");
    let chunk = |text: &str, finish: Option<&str>| {
        let mut c = json!({"content":{"role":"model","parts":[{"text":text}]},"index":0});
        if let Some(f) = finish {
            c["finishReason"] = json!(f);
        }
        json!({"candidates":[c],"modelVersion":"g"})
    };
    let thought = json!({"candidates":[{"content":{"parts":[{"text":"secret thoughts","thought":true}]},"index":0}]});
    let parts = [
        thought,
        chunk("my sec", None),
        chunk("ret and sec", None),
        chunk("", Some("STOP")),
    ];
    // SSE
    let sse: String = parts.iter().map(|v| data(v.clone())).collect();
    let mut body = Body::new(a.clone(), true, Dialect::Gemini, true);
    let out = through(&mut body, &sse, 9);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .flat_map(|f| {
            f["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|p| p["thought"] != true)
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        text, "my *** and sec",
        "最后一帧带着 finishReason：扣着的接在它的正文后面"
    );
    assert!(out.contains("secret thoughts"), "思考过程不碰");
    // JSON 数组
    let array = format!(
        "[{}]",
        parts
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",\r\n")
    );
    let mut body = Body::new(a, true, Dialect::Gemini, false);
    let out = through(&mut body, &array, 4);
    let v: Value = serde_json::from_str(&out).expect("still one JSON array");
    let text: String = v
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| {
            f["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|p| p["thought"] != true)
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "my *** and sec");
}

#[test]
fn a_json_array_that_ends_without_a_finish_reason_gets_the_tail_as_an_element() {
    let a = star("k", "secret");
    let chunk =
        |text: &str| json!({"candidates":[{"content":{"parts":[{"text":text}]},"index":0}]});
    let array = format!("[{},\r\n{}]", chunk("a sec"), chunk("ret b se"));
    let mut body = Body::new(a, true, Dialect::Gemini, false);
    let out = through(&mut body, &array, 2);
    let v: Value = serde_json::from_str(&out).expect("one JSON array");
    let text: String = v
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| {
            f["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(text, "a *** b se");
}

#[test]
fn bedrock_blocks_are_filtered_and_flushed_at_their_stop() {
    let a = star("k", "secret");
    let stream = [
        named("messageStart", json!({"role":"assistant"})),
        named(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"sec"}}),
        ),
        named(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"ret! se"}}),
        ),
        named("contentBlockStop", json!({"contentBlockIndex":0})),
        named("messageStop", json!({"stopReason":"end_turn"})),
    ]
    .concat();
    let mut body = Body::new(a, true, Dialect::Bedrock, true);
    let out = through(&mut body, &stream, 13);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "***! se");
    // 扣着的尾巴在块结束之前补出来
    let stop = frames
        .iter()
        .position(|f| f.get("contentBlockIndex").is_some() && f.get("delta").is_none())
        .unwrap();
    let before: String = frames[..stop]
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(before, "***! se");
}

#[test]
fn an_error_mid_stream_flushes_what_was_held() {
    let a = star("k", "secret");
    let stream = [
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so se"}}),
        ),
        named("error", json!({"type":"error","error":{"type":"overloaded_error"}})),
    ]
    .concat();
    let mut body = Body::new(a.clone(), true, Dialect::Anthropic, true);
    let out = through(&mut body, &stream, 1);
    let frames = frames_of(&out);
    let text: String = frames
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "so se");
    assert_eq!(frames.last().unwrap()["type"], "error");
    // 流断了、什么收尾都没有：flush 时补出来
    let cut = named(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so se"}}),
    );
    let mut body = Body::new(a, true, Dialect::Anthropic, true);
    let out = through(&mut body, &cut, 1);
    let text: String = frames_of(&out)
        .iter()
        .filter_map(|f| f["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "so se");
}

#[test]
fn observe_mode_changes_no_byte_and_still_reports() {
    let a = star("k", "secret");
    let stream = [
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a sec"}}),
        ),
        named(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ret"}}),
        ),
        named("content_block_stop", json!({"type":"content_block_stop","index":0})),
    ]
    .concat();
    let mut body = Body::new(a, false, Dialect::Anthropic, true);
    let out = through(&mut body, &stream, 6);
    assert_eq!(out, stream, "观察档一个字节都不改");
    let hits = body.take_hits();
    assert_eq!(hits[0].outcome, Outcome::Recorded);
    assert_eq!(hits[0].count, 1);
}

#[test]
fn a_frame_split_inside_a_character_is_put_back_together() {
    // 「星」的三个字节切在两块里
    let a = star("k", "北极星");
    let stream = named(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"北极星"}}),
    );
    let at = stream.find('星').unwrap() + 1;
    let mut body = Body::new(a, true, Dialect::Anthropic, true);
    let mut out = body.process(&stream.as_bytes()[..at]);
    out.extend(body.process(&stream.as_bytes()[at..]));
    out.extend(body.flush());
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(r#""text":"***""#), "{out}");
}

#[test]
fn whole_answers_are_filtered_in_every_format() {
    let a = star("k", "secret");
    let cases = [
        (
            Dialect::Anthropic,
            json!({"content":[{"type":"text","text":"a secret"},{"type":"tool_use","input":{"q":"secret"}}]}),
            "/content/0/text",
        ),
        (
            Dialect::Chat,
            json!({"choices":[{"index":0,"message":{"role":"assistant","content":"a secret"}}]}),
            "/choices/0/message/content",
        ),
        (
            Dialect::Responses,
            json!({"output":[{"type":"message","content":[{"type":"output_text","text":"a secret"}]}]}),
            "/output/0/content/0/text",
        ),
        (
            Dialect::Gemini,
            json!({"candidates":[{"content":{"parts":[{"text":"x","thought":true},{"text":"a secret"}]},"index":0}]}),
            "/candidates/0/content/parts/1/text",
        ),
        (
            Dialect::Bedrock,
            json!({"output":{"message":{"content":[{"text":"a secret"}]}}}),
            "/output/message/content/0/text",
        ),
    ];
    for (d, body, path) in cases {
        let (out, hits) = whole(&a, true, d, body.to_string().as_bytes());
        let v: Value = serde_json::from_slice(&out.expect("changed")).unwrap();
        assert_eq!(v.pointer(path).unwrap(), "a ***", "{d:?}");
        assert_eq!(hits.len(), 1, "{d:?}");
        // 观察档不改，照样报
        let (out, hits) = whole(&a, false, d, body.to_string().as_bytes());
        assert!(out.is_none() && hits.len() == 1, "{d:?}");
    }
    // 工具调用不碰
    let (out, _) = whole(
        &a,
        true,
        Dialect::Anthropic,
        json!({"content":[{"type":"tool_use","input":{"q":"secret"}}]})
            .to_string()
            .as_bytes(),
    );
    assert!(out.is_none());
    assert_eq!(
        whole(&a, true, Dialect::Chat, b"not json"),
        (None, Vec::new())
    );
}

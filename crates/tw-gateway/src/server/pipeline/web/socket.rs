//! Responses WebSocket requests use the same protected web-tool pipeline as HTTP.
//! Only this connection's last response can supply implicit conversation history.

use axum::extract::ws::{Message, WebSocket};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};

use super::*;

type Writer = futures::stream::SplitSink<WebSocket, Message>;

#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn serve_socket(
    socket: WebSocket,
    state: AppState,
    rt: Arc<Runtime>,
    client_name: String,
    uri: axum::http::Uri,
    query: Option<String>,
    headers: axum::http::HeaderMap,
    from: crate::server::Sender,
    _live: crate::live::Pass,
) {
    let (mut writer, mut reader) = socket.split();
    let mut previous: Option<(String, Value, Vec<Value>)> = None;
    while let Some(Ok(message)) = reader.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Ping(data) => {
                if writer.send(Message::Pong(data)).await.is_err() {
                    return;
                }
                continue;
            }
            Message::Close(_) => return,
            _ => continue,
        };
        let mut body: Value = match serde_json::from_str(&text) {
            Ok(body) => body,
            Err(_) => {
                send_error(
                    &mut writer,
                    "invalid_request_error",
                    "Expected a JSON response.create message",
                )
                .await;
                continue;
            }
        };
        if body["type"] != "response.create" {
            send_error(
                &mut writer,
                "invalid_request_error",
                "Only response.create is accepted while no response is running",
            )
            .await;
            continue;
        }
        body.as_object_mut()
            .expect("response.create object")
            .remove("type");
        body["stream"] = json!(true);
        if let Some((id, request, output)) = &previous
            && body["previous_response_id"] == *id
        {
            let mut input = input_items(&request["input"]);
            input.extend(output.clone());
            input.extend(input_items(&body["input"]));
            body["input"] = json!(input);
            body.as_object_mut()
                .expect("object")
                .remove("previous_response_id");
        }
        let bytes = body.to_string();
        if bytes.len() > 16 * 1024 * 1024 {
            send_error(
                &mut writer,
                "invalid_request_error",
                "Conversation exceeds 16 MiB; start a new conversation",
            )
            .await;
            continue;
        }
        let abort = crate::abort::Switch::default();
        let seat = crate::content::Seat::default();
        let mut request_headers = headers.clone();
        for header in [
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
        ] {
            request_headers.remove(header);
        }
        let req = Inbound {
            web_parent: false,
            uri: uri.clone(),
            query: query.clone(),
            headers: request_headers,
            body: Bytes::from(bytes),
            version: axum::http::Version::HTTP_11,
            client_name: client_name.clone(),
            api: Some(crate::client_api::ClientApi::OpenaiResponses),
            dialect: Dialect::Responses,
            started: std::time::Instant::now(),
            from: from.clone(),
            abort: abort.clone(),
            traffic: crate::traffic::Traffic::new(),
            seat: seat.clone(),
        };
        let mut ending = None;
        let response = {
            let run = super::super::pipeline(
                state.clone(),
                rt.clone(),
                req,
                state.live.enter(),
                &mut ending,
            );
            tokio::pin!(run);
            loop {
                tokio::select! {
                    _ = abort.wait() => break Err(GatewayError::aborted()),
                    result = &mut run => break result,
                    incoming = reader.next() => {
                        if !during(incoming, &mut writer, &abort).await { return; }
                    }
                }
            }
        };
        if let Some(end) = ending.take() {
            match &response {
                Ok(r) => end.finished(r.status().as_u16()),
                Err(e) => end.failed(e.source.into(), e.detail.clone()),
            }
        }
        if abort.thrown() {
            send_error(&mut writer, "cancelled", "Response was cancelled").await;
            continue;
        }
        let response =
            response.unwrap_or_else(|e| e.in_dialect(Dialect::Responses).into_response());
        let response = match seat.get() {
            Some(capture) => capture.client_answer(axum::http::Version::HTTP_11, response),
            None => response,
        };
        let success = response.status().is_success();
        let mut data = response.into_body().into_data_stream();
        let mut frames = tw_dialect::frame::Decoder::default();
        let mut error_body = Vec::new();
        loop {
            tokio::select! {
                _ = abort.wait() => { send_error(&mut writer, "cancelled", "Response was cancelled").await; break; }
                incoming = reader.next() => {
                    if !during(incoming, &mut writer, &abort).await { return; }
                }
                next = data.next() => {
                    let Some(chunk) = next else { break; };
                    let Ok(chunk) = chunk else { send_error(&mut writer, "server_error", "Response stream was interrupted").await; break; };
                    if !success {
                        if error_body.len() + chunk.len() <= 1024 * 1024 { error_body.extend_from_slice(&chunk); }
                        continue;
                    }
                    for frame in frames.feed(&chunk) {
                        if !send_frame(&mut writer, &mut previous, &body, &frame.data).await { return; }
                    }
                }
            }
        }
        if abort.thrown() {
            continue;
        }
        if success {
            for frame in frames.flush() {
                if !send_frame(&mut writer, &mut previous, &body, &frame.data).await {
                    return;
                }
            }
        } else {
            let error: Value = serde_json::from_slice(&error_body).unwrap_or_default();
            let message = error["error"]["message"]
                .as_str()
                .unwrap_or("The request failed");
            send_error(&mut writer, "server_error", message).await;
        }
    }
}

fn input_items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::String(text) => vec![json!({"role":"user","content":text})],
        _ => Vec::new(),
    }
}

async fn send_frame(
    writer: &mut Writer,
    previous: &mut Option<(String, Value, Vec<Value>)>,
    request: &Value,
    data: &str,
) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return true;
    };
    if matches!(
        value["type"].as_str(),
        Some("response.completed" | "response.incomplete")
    ) && let Some(id) = value["response"]["id"].as_str()
    {
        *previous = Some((
            id.into(),
            request.clone(),
            value["response"]["output"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        ));
    }
    writer
        .send(Message::Text(data.to_string().into()))
        .await
        .is_ok()
}

async fn during(
    incoming: Option<Result<Message, axum::Error>>,
    writer: &mut Writer,
    abort: &crate::abort::Switch,
) -> bool {
    match incoming {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => false,
        Some(Ok(Message::Ping(data))) => writer.send(Message::Pong(data)).await.is_ok(),
        Some(Ok(Message::Text(text))) => {
            if serde_json::from_str::<Value>(&text)
                .ok()
                .is_some_and(|v| v["type"] == "response.cancel")
            {
                abort.throw();
            } else {
                send_error(
                    writer,
                    "response_in_progress",
                    "Wait for the current response or cancel it before creating another response",
                )
                .await;
            }
            true
        }
        _ => true,
    }
}

async fn send_error(writer: &mut Writer, kind: &str, message: &str) {
    let _ = writer
        .send(Message::Text(
            json!({"type":"error","error":{"type":kind,"message":message}})
                .to_string()
                .into(),
        ))
        .await;
}

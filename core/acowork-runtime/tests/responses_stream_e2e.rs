//! ADR-103 S3 end-to-end smoke: real TCP SSE server → `ResponsesProvider`.
//!
//! The unit tests in `providers::responses` drive the state machine directly
//! and use a buffered mock server (one HTTP body, one read). This harness
//! owns a real socket, writes the SSE body in several TCP segments — one of
//! them split *mid-line* — and asserts the streamed events, so the line
//! buffer, the terminal-event decision and the truncation path are verified
//! over the wire rather than in-process.

use std::time::Duration;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, Provider, StreamEvent,
};
use acowork_runtime::providers::responses::ResponsesProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve `chunks` as an HTTP/1.1 SSE response, one TCP write per chunk,
/// then close the connection.
async fn serve_sse(chunks: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut head = [0u8; 8192];
        let _ = socket.read(&mut head).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write head");
        socket.flush().await.expect("flush head");
        for chunk in chunks {
            socket.write_all(chunk.as_bytes()).await.expect("write chunk");
            socket.flush().await.expect("flush chunk");
            // Force separate TCP segments so the client cannot coalesce them.
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}/v1")
}

/// Serve one buffered JSON body as an HTTP/1.1 response.
async fn serve_json(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut head = [0u8; 8192];
        let _ = socket.read(&mut head).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\
             content-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.expect("write");
        socket.flush().await.expect("flush");
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}/v1")
}

fn provider_for(base_url: String) -> ResponsesProvider {
    ResponsesProvider::with_base_url_and_timeouts(
        Some(base_url.as_str()),
        Some("sk-e2e"),
        Duration::from_secs(10),
        Duration::from_secs(5),
        // Short read timeout: the fixtures always keep the socket busy.
        Duration::from_secs(5),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-mini".to_string(),
        messages: vec![ChatMessage::user("hi")],
        temperature: None,
        max_tokens: None,
        tools: None,
        reasoning_effort: None,
        thinking_mode: None,
    }
}

async fn collect(provider: &ResponsesProvider) -> Vec<StreamEvent> {
    use futures_util::StreamExt;
    let stream = provider
        .chat_stream(request())
        .await
        .expect("stream must open");
    let mut stream = Box::into_pin(stream);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn multi_segment_sse_streams_text_reasoning_tool_call_and_usage() {
    let text_frame = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
    // Split the second text frame in the middle of its `data:` line so the
    // client's line assembler must carry state across TCP reads.
    let split_at = text_frame.len() / 2;
    let chunks = vec![
        format!(
            "data: {{\"type\":\"response.reasoning_text.delta\",\"delta\":\"thinking\"}}\n\n{}",
            &text_frame[..split_at]
        ),
        format!(
            "{}{}",
            &text_frame[split_at..],
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\
             \"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\"}}\n\n"
        ),
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\
         \"delta\":\"{\\\"path\\\":\"}\n\n"
            .to_string(),
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\
         \"delta\":\"\\\"a.txt\\\"}\"}\n\n"
            .to_string(),
        // `[DONE]` is present but must not be what ends the stream.
        "data: [DONE]\n\n".to_string(),
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\
         \"usage\":{\"input_tokens\":12,\"output_tokens\":8,\"total_tokens\":20,\
         \"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n"
            .to_string(),
    ];

    let base_url = serve_sse(chunks).await;
    let provider = provider_for(base_url);
    let events = collect(&provider).await;

    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ReasoningContent(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let content: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Content(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "thinking");
    // The mid-line split must not corrupt or duplicate text.
    assert_eq!(content, "hello");

    let tool_start = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::ToolCallStart(call) => Some(call),
            _ => None,
        })
        .expect("tool call start");
    assert_eq!(tool_start.id, "call_1");
    assert_eq!(tool_start.function.name, "read_file");

    let arguments: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallChunk { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(arguments, "{\"path\":\"a.txt\"}");

    match events.last().expect("non-empty") {
        StreamEvent::Finished(response) => {
            assert_eq!(
                response.finish_reason.as_deref(),
                Some("tool_calls"),
                "a streamed function_call must report tool_calls"
            );
            let usage = response.usage.as_ref().expect("usage");
            assert_eq!(usage.total_tokens, 20);
            assert_eq!(usage.reasoning_tokens, 3);
        }
        other => panic!("terminal event must be Finished, got {other:?}"),
    }
    assert!(
        !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
        "no error on a completed stream: {events:?}"
    );
}

#[tokio::test]
async fn socket_close_without_terminal_event_is_a_truncation() {
    let chunks = vec![
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".to_string(),
    ];
    let base_url = serve_sse(chunks).await;
    let provider = provider_for(base_url);
    let events = collect(&provider).await;

    let content: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Content(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(content, "partial");
    assert!(
        !events.iter().any(|e| matches!(e, StreamEvent::Finished(_))),
        "no terminal event may never produce Finished: {events:?}"
    );
    match events.last().expect("non-empty") {
        StreamEvent::Error(err) => {
            assert!(!err.retryable, "truncation is not retried blindly");
        }
        other => panic!("expected a truncation error, got {other:?}"),
    }
}

#[tokio::test]
async fn non_streaming_chat_over_a_real_socket() {
    let body = "{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\
                \"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}],\
                \"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}";
    let base_url = serve_json(body).await;
    let provider = provider_for(base_url);
    let response = provider.chat(request()).await.expect("chat succeeds");
    assert_eq!(response.content, "pong");
    assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    assert_eq!(response.usage.expect("usage").total_tokens, 6);
}
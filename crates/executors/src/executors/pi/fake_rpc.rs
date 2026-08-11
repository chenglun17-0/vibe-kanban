#![cfg(test)]

use std::time::Duration;

use serde_json::{Value, json};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

use super::rpc::{
    PiRpcClient, PiRpcCommand, PiRpcEntries, PiRpcEvent, PiRpcSessionState, PiRpcSessionStats,
};

async fn read_json_frame<R>(reader: &mut R) -> Value
where
    R: AsyncReadExt + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader.read(&mut byte).await.unwrap();
        assert!(read > 0);
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    serde_json::from_slice(&bytes).unwrap()
}

async fn write_frame<W>(writer: &mut W, value: Value)
where
    W: AsyncWriteExt + Unpin,
{
    writer
        .write_all(serde_json::to_vec(&value).unwrap().as_slice())
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

#[tokio::test]
async fn handles_chunked_crlf_and_unicode_separators() {
    let (child_stdin, mut fake_stdin) = io::duplex(4096);
    let (mut fake_stdout_server, child_stdout) = io::duplex(4096);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let client = PiRpcClient::spawn(
        child_stdin,
        child_stdout,
        event_tx,
        tokio::io::sink(),
        CancellationToken::new(),
    );

    let text = "line with unicode separator \u{2028} and \u{2029}";
    let mut event_frame = serde_json::to_vec(&json!({
        "type": "message_update",
        "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": text}
    }))
    .unwrap();
    let split = event_frame.len() / 2;
    let second_part = event_frame.split_off(split);

    let (event_written_tx, event_written_rx) = oneshot::channel();
    tokio::spawn(async move {
        let request = read_json_frame(&mut fake_stdin).await;
        assert_eq!(request["type"], "get_state");

        fake_stdout_server.write_all(&event_frame).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        fake_stdout_server.write_all(&second_part).await.unwrap();
        fake_stdout_server.write_all(b"\r\n").await.unwrap();
        fake_stdout_server.flush().await.unwrap();
        let _ = event_written_tx.send(());

        write_frame(
            &mut fake_stdout_server,
            json!({
                "id": request["id"],
                "type": "response",
                "command": "get_state",
                "success": true,
                "data": {"sessionId": "session-1", "isStreaming": false, "isCompacting": false}
            }),
        )
        .await;
    });

    let state_request = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .request::<PiRpcSessionState>(PiRpcCommand::GetState, CancellationToken::new())
                .await
        }
    });

    event_written_rx.await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match event {
        PiRpcEvent::MessageUpdate {
            assistant_message_event: super::rpc::PiRpcAssistantMessageEvent::TextDelta { delta },
        } => assert_eq!(delta, "line with unicode separator \u{2028} and \u{2029}"),
        other => panic!("unexpected event: {other:?}"),
    }

    let state = state_request.await.unwrap().unwrap();
    assert_eq!(state.session_id.as_deref(), Some("session-1"));
}

#[tokio::test]
async fn resolves_interleaved_response_by_request_id() {
    let (child_stdin, mut fake_stdin) = io::duplex(4096);
    let (mut fake_stdout, child_stdout) = io::duplex(4096);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let client = PiRpcClient::spawn(
        child_stdin,
        child_stdout,
        event_tx,
        tokio::io::sink(),
        CancellationToken::new(),
    );

    let server = tokio::spawn(async move {
        let first = read_json_frame(&mut fake_stdin).await;
        let second = read_json_frame(&mut fake_stdin).await;
        assert_eq!(first["type"], "get_state");
        assert_eq!(second["type"], "get_session_stats");

        write_frame(
            &mut fake_stdout,
            json!({
                "id": second["id"],
                "type": "response",
                "command": "get_session_stats",
                "success": true,
                "data": {
                    "sessionId": "s",
                    "tokens": {"input": 1},
                    "contextUsage": {"tokens": 1, "contextWindow": 100, "percent": 1.0}
                }
            }),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        write_frame(
            &mut fake_stdout,
            json!({
                "id": first["id"],
                "type": "response",
                "command": "get_state",
                "success": true,
                "data": {"sessionId": "s", "isStreaming": false, "isCompacting": false}
            }),
        )
        .await;
    });

    let state_handle = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .request::<PiRpcSessionState>(PiRpcCommand::GetState, CancellationToken::new())
                .await
        }
    });
    let stats_handle = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .request::<PiRpcSessionStats>(
                    PiRpcCommand::GetSessionStats,
                    CancellationToken::new(),
                )
                .await
        }
    });

    server.await.unwrap();
    let stats = stats_handle.await.unwrap().unwrap();
    let state = state_handle.await.unwrap().unwrap();
    assert_eq!(state.session_id.as_deref(), Some("s"));
    assert_eq!(stats.context_usage.unwrap().tokens, Some(1));
}

#[tokio::test]
async fn times_out_and_fails_pending_requests_on_eof() {
    let (child_stdin, fake_stdin) = io::duplex(4096);
    let (fake_stdout, child_stdout) = io::duplex(4096);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let client = PiRpcClient::spawn(
        child_stdin,
        child_stdout,
        event_tx,
        tokio::io::sink(),
        CancellationToken::new(),
    );

    let err = client
        .request_with_timeout::<Value>(
            PiRpcCommand::GetCommands,
            CancellationToken::new(),
            Some(Duration::from_millis(20)),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));

    drop(fake_stdin);
    drop(fake_stdout);
    let err = client
        .request::<PiRpcEntries>(PiRpcCommand::GetEntries, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("shut down") || err.to_string().contains("failed to queue"));
}

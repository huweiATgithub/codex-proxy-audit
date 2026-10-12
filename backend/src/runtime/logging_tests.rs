//! Host logging must preserve responses when logs are suppressed, rejected or stalled.
use super::*;
use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Message, PROTOCOL_VERSION, Stage,
    client::{
        CallFuture, CallReply, PluginCall, PluginHandler, PluginSession, SessionConfig, read_frame,
        write_frame,
    },
};
use serde_json::json;
use tokio::io::DuplexStream;

struct ManagementHandler;

impl PluginHandler for ManagementHandler {
    fn call(&self, call: PluginCall) -> CallFuture<'_> {
        Box::pin(async move {
            let status = serde_json::from_value(call.params.clone()).unwrap();
            log_management_failure(&call.host, status).await;
            Ok(CallReply::unary(call.params, call.payload))
        })
    }
}

async fn receive(host: &mut DuplexStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(1), read_frame(host))
        .await
        .expect("logging must not hold the response indefinitely")
        .unwrap()
}

#[tokio::test]
async fn management_logs_use_severity_and_preserve_responses_without_retries() {
    let (mut host, plugin) = tokio::io::duplex(16 * 1024);
    let (reader, writer) = tokio::io::split(plugin);
    let session = tokio::spawn(async move {
        PluginSession::accept(reader, writer, SessionConfig::default())
            .await?
            .run(ManagementHandler)
            .await
    });
    write_frame(
        &mut host,
        &Frame::control(Message::Hello {
            handshake: Handshake {
                protocol_version: PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: "huwei.prompt-audit".into(),
                instance_id: "test-instance".into(),
                generation: 1,
                incarnation: "test-incarnation".into(),
                configuration: json!({}),
                contributes: BTreeMap::new(),
            },
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        receive(&mut host).await.message,
        Message::Ready { .. }
    ));

    let mut call_id = 1;
    for status in [503, 500, 200, 400, 404, 410] {
        for reply in [
            Some(Ok(true)),
            Some(Ok(false)),
            Some(Err(ErrorCode::Capacity)),
            None,
        ] {
            let payload = b"private query and response content".to_vec();
            write_frame(
                &mut host,
                &Frame {
                    message: Message::Call {
                        id: call_id,
                        method: "management.handle".into(),
                        context: CallContext {
                            call_id,
                            instance_id: "test-instance".into(),
                            generation: 1,
                            incarnation: "test-incarnation".into(),
                            stage: Stage::Management,
                            timeout_ms: 1000,
                            resource_stream: false,
                            resource_scope_id: format!("scope-{call_id}"),
                            request_id: None,
                            attempt_id: None,
                            account_id: None,
                            credential_revision: None,
                        },
                        params: json!(status),
                    },
                    payload: payload.clone(),
                },
            )
            .await
            .unwrap();
            let mut frame = receive(&mut host).await;
            if status >= 500 {
                let Message::Callback {
                    id,
                    parent_id,
                    method,
                    params,
                } = frame.message
                else {
                    panic!("expected a host log callback")
                };
                assert_eq!(parent_id, call_id);
                assert_eq!(method, "host.log");
                assert!(frame.payload.is_empty());
                let (event, level) = if status == 503 {
                    ("audit.management.unavailable", "warn")
                } else {
                    ("audit.management.failed", "error")
                };
                assert_eq!(
                    params,
                    json!({"event":event,"level":level,"fields":{"status":status}})
                );
                if let Some(reply) = reply {
                    let message = match reply {
                        Ok(recorded) => Message::Result {
                            id,
                            result: json!({"recorded":recorded}),
                        },
                        Err(code) => Message::Error {
                            id,
                            error: PluginFault::new(code, "log rejected"),
                        },
                    };
                    write_frame(&mut host, &Frame::control(message))
                        .await
                        .unwrap();
                }
                frame = receive(&mut host).await;
                if reply.is_none() {
                    // A late acknowledgment after LOG_WAIT must not close the session.
                    write_frame(
                        &mut host,
                        &Frame::control(Message::Result {
                            id,
                            result: json!({"recorded":true}),
                        }),
                    )
                    .await
                    .unwrap();
                }
            }
            assert!(
                matches!(frame.message, Message::Result { id, result } if id == call_id && result == json!(status))
            );
            assert_eq!(frame.payload, payload);
            call_id += 2;
        }
    }
    write_frame(&mut host, &Frame::control(Message::Shutdown))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), session)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

use std::collections::BTreeMap;

use super::*;
use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Message, PROTOCOL_VERSION, PluginFault, Stage,
    client::{
        CallFuture, CallReply, PluginCall, PluginHandler, PluginSession, SessionConfig,
        SessionError, read_frame, write_frame,
    },
};
use serde_json::{Value, json};
use tokio::{io::DuplexStream, task::JoinHandle};

fn detail() -> Value {
    json!({"code":200,"message":"success","data":{
        "id":"request/1?&", "requestId":"request/1?&", "clientApiKeyId":"key-a",
        "userAgent":"test-UA", "requestedModel":"requested-model", "model":"resolved-model",
        "reasoningEffort":"high", "logicalOutcome":"running", "attempts":[]
    }})
}

#[test]
fn association_and_nullable_fields_are_parsed_before_projection() {
    let metadata = RequestMetadata::parse(
        &serde_json::to_vec(&detail()).unwrap(),
        "request/1?&",
        "key-a",
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(metadata).unwrap(),
        json!({"userAgent":"test-UA", "model":"requested-model", "reasoningEffort":"high"})
    );
    let mut nulls = detail();
    for field in ["userAgent", "requestedModel", "reasoningEffort"] {
        nulls["data"][field] = Value::Null;
    }
    assert_eq!(
        serde_json::to_value(
            RequestMetadata::parse(&serde_json::to_vec(&nulls).unwrap(), "request/1?&", "key-a")
                .unwrap()
        )
        .unwrap(),
        json!({"userAgent":null,"model":null,"reasoningEffort":null})
    );
}

#[test]
fn malformed_and_mismatched_host_records_are_errors() {
    for field in ["id", "requestId", "clientApiKeyId"] {
        let mut wrong = detail();
        wrong["data"][field] = json!("other");
        assert!(
            RequestMetadata::parse(&serde_json::to_vec(&wrong).unwrap(), "request/1?&", "key-a")
                .is_err()
        );
    }
    for field in [
        "id",
        "requestId",
        "clientApiKeyId",
        "userAgent",
        "requestedModel",
        "reasoningEffort",
    ] {
        let mut missing = detail();
        missing["data"].as_object_mut().unwrap().remove(field);
        assert!(
            RequestMetadata::parse(
                &serde_json::to_vec(&missing).unwrap(),
                "request/1?&",
                "key-a"
            )
            .is_err(),
            "missing {field}"
        );
        let mut wrong_type = detail();
        wrong_type["data"][field] = json!(42);
        assert!(
            RequestMetadata::parse(
                &serde_json::to_vec(&wrong_type).unwrap(),
                "request/1?&",
                "key-a"
            )
            .is_err()
        );
    }
    for malformed in [
        json!({"code":503,"message":"unavailable","data":detail()["data"]}),
        json!({"code":200,"message":"success","data":null}),
        json!({"code":200,"message":"success","data":{"clientApiKeyId":null}}),
    ] {
        assert!(
            RequestMetadata::parse(
                &serde_json::to_vec(&malformed).unwrap(),
                "request/1?&",
                "key-a"
            )
            .is_err()
        );
    }
    assert!(RequestMetadata::parse(b"not JSON", "request/1?&", "key-a").is_err());
}

#[test]
fn metadata_limits_count_unicode_points_and_do_not_truncate() {
    for (field, projected, limit) in [
        ("userAgent", "userAgent", 8192),
        ("requestedModel", "model", 256),
        ("reasoningEffort", "reasoningEffort", 80),
    ] {
        for length in [limit, limit + 1] {
            let mut value = detail();
            value["data"][field] = json!("😀".repeat(length));
            let parsed = RequestMetadata::parse(
                &serde_json::to_vec(&value).unwrap(),
                "request/1?&",
                "key-a",
            )
            .unwrap();
            let parsed = serde_json::to_value(parsed).unwrap();
            if length == limit {
                assert_eq!(parsed[projected].as_str().unwrap().chars().count(), limit);
            } else {
                assert!(parsed[projected].is_null());
            }
        }
    }
}

struct LookupHandler {
    request_id: Option<String>,
}

impl PluginHandler for LookupHandler {
    fn call(&self, call: PluginCall) -> CallFuture<'_> {
        Box::pin(async move {
            let result =
                match RequestMetadata::fetch(&call.host, self.request_id.as_deref(), "key-a").await
                {
                    Ok(metadata) => json!({"metadata":metadata}),
                    Err(_) => json!({"error":true}),
                };
            Ok(CallReply::unary(result, Vec::new()))
        })
    }
}

struct HostMock {
    stream: DuplexStream,
    session: JoinHandle<Result<(), SessionError>>,
}

impl HostMock {
    async fn start(request_id: Option<&str>) -> Self {
        let (mut stream, plugin) = tokio::io::duplex(1024 * 1024);
        let (reader, writer) = tokio::io::split(plugin);
        let request_id = request_id.map(str::to_owned);
        let session = tokio::spawn(async move {
            PluginSession::accept(reader, writer, SessionConfig::default())
                .await?
                .run(LookupHandler { request_id })
                .await
        });
        write_frame(
            &mut stream,
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
            read_frame(&mut stream).await.unwrap().message,
            Message::Ready { .. }
        ));
        write_frame(
            &mut stream,
            &Frame::control(Message::Call {
                id: 1,
                method: "management.handle".into(),
                context: CallContext {
                    call_id: 1,
                    instance_id: "test-instance".into(),
                    generation: 1,
                    incarnation: "test-incarnation".into(),
                    stage: Stage::Management,
                    timeout_ms: 30_000,
                    resource_stream: false,
                    resource_scope_id: "scope-1".into(),
                    request_id: None,
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params: json!({}),
            }),
        )
        .await
        .unwrap();
        Self { stream, session }
    }

    async fn callback(&mut self, expected: &str) -> (u64, Value) {
        let frame = timeout(Duration::from_secs(1), read_frame(&mut self.stream))
            .await
            .unwrap()
            .unwrap();
        let Message::Callback {
            id,
            parent_id,
            method,
            params,
        } = frame.message
        else {
            panic!("expected {expected} callback");
        };
        assert_eq!(parent_id, 1);
        assert_eq!(method, expected);
        assert!(frame.payload.is_empty());
        (id, params)
    }

    async fn result(&mut self, id: u64, result: Value, payload: Vec<u8>) {
        write_frame(
            &mut self.stream,
            &Frame {
                message: Message::Result { id, result },
                payload,
            },
        )
        .await
        .unwrap();
    }

    async fn dispatch(&mut self, status: u16) {
        let (id, params) = self.callback("host.http.dispatch").await;
        assert_eq!(
            params,
            json!({
                "settings":null, "method":"GET", "uri":"/api/admin/usage/records/detail?id=request%2F1%3F%26",
                "version":"HTTP/1.1", "headers":[], "timeout_ms":5000, "body":{"kind":"empty"}
            })
        );
        self.result(
            id,
            json!({
                "status":status, "version":"HTTP/1.1", "headers":[],
                "body":{"kind":"handle","handle":"detail-body"}, "response":null, "session":false
            }),
            Vec::new(),
        )
        .await;
    }

    async fn body(&mut self, bytes: Vec<u8>, eof: bool) {
        let (id, params) = self.callback("host.http.body_read").await;
        assert_eq!(params["handle"], "detail-body");
        assert!(params["maximum_bytes"].as_u64().unwrap() > 0);
        self.result(id, json!({"eof":eof,"trailers":null}), bytes)
            .await;
    }

    async fn close(&mut self) {
        let (id, params) = self.callback("host.http.body_close").await;
        assert_eq!(params, json!({"handle":"detail-body"}));
        self.result(id, json!({}), Vec::new()).await;
    }

    async fn finish(mut self, expected: Value) {
        let frame = timeout(Duration::from_secs(1), read_frame(&mut self.stream))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(frame.message, Message::Result { id:1, result } if result == expected));
        assert!(frame.payload.is_empty());
        write_frame(&mut self.stream, &Frame::control(Message::Shutdown))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), self.session)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn one_dispatch_reads_bounded_body_and_supports_running_and_failed_records() {
    for outcome in ["running", "failed"] {
        let mut host = HostMock::start(Some("request/1?&")).await;
        host.dispatch(200).await;
        let mut value = detail();
        value["data"]["logicalOutcome"] = json!(outcome);
        let bytes = serde_json::to_vec(&value).unwrap();
        for chunk in bytes.chunks(31) {
            host.body(chunk.to_vec(), false).await;
        }
        host.body(Vec::new(), true).await;
        host.finish(json!({"metadata":{"userAgent":"test-UA","model":"requested-model","reasoningEffort":"high"}})).await;
    }
}

#[tokio::test]
async fn absent_source_id_does_not_dispatch_and_missing_host_record_closes_body() {
    HostMock::start(None)
        .await
        .finish(json!({"metadata":null}))
        .await;
    let mut host = HostMock::start(Some("request/1?&")).await;
    host.dispatch(404).await;
    host.close().await;
    host.finish(json!({"metadata":null})).await;
}

#[tokio::test]
async fn host_errors_and_malformed_json_are_errors_instead_of_absent_metadata() {
    for status in [401, 403, 500, 503] {
        let mut host = HostMock::start(Some("request/1?&")).await;
        host.dispatch(status).await;
        host.close().await;
        host.finish(json!({"error":true})).await;
    }
    let mut host = HostMock::start(Some("request/1?&")).await;
    host.dispatch(200).await;
    host.body(b"invalid JSON".to_vec(), false).await;
    host.body(Vec::new(), true).await;
    host.finish(json!({"error":true})).await;

    let mut host = HostMock::start(Some("request/1?&")).await;
    let (id, _) = host.callback("host.http.dispatch").await;
    write_frame(
        &mut host.stream,
        &Frame::control(Message::Error {
            id,
            error: PluginFault::new(ErrorCode::Fault, "host request failed"),
        }),
    )
    .await
    .unwrap();
    host.finish(json!({"error":true})).await;
}

#[tokio::test]
async fn oversized_response_closes_the_unconsumed_body() {
    let mut host = HostMock::start(Some("request/1?&")).await;
    host.dispatch(200).await;
    host.body(vec![b'x'; MAX_HOST_RESPONSE_BYTES + 1], false)
        .await;
    host.close().await;
    host.finish(json!({"error":true})).await;
}

#[tokio::test]
async fn stalled_body_read_uses_total_deadline_and_attempts_close() {
    let mut host = HostMock::start(Some("request/1?&")).await;
    host.dispatch(200).await;
    host.callback("host.http.body_read").await;
    tokio::time::pause();
    tokio::time::advance(HOST_WAIT).await;
    host.close().await;
    host.finish(json!({"error":true})).await;
}

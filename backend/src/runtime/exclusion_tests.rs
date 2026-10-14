//! Settings and collection may change storage, but must preserve the host request and response.
use super::*;
use crate::store::AuditListFilter;
use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Message, PROTOCOL_VERSION, Stage,
    call::{
        management::ManagementRequest,
        middleware::{
            HANDLE_METHOD, MiddlewareBodyFraming, MiddlewareBodyHandle, MiddlewareHeader,
            MiddlewareMount, MiddlewareNextRequest, MiddlewareNextResponse, MiddlewareRequestBody,
            MiddlewareRequestHead, MiddlewareResponseBody, MiddlewareResponseHead,
            MiddlewareTransport, NEXT_METHOD,
        },
    },
    client::{PluginSession, SessionConfig, read_frame, write_frame},
};
use serde_json::{Value, json};
use tokio::io::DuplexStream;

async fn receive(host: &mut DuplexStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(2), read_frame(host))
        .await
        .expect("audit settings must not stall request forwarding")
        .unwrap()
}

fn context(id: u64, stage: Stage) -> CallContext {
    CallContext {
        call_id: id,
        instance_id: "test-instance".into(),
        generation: 1,
        incarnation: "test-incarnation".into(),
        stage,
        timeout_ms: 2_000,
        resource_stream: false,
        resource_scope_id: format!("scope-{id}"),
        request_id: (stage == Stage::Request).then(|| format!("request-{id}")),
        attempt_id: None,
        account_id: None,
        credential_revision: None,
    }
}

async fn management(
    host: &mut DuplexStream,
    id: u64,
    method: &str,
    value: Option<Value>,
) -> (u16, Value) {
    let body = value.map(|value| serde_json::to_vec(&value).unwrap());
    write_frame(
        host,
        &Frame {
            message: Message::Call {
                id,
                method: "management.handle".into(),
                context: context(id, Stage::Management),
                params: serde_json::to_value(ManagementRequest {
                    method: method.into(),
                    path: "api/exclusions".into(),
                    query: String::new(),
                    content_type: body.as_ref().map(|_| "application/json".into()),
                    headers: Vec::new(),
                })
                .unwrap(),
            },
            payload: body.unwrap_or_default(),
        },
    )
    .await
    .unwrap();
    let reply = receive(host).await;
    let Message::Result {
        id: reply_id,
        result,
    } = reply.message
    else {
        panic!("expected a management response")
    };
    assert_eq!(reply_id, id);
    let response: ManagementResponse = serde_json::from_value(result).unwrap();
    assert_eq!(response.content_type, "application/json");
    (
        response.status,
        serde_json::from_slice(&reply.payload).unwrap(),
    )
}

fn header(name: &str, value: impl Into<Vec<u8>>) -> MiddlewareHeader {
    MiddlewareHeader {
        name: name.into(),
        value: value.into(),
    }
}

async fn preserved_request(
    host: &mut DuplexStream,
    id: u64,
    transport: MiddlewareTransport,
    mut headers: Vec<MiddlewareHeader>,
) {
    // Deliberately retain formatting, repeated headers and arbitrary header bytes.
    // A preserve callback delegates their original representation to the host.
    headers.extend([
        header("x-client-header", b"first".to_vec()),
        header("X-Client-Header", vec![0, 255, 128]),
    ]);
    let request = MiddlewareRequestHead {
        settings_sources: json!({"client":"configured"}),
        settings: json!({"reasoning":{"effort":"high"}}),
        request_id: format!("request-{id}"),
        client_key_id: "fixture-key".into(),
        account_group_ids: vec!["fixture-group".into()],
        mount: MiddlewareMount::Request,
        attempt_index: None,
        operation: "generate".into(),
        protocol: "openai".into(),
        endpoint: "/v1/responses".into(),
        transport,
        provider: None,
        model: Some("public-model".into()),
        account_id: None,
        headers,
    };
    let body = format!(
        " \n{{ \"input\" : [{{\"role\":\"assistant\",\"content\":\"prior context\"}},\n{{\"role\":\"user\",\"content\":\"request-{id}\"}}] }}\t\n"
    )
    .into_bytes();
    write_frame(
        host,
        &Frame {
            message: Message::Call {
                id,
                method: HANDLE_METHOD.into(),
                context: context(id, Stage::Request),
                params: serde_json::to_value(request).unwrap(),
            },
            payload: body,
        },
    )
    .await
    .unwrap();

    let callback = receive(host).await;
    let Message::Callback {
        id: callback_id,
        parent_id,
        method,
        params,
    } = callback.message
    else {
        panic!("expected request to be forwarded to next")
    };
    assert_eq!(parent_id, id);
    assert_eq!(method, NEXT_METHOD);
    let forwarded: MiddlewareNextRequest = serde_json::from_value(params).unwrap();
    assert_eq!(forwarded.body, MiddlewareRequestBody::Preserve);
    assert!(forwarded.header_mutations.is_empty());
    assert!(forwarded.protocol.is_none());
    assert!(forwarded.settings.is_none());
    assert!(forwarded.capabilities.is_none());
    assert!(callback.payload.is_empty());

    let framing = match transport {
        MiddlewareTransport::HttpJson | MiddlewareTransport::WebSocket => {
            MiddlewareBodyFraming::JsonDocument
        }
        _ => MiddlewareBodyFraming::SseEvent,
    };
    let response_id = format!("upstream-{id}");
    let body_handle = MiddlewareBodyHandle {
        handle: format!("opaque-body-{id}"),
        framing,
    };
    write_frame(
        host,
        &Frame {
            message: Message::Result {
                id: callback_id,
                result: serde_json::to_value(MiddlewareNextResponse {
                    metadata: None,
                    response: response_id.clone(),
                    protocol: "openai".into(),
                    status: 201,
                    headers: vec![
                        header("x-upstream", b"first".to_vec()),
                        header("x-upstream", vec![128, 255]),
                    ],
                    body: Some(body_handle.clone()),
                })
                .unwrap(),
            },
            payload: Vec::new(),
        },
    )
    .await
    .unwrap();
    write_frame(
        host,
        &Frame::control(Message::Credit {
            id,
            bytes: 1,
            frames: 1,
        }),
    )
    .await
    .unwrap();

    // Requiring a final result immediately after the one next callback also
    // catches retries, additional next calls and response-body reads.
    let reply = receive(host).await;
    let Message::Result {
        id: reply_id,
        result,
    } = reply.message
    else {
        panic!("expected an unchanged middleware response")
    };
    assert_eq!(reply_id, id);
    assert!(reply.payload.is_empty());
    let response: MiddlewareResponseHead = serde_json::from_value(result).unwrap();
    assert_eq!(response.response, Some(response_id));
    assert!(response.protocol.is_none());
    assert!(response.status.is_none());
    assert!(response.header_mutations.is_empty());
    assert_eq!(
        response.body,
        MiddlewareResponseBody::PassThrough { body: body_handle }
    );
    assert!(
        matches!(receive(host).await.message, Message::End { id: end_id, error: None } if end_id == id)
    );
}

#[tokio::test]
async fn live_user_agent_exclusions_only_change_persisted_audits_for_every_transport() {
    for transport in [
        MiddlewareTransport::HttpJson,
        MiddlewareTransport::HttpSse,
        MiddlewareTransport::WebSocket,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("audit.sqlite3"))
            .await
            .unwrap();
        let writer = Writer::start(store.clone());
        let handler = plugin(store.clone(), writer.clone()).await.unwrap();
        let (mut host, connection) = tokio::io::duplex(64 * 1024);
        let (reader, sink) = tokio::io::split(connection);
        let session = tokio::spawn(async move {
            PluginSession::accept(reader, sink, SessionConfig::default())
                .await?
                .run(handler)
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

        let empty = json!({"userAgentPatterns":[]});
        assert_eq!(
            management(&mut host, 1, "GET", None).await,
            (200, empty.clone())
        );
        let monitor = json!({"userAgentPatterns":["(?i)^monitor/"]});
        assert_eq!(
            management(&mut host, 3, "PUT", Some(monitor.clone())).await,
            (200, monitor.clone())
        );
        assert_eq!(management(&mut host, 5, "GET", None).await, (200, monitor));

        for (id, headers) in [
            (7, vec![header("User-Agent", b"MONITOR/1".to_vec())]),
            (9, vec![header("User-Agent", b"Browser/1".to_vec())]),
            (11, vec![]),
            (
                13,
                vec![
                    header("User-Agent", b"Browser/1".to_vec()),
                    header("uSeR-aGeNt", b"monitor/2".to_vec()),
                ],
            ),
            (15, vec![header("User-Agent", vec![255, 128])]),
            (
                17,
                vec![
                    header("User-Agent", b"Browser/1".to_vec()),
                    header("thread_source", b"compaction".to_vec()),
                ],
            ),
            (
                19,
                vec![header(
                    "User-Agent",
                    format!("monitor/{}", "x".repeat(MAX_USER_AGENT_BYTES)).into_bytes(),
                )],
            ),
        ] {
            preserved_request(&mut host, id, transport, headers).await;
        }

        let browser = json!({"userAgentPatterns":["^Browser/"]});
        assert_eq!(
            management(&mut host, 21, "PUT", Some(browser.clone())).await,
            (200, browser.clone())
        );
        preserved_request(
            &mut host,
            23,
            transport,
            vec![header("User-Agent", b"Browser/1".to_vec())],
        )
        .await;
        preserved_request(
            &mut host,
            25,
            transport,
            vec![header("User-Agent", b"monitor/1".to_vec())],
        )
        .await;
        let (status, invalid) = management(
            &mut host,
            27,
            "PUT",
            Some(json!({"userAgentPatterns":["("]})),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(invalid["error"]["patternIndex"], 0);
        assert_eq!(management(&mut host, 29, "GET", None).await, (200, browser));
        preserved_request(
            &mut host,
            31,
            transport,
            vec![header("User-Agent", b"Browser/2".to_vec())],
        )
        .await;
        assert_eq!(
            management(&mut host, 33, "PUT", Some(empty.clone())).await,
            (200, empty)
        );
        preserved_request(
            &mut host,
            35,
            transport,
            vec![header("User-Agent", b"Browser/3".to_vec())],
        )
        .await;

        write_frame(&mut host, &Frame::control(Message::Shutdown))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), session)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        writer.drain().await;

        let persisted = store
            .list(AuditListFilter {
                start: None,
                end: None,
                key: None,
                search: "",
                after: None,
                limit: 100,
            })
            .await
            .unwrap();
        assert_eq!(persisted.total, 6);
        let mut stored_requests = Vec::new();
        for row in persisted.rows {
            assert_eq!(row.context_count, 1);
            let audit = store.detail(&row.id).await.unwrap().unwrap();
            assert_eq!(audit.contents.len(), 2);
            stored_requests.push(audit.request_id.unwrap());
        }
        stored_requests.sort();
        assert_eq!(
            stored_requests,
            [
                "request-11",
                "request-15",
                "request-19",
                "request-25",
                "request-35",
                "request-9"
            ]
        );
    }
}

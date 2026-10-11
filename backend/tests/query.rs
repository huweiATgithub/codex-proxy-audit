//! 验证审计查询契约、分页和宿主元数据投影，需要隔离的宿主迁移数据库
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use codex_proxy_audit::{
    collection::{RequestObservation, collect},
    query::QueryService,
    store::{ObservedKey, Store},
};
use serde_json::{Value, json};
use uuid::Uuid;

fn query(values: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values.iter().copied())
        .finish()
}
fn observation(
    key: &str,
    request: &str,
    thread: &str,
    input: Value,
    time: &str,
) -> RequestObservation {
    RequestObservation {
        key_id: key.to_string(),
        request_id: request.to_string(),
        body: json!({"client_metadata":{"thread_id":thread},"input":input}),
        headers: BTreeMap::new(),
        observed_at: DateTime::parse_from_rfc3339(time)
            .unwrap()
            .with_timezone(&Utc),
        key_name: Some("Query Test Key".into()),
        key_name_observed_at: Some(Utc::now()),
    }
}
async fn ok(service: &QueryService, path: &str, pairs: &[(&str, &str)]) -> Value {
    let response = service.handle(path, &query(pairs)).await;
    assert_eq!(response.status, 200, "{}", response.value);
    response.value
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL with v3.23.0 host migrations"]
async fn pg_queries_follow_contract_and_read_host_metadata() {
    let url = std::env::var("AUDIT_TEST_DATABASE_URL").expect("set isolated test database URL");
    // This ignored fixture test uses a database administrator credential;
    // production installation provisions schema ownership separately.
    let setup_pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("CREATE SCHEMA IF NOT EXISTS prompt_audit")
        .execute(&setup_pool)
        .await
        .unwrap();
    setup_pool.close().await;
    let store = Store::connect(&url).await.unwrap();
    let key = Uuid::new_v4().to_string();
    let thread = Uuid::new_v4().to_string();
    let secret = store.secret("query_cursor").await.unwrap();
    let service = QueryService::new(
        store.pool().clone(),
        "America/New_York".parse().unwrap(),
        secret.clone(),
    )
    .unwrap();
    for index in 0..53 {
        let input = json!([{"id":format!("input-{index}"),"role":"user","content":format!("API request {index}: 100% _ \\ Ä 😀")}]);
        store
            .persist_batch(&[collect(&observation(
                &key,
                &Uuid::new_v4().to_string(),
                &thread,
                input,
                "2026-11-01T06:30:00.000Z",
            ))
            .unwrap()])
            .await
            .unwrap();
    }
    let filters = [
        ("key", key.as_str()),
        ("from", "2026-11-01"),
        ("to", "2026-11-01"),
        ("q", "api"),
        ("limit", "50"),
    ];
    let first = ok(&service, "api/audits", &filters).await;
    assert_eq!(first["total"], 53);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    let items = first["items"].as_array().unwrap();
    assert!(
        items
            .windows(2)
            .all(|pair| pair[0]["id"].as_str().unwrap() > pair[1]["id"].as_str().unwrap())
    );
    assert_eq!(items[0]["recordedAtDisplay"], "2026-11-01 01:30:00");
    let cursor = first["nextCursor"].as_str().unwrap();
    let second = ok(
        &service,
        "api/audits",
        &[
            ("key", &key),
            ("from", "2026-11-01"),
            ("to", "2026-11-01"),
            ("q", "api"),
            ("limit", "50"),
            ("cursor", cursor),
        ],
    )
    .await;
    assert_eq!(second["total"], 53);
    assert_eq!(second["items"].as_array().unwrap().len(), 3);
    assert!(second["nextCursor"].is_null());
    assert_eq!(
        service
            .handle(
                "api/audits",
                &query(&[("key", &key), ("q", "API"), ("cursor", cursor)])
            )
            .await
            .value["error"]["code"],
        "invalid_cursor"
    );
    let restarted = QueryService::new(
        store.pool().clone(),
        "America/New_York".parse().unwrap(),
        store.secret("query_cursor").await.unwrap(),
    )
    .unwrap();
    assert_eq!(
        ok(
            &restarted,
            "api/audits",
            &[
                ("key", &key),
                ("from", "2026-11-01"),
                ("to", "2026-11-01"),
                ("q", "api"),
                ("limit", "50"),
                ("cursor", cursor)
            ]
        )
        .await["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        ok(&service, "api/audits", &[("key", &key), ("q", "100% _ \\")]).await["total"],
        53
    );
    assert_eq!(
        ok(&service, "api/audits", &[("key", &key), ("q", "ä")]).await["total"],
        0
    );
    assert_eq!(
        ok(
            &service,
            "api/audits",
            &[("key", &key), ("from", "2026-11-02"), ("to", "2026-11-02")]
        )
        .await["total"],
        0
    );
    let utc = QueryService::new(store.pool().clone(), chrono_tz::UTC, secret).unwrap();
    assert_eq!(
        utc.handle(
            "api/audits",
            &query(&[
                ("key", &key),
                ("from", "2026-11-01"),
                ("to", "2026-11-01"),
                ("q", "api"),
                ("cursor", cursor)
            ])
        )
        .await
        .value["error"]["code"],
        "invalid_cursor"
    );

    let request = Uuid::new_v4().to_string();
    let long_text = format!("{}TAIL MATCH\0😀{}", "前".repeat(2000), "é".repeat(2000));
    let input = json!([
        {"id":"context-user","role":"user","content":"earlier question"},
        {"id":"context-assistant","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"private context search must not match"}]},
        {"id":"query-long","role":"user","internal_chat_message_metadata_passthrough":{"turn_id":"message-turn","content_item_kinds":["future.kind","future.empty","user.plain"]},"content":[{"type":"input_text","text":long_text},{"type":"input_text","text":""},{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}
    ]);
    store
        .persist_batch(&[collect(&observation(
            &key,
            &request,
            &thread,
            input,
            "2026-11-01T07:00:00.000Z",
        ))
        .unwrap()])
        .await
        .unwrap();
    let matching = ok(
        &service,
        "api/audits",
        &[("key", &key), ("q", "TAIL MATCH\0😀")],
    )
    .await;
    assert_eq!(matching["total"], 1);
    assert!(
        matching["items"][0]["preview"]
            .as_str()
            .unwrap()
            .contains("TAIL MATCH\0😀")
    );
    assert!(
        matching["items"][0]["preview"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 400
    );
    assert_eq!(
        ok(
            &service,
            "api/audits",
            &[("key", &key), ("q", "private context search")]
        )
        .await["total"],
        0
    );
    let audit = matching["items"][0]["id"].as_str().unwrap();
    let detail = ok(&service, "api/audit", &[("id", audit)]).await;
    assert!(detail["requestMetadata"].is_null());
    assert_eq!(detail["turnId"], "message-turn");
    assert_eq!(detail["input"]["blockCount"], 3);
    assert_eq!(
        detail["inputKinds"],
        json!(["future.kind", "future.empty", "user.plain"])
    );
    assert_eq!(detail["context"].as_array().unwrap().len(), 2);
    assert_eq!(detail["context"][1]["phase"], "commentary");
    let content = detail["input"]["id"].as_str().unwrap();
    let mut cursor = None::<String>;
    let mut combined = String::new();
    let mut segments = 0;
    let mut offset = 0;
    loop {
        let mut params = vec![("id", audit), ("contentId", content)];
        if let Some(ref cursor) = cursor {
            params.push(("cursor", cursor));
        }
        let page = ok(&service, "api/audit-content", &params).await;
        assert_eq!(page["auditId"], audit);
        assert_eq!(page["contentId"], content);
        let mut page_characters = 0;
        for block in page["blocks"].as_array().unwrap() {
            if block["index"] == 0 {
                assert_eq!(block["offset"], offset);
                let text = block["text"].as_str().unwrap();
                offset += text.chars().count();
                combined.push_str(text);
                page_characters += text.chars().count();
            }
            segments += 1;
        }
        assert!(page_characters <= 1200);
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(combined, long_text);
    assert!(segments >= 6);
    let other = first["items"][0]["id"].as_str().unwrap();
    assert_eq!(
        service
            .handle(
                "api/audit-content",
                &query(&[("id", other), ("contentId", content)])
            )
            .await
            .status,
        404
    );

    // Host row appears after the immutable audit was saved; projections change,
    // original input/context remain byte-for-byte identical.
    let observation = json!({"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false,"userAgent":"fixture-client/1","reasoningEffort":"high"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}});
    sqlx::query("INSERT INTO public.model_requests(id,client_api_key_ref,operation,client_transport,requested_model_id,started_at,deadline_at,request_observation_json) VALUES($1,$2,'responses','http_sse','requested-model',now(),now()+interval '10 minutes',$3)").bind(&request).bind(&key).bind(observation).execute(store.pool()).await.unwrap();
    let with_host = ok(&service, "api/audit", &[("id", audit)]).await;
    assert_eq!(
        with_host["requestMetadata"],
        json!({"userAgent":"fixture-client/1","model":"requested-model","reasoningEffort":"high"})
    );
    assert_eq!(with_host["input"], detail["input"]);
    assert_eq!(with_host["context"], detail["context"]);
    sqlx::query("UPDATE public.model_requests SET client_api_key_ref=$2 WHERE id=$1")
        .bind(&request)
        .bind(Uuid::new_v4().to_string())
        .execute(store.pool())
        .await
        .unwrap();
    assert!(ok(&service, "api/audit", &[("id", audit)]).await["requestMetadata"].is_null());
    sqlx::query("DELETE FROM public.model_requests WHERE id=$1")
        .bind(&request)
        .execute(store.pool())
        .await
        .unwrap();
    assert!(ok(&service, "api/audit", &[("id", audit)]).await["requestMetadata"].is_null());
    assert_eq!(
        ok(
            &service,
            "api/audit-content",
            &[("id", audit), ("contentId", content)]
        )
        .await["blocks"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1200
    );

    store
        .update_keys(
            &[ObservedKey {
                id: key.clone(),
                name: Some("Renamed".into()),
            }],
            Utc::now(),
        )
        .await
        .unwrap();
    store.update_keys(&[], Utc::now()).await.unwrap();
    let mut names = Vec::new();
    let mut cursor = None::<String>;
    let mut historical = false;
    loop {
        let params = cursor
            .as_deref()
            .map(|value| vec![("cursor", value)])
            .unwrap_or_default();
        let page = ok(&service, "api/keys", &params).await;
        if let Some(item) = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == key)
        {
            names = item["names"].as_array().unwrap().clone();
            historical = item["historical"].as_bool().unwrap();
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert!(historical);
    assert_eq!(names[0], "Renamed");
    assert!(names.contains(&json!("Query Test Key")));
    store.close().await;
    assert_eq!(
        service
            .handle("api/audit", &query(&[("id", audit)]))
            .await
            .status,
        503
    );
}

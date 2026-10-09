//! request 阶段的透传采集、有界写入调度与宿主管理入口
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use gateway_plugin_sdk::{
    PluginFault,
    call::{
        host::KeyListRequest,
        management::{ManagementRegistration, ManagementResponse},
        middleware::MiddlewareTransport,
    },
    client::{ComposedPlugin, HostClient, PluginBuilder, RequestCall, TypedReply},
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    collection::{RequestObservation, ResponseContext, ResponseSnapshot},
    query::QueryService,
    response::{ResponseAccumulator, ResponseFormat},
    store::{ObservedKey, Store},
};

const QUEUE_CAPACITY: usize = 64;
const PERSIST_WAIT: Duration = Duration::from_secs(2);
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;

enum Write {
    Input(
        Box<RequestObservation>,
        oneshot::Sender<Option<ResponseContext>>,
    ),
    Response(Box<ResponseContext>, ResponseSnapshot),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Writer {
    sender: mpsc::Sender<Write>,
}

impl Writer {
    pub fn start(store: Store) -> Self {
        let (sender, mut receiver) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(async move {
            let mut pruning = tokio::time::interval(Duration::from_secs(60));
            let mut writes = 0usize;
            loop {
                let write = tokio::select! {
                    _ = pruning.tick() => {
                        if store.prune_sessions().await.is_err() { eprintln!("audit: continuation state pruning failed"); }
                        continue;
                    }
                    write = receiver.recv() => match write { Some(write) => write, None => break },
                };
                match write {
                    Write::Input(observation, reply) => {
                        let result =
                            tokio::time::timeout(PERSIST_WAIT, store.collect_request(&observation))
                                .await;
                        let context = match result {
                            Ok(Ok(prepared)) => {
                                if !prepared.losses.is_empty() {
                                    eprintln!(
                                        "audit: optional metadata exceeded collection budget"
                                    );
                                }
                                prepared.response_context
                            }
                            _ => {
                                eprintln!("audit: input persistence failed or timed out");
                                None
                            }
                        };
                        let _ = reply.send(context);
                    }
                    Write::Response(context, snapshot) => {
                        if !matches!(
                            tokio::time::timeout(
                                PERSIST_WAIT,
                                store.persist_response(&context, &snapshot)
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            eprintln!("audit: response state persistence failed");
                        }
                    }
                    Write::Shutdown(reply) => {
                        let _ = reply.send(());
                        break;
                    }
                }
                writes += 1;
                if writes.is_multiple_of(64) && store.prune_sessions().await.is_err() {
                    eprintln!("audit: continuation state pruning failed");
                }
            }
        });
        Self { sender }
    }

    async fn input(&self, observation: RequestObservation) -> Option<ResponseContext> {
        let (reply, received) = oneshot::channel();
        if self
            .sender
            .try_send(Write::Input(Box::new(observation), reply))
            .is_err()
        {
            eprintln!("audit: input queue full or closed; observation dropped");
            return None;
        }
        match tokio::time::timeout(PERSIST_WAIT, received).await {
            Ok(Ok(context)) => context,
            _ => {
                eprintln!(
                    "audit: input persistence acknowledgement unavailable; response association skipped"
                );
                None
            }
        }
    }

    fn response(&self, context: &ResponseContext, snapshot: ResponseSnapshot) -> bool {
        if self
            .sender
            .try_send(Write::Response(Box::new(context.clone()), snapshot))
            .is_err()
        {
            eprintln!("audit: response queue full or closed; continuation state dropped");
            return false;
        }
        true
    }

    pub async fn drain(&self) {
        let (reply, received) = oneshot::channel();
        let shutdown = async {
            if self.sender.send(Write::Shutdown(reply)).await.is_ok() {
                let _ = received.await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .is_err()
        {
            eprintln!("audit: shutdown drain deadline reached; queued observations may be lost");
        }
    }
}

struct ResponseObserver {
    accumulator: ResponseAccumulator,
    context: ResponseContext,
    writer: Writer,
    saved_complete: bool,
}

impl ResponseObserver {
    fn push(&mut self, payload: &[u8]) {
        self.accumulator.push(payload);
        if !self.saved_complete
            && let Some(snapshot) = self.accumulator.snapshot()
            && snapshot.complete
        {
            self.saved_complete = self.writer.response(&self.context, snapshot);
        }
    }
}

impl Drop for ResponseObserver {
    fn drop(&mut self) {
        // 消费停止时只保存实际观察的片段，不把 Drop 视为完整响应的证明
        if !self.saved_complete
            && let Some(snapshot) = self.accumulator.snapshot()
        {
            self.writer.response(&self.context, snapshot);
        }
    }
}

#[derive(Default)]
struct KeyCache {
    refreshed: Option<Instant>,
    attempted: Option<Instant>,
    observed_at: Option<DateTime<Utc>>,
    names: HashMap<String, String>,
}

impl KeyCache {
    async fn refresh(&mut self, host: &HostClient, store: &Store) {
        if self
            .attempted
            .is_some_and(|time| time.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.attempted = Some(Instant::now());
        let mut keys = Vec::new();
        let mut cursor = None;
        let mut seen = HashSet::new();
        loop {
            let Ok(page) = host.list_keys(KeyListRequest { cursor, limit: 100 }).await else {
                eprintln!("audit: Key directory unavailable; previous snapshot retained");
                return;
            };
            keys.extend(page.keys);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            if !seen.insert(cursor.clone()) || keys.len() > 100_000 {
                eprintln!(
                    "audit: Key directory pagination budget exceeded; previous snapshot retained"
                );
                return;
            }
        }
        let observed_at = Utc::now();
        let directory = keys
            .iter()
            .map(|key| ObservedKey {
                id: key.id.clone(),
                name: (key.name.chars().count() <= 512).then(|| key.name.clone()),
            })
            .collect::<Vec<_>>();
        if store.update_keys(&directory, observed_at).await.is_err() {
            eprintln!("audit: Key directory persistence failed");
            return;
        }
        self.names = keys
            .into_iter()
            .filter(|key| key.name.chars().count() <= 512)
            .map(|key| (key.id, key.name))
            .collect();
        self.observed_at = Some(observed_at);
        self.refreshed = Some(Instant::now());
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentContext {
    time_zone: String,
    maximum_buffered_body_bytes: usize,
}

pub async fn plugin(store: Store, writer: Writer) -> anyhow::Result<ComposedPlugin> {
    let registration: ManagementRegistration =
        serde_json::from_slice(include_bytes!("../../dist/plugin/management.json"))?;
    let secret = store.secret("query_cursor").await?;
    let keys = Arc::new(Mutex::new(KeyCache::default()));
    let middleware_store = store.clone();
    let middleware_keys = keys.clone();
    let builder = PluginBuilder::from_json(include_bytes!("../../plugin.json"))?
        .middleware(move |call: RequestCall| {
            let writer = writer.clone();
            let store = middleware_store.clone();
            let keys = middleware_keys.clone();
            async move {
                let head = &call.request.head;
                let incremental = head.transport == MiddlewareTransport::WebSocket;
                let observed_at = Utc::now();
                let context = if head.endpoint == "/v1/responses" && call.request.body.len() <= MAX_OBSERVATION_BYTES {
                    if let Ok(body) = serde_json::from_slice(&call.request.body) {
                        let mut cache = keys.lock().await;
                        if tokio::time::timeout(Duration::from_secs(1), cache.refresh(&call.host, &store)).await.is_err() {
                            eprintln!("audit: Key directory refresh timed out; previous snapshot retained");
                        }
                        let observation = RequestObservation {
                            key_id: head.client_key_id.clone(), request_id: head.request_id.clone(), body,
                            headers: head.headers.iter().filter_map(|header| {
                                std::str::from_utf8(&header.value).ok().map(|value| (header.name.to_ascii_lowercase(), value.to_owned()))
                            }).collect::<BTreeMap<_, _>>(),
                            observed_at, key_name: cache.names.get(&head.client_key_id).cloned(),
                            key_name_observed_at: cache.observed_at, incremental,
                        };
                        drop(cache);
                        writer.input(observation).await
                    } else { eprintln!("audit: request JSON unavailable; observation skipped"); None }
                } else { None };
                let mut response = call.next.run(call.request).await?;
                if let Some(context) = context {
                    let format = if incremental { ResponseFormat::WebSocket }
                    else if response.headers.iter().any(|header| header.name.eq_ignore_ascii_case("content-type") && header.value.starts_with(b"text/event-stream")) { ResponseFormat::Sse }
                    else { ResponseFormat::Json };
                    let mut observer = ResponseObserver { accumulator: ResponseAccumulator::new(format), context, writer, saved_complete: false };
                    response.body = response.body.inspect_frames(move |frame| observer.push(&frame.payload))?;
                }
                Ok::<_, PluginFault>(response)
            }
        })?
        .management(registration, move |call| {
            let store = store.clone();
            let secret = secret.clone();
            let keys = keys.clone();
            async move {
                let reply = call.host.call("host.deployment_context", json!({}), Vec::new()).await;
                let context = reply.ok().filter(|reply| reply.payload.is_empty()).and_then(|reply| serde_json::from_value::<DeploymentContext>(reply.result).ok());
                let result = if let Some(context) = context {
                    match QueryService::new(store.pool().clone(), &context.time_zone, context.maximum_buffered_body_bytes.min(512 * 1024), secret) {
                        Ok(query) => {
                            if call.request.path == "api/keys" {
                                let mut cache = keys.lock().await;
                                if tokio::time::timeout(Duration::from_secs(1), cache.refresh(&call.host, &store)).await.is_err() {
                                    eprintln!("audit: Key directory refresh timed out; previous snapshot retained");
                                }
                            }
                            query.handle(&call.request.path, &call.request.query).await
                        }
                        Err(_) => crate::query::QueryResponse { status: 503, value: json!({"error":{"code":"unavailable","message":"宿主部署上下文暂不可用"}}) },
                    }
                } else { crate::query::QueryResponse { status: 503, value: json!({"error":{"code":"unavailable","message":"宿主部署上下文暂不可用"}}) } };
                let payload = serde_json::to_vec(&result.value).map_err(|_| PluginFault::new(gateway_plugin_sdk::ErrorCode::Fault, "audit response encoding failed"))?;
                Ok(TypedReply::new(ManagementResponse { status: result.status, content_type: "application/json".into(), headers: Vec::new() }).with_payload(payload))
            }
        })?;
    Ok(builder.build()?)
}

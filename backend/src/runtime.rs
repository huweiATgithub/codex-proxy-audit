//! request 阶段的透传采集、有界写入调度与宿主管理入口
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use crossbeam_queue::ArrayQueue;
use gateway_plugin_sdk::{
    PluginFault,
    call::management::{ManagementRegistration, ManagementResponse},
    client::{ComposedPlugin, PluginBuilder, RequestCall, TypedReply},
};
use tokio::{
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
};

use crate::{
    collection::{AuditDraft, RequestObservation, collect},
    keys::CurrentKeys,
    query::QueryService,
    store::{RETENTION_BATCH_SIZE, Store},
};

const QUEUE_CAPACITY: usize = 64;
const BATCH_CAPACITY: usize = 32;
const PERSIST_WAIT: Duration = Duration::from_secs(2);
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;
const RETENTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RETENTION_BATCH_PAUSE: Duration = Duration::from_millis(100);
const RETENTION_WAIT: Duration = Duration::from_secs(4);

/// 独立于采集和写入的清理任务；无请求时仍执行，退出时停止。
pub struct Retention {
    task: JoinHandle<()>,
}

impl Retention {
    pub fn start(store: Store) -> Self {
        let task = tokio::spawn(async move {
            loop {
                let delay = match tokio::time::timeout(
                    RETENTION_WAIT,
                    store.delete_expired_batch(Utc::now()),
                )
                .await
                {
                    Ok(Ok(deleted)) if deleted == RETENTION_BATCH_SIZE as u64 => {
                        RETENTION_BATCH_PAUSE
                    }
                    Ok(Ok(_)) => RETENTION_INTERVAL,
                    _ => {
                        eprintln!(
                            "audit: retention batch failed or timed out; retrying at the next daily sweep"
                        );
                        RETENTION_INTERVAL
                    }
                };
                tokio::time::sleep(delay).await;
            }
        });
        Self { task }
    }

    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

struct AuditQueue {
    tasks: ArrayQueue<AuditDraft>,
    ready: Notify,
    closed: AtomicBool,
}

impl AuditQueue {
    fn new() -> Self {
        Self {
            tasks: ArrayQueue::new(QUEUE_CAPACITY),
            ready: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn submit(&self, draft: AuditDraft) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        // 固定容量，满时覆盖最旧任务；请求不等待空位或数据库写入。
        drop(self.tasks.force_push(draft));
        self.ready.notify_one();
    }

    async fn next_batch(&self) -> Option<Vec<AuditDraft>> {
        loop {
            let notified = self.ready.notified();
            let mut batch = Vec::with_capacity(BATCH_CAPACITY);
            while batch.len() < BATCH_CAPACITY {
                let Some(draft) = self.tasks.pop() else { break };
                batch.push(draft);
            }
            if !batch.is_empty() {
                return Some(batch);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            notified.await;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_one();
    }
}

#[derive(Clone)]
pub struct Writer {
    queue: Arc<AuditQueue>,
    finished: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Writer {
    pub fn start(store: Store) -> Self {
        let queue = Arc::new(AuditQueue::new());
        let worker_queue = queue.clone();
        let (finished, received) = oneshot::channel();
        tokio::spawn(async move {
            while let Some(batch) = worker_queue.next_batch().await {
                if !matches!(
                    tokio::time::timeout(PERSIST_WAIT, store.persist_batch(&batch)).await,
                    Ok(Ok(()))
                ) {
                    eprintln!("audit: batch persistence failed or timed out; tasks dropped");
                }
            }
            let _ = finished.send(());
        });
        Self {
            queue,
            finished: Arc::new(Mutex::new(Some(received))),
        }
    }

    fn submit(&self, draft: AuditDraft) {
        self.queue.submit(draft);
    }

    pub async fn drain(&self) {
        self.queue.close();
        let Some(finished) = self.finished.lock().await.take() else {
            return;
        };
        if tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .is_err()
        {
            eprintln!("audit: shutdown drain deadline reached; queued tasks may be lost");
        }
    }
}

pub async fn plugin(store: Store, writer: Writer) -> anyhow::Result<ComposedPlugin> {
    let registration: ManagementRegistration =
        serde_json::from_slice(include_bytes!("../../dist/plugin/management.json"))?;
    let secret = store.secret("query_cursor").await?;
    let builder = PluginBuilder::from_json(include_bytes!("../../plugin.json"))?
        .middleware(move |call: RequestCall| {
            let writer = writer.clone();
            async move {
                let head = &call.request.head;
                if head.endpoint == "/v1/responses"
                    && call.request.body.len() <= MAX_OBSERVATION_BYTES
                    && let Ok(body) = serde_json::from_slice(&call.request.body)
                {
                    let observation = RequestObservation {
                        key_id: head.client_key_id.clone(),
                        request_id: head.request_id.clone(),
                        body,
                        headers: head
                            .headers
                            .iter()
                            .filter_map(|header| {
                                let name = header.name.to_ascii_lowercase();
                                if !matches!(
                                    name.as_str(),
                                    "x-codex-turn-metadata"
                                        | "session_id"
                                        | "x-codex-session-id"
                                        | "thread_source"
                                        | "x-codex-thread-source"
                                        | "subagent_kind"
                                        | "x-codex-subagent-kind"
                                        | "request_kind"
                                        | "x-codex-request-kind"
                                        | "turn_trigger"
                                        | "x-codex-turn-trigger"
                                        | "x-openai-subagent"
                                ) {
                                    return None;
                                }
                                std::str::from_utf8(&header.value)
                                    .ok()
                                    .map(|value| (name, value.to_owned()))
                            })
                            .collect::<BTreeMap<_, _>>(),
                        observed_at: Utc::now(),
                    };
                    if let Some(draft) = collect(&observation) {
                        writer.submit(draft);
                    }
                }
                call.next.run(call.request).await
            }
        })?
        .management(registration, move |call| {
            let store = store.clone();
            let secret = secret.clone();
            async move {
                let result = QueryService::management(
                    &store,
                    secret,
                    &call.request.method,
                    &call.request.path,
                    &call.request.query,
                    &call.payload,
                    CurrentKeys::fetch(&call.host),
                )
                .await;
                let payload = serde_json::to_vec(&result.value).map_err(|_| {
                    PluginFault::new(
                        gateway_plugin_sdk::ErrorCode::Fault,
                        "audit response encoding failed",
                    )
                })?;
                Ok(TypedReply::new(ManagementResponse {
                    status: result.status,
                    content_type: "application/json".into(),
                    headers: Vec::new(),
                })
                .with_payload(payload))
            }
        })?;
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::ContentBlock;

    fn draft(index: usize) -> AuditDraft {
        AuditDraft {
            key_id: "key".into(),
            observed_at: Utc::now(),
            request_id: index.to_string(),
            thread_id: None,
            session_id: None,
            turn_id: None,
            blocks: vec![ContentBlock::Text {
                text: index.to_string(),
            }],
            input_kinds: vec![None],
            repositories: None,
            context: Vec::new(),
        }
    }

    #[test]
    fn full_queue_overwrites_oldest_and_retains_new_tasks_in_order() {
        let queue = AuditQueue::new();
        for index in 0..QUEUE_CAPACITY + 3 {
            queue.submit(draft(index));
        }
        assert_eq!(queue.tasks.len(), QUEUE_CAPACITY);
        for index in 3..QUEUE_CAPACITY + 3 {
            assert_eq!(queue.tasks.pop().unwrap().request_id, index.to_string());
        }
        assert!(queue.tasks.is_empty());
    }

    #[tokio::test]
    async fn batches_are_bounded_and_shutdown_drains_existing_tasks() {
        let queue = AuditQueue::new();
        for index in 0..QUEUE_CAPACITY {
            queue.submit(draft(index));
        }
        queue.close();
        queue.submit(draft(QUEUE_CAPACITY));
        let first = queue.next_batch().await.unwrap();
        let second = queue.next_batch().await.unwrap();
        assert_eq!(first.len(), BATCH_CAPACITY);
        assert_eq!(second.len(), BATCH_CAPACITY);
        assert_eq!(first[0].request_id, "0");
        assert_eq!(second.last().unwrap().request_id, "63");
        assert!(queue.next_batch().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submission_continues_when_worker_holds_a_batch_and_is_blocked() {
        let queue = Arc::new(AuditQueue::new());
        queue.submit(draft(0));
        let (acquired, acquired_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let worker_queue = queue.clone();
        let worker = tokio::spawn(async move {
            let batch = worker_queue.next_batch().await.unwrap();
            acquired.send(()).unwrap();
            release_receiver.await.unwrap();
            batch
        });
        acquired_receiver.await.unwrap();
        let producer_queue = queue.clone();
        let producer = tokio::spawn(async move {
            for index in 1..=QUEUE_CAPACITY * 2 {
                producer_queue.submit(draft(index));
            }
        });
        tokio::time::timeout(Duration::from_secs(1), producer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queue.tasks.len(), QUEUE_CAPACITY);
        assert_eq!(queue.tasks.pop().unwrap().request_id, "65");
        release.send(()).unwrap();
        assert_eq!(worker.await.unwrap()[0].request_id, "0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sleeping_worker_wakes_for_concurrent_submissions_and_close() {
        let queue = Arc::new(AuditQueue::new());
        let worker_queue = queue.clone();
        let worker = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(batch) = worker_queue.next_batch().await {
                received.extend(batch.into_iter().map(|task| task.request_id));
            }
            received
        });
        for index in 0..32 {
            tokio::task::yield_now().await;
            queue.submit(draft(index));
        }
        queue.close();
        let received = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received,
            (0..32).map(|index| index.to_string()).collect::<Vec<_>>()
        );
    }
}

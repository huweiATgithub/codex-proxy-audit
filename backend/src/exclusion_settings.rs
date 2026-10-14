//! Persist and publish one shared collection policy without I/O on the request path.
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    collection::exclusions::{
        CompiledExclusions, ExclusionConfigError, ExclusionPolicy, ExclusionSettings,
    },
    query::QueryResponse,
    store::Store,
};

const MAX_SETTINGS_BYTES: usize = 256 * 1024;

struct State {
    store: Store,
    active: ArcSwap<ExclusionPolicy>,
    updates: Arc<Mutex<()>>,
}

#[derive(Clone)]
pub struct Exclusions {
    state: Arc<State>,
}

impl Exclusions {
    pub async fn load(store: Store) -> Self {
        let policy = match store.exclusion_settings().await {
            Ok(settings) => ExclusionPolicy::compile(settings.unwrap_or_default())
                .ok()
                .map(ExclusionPolicy::from),
            Err(_) => None,
        };
        if policy.is_none() {
            // Keep management available for an explicit repair; never silently
            // discard saved exclusions and start recording their traffic.
            eprintln!("audit: exclusion settings unavailable; capture suspended until repaired");
        }
        Self {
            state: Arc::new(State {
                store,
                active: ArcSwap::from_pointee(policy.unwrap_or_else(ExclusionPolicy::unavailable)),
                updates: Arc::new(Mutex::new(())),
            }),
        }
    }

    pub fn snapshot(&self) -> Arc<ExclusionPolicy> {
        self.state.active.load_full()
    }

    pub async fn management(&self, method: &str, query: &str, body: &[u8]) -> QueryResponse {
        if !query.is_empty() {
            return invalid("排除设置不接受查询参数。", None);
        }
        match method {
            "GET" if body.is_empty() => self
                .snapshot()
                .settings()
                .map(|settings| success(json!(settings)))
                .unwrap_or_else(unavailable),
            "PUT" => {
                if body.len() > MAX_SETTINGS_BYTES {
                    return invalid("排除设置正文过长。", None);
                }
                let settings: ExclusionSettings = match serde_json::from_slice(body) {
                    Ok(settings) => settings,
                    Err(_) => {
                        return invalid("排除设置必须只包含 userAgentPatterns 字符串数组。", None);
                    }
                };
                let policy = match ExclusionPolicy::compile(settings) {
                    Ok(policy) => policy,
                    Err(error) => return invalid_config(error),
                };
                self.save(policy).await
            }
            _ => invalid("排除设置请求方法或正文无效。", None),
        }
    }

    async fn save(&self, compiled: CompiledExclusions) -> QueryResponse {
        // Cancellation before the lock is acquired has no effect. After that,
        // an owned task completes durable persistence and publication together,
        // even if the management caller disconnects while SQLite is writing.
        let guard = self.state.updates.clone().lock_owned().await;
        let state = self.state.clone();
        let saved = tokio::spawn(async move {
            let _guard = guard;
            let settings = compiled.settings();
            state.store.save_exclusion_settings(settings).await?;
            let value = json!(settings);
            state.active.store(Arc::new(compiled.into()));
            Ok::<_, crate::store::StoreError>(value)
        })
        .await;
        match saved {
            Ok(Ok(value)) => success(value),
            _ => unavailable(),
        }
    }
}

fn success(value: Value) -> QueryResponse {
    QueryResponse { status: 200, value }
}

fn invalid_config(error: ExclusionConfigError) -> QueryResponse {
    invalid(&error.message, error.pattern_index)
}

fn invalid(message: &str, pattern_index: Option<usize>) -> QueryResponse {
    let mut error = json!({"code":"invalid_exclusions", "message":message.chars().take(300).collect::<String>()});
    if let Some(index) = pattern_index {
        error["patternIndex"] = json!(index);
    }
    QueryResponse {
        status: 400,
        value: json!({"error":error}),
    }
}

fn unavailable() -> QueryResponse {
    QueryResponse {
        status: 503,
        value: json!({"error":{"code":"unavailable", "message":"排除设置暂时不可用，请重新保存设置或稍后重试。"}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnected_save_completes_publication_without_blocking_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.sqlite3");
        let store = Store::open(&path).await.unwrap();
        let exclusions = Exclusions::load(store.clone()).await;
        let initial = json!({"userAgentPatterns":["^initial/"]});
        assert_eq!(
            exclusions
                .management("PUT", "", &serde_json::to_vec(&initial).unwrap())
                .await
                .status,
            200
        );
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .unwrap();
        let saved = json!({"userAgentPatterns":["^saved/"]});
        let request_exclusions = exclusions.clone();
        let body = serde_json::to_vec(&saved).unwrap();
        let caller =
            tokio::spawn(async move { request_exclusions.management("PUT", "", &body).await });
        // Synchronize cancellation with the admitted save while an independent
        // SQLite writer holds persistence. Assertions exercise management and
        // snapshots rather than relying on the discarded caller's response.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if exclusions.state.updates.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let during_write = tokio::time::timeout(
            Duration::from_millis(100),
            exclusions.management("GET", "", &[]),
        )
        .await
        .unwrap();
        assert_eq!(during_write.value, initial);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        sqlx::query("COMMIT")
            .execute(&mut *connection)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while exclusions.management("GET", "", &[]).await.value != saved {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        store.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            Exclusions::load(reopened.clone())
                .await
                .management("GET", "", &[])
                .await
                .value,
            saved
        );
        reopened.close().await;
        drop(connection);
        pool.close().await;
    }
}

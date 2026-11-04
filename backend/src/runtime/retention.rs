//! 通过宿主维护回调分批清理到期审计，按剩余预算续接每日清理

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::Utc;
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::host::{LogLevel, LogRequest},
    client::HostClient,
};
use tokio::{sync::Mutex, time::Instant};

use crate::store::{RETENTION_BATCH_SIZE, Store};

use super::{LOG_WAIT, log_event};

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const BATCH_PAUSE: Duration = Duration::from_millis(100);
const BATCH_WAIT: Duration = Duration::from_secs(4);
const BATCH_BUDGET: Duration = BATCH_WAIT
    .saturating_add(LOG_WAIT)
    .saturating_add(BATCH_PAUSE);

#[derive(Clone)]
pub(super) struct Retention {
    store: Store,
    next_sweep: Arc<Mutex<Option<Instant>>>,
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupError {
    Storage,
    Timeout,
}

impl Retention {
    pub(super) fn new(store: Store) -> Self {
        Self {
            store,
            next_sweep: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) async fn reconcile(&self, host: &HostClient) -> Result<(), PluginFault> {
        if let Err(error) = self.sweep(|| host.remaining_budget()).await {
            let reason = match error {
                CleanupError::Storage => "storage",
                CleanupError::Timeout => "timeout",
            };
            log_event(
                host,
                LogRequest {
                    event: "audit.retention.failed".into(),
                    level: LogLevel::Warn,
                    fields: BTreeMap::from([("reason".into(), reason.into())]),
                },
            )
            .await;
            return Err(PluginFault::new(
                ErrorCode::Fault,
                "audit retention cleanup failed",
            ));
        }
        Ok(())
    }

    async fn sweep(
        &self,
        remaining_budget: impl Fn() -> Option<Duration>,
    ) -> Result<(), CleanupError> {
        let mut next_sweep = self.next_sweep.lock().await;
        if next_sweep.is_some_and(|due| due > Instant::now()) {
            return Ok(());
        }
        loop {
            // 为完整批次、日志和批间让步保留预算，未完成的清理由后续回调继续
            if remaining_budget().is_none_or(|remaining| remaining < BATCH_BUDGET) {
                return Ok(());
            }
            let deleted =
                match tokio::time::timeout(BATCH_WAIT, self.store.delete_expired_batch(Utc::now()))
                    .await
                {
                    Ok(Ok(deleted)) => deleted,
                    Ok(Err(_)) => return Err(CleanupError::Storage),
                    Err(_) => return Err(CleanupError::Timeout),
                };
            if deleted < RETENTION_BATCH_SIZE as u64 {
                *next_sweep = Some(Instant::now() + INTERVAL);
                return Ok(());
            }
            tokio::time::sleep(BATCH_PAUSE).await;
        }
    }
}

#[cfg(test)]
mod tests;

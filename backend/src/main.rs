//! 独立插件进程入口；初始化插件私有存储并管理任务生命周期
use codex_proxy_audit::{
    runtime::{Retention, Writer, plugin},
    store::Store,
};
use gateway_plugin_sdk::client::{PluginSession, SessionConfig};
use std::time::Duration;

#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("audit: plugin initialization or session failed");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let (store, writer, handler, retention) = tokio::time::timeout(Duration::from_secs(4), async {
        let store = Store::initialize().await?;
        let writer = Writer::start(store.clone());
        let handler = plugin(store.clone(), writer.clone()).await?;
        let retention = Retention::start(store.clone());
        Ok::<_, anyhow::Error>((store, writer, handler, retention))
    })
    .await??;
    let result = tokio::select! {
        result = session.run(handler) => result,
        _ = shutdown_signal() => Ok(()),
    };
    retention.stop().await;
    writer.drain().await;
    store.close().await;
    result?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = signal.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

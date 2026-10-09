//! 独立插件进程入口；数据库连接仅取自宿主 secrets 合并后的握手
use codex_proxy_audit::{
    runtime::{Writer, plugin},
    store::Store,
};
use gateway_plugin_sdk::client::{PluginSession, SessionConfig};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Configuration {
    database_url: String,
}

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
    let configuration: Configuration =
        serde_json::from_value(session.handshake().configuration.clone())?;
    let (writer, handler) = tokio::time::timeout(Duration::from_secs(4), async {
        let store = Store::connect(&configuration.database_url).await?;
        let writer = Writer::start(store.clone());
        let handler = plugin(store, writer.clone()).await?;
        Ok::<_, anyhow::Error>((writer, handler))
    })
    .await??;
    let result = tokio::select! {
        result = session.run(handler) => result,
        _ = shutdown_signal() => Ok(()),
    };
    writer.drain().await;
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

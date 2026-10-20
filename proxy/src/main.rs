use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokenlens_proxy::{admin, build_state, config, control, proxy, spawn_sweeper};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  tokenlens-proxy               run the proxy (config path from TL_CONFIG, default config/dev.toml)
  tokenlens-proxy gen-key       print a new virtual key and the sha256 hash to put in config
  tokenlens-proxy hash-key KEY  print the sha256 hash of an existing virtual key";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen-key") => {
            let (key, hash) = config::generate_virtual_key();
            println!("virtual key (give to the customer, shown once): {key}");
            println!("sha256 (add to virtual_key_hashes in config):   {hash}");
            return Ok(());
        }
        Some("hash-key") => {
            let key = args.get(2).context(USAGE)?;
            println!("{}", config::sha256_hex(key));
            return Ok(());
        }
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown command {other:?}\n{USAGE}"),
        None => {}
    }

    // Logs go to stderr; stdout carries one JSON event per request.
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = std::env::var("TL_CONFIG").unwrap_or_else(|_| "config/dev.toml".into());
    let rt = config::Resolved::load(&path, |name| std::env::var(name).ok())?;
    let listen: SocketAddr = rt.cfg.server.listen.parse().context("server.listen")?;
    let admin_listen: SocketAddr = rt
        .cfg
        .server
        .admin_listen
        .parse()
        .context("server.admin_listen")?;
    if rt.admin_token.is_none() {
        tracing::warn!(
            env = %rt.cfg.server.admin_token_env,
            "admin API disabled: token env var is unset or shorter than 16 characters"
        );
    }

    let state = build_state(rt).await?;
    control::init(&state)
        .await
        .context("loading managed orgs, keys and budgets from Redis")?;
    control::spawn_sync(state.clone());
    spawn_sweeper(state.clone());

    let proxy_listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let admin_listener = tokio::net::TcpListener::bind(admin_listen)
        .await
        .with_context(|| format!("binding {admin_listen}"))?;
    tracing::info!(%listen, %admin_listen, config = %path, dashboard = %format!("http://{admin_listen}/"), "tokenlens proxy started");

    let proxy_server = axum::serve(proxy_listener, proxy::router(state.clone()))
        .with_graceful_shutdown(shutdown_signal());
    let admin_server =
        axum::serve(admin_listener, admin::router(state)).with_graceful_shutdown(shutdown_signal());
    tokio::try_join!(async { proxy_server.await }, async { admin_server.await })?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

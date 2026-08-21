pub mod admin;
pub mod handlers;
pub mod router;

use crate::runtime::RuntimeHandle;
use crate::AppState;
use tauri::AppHandle;

pub async fn start_server(
    app: AppHandle,
    state: std::sync::Arc<AppState>,
) -> Result<(), anyhow::Error> {
    start_server_with_runtime(RuntimeHandle::desktop(app), state, None, None, None).await
}

/// Start either the desktop loopback server or the standalone Linux server.
/// `admin_token` is only supplied by the latter and is enforced by its router.
pub async fn start_server_with_runtime(
    runtime: RuntimeHandle,
    state: std::sync::Arc<AppState>,
    bind: Option<(String, u16)>,
    admin_token: Option<String>,
    mcp_token: Option<String>,
) -> Result<(), anyhow::Error> {
    let (host, port) =
        bind.unwrap_or_else(|| (get_server_host(&runtime), get_server_port(&runtime)));

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_addr = listener.local_addr()?;
    let actual_port = local_addr.port();

    *state.server_port.write().await = actual_port;
    state
        .server_running
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let router =
        router::create_router_with_tokens(runtime.clone(), state.clone(), admin_token, mcp_token);

    runtime
        .emit(
            "server-started",
            serde_json::json!({
                "port": actual_port,
                "url": format!("http://{}:{}", host, actual_port)
            }),
        )
        .ok();

    tracing::info!(
        "WaLiAPI server listening on http://{}:{}",
        host,
        actual_port
    );

    axum::serve(listener, router).await?;

    state
        .server_running
        .store(false, std::sync::atomic::Ordering::SeqCst);

    Ok(())
}

fn get_server_host(app: &RuntimeHandle) -> String {
    if let Some(host) = app.setting("server.host") {
        if let Some(value) = host.as_str() {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

fn get_server_port(app: &RuntimeHandle) -> u16 {
    if let Some(port) = app.setting("server.port") {
        if let Some(value) = port.as_u64() {
            return value as u16;
        }
    }
    8777
}

use super::handlers::*;
use crate::runtime::RuntimeHandle;
use crate::services::Service;
use crate::AppState;
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    middleware,
    routing::{any, get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

pub fn create_router(runtime: RuntimeHandle, state: Arc<AppState>) -> Router {
    create_router_with_tokens(runtime, state, None, None)
}

pub fn create_router_with_admin(
    runtime: RuntimeHandle,
    state: Arc<AppState>,
    admin_token: Option<String>,
) -> Router {
    create_router_with_tokens(runtime, state, admin_token, None)
}

pub fn create_router_with_tokens(
    runtime: RuntimeHandle,
    state: Arc<AppState>,
    admin_token: Option<String>,
    mcp_token: Option<String>,
) -> Router {
    let web_dir = std::env::var_os("WALIAPI_WEB_DIR")
        .or_else(|| std::env::var_os("WALIAPI_STATIC_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist"));
    create_router_configured(runtime, state, admin_token, mcp_token, web_dir)
}

fn create_router_configured(
    runtime: RuntimeHandle,
    state: Arc<AppState>,
    admin_token: Option<String>,
    mcp_token: Option<String>,
    web_dir: std::path::PathBuf,
) -> Router {
    let shared = SharedState {
        app: runtime.clone(),
        state: state.clone(),
        admin_token,
        mcp_token,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let service_router = crate::services::ServiceRegistry::new().merge_routes(state.clone());

    let public_base = Router::new()
        // OpenAI Chat Completions
        .route("/v1/chat/completions", post(handle_chat_completions))
        // OpenAI Completions (legacy)
        .route("/v1/completions", post(handle_completions))
        // OpenAI Responses API
        .route("/v1/responses", post(handle_responses))
        // OpenAI Embeddings
        .route("/v1/embeddings", post(handle_embeddings))
        // OpenAI Models
        .route("/v1/models", get(handle_list_models))
        // OpenAI Images
        .route("/v1/images/generations", post(handle_images))
        // OpenAI Audio
        .route(
            "/v1/audio/transcriptions",
            post(handle_audio_transcriptions),
        )
        .route("/v1/audio/speech", post(handle_audio_speech))
        // Anthropic Messages API
        .route(
            "/v1/messages",
            post(handle_messages).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(handle_messages_count_tokens).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        // Health check
        .route("/health", get(handle_health));

    // Desktop keeps the original loopback service behavior. In headless mode,
    // KB/Wiki remain behind the administrator token while MCP has its own
    // least-privilege credential for external Agents.
    let public = if runtime.is_headless() {
        public_base
            .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
            .layer(cors)
            .with_state(shared.clone())
    } else {
        public_base
            .merge(service_router.clone())
            .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
            .layer(cors)
            .with_state(shared.clone())
    };

    // Kept outside the permissive OpenAI CORS layer. The bounded limit permits
    // browser import/export payloads without becoming an unbounded upload API.
    let admin = Router::new()
        .route("/api/admin/invoke", post(crate::server::admin::invoke))
        .route("/api/admin/events", get(crate::server::admin::events))
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(shared.clone());
    let router = if runtime.is_headless() {
        let protected_management = crate::services::knowledge::routes::create_router(state.clone())
            .merge(crate::services::wiki::routes::create_router(state.clone()))
            .route_layer(middleware::from_fn_with_state(
                shared.clone(),
                crate::server::admin::require_admin,
            ))
            .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
            .with_state(shared.clone());
        let protected_mcp = crate::services::mcp::McpService
            .routes(state.clone())
            .route_layer(middleware::from_fn_with_state(
                shared.clone(),
                crate::server::admin::require_mcp,
            ))
            .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
            .with_state(shared.clone());
        public
            .merge(protected_management)
            .merge(protected_mcp)
            .merge(admin)
    } else {
        public.merge(admin)
    };
    if runtime.is_headless() {
        let index = web_dir.join("index.html");
        if index.is_file() {
            return router
                // Unknown API/data/tool paths must remain real 404s instead of
                // being swallowed by the SPA's index.html fallback.
                .route("/api", any(namespace_not_found))
                .route("/api/{*path}", any(namespace_not_found))
                .route("/v1", any(namespace_not_found))
                .route("/v1/{*path}", any(namespace_not_found))
                .route("/mcp/{*path}", any(namespace_not_found))
                .nest_service("/assets", ServeDir::new(web_dir.join("assets")))
                .fallback_service(ServeDir::new(web_dir).fallback(ServeFile::new(index)));
        }
        tracing::warn!("web UI dist not found; set WALIAPI_WEB_DIR to enable the browser UI");
    }
    router
}

async fn namespace_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[derive(Clone)]
pub struct SharedState {
    pub app: RuntimeHandle,
    pub state: Arc<AppState>,
    pub admin_token: Option<String>,
    pub mcp_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Method, Request},
    };
    use tower::ServiceExt;

    async fn headless_router() -> Router {
        let data_dir =
            std::env::temp_dir().join(format!("waliapi-router-{}", uuid::Uuid::new_v4()));
        let web_dir = data_dir.join("web");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(
            web_dir.join("index.html"),
            "<!doctype html><title>WaLiAPI</title>",
        )
        .unwrap();

        let runtime = RuntimeHandle::headless(&data_dir).unwrap();
        let db = Arc::new(crate::db::Database::new_in_dir(data_dir).await.unwrap());
        let auth_service = Arc::new(crate::auth_provider::service::AuthService::new(
            Arc::new(crate::db::repository::Repository::new(db.pool.clone())),
            crate::auth_provider::ProviderRegistry::new(),
        ));
        let state = Arc::new(AppState {
            runtime: runtime.clone(),
            db,
            auth_service,
            login_sessions: Arc::new(crate::commands::auth::LoginSessions::new()),
            server_port: Arc::new(tokio::sync::RwLock::new(8777)),
            server_running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            server_handle: Arc::new(tokio::sync::RwLock::new(None)),
            test_receipts: Arc::new(crate::services::channel_test::TestReceiptStore::new(
                std::time::Duration::from_secs(60),
            )),
        });
        create_router_configured(
            runtime,
            state,
            Some("test-admin-token".into()),
            Some("test-mcp-token".into()),
            web_dir,
        )
    }

    async fn status(router: &Router, request: Request<Body>) -> axum::http::Response<Body> {
        router.clone().oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn headless_security_boundaries_are_preserved() {
        let router = headless_router().await;

        for path in ["/api/kb", "/api/wiki/projects", "/mcp"] {
            let response = status(
                &router,
                Request::builder().uri(path).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "path: {path}");
            assert!(response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none());
        }

        for path in ["/api/unknown", "/v1/unknown", "/mcp/unknown"] {
            let response = status(
                &router,
                Request::builder().uri(path).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path: {path}");
        }

        let response = status(
            &router,
            Request::builder()
                .uri("/api/kb")
                .header(header::AUTHORIZATION, "Bearer test-admin-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/mcp")
                .header(header::AUTHORIZATION, "Bearer test-admin-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/mcp")
                .header(header::AUTHORIZATION, "Bearer test-mcp-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = status(
            &router,
            Request::builder()
                .uri("/api/kb")
                .header(header::AUTHORIZATION, "Bearer test-mcp-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/invoke")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer test-mcp-token")
                .body(Body::from(r#"{"command":"get_server_status","args":{}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let invoke_body = Body::from(r#"{"command":"get_server_status","args":{}}"#);
        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/invoke")
                .header(header::CONTENT_TYPE, "application/json")
                .body(invoke_body)
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/invoke")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer wrong-token")
                .body(Body::from(r#"{"command":"get_server_status","args":{}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = status(
            &router,
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/invoke")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer test-admin-token")
                .body(Body::from(r#"{"command":"get_server_status","args":{}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = status(
            &router,
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/admin/invoke")
                .header(header::ORIGIN, "https://evil.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());

        let response = status(
            &router,
            Request::builder()
                .uri("/channels")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}

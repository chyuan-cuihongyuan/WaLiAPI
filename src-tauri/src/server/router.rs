use super::handlers::*;
use crate::AppState;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

#[cfg(not(feature = "desktop-ui"))]
use tauri::Manager;
#[cfg(feature = "desktop-ui")]
use tauri::{AppHandle, Manager};

/// 构建 axum 路由。`desktop_app` 仅桌面端传入（对话框等桌面专属命令需要真实 AppHandle）；
/// headless 传 None，此时通过 tauri MockRuntime 获得 `State<'static, Arc<AppState>>` 供命令分发。
///
/// 注意：任何对 `AppHandle<Wry>` 的方法调用都会把整套 wry/webkit 链接进二进制，
/// 因此 desktop 分支代码全部用 `desktop-ui` feature 门控。
#[cfg(feature = "desktop-ui")]
pub fn create_router(state: Arc<AppState>, desktop_app: Option<AppHandle>) -> Router {
    // 桌面端：泄漏真实 AppHandle 换取 'static 引用（App 生命周期即进程生命周期）。
    let (state_static, desktop_app_static) = match desktop_app {
        Some(app) => {
            let leaked: &'static AppHandle = Box::leak(Box::new(app));
            (leaked.state::<Arc<AppState>>(), Some(leaked))
        }
        None => (mock_state_handle(&state), None),
    };

    let tokens = super::admin::ServiceTokens::from_env();
    let shared = SharedState {
        state: state.clone(),
        state_static,
        admin_token: tokens.admin,
        mcp_token: tokens.mcp,
        desktop_app: desktop_app_static,
    };

    build_router(state, shared)
}

/// headless（无 desktop-ui）：仅 mock State，无任何 AppHandle<Wry> 引用。
#[cfg(not(feature = "desktop-ui"))]
pub fn create_router(state: Arc<AppState>) -> Router {
    let tokens = super::admin::ServiceTokens::from_env();
    let shared = SharedState {
        state: state.clone(),
        state_static: mock_state_handle(&state),
        admin_token: tokens.admin,
        mcp_token: tokens.mcp,
    };

    build_router(state, shared)
}

/// 通过 MockRuntime 获得 `State<'static, Arc<AppState>>`（无窗口/事件循环，仅作状态容器）。
fn mock_state_handle(state: &Arc<AppState>) -> tauri::State<'static, Arc<AppState>> {
    let mock = Box::leak(Box::new(tauri::test::mock_app()));
    mock.manage(state.clone());
    let handle: &'static tauri::AppHandle<tauri::test::MockRuntime> =
        Box::leak(Box::new(mock.handle().clone()));
    handle.state::<Arc<AppState>>()
}

fn build_router(state: Arc<AppState>, shared: SharedState) -> Router {
    // 宽松 CORS 仅作用于数据面 /v1/* 与 /health（API Key 鉴权，供浏览器/跨域客户端调用）；
    // KB/Wiki/MCP 服务路由带独立 token 鉴权，不附带宽松 CORS（防止任意网页跨域读取知识资产）；
    // /admin/api 与静态资源不附带 CORS 头（仅同源可用，配合 CSRF 中间件）。
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    // Service registry — 服务路由按鉴权域分组装配：
    // KB/Wiki REST 挂管理员 token（WALIAPI_ADMIN_TOKEN），MCP 挂独立 token（WALIAPI_MCP_TOKEN）。
    let registry = crate::services::ServiceRegistry::new();
    let kb_wiki_router = registry
        .merge_routes_for(&["knowledge", "wiki"], state.clone())
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            super::admin::require_admin,
        ));
    let mcp_router =
        registry
            .merge_routes_for(&["mcp"], state.clone())
            .layer(middleware::from_fn_with_state(
                shared.clone(),
                super::admin::require_mcp,
            ));

    // Web 管理面板（/admin/api/*，自带会话鉴权 + CSRF 防护）
    let admin = super::admin_routes::router(shared.clone());

    // 数据面（LLM 网关）路由：API Key 鉴权 + 宽松 CORS
    let data_plane_router = Router::new()
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
        .route("/health", get(handle_health))
        .layer(cors);

    let gateway_router = data_plane_router.merge(kb_wiki_router).merge(mcp_router);

    Router::new()
        .merge(gateway_router)
        // Web 管理面板 API（无宽松 CORS）
        .nest("/admin/api", admin)
        // 内嵌 Web 静态资源（SPA fallback，须放在所有 API 路由之后）
        .merge(super::static_assets::static_router())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(shared)
}

#[derive(Clone)]
pub struct SharedState {
    pub state: Arc<AppState>,
    /// 供 /admin/api/invoke 分发用的 'static State（桌面：真实 handle；headless：mock handle）。
    pub state_static: tauri::State<'static, Arc<AppState>>,
    /// KB/Wiki REST 端点（/api/kb、/api/wiki）的管理员 Bearer token
    /// （WALIAPI_ADMIN_TOKEN；None = 端点关闭，一律 401）。
    pub admin_token: Option<Arc<str>>,
    /// MCP 端点（/mcp*）的独立 Bearer token（WALIAPI_MCP_TOKEN；None = 端点关闭，一律 401）。
    pub mcp_token: Option<Arc<str>>,
    /// 桌面端真实 AppHandle（文件对话框、自动启动等桌面专属命令）；headless 编译期不存在该字段。
    #[cfg(feature = "desktop-ui")]
    pub desktop_app: Option<&'static AppHandle>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
    const MCP_TOKEN: &str = "mcp-token-fedcba9876543210fedcba9876543210";

    /// 构造最小可用的 AppState（临时目录真实 SQLite + 真实迁移），让 build_router
    /// 中的处理器可以真正执行（列表查询、JSON-RPC 分发）。
    async fn test_state() -> Arc<AppState> {
        let data_dir =
            std::env::temp_dir().join(format!("waliapi-router-test-{}", uuid::Uuid::new_v4()));
        let db = Arc::new(crate::db::Database::new_with_path(&data_dir).await);
        let auth_service = Arc::new(crate::auth_provider::service::AuthService::new(
            Arc::new(crate::db::repository::Repository::new(db.pool.clone())),
            crate::auth_provider::ProviderRegistry::new(),
        ));
        let (event_tx, _) =
            tokio::sync::broadcast::channel(crate::server::event_bridge::EVENT_CHANNEL_CAPACITY);
        Arc::new(AppState {
            db,
            auth_service,
            login_sessions: Arc::new(crate::commands::auth::LoginSessions::new()),
            server_port: Arc::new(tokio::sync::RwLock::new(0)),
            server_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_handle: Arc::new(tokio::sync::RwLock::new(None)),
            test_receipts: Arc::new(crate::services::channel_test::TestReceiptStore::new(
                std::time::Duration::from_secs(60),
            )),
            admin_sessions: crate::server::admin_auth::SessionStore::new(),
            login_throttle: crate::server::admin_auth::LoginThrottle::new(),
            events: crate::server::event_bridge::EventSink::headless(event_tx),
            settings: crate::settings_store::SettingsStore::file(data_dir.join("settings.json")),
            data_dir,
        })
    }

    fn test_shared(state: &Arc<AppState>, admin: Option<&str>, mcp: Option<&str>) -> SharedState {
        SharedState {
            state: state.clone(),
            state_static: mock_state_handle(state),
            admin_token: admin.map(Arc::from),
            mcp_token: mcp.map(Arc::from),
            #[cfg(feature = "desktop-ui")]
            desktop_app: None,
        }
    }

    fn request(method: &str, uri: &str, bearer: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    fn json_request(method: &str, uri: &str, bearer: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn kb_and_wiki_rest_require_admin_token() {
        let state = test_state().await;
        let shared = test_shared(&state, Some(ADMIN_TOKEN), Some(MCP_TOKEN));
        let app = build_router(state.clone(), shared);

        // 无 token → 401
        let res = app
            .clone()
            .oneshot(request("GET", "/api/kb", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // MCP token 与管理员 token 是独立凭证域：MCP token 打不开 KB
        let res = app
            .clone()
            .oneshot(request("GET", "/api/kb", Some(MCP_TOKEN)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 正确管理员 token → 200（空库列表）
        let res = app
            .clone()
            .oneshot(request("GET", "/api/kb", Some(ADMIN_TOKEN)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Wiki 端点同样要求管理员 token
        let res = app
            .clone()
            .oneshot(request("GET", "/api/wiki/projects", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .clone()
            .oneshot(request("GET", "/api/wiki/projects", Some(ADMIN_TOKEN)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_endpoint_requires_mcp_token() {
        let state = test_state().await;
        let shared = test_shared(&state, Some(ADMIN_TOKEN), Some(MCP_TOKEN));
        let app = build_router(state.clone(), shared);

        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.0.1"}}}"#;

        // 无 token / 错误 token → 401
        let res = app
            .clone()
            .oneshot(json_request("POST", "/mcp", None, initialize))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .clone()
            .oneshot(json_request("POST", "/mcp", Some(ADMIN_TOKEN), initialize))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 正确 MCP token → 通过鉴权并进入 JSON-RPC 分发（initialize 成功）
        let res = app
            .clone()
            .oneshot(json_request("POST", "/mcp", Some(MCP_TOKEN), initialize))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn service_endpoints_fail_closed_without_configured_tokens() {
        let state = test_state().await;
        // 两个 token 都未配置（桌面默认形态）：端点关闭，一律 401，绝不无鉴权放行
        let shared = test_shared(&state, None, None);
        let app = build_router(state.clone(), shared);

        for (method, uri) in [
            ("GET", "/api/kb"),
            ("GET", "/api/wiki/projects"),
            ("POST", "/mcp"),
        ] {
            let res = app
                .clone()
                .oneshot(request(method, uri, None))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        }

        // 数据面 /health 不受服务 token 影响
        let res = app.oneshot(request("GET", "/health", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cors_only_covers_data_plane_not_service_routes() {
        let state = test_state().await;
        let shared = test_shared(&state, Some(ADMIN_TOKEN), Some(MCP_TOKEN));
        let app = build_router(state.clone(), shared);

        let preflight = |uri: &str| {
            Request::builder()
                .method("OPTIONS")
                .uri(uri)
                .header("origin", "https://evil.example")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap()
        };

        // 数据面：预检通过，返回宽松 CORS 头（跨域调用是网关的设计用法）
        let res = app
            .clone()
            .oneshot(preflight("/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );

        // 服务路由：不再挂宽松 CORS（KB 预检被鉴权拦截，响应无 CORS 头）
        let res = app.clone().oneshot(preflight("/api/kb")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get("access-control-allow-origin").is_none());

        // 带 token 的实际跨域请求可以成功，但响应无 CORS 头 → 浏览器跨域读取不可行
        let mut req = request("GET", "/api/kb", Some(ADMIN_TOKEN));
        req.headers_mut().insert(
            axum::http::header::ORIGIN,
            "https://evil.example".parse().unwrap(),
        );
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }

    // ─── FIX-17：管理面认证加固（端到端） ─────────────────────────────────

    fn admin_json_post(uri: &str, bearer: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-requested-with", "XMLHttpRequest");
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn admin_login_flow_hardening() {
        let state = test_state().await;
        let shared = test_shared(&state, Some(ADMIN_TOKEN), Some(MCP_TOKEN));
        let app = build_router(state.clone(), shared);

        // 初始管理员 + INITIAL_PASSWORD 文件；密码从文件读出，不在源码落字面量
        crate::server::admin_auth::ensure_initial_admin(&state.db.pool, &state.data_dir)
            .await
            .unwrap();
        let pw_file = state.data_dir.join("INITIAL_PASSWORD");
        assert!(pw_file.exists());
        let content = std::fs::read_to_string(&pw_file).unwrap();
        let password = content
            .lines()
            .find_map(|l| l.strip_prefix("password: "))
            .unwrap()
            .to_string();
        let login_body = |pw: &str| {
            serde_json::json!({ "username": "admin", "password": pw }).to_string()
        };

        // 成功登录：HttpOnly Cookie + 初始密码文件删除 + token 可用
        let res = app
            .clone()
            .oneshot(admin_json_post(
                "/admin/api/auth/login",
                None,
                &login_body(&password),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_string();
        assert!(cookie.contains("HttpOnly"), "cookie 必须带 HttpOnly: {cookie}");
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let token = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let res = app
            .clone()
            .oneshot(request("GET", "/admin/api/auth/check", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!pw_file.exists(), "首登成功后初始密码文件必须删除");

        // 第二个会话（模拟另一设备登录）
        let res = app
            .clone()
            .oneshot(admin_json_post(
                "/admin/api/auth/login",
                None,
                &login_body(&password),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let token2 = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

        // 改密：当前会话续期，其他旧会话全部吊销（FIX-17）。
        // 新密码运行期随机生成，请求体经 json! 宏注入，源码不落可用凭据字面量。
        let new_password = format!("rotated-{}", uuid::Uuid::new_v4());
        let change_body = serde_json::json!({
            "old_password": password,
            "new_password": new_password,
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(admin_json_post(
                "/admin/api/auth/change-password",
                Some(&token),
                &change_body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(request("GET", "/admin/api/auth/check", Some(&token)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "当前会话应保持有效");
        let res = app
            .clone()
            .oneshot(request("GET", "/admin/api/auth/check", Some(&token2)))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "改密后旧会话必须全部吊销"
        );

        // 连续失败触发限速：错密码连打 6 次（400），第 7 次 429 + Retry-After
        let wrong_password = format!("wrong-{}", uuid::Uuid::new_v4());
        for i in 1..=6 {
            let res = app
                .clone()
                .oneshot(admin_json_post(
                    "/admin/api/auth/login",
                    None,
                    &login_body(&wrong_password),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "第 {i} 次失败应为 400");
        }
        let res = app
            .clone()
            .oneshot(admin_json_post(
                "/admin/api/auth/login",
                None,
                &login_body(&wrong_password),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS, "第 7 次应被限速");
        assert!(res.headers().get("Retry-After").is_some());
    }
}

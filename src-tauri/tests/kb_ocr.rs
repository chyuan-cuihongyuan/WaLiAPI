//! 知识库 VLM OCR（方案A）集成测试。
//!
//! 覆盖：
//!   * migration 025：kb_knowledge_bases.ocr_model / kb_documents OCR 三列的写入与回读；
//!   * ocr 全局开关在 SettingsStore 中的默认值与往返；
//!   * VlmOcrClient 经 mock VLM server（axum 本地端口）验证：
//!     识别成功 + 请求体形状（模型映射、视觉消息格式）、首渠道 500 故障切换、
//!     无渠道声明模型时报 OCR_NO_VISION_CHANNEL、过短结果视为失败。

use std::sync::{Arc, Mutex};

use axum::{extract::State, http::StatusCode, response::Json, routing::post, Router};
use sqlx::SqlitePool;
use waliapi_lib::db::repository::Repository;
use waliapi_lib::services::knowledge::models::{CreateKbInput, UpdateKbInput};
use waliapi_lib::services::knowledge::ocr::vlm::VlmOcrClient;
use waliapi_lib::services::knowledge::repository::KbRepository;
use waliapi_lib::settings_store::SettingsStore;

/// In-memory SQLite with all migrations (incl. 025) applied.
async fn fresh_db() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory db");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate fresh db");
    pool
}

async fn insert_channel(pool: &SqlitePool, id: &str, base_url: &str, models: &str, priority: i64) {
    insert_channel_full(pool, id, base_url, models, "{}", priority).await;
}

async fn insert_channel_full(
    pool: &SqlitePool,
    id: &str,
    base_url: &str,
    models: &str,
    model_mapping: &str,
    priority: i64,
) {
    sqlx::query(
        "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, weight, config, model_mapping, timeout_secs, created_at, updated_at)
         VALUES (?, ?, 'openai', ?, 'sk-test', ?, 1, ?, 1, '{}', ?, 60, '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
    )
    .bind(id)
    .bind(format!("chan-{id}"))
    .bind(base_url)
    .bind(models)
    .bind(priority)
    .bind(model_mapping)
    .execute(pool)
    .await
    .expect("insert channel");
}

// ── Mock VLM server ───────────────────────────────────────────────

#[derive(Clone)]
struct MockVlm {
    status: StatusCode,
    content: String,
    /// 捕获的请求体（用于断言模型映射与视觉消息格式）
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn mock_chat_completions(
    State(mock): State<MockVlm>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    mock.requests.lock().unwrap().push(body);
    if mock.status.is_success() {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": mock.content}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150}
            })),
        )
    } else {
        (
            mock.status,
            Json(serde_json::json!({"error": "upstream boom"})),
        )
    }
}

/// 启动 mock VLM server，返回 (base_url, 请求捕获句柄)
async fn start_mock(
    status: StatusCode,
    content: &str,
) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let mock = MockVlm {
        status,
        content: content.to_string(),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let requests = mock.requests.clone();
    let app = Router::new()
        .route("/chat/completions", post(mock_chat_completions))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{}", addr), requests)
}

// ── VlmOcrClient ──────────────────────────────────────────────────

#[tokio::test]
async fn ocr_page_success_applies_model_mapping_and_vision_format() {
    let pool = fresh_db().await;
    let (base_url, requests) =
        start_mock(StatusCode::OK, "这是第一页的识别内容，包含题目与公式。").await;
    // 渠道声明 test-vl，映射到上游真实模型 actual-vl-model
    insert_channel_full(
        &pool,
        "ch1",
        &base_url,
        r#"["test-vl"]"#,
        r#"{"test-vl":"actual-vl-model"}"#,
        10,
    )
    .await;

    let repo = Repository::new(pool.clone());
    let client = VlmOcrClient::new(&repo, "test-vl");
    let result = client.ocr_page(b"fake-jpeg", 1).await.expect("ocr_page");

    assert_eq!(result.markdown, "这是第一页的识别内容，包含题目与公式。");
    assert_eq!(result.total_tokens, 150);

    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let body = &reqs[0];
    // 模型映射已应用
    assert_eq!(body["model"], "actual-vl-model");
    assert_eq!(body["stream"], false);
    // OpenAI 视觉消息格式：text + image_url(data:image/jpeg;base64,...)
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("Markdown"));
    assert_eq!(content[1]["type"], "image_url");
    assert!(content[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
}

#[tokio::test]
async fn ocr_page_failover_from_500_to_next_channel() {
    let pool = fresh_db().await;
    let (bad_url, bad_reqs) = start_mock(StatusCode::INTERNAL_SERVER_ERROR, "").await;
    let (ok_url, ok_reqs) = start_mock(StatusCode::OK, "来自备用渠道的识别结果。").await;
    // bad 渠道优先级高 → 先尝试；500 后应 failover 到 ok
    insert_channel(&pool, "bad", &bad_url, r#"["test-vl"]"#, 10).await;
    insert_channel(&pool, "ok", &ok_url, r#"["test-vl"]"#, 5).await;

    let repo = Repository::new(pool.clone());
    let client = VlmOcrClient::new(&repo, "test-vl");
    let result = client
        .ocr_page(b"fake-jpeg", 3)
        .await
        .expect("failover should succeed");

    assert_eq!(result.markdown, "来自备用渠道的识别结果。");
    assert_eq!(bad_reqs.lock().unwrap().len(), 1);
    assert_eq!(ok_reqs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ocr_page_without_matching_channel_returns_no_vision_channel() {
    let pool = fresh_db().await;
    let (url, _) = start_mock(StatusCode::OK, "不会到达这里。").await;
    // 渠道只声明了其他模型
    insert_channel(&pool, "ch1", &url, r#"["other-model"]"#, 10).await;

    let repo = Repository::new(pool.clone());
    let client = VlmOcrClient::new(&repo, "test-vl");
    let err = client.ocr_page(b"fake-jpeg", 1).await.unwrap_err();
    assert!(
        err.to_string().contains("OCR_NO_VISION_CHANNEL"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn ocr_page_short_content_treated_as_failure() {
    let pool = fresh_db().await;
    let (url, _) = start_mock(StatusCode::OK, "过短").await;
    insert_channel(&pool, "ch1", &url, r#"["test-vl"]"#, 10).await;

    let repo = Repository::new(pool.clone());
    let client = VlmOcrClient::new(&repo, "test-vl");
    let err = client.ocr_page(b"fake-jpeg", 2).await.unwrap_err();
    assert!(
        err.to_string().contains("OCR_PAGE_FAILED"),
        "unexpected error: {err}"
    );
}

// ── migration 025 ─────────────────────────────────────────────────

#[tokio::test]
async fn migration_025_kb_ocr_columns_roundtrip() {
    let pool = fresh_db().await;
    let repo = KbRepository::new(pool.clone());

    // create_kb 携带 ocr_model
    let kb = repo
        .create_kb(&CreateKbInput {
            name: "考试库".into(),
            description: None,
            embedding_model: None,
            embedding_channel_id: None,
            ocr_model: Some("qwen-vl-max".into()),
        })
        .await
        .expect("create kb");
    assert_eq!(kb.ocr_model.as_deref(), Some("qwen-vl-max"));

    // update_kb 修改 ocr_model
    let kb = repo
        .update_kb(
            &kb.id,
            &UpdateKbInput {
                name: None,
                description: None,
                embedding_model: None,
                embedding_channel_id: None,
                status: None,
                mcp_enabled: None,
                chunk_size: None,
                chunk_overlap: None,
                excluded_dirs: None,
                excluded_files: None,
                included_files: None,
                embedding_batch_size: None,
                ocr_model: Some("glm-4v-flash".into()),
            },
        )
        .await
        .expect("update kb");
    assert_eq!(kb.ocr_model.as_deref(), Some("glm-4v-flash"));

    // kb_documents 新列默认值（老数据无感）
    let doc = repo
        .create_document(&kb.id, "a.pdf", None, "pdf", 100, "hash-1")
        .await
        .expect("create doc");
    assert_eq!(doc.ocr_engine, None);
    assert_eq!(doc.page_count, 0);
    assert_eq!(doc.ocr_failed_pages, "[]");

    // update_document_ocr_info 回填
    repo.update_document_ocr_info(&doc.id, "vlm", 87, "[3,7]")
        .await
        .expect("update ocr info");
    let doc = repo.get_document(&doc.id).await.expect("get doc");
    assert_eq!(doc.ocr_engine.as_deref(), Some("vlm"));
    assert_eq!(doc.page_count, 87);
    assert_eq!(doc.ocr_failed_pages, "[3,7]");
}

// ── 全局开关（两级 gate 的第一级）─────────────────────────────────

#[test]
fn ocr_settings_default_off_and_roundtrip() {
    let dir = std::env::temp_dir().join(format!("waliapi_ocr_settings_{}", uuid::Uuid::new_v4()));
    let store = SettingsStore::file(dir.join("settings.json"));

    // 默认关闭：未写入任何配置时 ocr.enabled = false
    assert!(!store.get_bool("ocr.enabled", false));
    assert_eq!(store.get_u64("ocr.max_pages", 200), 200);
    assert_eq!(store.get_u64("ocr.concurrency", 2), 2);
    assert_eq!(store.get_u64("ocr.dpi", 200), 200);

    // 写入后重开文件（模拟重启）仍生效
    store
        .set_many(&[("ocr.enabled".to_string(), serde_json::json!(true))])
        .unwrap();
    let store2 = SettingsStore::file(dir.join("settings.json"));
    assert!(store2.get_bool("ocr.enabled", false));

    std::fs::remove_dir_all(&dir).ok();
}

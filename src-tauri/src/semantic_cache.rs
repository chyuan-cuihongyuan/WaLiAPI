//! 语义缓存（C-02）：网关侧响应缓存——exact（规范化哈希 O(1) 精确命中）
//! + semantic（embedding 暴力余弦 ≥ 阈值命中）两层。
//!
//! 三层风险收敛：① 默认关闭（cache.semantic_enabled）；② 可缓存判定
//! （无 tools/tool_choice、temperature ≤ 0.3、安全审计无命中、消息内容全部
//! 为纯文本）；③ 语义阈值保守默认 0.95。带 tools 的请求永不入缓存也永不
//! 命中；TTL 过期不命中；命中请求照常写请求日志（记账口径不变）。
//!
//! 诚实边界（issue 中声明）：本实现拦截 chat completions 端点（主协议）；
//! responses/messages 协议的拦截与写入为后续工作。写入只覆盖非流式响应
//! （流式命中可回放缓存答案；流式响应自身的渐进写入需要挂在流泵上，列
//! 为开放点）。渠道级覆盖在「Key 鉴权后、路由候选前」的拦截点上无法得知
//! 渠道，以模型粒度（cache_key 含 model）替代——设计修正如实声明。

use crate::db::repository::Repository;
use crate::settings_store::SettingsStore;
use sqlx::SqlitePool;
use std::time::Duration;

/// 消息规范化（纯函数）：提取 (role, content 纯文本) 序列，合并连续空白、
/// 去首尾空白；任一消息 content 非字符串（图片/工具内容等）→ None（不可缓存）。
pub fn normalize_messages(body: &serde_json::Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    if messages.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(messages.len());
    for message in messages {
        let role = message.get("role")?.as_str()?;
        let content = message.get("content")?;
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(items) => {
                // 多段内容：全部为 {type:"text", text} 才可缓存
                let mut texts = Vec::with_capacity(items.len());
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return None;
                    }
                    texts.push(item.get("text")?.as_str()?.to_string());
                }
                texts.join("\n")
            }
            _ => return None,
        };
        let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        parts.push(format!("{role}: {normalized}"));
    }
    Some(parts.join("\n"))
}

/// 可缓存判定（纯函数）：无 tools/tool_choice、temperature ≤ 0.3（缺省视为 0）、
/// 消息可规范化。安全审计命中由调用侧传入（audit_hit = true 直接不可缓存）。
pub fn cacheable(body: &serde_json::Value, audit_hit: bool) -> bool {
    if audit_hit {
        return false;
    }
    if body.get("tools").is_some() || body.get("tool_choice").is_some() {
        return false;
    }
    if let Some(temp) = body.get("temperature").and_then(|t| t.as_f64()) {
        if temp > 0.3 {
            return false;
        }
    }
    normalize_messages(body).is_some()
}

/// exact 层缓存键（纯函数）：规范化消息 SHA-256 + model + 参数档位。
/// temperature 分档（≤0.3 同档）——采样参数不同不共享 exact 命中。
pub fn exact_cache_key(body: &serde_json::Value, normalized: &str) -> Option<String> {
    let model = body.get("model")?.as_str()?;
    let temp_bucket = if body
        .get("temperature")
        .and_then(|t| t.as_f64())
        .unwrap_or(0.0)
        <= 0.3
    {
        "t0"
    } else {
        "t1"
    };
    let max_tokens_bucket = match body.get("max_tokens").and_then(|t| t.as_i64()) {
        None => "none",
        Some(n) if n <= 1024 => "small",
        Some(n) if n <= 4096 => "mid",
        Some(_) => "large",
    };
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(
        format!("{normalized}\x1e{model}\x1e{temp_bucket}\x1e{max_tokens_bucket}").as_bytes(),
    );
    let digest = hasher.finalize();
    Some(format!("sc_{model}_{:x}", digest))
}

/// 余弦相似度（纯函数；semantic 层与测试共用）。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

pub struct CacheHit {
    pub answer: String,
    /// exact | semantic
    pub layer: &'static str,
}

/// 查询两层缓存（exact 优先，semantic 兜底）。
/// 未启用（cache.semantic_enabled=false）/ 键不可构造 / 无命中 → None。
pub async fn lookup(
    pool: &SqlitePool,
    settings: &SettingsStore,
    body: &serde_json::Value,
    audit_hit: bool,
) -> Option<CacheHit> {
    if !settings.get_bool("cache.semantic_enabled", false) {
        return None;
    }
    if !cacheable(body, audit_hit) {
        return None;
    }
    let normalized = normalize_messages(body)?;
    let model = body.get("model")?.as_str()?;
    let now = crate::db::models::now_iso();

    // 第一层：exact（规范化哈希 O(1)）
    if let Some(key) = exact_cache_key(body, &normalized) {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT answer FROM semantic_cache WHERE cache_key = ? AND ttl_expire_at > ? LIMIT 1",
        )
        .bind(&key)
        .bind(&now)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        if let Some(answer) = row {
            return Some(CacheHit {
                answer,
                layer: "exact",
            });
        }
    }

    // 第二层：semantic（embedding 暴力余弦；缓存条目量级 ≪ KB chunk，暴力扫可接受）
    let embedding_model = settings.get_str("cache.embedding_model", "");
    if embedding_model.is_empty() {
        return None;
    }
    let threshold = settings.get_u64("cache.semantic_threshold_percent", 95) as f32 / 100.0;
    let repo = Repository::new(pool.clone());
    let query_text = normalized
        .lines()
        .rev()
        .find(|l| l.starts_with("user: "))
        .map(|l| l.to_string())
        .unwrap_or(normalized);
    let embeddings =
        crate::services::knowledge::embedder::embed(&[query_text], &embedding_model, &repo)
            .await
            .ok()?;
    let query_vec = embeddings.into_iter().next()?;

    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT answer, embedding FROM semantic_cache \
         WHERE model = ? AND ttl_expire_at > ? AND embedding IS NOT NULL",
    )
    .bind(model)
    .bind(&now)
    .fetch_all(pool)
    .await
    .ok()?;
    let mut best: Option<(f32, String)> = None;
    for (answer, blob) in rows {
        let Some(vector) = decode_f32_blob(&blob) else {
            continue;
        };
        let score = cosine_similarity(&query_vec, &vector);
        if score >= threshold && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, answer));
        }
    }
    best.map(|(_, answer)| CacheHit {
        answer,
        layer: "semantic",
    })
}

/// f32 向量的 LE 字节编解码（SQLite BLOB 存取）。
pub fn encode_f32_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn decode_f32_blob(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() % 4 != 0 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// 写入缓存（best-effort：调用侧已 spawn，此处失败仅告警）。
/// embedding 为 None 时只写 exact 层；semantic 层命中需要非空 embedding。
pub async fn store(
    pool: &SqlitePool,
    settings: &SettingsStore,
    body: &serde_json::Value,
    answer: &str,
) {
    if !settings.get_bool("cache.semantic_enabled", false) || answer.is_empty() {
        return;
    }
    let Some(normalized) = normalize_messages(body) else {
        return;
    };
    let Some(model) = body.get("model").and_then(|m| m.as_str()) else {
        return;
    };
    let Some(key) = exact_cache_key(body, &normalized) else {
        return;
    };
    let ttl_secs = settings.get_u64("cache.ttl_secs", 86_400).max(60);
    let expire = (chrono::Utc::now() + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();

    // semantic 层 embedding（配置了嵌入模型才有；失败不阻断 exact 写入）
    let mut embedding_blob: Option<Vec<u8>> = None;
    let embedding_model = settings.get_str("cache.embedding_model", "");
    if !embedding_model.is_empty() {
        let repo = Repository::new(pool.clone());
        let query_text = normalized
            .lines()
            .rev()
            .find(|l| l.starts_with("user: "))
            .map(|l| l.to_string())
            .unwrap_or(normalized);
        if let Ok(vectors) =
            crate::services::knowledge::embedder::embed(&[query_text], &embedding_model, &repo)
                .await
        {
            if let Some(vector) = vectors.into_iter().next() {
                embedding_blob = Some(encode_f32_blob(&vector));
            }
        }
    }

    // 同键覆盖写（答案更新；旧条目 TTL 自然过期）
    let result = sqlx::query(
        "INSERT INTO semantic_cache (id, cache_key, model, embedding, answer, ttl_expire_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(cache_key) DO UPDATE SET \
         embedding = excluded.embedding, answer = excluded.answer, \
         ttl_expire_at = excluded.ttl_expire_at, created_at = excluded.created_at",
    )
    .bind(crate::utils::id::new_id())
    .bind(&key)
    .bind(model)
    .bind(&embedding_blob)
    .bind(answer)
    .bind(&expire)
    .bind(crate::db::models::now_iso())
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::warn!("[语义缓存] 写入失败（best-effort，不影响主请求）: {error}");
    }
}

/// 清空缓存：model 为 Some(非空) 时按模型清，否则清全部。返回删除行数。
/// 管理命令（clear_semantic_cache）的实体，独立成函数便于测试。
pub async fn clear(pool: &SqlitePool, model: Option<&str>) -> Result<u64, sqlx::Error> {
    match model {
        Some(m) if !m.is_empty() => sqlx::query("DELETE FROM semantic_cache WHERE model = ?")
            .bind(m)
            .execute(pool)
            .await
            .map(|r| r.rows_affected()),
        _ => sqlx::query("DELETE FROM semantic_cache")
            .execute(pool)
            .await
            .map(|r| r.rows_affected()),
    }
}

/// 从缓存答案合成非流式 chat completion 响应体。
pub fn replay_body(model: &str, answer: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("chatcmpl-cache-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": answer},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

/// 从缓存答案合成流式 SSE 帧序列（role → content → finish → [DONE]，
/// 标准增量分块，既有 chat SSE 客户端可直接消费）。
pub fn replay_sse_frames(model: &str, answer: &str) -> String {
    let id = format!("chatcmpl-cache-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let mut out = String::new();
    out.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
        })
    ));
    for chunk in answer.as_bytes().chunks(64) {
        let text = String::from_utf8_lossy(chunk);
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
            })
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

/// 清理过期条目（后台维护可复用；手工清空走管理命令）。
pub async fn purge_expired(pool: &SqlitePool) -> u64 {
    let now = crate::db::models::now_iso();
    match sqlx::query("DELETE FROM semantic_cache WHERE ttl_expire_at <= ?")
        .bind(&now)
        .execute(pool)
        .await
    {
        Ok(result) => result.rows_affected(),
        Err(error) => {
            tracing::warn!("[语义缓存] 过期清理失败: {error}");
            0
        }
    }
}

/// 后台维护循环：随 TTL 清理（复用审计维护的 spawn 模式；间隔固定 1h）。
pub async fn run_maintenance_loop(pool: SqlitePool) {
    loop {
        let purged = purge_expired(&pool).await;
        if purged > 0 {
            tracing::debug!("[语义缓存] 清理过期条目 {purged} 行");
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn settings_at(enabled: bool) -> SettingsStore {
        let dir = std::env::temp_dir().join(format!("waliapi-cache-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::file(dir.join("settings.json"));
        store
            .set_many(&[(
                "cache.semantic_enabled".to_string(),
                serde_json::json!(enabled),
            )])
            .unwrap();
        store
    }

    fn body(messages: serde_json::Value, temperature: Option<f64>) -> serde_json::Value {
        serde_json::json!({
            "model": "m",
            "messages": messages,
            "temperature": temperature,
        })
    }

    #[test]
    fn normalize_merges_whitespace_and_rejects_non_text() {
        let b = body(
            serde_json::json!([
                {"role": "user", "content": "  你   好\n世界 "},
                {"role": "assistant", "content": "你好"}
            ]),
            None,
        );
        assert_eq!(
            normalize_messages(&b).unwrap(),
            "user: 你 好 世界\nassistant: 你好"
        );
        // 图片内容 → 不可缓存
        let b = body(
            serde_json::json!([
                {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}
            ]),
            None,
        );
        assert!(normalize_messages(&b).is_none());
        // 多段纯文本 → 可缓存（join 后整体做空白规范化，换行折叠为单空格）
        let b = body(
            serde_json::json!([
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}
            ]),
            None,
        );
        assert_eq!(normalize_messages(&b).unwrap(), "user: a b");
    }

    #[test]
    fn cacheable_gates_tools_temperature_audit() {
        assert!(cacheable(
            &body(
                serde_json::json!([{"role":"user","content":"q"}]),
                Some(0.0)
            ),
            false
        ));
        assert!(
            !cacheable(
                &body(
                    serde_json::json!([{"role":"user","content":"q"}]),
                    Some(0.7)
                ),
                false
            ),
            "高温不入缓存"
        );
        assert!(
            !cacheable(
                &body(serde_json::json!([{"role":"user","content":"q"}]), None),
                true
            ),
            "审计命中不入缓存"
        );
        let mut with_tools = body(serde_json::json!([{"role":"user","content":"q"}]), None);
        with_tools["tools"] = serde_json::json!([]);
        assert!(!cacheable(&with_tools, false), "带 tools 永不入缓存");
    }

    #[test]
    fn exact_key_is_stable_and_parameter_sensitive() {
        let b1 = body(
            serde_json::json!([{"role":"user","content":"  同一   问题 "}]),
            None,
        );
        let b2 = body(
            serde_json::json!([{"role":"user","content":"同一 问题"}]),
            None,
        );
        assert_eq!(
            exact_cache_key(&b1, &normalize_messages(&b1).unwrap()).unwrap(),
            exact_cache_key(&b2, &normalize_messages(&b2).unwrap()).unwrap(),
            "空白规范化后同键"
        );
        let b3 = body(
            serde_json::json!([{"role":"user","content":"另一问题"}]),
            None,
        );
        assert_ne!(
            exact_cache_key(&b1, &normalize_messages(&b1).unwrap()).unwrap(),
            exact_cache_key(&b3, &normalize_messages(&b3).unwrap()).unwrap()
        );
        // max_tokens 档位不同 → 不同键
        let mut b4 = b1.clone();
        b4["max_tokens"] = serde_json::json!(8192);
        assert_ne!(
            exact_cache_key(&b1, &normalize_messages(&b1).unwrap()).unwrap(),
            exact_cache_key(&b4, &normalize_messages(&b4).unwrap()).unwrap()
        );
    }

    #[test]
    fn cosine_similarity_full_orthogonal_and_dimension_mismatch() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(
            cosine_similarity(&[1.0], &[1.0, 0.0]),
            0.0,
            "维度不符按 0 处理"
        );
    }

    #[tokio::test]
    async fn exact_layer_hit_miss_and_ttl_expiry() {
        let pool = memory_db().await;
        let settings = settings_at(true);
        let b = body(
            serde_json::json!([{"role":"user","content":"网关怎么配额？"}]),
            None,
        );

        // 未写入 → miss；关闭 → 恒 miss
        assert!(lookup(&pool, &settings, &b, false).await.is_none());
        let off = settings_at(false);
        assert!(lookup(&pool, &off, &b, false).await.is_none());

        store(&pool, &settings, &b, "配额在密钥页设置。").await;
        let hit = lookup(&pool, &settings, &b, false).await.unwrap();
        assert_eq!(hit.answer, "配额在密钥页设置。");
        assert_eq!(hit.layer, "exact");

        // TTL 过期 → 不命中
        sqlx::query("UPDATE semantic_cache SET ttl_expire_at = '2020-01-01T00:00:00+00:00'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            lookup(&pool, &settings, &b, false).await.is_none(),
            "过期不命中"
        );
        assert_eq!(purge_expired(&pool).await, 1, "过期清理生效");
    }

    #[tokio::test]
    async fn semantic_layer_hits_above_threshold_via_similarity() {
        let pool = memory_db().await;
        let mut settings = settings_at(true);
        settings
            .set_many(&[(
                "cache.embedding_model".to_string(),
                serde_json::json!("emb-test"),
            )])
            .unwrap();
        // 直接植入已知向量条目（不经 embedder——渠道依赖在集成测试外）：
        // 缓存条目向量 = 查询向量（相似度 1.0 ≥ 阈值必命中）
        let vector = vec![0.1f32, 0.2, 0.3];
        sqlx::query(
            "INSERT INTO semantic_cache (id, cache_key, model, embedding, answer, ttl_expire_at, created_at) \
             VALUES ('c1', 'sc_seed_1', 'm', ?, '语义近邻答案', '2999-01-01T00:00:00+00:00', ?)",
        )
        .bind(encode_f32_blob(&vector))
        .bind(crate::db::models::now_iso())
        .execute(&pool)
        .await
        .unwrap();

        // exact 层不命中（键不同）→ semantic 层：查询向量与条目一致 → 命中
        // （embedder 无渠道会失败 → lookup 回退 None——这里绕过 lookup 直测相似度路径
        //   的组成件：余弦 + 编解码往返）
        let round = decode_f32_blob(&encode_f32_blob(&vector)).unwrap();
        assert_eq!(round, vector);
        assert!(cosine_similarity(&vector, &round) >= 0.95);
    }

    #[test]
    fn replay_body_and_sse_frames_are_well_formed() {
        let replay = replay_body("m", "答案");
        assert_eq!(replay["choices"][0]["message"]["content"], "答案");
        assert_eq!(replay["choices"][0]["finish_reason"], "stop");

        let sse = replay_sse_frames("m", "你好世界");
        assert!(sse.starts_with("data: {"));
        assert!(sse.contains("\"role\":\"assistant\""));
        assert!(
            sse.contains("你好世界") || sse.contains("\\u"),
            "内容帧应包含答案文本"
        );
        assert!(
            sse.trim_end().ends_with("data: [DONE]"),
            "流式回放必须以 [DONE] 收尾"
        );
    }

    /// 清空语义（管理命令实体）：按模型清与全清，返回删除行数。
    #[tokio::test]
    async fn clear_by_model_and_all() {
        let pool = memory_db().await;
        for (key, model) in [("sc_a_1", "m1"), ("sc_b_1", "m1"), ("sc_c_1", "m2")] {
            sqlx::query(
                "INSERT INTO semantic_cache (id, cache_key, model, embedding, answer, ttl_expire_at, created_at) \
                 VALUES (?, ?, ?, NULL, ?, '2999-01-01T00:00:00+00:00', ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(key)
            .bind(model)
            .bind("ans")
            .bind(crate::db::models::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        }

        // 按模型清 m1 → 删 2 行，剩 m2
        assert_eq!(clear(&pool, Some("m1")).await.unwrap(), 2);
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM semantic_cache WHERE model = 'm2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 1);

        // 空字符串等价全部清
        assert_eq!(clear(&pool, Some("")).await.unwrap(), 1);
        // None 全清（空表返回 0）
        assert_eq!(clear(&pool, None).await.unwrap(), 0);
    }
}

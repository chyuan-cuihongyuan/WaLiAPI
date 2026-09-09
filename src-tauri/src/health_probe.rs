//! 渠道主动健康探测（C-04）：后台循环对启用中的 API 渠道发廉价探测
//! （GET {base_url}/models，带渠道 Key，短超时），结果落渠道探测列并记
//! `is_probe=1` 请求日志。探测失败的渠道在候选排序中**沉底不剔除**——
//! 保守策略，误杀渠道的代价高于排序损失。OAuth/Auth 账号渠道零探测
//! （账号表不参与本循环，成本与配额敏感）。
//!
//! 与既有 `channel_mode_health` 被动冷却**正交**：那是请求驱动的
//! （渠道×端点×流式）模式级冷却；本模块是渠道级的主动信号，
//! GET /models 可达 ≠ 聊天模式健康，互不写对方的表。

use crate::db::repository::Repository;
use crate::settings_store::SettingsStore;
use sqlx::SqlitePool;
use std::time::Duration;

const DEFAULT_INTERVAL_SECS: u64 = 300;
/// 探测超时：比普通请求更严——探测的意义就是快速判定可达性。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 单渠道探测结果。
#[derive(Debug, Clone, Copy)]
pub struct ProbeOutcome {
    pub ok: bool,
    pub latency_ms: i64,
}

/// 对单个渠道发一次廉价探测。GET {base_url}/models（2xx 即健康）。
pub async fn probe_channel(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> ProbeOutcome {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let started = std::time::Instant::now();
    let ok = client
        .get(&url)
        .bearer_auth(api_key)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false);
    ProbeOutcome {
        ok,
        latency_ms: started.elapsed().as_millis() as i64,
    }
}

/// 单轮探测步骤（测试与循环共用）：探测全部启用渠道 → 写探测列 + is_probe 日志行。
/// 探测关闭（`probe.enabled=false`）时零网络请求零写库。
pub async fn probe_step(
    pool: &SqlitePool,
    settings: &SettingsStore,
) -> Vec<(String, ProbeOutcome)> {
    if !settings.get_bool("probe.enabled", true) {
        return Vec::new();
    }
    let repo = Repository::new(pool.clone());
    let channels = match repo.get_enabled_channels().await {
        Ok(channels) => channels,
        Err(error) => {
            tracing::warn!("[探测] 拉取启用渠道失败: {error}");
            return Vec::new();
        }
    };
    let client = reqwest::Client::new();
    let mut outcomes = Vec::new();
    for channel in channels {
        let outcome = probe_channel(&client, &channel.base_url, &channel.api_key).await;
        tracing::debug!(
            "[探测] 渠道 {} ({}) -> ok={} latency={}ms",
            channel.name,
            channel.id,
            outcome.ok,
            outcome.latency_ms
        );
        if let Err(error) = repo
            .record_channel_probe(
                &channel.id,
                &channel.name,
                outcome,
                &crate::db::models::now_iso(),
            )
            .await
        {
            tracing::warn!("[探测] 写入渠道 {} 探测结果失败: {error}", channel.id);
        }
        outcomes.push((channel.id, outcome));
    }
    outcomes
}

/// 后台探测循环：默认 300s 一轮，可整体关闭（关闭时零后台流量）。
pub async fn run_probe_loop(pool: SqlitePool, settings: SettingsStore) {
    loop {
        let interval = Duration::from_secs(
            settings
                .get_u64("probe.interval_secs", DEFAULT_INTERVAL_SECS)
                .max(30),
        );
        let outcomes = probe_step(&pool, &settings).await;
        if !outcomes.is_empty() {
            let failed = outcomes.iter().filter(|(_, o)| !o.ok).count();
            if failed > 0 {
                tracing::info!(
                    "[探测] 本轮 {} 渠道，{} 个不健康（排序沉底）",
                    outcomes.len(),
                    failed
                );
            }
        }
        tokio::time::sleep(interval).await;
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

    fn settings_at(dir: &std::path::Path, enabled: bool) -> SettingsStore {
        let store = SettingsStore::file(dir.join("settings.json"));
        store
            .set_many(&[("probe.enabled".to_string(), serde_json::json!(enabled))])
            .unwrap();
        store
    }

    async fn seed_channel(pool: &SqlitePool, id: &str, base_url: &str) {
        sqlx::query(
            "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, \
             weight, config, model_mapping, timeout_secs, identity_revision, created_at, updated_at) \
             VALUES (?, ?, 'openai', ?, 'sk-x', '[]', 1, 1, 1, '{}', '{}', 60, 0, ?, ?)",
        )
        .bind(id)
        .bind(id)
        .bind(base_url)
        .bind(crate::db::models::now_iso())
        .bind(crate::db::models::now_iso())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn mock_models_endpoint() -> String {
        let app = axum::Router::new().route("/models", axum::routing::get(|| async { "[]" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn probe_step_updates_columns_and_writes_probe_logs() {
        let pool = memory_db().await;
        let dir = std::env::temp_dir().join(format!("waliapi-probe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = settings_at(&dir, true);

        let healthy_base = mock_models_endpoint().await;
        seed_channel(&pool, "ch-ok", &healthy_base).await;
        // 不可达端口（保留地址，必然连接失败）
        seed_channel(&pool, "ch-bad", "http://127.0.0.1:1").await;
        let outcomes = probe_step(&pool, &settings).await;
        assert_eq!(outcomes.len(), 2, "两个启用渠道都应被探测");

        let row = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT last_probe_ok, probe_latency_ms FROM channels WHERE id = 'ch-ok'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, Some(1), "健康渠道 last_probe_ok=1");

        let row = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT last_probe_ok, probe_latency_ms FROM channels WHERE id = 'ch-bad'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, Some(0), "不可达渠道 last_probe_ok=0");

        // is_probe 日志行：每渠道一条
        let probes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE is_probe = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(probes, 2, "每个被探测渠道应记一条 is_probe 日志");
        let mode: String =
            sqlx::query_scalar("SELECT mode FROM request_logs WHERE is_probe = 1 LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mode, "probe");
    }

    #[tokio::test]
    async fn probe_disabled_means_zero_traffic_and_writes() {
        let pool = memory_db().await;
        let dir = std::env::temp_dir().join(format!("waliapi-probe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = settings_at(&dir, false);

        let base = mock_models_endpoint().await;
        seed_channel(&pool, "ch-x", &base).await;

        let outcomes = probe_step(&pool, &settings).await;
        assert!(outcomes.is_empty(), "关闭时零探测");
        let probes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE is_probe = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(probes, 0, "关闭时零日志写入");
        let never: Option<i64> =
            sqlx::query_scalar("SELECT last_probe_ok FROM channels WHERE id = 'ch-x'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(never.is_none(), "关闭时探测列保持 NULL（从未探测）");
    }

    #[tokio::test]
    async fn unhealthy_channels_sink_in_candidate_ordering() {
        let pool = memory_db().await;
        let repo = Repository::new(pool.clone());
        // 高优先级但探测失败 / 中优先级从未探测 / 低优先级探测健康
        for (id, priority) in [("sink", 9), ("mid", 5), ("ok", 1)] {
            sqlx::query(
                "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, \
                 weight, config, model_mapping, timeout_secs, identity_revision, created_at, updated_at) \
                 VALUES (?, ?, 'openai', 'http://127.0.0.1:1', 'sk-x', '[]', 1, ?, 1, '{}', '{}', 60, 0, ?, ?)",
            )
            .bind(id)
            .bind(id)
            .bind(priority)
            .bind(crate::db::models::now_iso())
            .bind(crate::db::models::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE channels SET last_probe_ok = 0 WHERE id = 'sink'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE channels SET last_probe_ok = 1 WHERE id = 'ok'")
            .execute(&pool)
            .await
            .unwrap();

        let ordered = repo.get_enabled_channels().await.unwrap();
        let ids: Vec<&str> = ordered.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["mid", "ok", "sink"],
            "探测失败渠道沉底（不剔除）；健康档内按优先级/权重，从未探测视为健康"
        );
    }

    #[tokio::test]
    async fn usage_stats_exclude_probe_rows() {
        let pool = memory_db().await;
        let repo = Repository::new(pool.clone());
        let now = crate::db::models::now_iso();
        // 一条正常请求行 + 一条探测行（model 都为 m）
        for (model, is_probe) in [("m", 0), ("m", 1)] {
            sqlx::query(
                "INSERT INTO request_logs (id, seq, model, mode, status_code, duration_ms, \
                 is_stream, is_retry, created_at, risk_level, security_action, upstream_type, \
                 is_probe, total_tokens) \
                 VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM request_logs), ?, 'chat', 200, \
                 10, 0, 0, ?, 'low', 'audit', 'channel', ?, 100)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(model)
            .bind(&now)
            .bind(is_probe)
            .execute(&pool)
            .await
            .unwrap();
        }

        let model_stats = repo.get_model_stats().await.unwrap();
        let row = model_stats
            .iter()
            .find(|s| s.model == "m")
            .expect("正常行应计入模型统计");
        assert_eq!(
            row.request_count, 1,
            "探测行不计入用量统计（is_probe=0 过滤）"
        );

        let dashboard = repo.get_dashboard_stats().await.unwrap();
        assert_eq!(dashboard.total_requests, 1, "仪表盘请求数排除探测行");
    }

    /// 被动反哺：探测失败的渠道经一次 mark_probe_ok 立即恢复排序位
    /// （验收标准「一次真实请求成功 → 恢复正常排序」）。
    #[tokio::test]
    async fn mark_probe_ok_recovers_ordering_after_real_success() {
        let pool = memory_db().await;
        let repo = Repository::new(pool.clone());
        for (id, priority) in [("sink", 9), ("healthy", 1)] {
            sqlx::query(
                "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, \
                 weight, config, model_mapping, timeout_secs, identity_revision, created_at, updated_at) \
                 VALUES (?, ?, 'openai', 'http://127.0.0.1:1', 'sk-x', '[]', 1, ?, 1, '{}', '{}', 60, 0, ?, ?)",
            )
            .bind(id)
            .bind(id)
            .bind(priority)
            .bind(crate::db::models::now_iso())
            .bind(crate::db::models::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE channels SET last_probe_ok = 0 WHERE id = 'sink'")
            .execute(&pool)
            .await
            .unwrap();

        // 反哺前：sink 沉底
        let ids: Vec<String> = repo
            .get_enabled_channels()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["healthy", "sink"]);

        // 真实请求成功路径调用的反哺 → 立即恢复高优先级位
        repo.mark_probe_ok("sink").await;
        let ids: Vec<String> = repo
            .get_enabled_channels()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["sink", "healthy"], "被动反哺后应恢复优先级排序");
    }

    /// 两轨覆盖：driver 轨入口的 get_enabled_channels_for_mode 同样吃沉底排序键
    /// （且与 mode_health 冷却并存时，冷却剔除优先、健康排序作用于剩余候选）。
    #[tokio::test]
    async fn mode_query_also_sinks_unhealthy_candidates() {
        let pool = memory_db().await;
        let repo = Repository::new(pool.clone());
        for (id, priority, probe_ok) in [("hot", 9, 0), ("mid", 5, 1), ("low", 1, 1)] {
            sqlx::query(
                "INSERT INTO channels (id, name, type, base_url, api_key, models, status, priority, \
                 weight, config, model_mapping, timeout_secs, identity_revision, created_at, updated_at, \
                 last_probe_ok) \
                 VALUES (?, ?, 'openai', 'http://127.0.0.1:1', 'sk-x', '[]', 1, ?, 1, '{}', '{}', 60, 0, ?, ?, ?)",
            )
            .bind(id)
            .bind(id)
            .bind(priority)
            .bind(crate::db::models::now_iso())
            .bind(crate::db::models::now_iso())
            .bind(probe_ok)
            .execute(&pool)
            .await
            .unwrap();
        }

        let ids: Vec<String> = repo
            .get_enabled_channels_for_mode("chat_completions", false, &crate::db::models::now_iso())
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            ids,
            vec!["mid", "low", "hot"],
            "for_mode 查询同样沉底探测失败渠道（两轨一致的排序语义）"
        );
    }
}

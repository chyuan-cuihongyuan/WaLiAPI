use crate::db::repository::Repository;
use crate::AppState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DashboardStatsDto {
    pub today_requests: i64,
    pub today_total_tokens: i64,
    /// 今日缓存命中读取的输入 token（issue #51）。
    pub today_cache_read_tokens: i64,
    /// 今日可缓存输入基数（分母；0 = 无 usage 数据，前端显示占位）。
    pub today_cache_eligible_tokens: i64,
    pub active_channels: i64,
    pub avg_latency_ms: f64,
    pub total_channels: i64,
    pub total_api_keys: i64,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub total_knowledge_bases: i64,
    pub total_kb_documents: i64,
    pub total_kb_chunks: i64,
    pub total_wiki_projects: i64,
    pub total_wiki_pages: i64,
}

#[tauri::command]
pub async fn get_dashboard_stats(
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<DashboardStatsDto, String> {
    get_dashboard_stats_impl(&*state).await
}

pub async fn get_dashboard_stats_impl(
    state: &std::sync::Arc<AppState>,
) -> Result<DashboardStatsDto, String> {
    let repo = Repository::new(state.db.pool.clone());
    let s = repo
        .get_dashboard_stats()
        .await
        .map_err(|e| e.to_string())?;
    Ok(DashboardStatsDto {
        today_requests: s.today_requests,
        today_total_tokens: s.today_total_tokens,
        today_cache_read_tokens: s.today_cache_read_tokens,
        today_cache_eligible_tokens: s.today_cache_eligible_tokens,
        active_channels: s.active_channels,
        avg_latency_ms: s.avg_latency_ms,
        total_channels: s.total_channels,
        total_api_keys: s.total_api_keys,
        total_requests: s.total_requests,
        total_tokens: s.total_tokens,
        total_knowledge_bases: s.total_knowledge_bases,
        total_kb_documents: s.total_kb_documents,
        total_kb_chunks: s.total_kb_chunks,
        total_wiki_projects: s.total_wiki_projects,
        total_wiki_pages: s.total_wiki_pages,
    })
}

// ── 模型分布 ──

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelStatsDto {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
}

#[tauri::command]
pub async fn get_model_stats(
    hours: Option<i64>,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<Vec<ModelStatsDto>, String> {
    get_model_stats_impl(hours, &*state).await
}

pub async fn get_model_stats_impl(
    hours: Option<i64>,
    state: &std::sync::Arc<AppState>,
) -> Result<Vec<ModelStatsDto>, String> {
    let repo = Repository::new(state.db.pool.clone());
    let stats = repo
        .get_model_stats(hours)
        .await
        .map_err(|e| e.to_string())?;
    Ok(stats
        .into_iter()
        .map(|s| ModelStatsDto {
            model: s.model,
            request_count: s.request_count,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            total_tokens: s.total_tokens,
            success_rate: s.success_rate,
            avg_latency_ms: s.avg_latency_ms,
        })
        .collect())
}

// ── Token 趋势 ──

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenTrendPointDto {
    pub hour: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
    /// issue #51：小时桶扩展指标。
    pub success_count: i64,
    pub avg_duration_ms: f64,
    /// 平均首字延迟；该桶内全是非流式请求时为 null。
    pub avg_ttft_ms: Option<f64>,
    pub cache_read_tokens: i64,
}

#[tauri::command]
pub async fn get_token_trend(
    hours: Option<i64>,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<Vec<TokenTrendPointDto>, String> {
    get_token_trend_impl(hours, &*state).await
}

pub async fn get_token_trend_impl(
    hours: Option<i64>,
    state: &std::sync::Arc<AppState>,
) -> Result<Vec<TokenTrendPointDto>, String> {
    let hours = hours.unwrap_or(24);
    let repo = Repository::new(state.db.pool.clone());
    let points = repo
        .get_token_trend(hours)
        .await
        .map_err(|e| e.to_string())?;
    Ok(points
        .into_iter()
        .map(|p| TokenTrendPointDto {
            hour: p.hour,
            model: p.model,
            input_tokens: p.input_tokens,
            output_tokens: p.output_tokens,
            total_tokens: p.total_tokens,
            request_count: p.request_count,
            success_count: p.success_count,
            avg_duration_ms: p.avg_duration_ms,
            avg_ttft_ms: p.avg_ttft_ms,
            cache_read_tokens: p.cache_read_tokens,
        })
        .collect())
}

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

use crate::settings_store::SettingsStore;
use crate::AppState;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    #[serde(default = "default_log_detail_level")]
    pub log_detail_level: String,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u64,
    #[serde(default = "default_port")]
    pub server_port: u16,
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_theme")]
    pub ui_theme: String,
    #[serde(default = "default_language")]
    pub ui_language: String,
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_false")]
    pub auto_start: bool,
    #[serde(default = "default_retry_enabled")]
    pub retry_enabled: bool,
    #[serde(default = "default_retry_times")]
    pub retry_times: i32,
    #[serde(default = "default_security_enabled")]
    pub security_enabled: bool,
    #[serde(default = "default_security_mode")]
    pub security_mode: String,
    #[serde(default = "default_true")]
    pub security_scan_unicode: bool,
    #[serde(default = "default_true")]
    pub security_scan_tools: bool,
    #[serde(default = "default_true")]
    pub security_scan_network: bool,
    #[serde(default = "default_false")]
    pub security_scan_response: bool,
    #[serde(default = "default_false")]
    pub security_redact_secrets: bool,
    #[serde(default = "default_false")]
    pub security_block_on_critical: bool,
    #[serde(default = "default_true")]
    pub routing_prefer_auth_accounts: bool,
    #[serde(default = "default_true")]
    pub routing_prefer_same_protocol: bool,
    /// LLM OCR 总开关（默认关）。关闭时所有 PDF 走原有解析逻辑，不做扫描判定、无 LLM 调用。
    #[serde(default = "default_false")]
    pub ocr_enabled: bool,
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages: i32,
    #[serde(default = "default_ocr_concurrency")]
    pub ocr_concurrency: i32,
    #[serde(default = "default_ocr_dpi")]
    pub ocr_dpi: i32,
    /// 语义缓存开关（C-02，默认关）。关闭时拦截/写入/清理全部旁路。
    #[serde(default = "default_false")]
    pub cache_enabled: bool,
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// 语义层相似度阈值（百分比，95 = 0.95；保守默认）。
    #[serde(default = "default_cache_threshold")]
    pub cache_threshold_percent: u64,
    /// 语义层嵌入模型（空 = 只启用 exact 层）。
    #[serde(default)]
    pub cache_embedding_model: String,
}

fn default_cache_ttl_secs() -> u64 {
    86_400
}
fn default_cache_threshold() -> u64 {
    95
}

fn default_port() -> u16 {
    8777
}
fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_theme() -> String {
    "dark".to_string()
}
fn default_language() -> String {
    "zh-CN".to_string()
}
fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}
fn default_retry_enabled() -> bool {
    true
}
fn default_retry_times() -> i32 {
    2
}
fn default_security_enabled() -> bool {
    false
}
fn default_security_mode() -> String {
    "audit".to_string()
}
fn default_ocr_max_pages() -> i32 {
    200
}
fn default_ocr_concurrency() -> i32 {
    2
}
fn default_ocr_dpi() -> i32 {
    200
}
fn default_log_detail_level() -> String {
    "basic".to_string()
}
fn default_log_retention_days() -> u64 {
    7
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            log_detail_level: default_log_detail_level(),
            log_retention_days: default_log_retention_days(),
            server_port: default_port(),
            server_host: default_host(),
            ui_theme: default_theme(),
            ui_language: default_language(),
            minimize_to_tray: default_true(),
            close_to_tray: default_true(),
            auto_start: default_false(),
            retry_enabled: default_retry_enabled(),
            retry_times: default_retry_times(),
            security_enabled: default_security_enabled(),
            security_mode: default_security_mode(),
            security_scan_unicode: default_false(),
            security_scan_tools: default_false(),
            security_scan_network: default_false(),
            security_scan_response: default_false(),
            security_redact_secrets: default_false(),
            security_block_on_critical: default_false(),
            routing_prefer_auth_accounts: default_true(),
            routing_prefer_same_protocol: default_true(),
            ocr_enabled: default_false(),
            ocr_max_pages: default_ocr_max_pages(),
            ocr_concurrency: default_ocr_concurrency(),
            ocr_dpi: default_ocr_dpi(),
            cache_enabled: default_false(),
            cache_ttl_secs: default_cache_ttl_secs(),
            cache_threshold_percent: default_cache_threshold(),
            cache_embedding_model: String::new(),
        }
    }
}

fn get_str(store: &SettingsStore, key: &str, default: &str) -> String {
    store.get_str(key, default)
}

fn get_u64(store: &SettingsStore, key: &str, default: u64) -> u64 {
    store.get_u64(key, default)
}

fn get_bool(store: &SettingsStore, key: &str, default: bool) -> bool {
    store.get_bool(key, default)
}

/// Feature-flag snapshot exposed to the UI (T00 decision 9 / T10 rollout).
///
/// The frontend uses these to disable/hide protocol tabs whose backend path is
/// not yet enabled (e.g. Ollama when `ollama_native` is OFF) so a user never
/// creates a channel that 503s at runtime.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeatureFlagsDto {
    pub new_routeplan: bool,
    pub cross_protocol_codec: bool,
    pub native_responses: bool,
    pub ollama_native: bool,
    pub prefer_auth_accounts: bool,
    pub prefer_same_protocol: bool,
}

#[tauri::command]
pub fn get_feature_flags(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<FeatureFlagsDto, String> {
    let f = crate::core::feature_flags::read_feature_flags(&state.settings);
    Ok(FeatureFlagsDto {
        new_routeplan: f.new_routeplan,
        cross_protocol_codec: f.cross_protocol_codec,
        native_responses: f.native_responses,
        ollama_native: f.ollama_native,
        prefer_auth_accounts: f.prefer_auth_accounts,
        prefer_same_protocol: f.prefer_same_protocol,
    })
}

#[tauri::command]
pub async fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Result<Settings, String> {
    let store = &state.settings;
    let detail_level = get_str(store, "logs.detail_level", "basic");
    let log_detail_level = if detail_level.eq_ignore_ascii_case("detailed") {
        "detailed".to_string()
    } else {
        "basic".to_string()
    };
    let log_retention_days =
        crate::audit_log::normalize_retention_days(get_u64(store, "logs.retention_days", 7));
    let settings = Settings {
        log_detail_level,
        log_retention_days,
        server_port: get_u64(store, "server.port", 8777) as u16,
        server_host: get_str(store, "server.host", "127.0.0.1"),
        ui_theme: get_str(store, "ui.theme", "dark"),
        ui_language: get_str(store, "ui.language", "zh-CN"),
        minimize_to_tray: get_bool(store, "general.minimize_to_tray", true),
        close_to_tray: get_bool(store, "general.close_to_tray", true),
        auto_start: get_bool(store, "general.auto_start", false),
        retry_enabled: get_bool(store, "retry.enabled", true),
        retry_times: get_u64(store, "retry.times", 2) as i32,
        security_enabled: get_bool(store, "security.enabled", false),
        security_mode: get_str(store, "security.mode", "audit"),
        security_scan_unicode: get_bool(store, "security.scan_unicode", false),
        security_scan_tools: get_bool(store, "security.scan_tools", false),
        security_scan_network: get_bool(store, "security.scan_network", false),
        security_scan_response: get_bool(store, "security.scan_response", false),
        security_redact_secrets: get_bool(store, "security.redact_secrets", false),
        security_block_on_critical: get_bool(store, "security.block_on_critical", false),
        routing_prefer_auth_accounts: get_bool(store, "routing.prefer_auth_accounts", true),
        routing_prefer_same_protocol: get_bool(store, "routing.prefer_same_protocol", true),
        ocr_enabled: get_bool(store, "ocr.enabled", false),
        ocr_max_pages: get_u64(store, "ocr.max_pages", 200) as i32,
        ocr_concurrency: get_u64(store, "ocr.concurrency", 2) as i32,
        ocr_dpi: get_u64(store, "ocr.dpi", 200) as i32,
        cache_enabled: get_bool(store, "cache.semantic_enabled", false),
        cache_ttl_secs: get_u64(store, "cache.ttl_secs", 86_400),
        cache_threshold_percent: get_u64(store, "cache.semantic_threshold_percent", 95),
        cache_embedding_model: get_str(store, "cache.embedding_model", ""),
    };
    Ok(settings)
}

#[tauri::command]
pub async fn save_settings(
    settings: Settings,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    if !settings.log_detail_level.eq_ignore_ascii_case("basic")
        && !settings.log_detail_level.eq_ignore_ascii_case("detailed")
    {
        return Err("无效的日志级别，仅支持 basic 或 detailed".to_string());
    }
    if !matches!(settings.log_retention_days, 0 | 1 | 7 | 30 | 90) {
        return Err("无效的日志保留期，仅支持 1、7、30、90 天或永久".to_string());
    }
    state.settings.set_many(&[
        (
            "logs.detail_level".to_string(),
            serde_json::json!(settings.log_detail_level),
        ),
        (
            "logs.retention_days".to_string(),
            serde_json::json!(settings.log_retention_days),
        ),
        (
            "server.port".to_string(),
            serde_json::json!(settings.server_port),
        ),
        (
            "server.host".to_string(),
            serde_json::json!(settings.server_host),
        ),
        ("ui.theme".to_string(), serde_json::json!(settings.ui_theme)),
        (
            "ui.language".to_string(),
            serde_json::json!(settings.ui_language),
        ),
        (
            "general.minimize_to_tray".to_string(),
            serde_json::json!(settings.minimize_to_tray),
        ),
        (
            "general.close_to_tray".to_string(),
            serde_json::json!(settings.close_to_tray),
        ),
        (
            "general.auto_start".to_string(),
            serde_json::json!(settings.auto_start),
        ),
        (
            "retry.enabled".to_string(),
            serde_json::json!(settings.retry_enabled),
        ),
        (
            "retry.times".to_string(),
            serde_json::json!(settings.retry_times),
        ),
        (
            "security.enabled".to_string(),
            serde_json::json!(settings.security_enabled),
        ),
        (
            "security.mode".to_string(),
            serde_json::json!(settings.security_mode),
        ),
        (
            "security.scan_unicode".to_string(),
            serde_json::json!(settings.security_scan_unicode),
        ),
        (
            "security.scan_tools".to_string(),
            serde_json::json!(settings.security_scan_tools),
        ),
        (
            "security.scan_network".to_string(),
            serde_json::json!(settings.security_scan_network),
        ),
        (
            "security.scan_response".to_string(),
            serde_json::json!(settings.security_scan_response),
        ),
        (
            "security.redact_secrets".to_string(),
            serde_json::json!(settings.security_redact_secrets),
        ),
        (
            "security.block_on_critical".to_string(),
            serde_json::json!(settings.security_block_on_critical),
        ),
        (
            "routing.prefer_auth_accounts".to_string(),
            serde_json::json!(settings.routing_prefer_auth_accounts),
        ),
        (
            "routing.prefer_same_protocol".to_string(),
            serde_json::json!(settings.routing_prefer_same_protocol),
        ),
        (
            "ocr.enabled".to_string(),
            serde_json::json!(settings.ocr_enabled),
        ),
        (
            "ocr.max_pages".to_string(),
            serde_json::json!(settings.ocr_max_pages),
        ),
        (
            "ocr.concurrency".to_string(),
            serde_json::json!(settings.ocr_concurrency),
        ),
        ("ocr.dpi".to_string(), serde_json::json!(settings.ocr_dpi)),
        (
            "cache.semantic_enabled".to_string(),
            serde_json::json!(settings.cache_enabled),
        ),
        (
            "cache.ttl_secs".to_string(),
            serde_json::json!(settings.cache_ttl_secs),
        ),
        (
            "cache.semantic_threshold_percent".to_string(),
            serde_json::json!(settings.cache_threshold_percent),
        ),
        (
            "cache.embedding_model".to_string(),
            serde_json::json!(settings.cache_embedding_model),
        ),
    ])?;
    crate::audit_log::apply_settings(&state.settings);
    // 缩短保留期后立即清理，避免等待后台维护周期。
    let retention_days = crate::audit_log::policy_from_settings(&state.settings).retention_days;
    let pool = state.db.pool.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = crate::audit_log::cleanup_expired_logs(&pool, retention_days).await {
            tracing::warn!(%error, "审计日志设置变更后的清理失败");
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn apply_theme(
    theme: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    state
        .events
        .emit("theme-changed", serde_json::json!({ "theme": theme }));
    Ok(())
}

#[tauri::command]
pub async fn set_auto_start(enabled: bool, app: AppHandle) -> Result<(), String> {
    #[cfg(not(feature = "desktop-ui"))]
    {
        let _ = (enabled, &app);
        return Ok(());
    }
    #[cfg(feature = "desktop-ui")]
    {
        let autostart = app.autolaunch();
        if enabled {
            autostart.enable().map_err(|e| e.to_string())?;
        } else {
            autostart.disable().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// 清空语义缓存（C-02 管理命令）：model 为 None 时清全部，否则按模型清。
#[tauri::command]
pub async fn clear_semantic_cache(
    model: Option<String>,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<u64, String> {
    crate::semantic_cache::clear(&state.db.pool, model.as_deref())
        .await
        .map_err(|e| e.to_string())
}

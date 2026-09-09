//! Prompt 模板管理命令（C-07）：按 key 列版本、编辑新建版本、一键激活回滚。
//! 变更写请求日志（mode=prompt_admin）——prompt 变更可追溯。

use crate::AppState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptTemplateDto {
    pub id: String,
    pub template_key: String,
    pub version: i64,
    pub content: String,
    pub active: bool,
    pub created_at: String,
}

/// 变更审计行（不走 create_log 漏斗：管理面操作，无正文策略语义）。
async fn write_audit(
    state: &std::sync::Arc<AppState>,
    action: &str,
    template_key: &str,
    detail: &str,
) {
    let result = sqlx::query(
        "INSERT INTO request_logs (id, seq, model, mode, status_code, duration_ms, is_stream, \
         is_retry, created_at, risk_level, security_action, upstream_type, error_message) \
         VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM request_logs), ?, 'prompt_admin', 200, \
         0, 0, 0, ?, 'low', 'audit', 'admin', ?)",
    )
    .bind(crate::utils::id::new_id())
    .bind(template_key)
    .bind(crate::db::models::now_iso())
    .bind(format!("{action}: {detail}"))
    .execute(&state.db.pool)
    .await;
    if let Err(error) = result {
        tracing::warn!("[模板] 审计行写入失败: {error}");
    }
}

#[tauri::command]
pub async fn list_prompt_templates(
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<Vec<PromptTemplateDto>, String> {
    list_prompt_templates_impl(&state).await
}

pub async fn list_prompt_templates_impl(
    state: &std::sync::Arc<AppState>,
) -> Result<Vec<PromptTemplateDto>, String> {
    let rows = crate::prompt_templates::list_templates(&state.db.pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|row| PromptTemplateDto {
            id: row.id,
            template_key: row.template_key,
            version: row.version,
            content: row.content,
            active: row.active,
            created_at: row.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn create_prompt_template(
    template_key: String,
    content: String,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<i64, String> {
    create_prompt_template_impl(&template_key, &content, &state).await
}

pub async fn create_prompt_template_impl(
    template_key: &str,
    content: &str,
    state: &std::sync::Arc<AppState>,
) -> Result<i64, String> {
    let version =
        crate::prompt_templates::create_version(&state.db.pool, template_key, content).await?;
    write_audit(state, "create", template_key, &format!("v{version}")).await;
    Ok(version)
}

#[tauri::command]
pub async fn activate_prompt_template(
    template_key: String,
    version: i64,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<(), String> {
    activate_prompt_template_impl(&template_key, version, &state).await
}

pub async fn activate_prompt_template_impl(
    template_key: &str,
    version: i64,
    state: &std::sync::Arc<AppState>,
) -> Result<(), String> {
    crate::prompt_templates::activate_version(&state.db.pool, template_key, version).await?;
    write_audit(state, "activate", template_key, &format!("v{version}")).await;
    Ok(())
}

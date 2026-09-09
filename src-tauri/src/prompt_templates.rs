//! Prompt 模板版本化（C-07）：RAG 系统提示词从源码硬编码迁移为可版本化管理。
//!
//! 兼容硬保证：启动时按 key 检测无行则把**源码字面量逐字节**写入 v1 并激活
//! ——升级后 RAG 行为与升级前完全一致。运行时按 key + active 读取，表损坏/
//! 清空/读取失败时回退编译期内置默认（双保险），服务不中断。
//!
//! 占位符校验：模板必须**恰好包含**该 key 声明的全部命名占位符（如
//! `{query}`/`{context}`），不得包含未知占位符——防止激活坏模板导致检索
//! 上下文注入丢失或渲染错位。

use sqlx::SqlitePool;

/// 模板定义：key + 允许（且必需）的命名占位符 + 编译期默认（= 源码字面量）。
pub struct TemplateDef {
    pub key: &'static str,
    pub placeholders: &'static [&'static str],
    pub builtin: &'static str,
}

pub const KEY_RAG_SYSTEM: &str = "rag_system";
pub const KEY_RESEARCH_NEXT_QUERY: &str = "research_next_query_system";
pub const KEY_DEEP_RESEARCH_SYSTEM: &str = "deep_research_system";
pub const KEY_DEEP_RESEARCH_FINAL_SYSTEM: &str = "deep_research_final_system";
pub const KEY_DEEP_RESEARCH_ROUND0: &str = "deep_research_round0";
pub const KEY_DEEP_RESEARCH_ROUND_NEXT: &str = "deep_research_round_next";
pub const KEY_DEEP_RESEARCH_FINAL: &str = "deep_research_final";
pub const KEY_QUERY_REWRITE: &str = "query_rewrite";

/// 首批纳入的模板（RAG 问答 + 深度研究系列）。渠道转发类内容是用户请求的
/// 一部分，不属于系统模板，不纳入。
pub const TEMPLATE_DEFS: &[TemplateDef] = &[
    TemplateDef {
        key: KEY_RAG_SYSTEM,
        placeholders: &[],
        builtin: "你是 RAG 助手。基于检索到的内容回答问题。回答要准确、简洁，并标注信息来源。如果没有相关信息，请明确说明。",
    },
    TemplateDef {
        key: KEY_RESEARCH_NEXT_QUERY,
        placeholders: &[],
        builtin: "你是一个研究助手，根据已有发现生成下一步搜索查询。只返回查询本身。",
    },
    TemplateDef {
        key: KEY_DEEP_RESEARCH_SYSTEM,
        placeholders: &[],
        builtin: "你是深度研究助手。基于 RAG 内容进行多轮迭代研究，逐步深入分析。",
    },
    TemplateDef {
        key: KEY_DEEP_RESEARCH_FINAL_SYSTEM,
        placeholders: &[],
        builtin: "你是深度研究助手。综合多轮研究发现，给出完整准确的回答。",
    },
    TemplateDef {
        key: KEY_DEEP_RESEARCH_ROUND0,
        placeholders: &["query", "context"],
        builtin: r#"你是一个深度研究助手。请分析以下 RAG 内容，并给出初步发现。

原始问题: {query}

<knowledge_base>
{context}
</knowledge_base>

请完成：
1. 理解问题的核心需求
2. 从 RAG 中提取相关信息
3. 给出初步发现
4. 如果信息不足，指出还需要哪些方面"#,
    },
    TemplateDef {
        key: KEY_DEEP_RESEARCH_ROUND_NEXT,
        placeholders: &["query", "findings", "context"],
        builtin: r#"继续深度研究。

原始问题: {query}

已有发现:
{findings}

新检索到的内容:
<knowledge_base>
{context}
</knowledge_base>

请完成：
1. 分析新内容与已有发现的关系
2. 补充或修正之前的发现
3. 指出是否需要继续研究"#,
    },
    TemplateDef {
        key: KEY_DEEP_RESEARCH_FINAL,
        placeholders: &["query", "findings"],
        builtin: r#"基于多轮深度研究的发现，请综合回答原始问题。

原始问题: {query}

多轮研究发现:
{findings}

请综合所有发现，给出完整、准确的回答。标注信息来源。"#,
    },
    TemplateDef {
        key: KEY_QUERY_REWRITE,
        placeholders: &["history", "query"],
        builtin: r#"你是检索查询改写器。根据多轮对话历史，把用户的最新问题改写为独立、完整、可脱离上下文理解的检索查询。

对话历史:
{history}

最新问题: {query}

只输出改写后的检索查询本身，不要输出其他内容。"#,
    },
];

/// 按 key 取定义（校验与回退用）。
pub fn def_for(key: &str) -> Option<&'static TemplateDef> {
    TEMPLATE_DEFS.iter().find(|d| d.key == key)
}

/// 提取模板中出现的命名占位符（`{word}`，word 为 ASCII 字母数字下划线）。
fn placeholders_in(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = content.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '{' {
            let rest = &content[i + 1..];
            if let Some(end) = rest.find('}') {
                let name = &rest[..end];
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// 校验模板内容：必须恰好包含声明占位符的全体（不多不少）。
/// 缺占位符 = 渲染丢上下文；多占位符 = 渲染留残片——都拒绝激活。
pub fn validate_template(key: &str, content: &str) -> Result<(), String> {
    let def = def_for(key).ok_or_else(|| format!("未知模板 key: {key}"))?;
    let mut present = placeholders_in(content);
    present.sort();
    let mut expected: Vec<&str> = def.placeholders.to_vec();
    expected.sort();
    if present == expected {
        Ok(())
    } else {
        Err(format!(
            "模板 {} 占位符不符：需要 {{{}}}/{}，实际 {{{}}}/{}",
            key,
            expected.join("}, {"),
            expected.len(),
            present.join("}, {"),
            present.len()
        ))
    }
}

/// 渲染：命名占位符替换（args 必须覆盖全部声明占位符，调用侧静态保证）。
pub fn render(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// 启动种子：按 key 检测无行则写入 v1（active）——字面量逐字节等于源码。
pub async fn seed_if_empty(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    for def in TEMPLATE_DEFS {
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM prompt_templates WHERE template_key = ? LIMIT 1")
                .bind(def.key)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            sqlx::query(
                "INSERT INTO prompt_templates (id, template_key, version, content, active, created_at) \
                 VALUES (?, ?, 1, ?, 1, ?)",
            )
            .bind(crate::utils::id::new_id())
            .bind(def.key)
            .bind(def.builtin)
            .bind(crate::db::models::now_iso())
            .execute(pool)
            .await?;
            tracing::info!("[模板] 种子写入 {} v1（源码字面量）", def.key);
        }
    }
    Ok(())
}

/// 读取某 key 的激活模板；读取失败或无行时回退编译期默认（双保险）。
pub async fn load(pool: &SqlitePool, key: &str) -> String {
    match active_content(pool, key).await {
        Ok(Some(content)) => content,
        Ok(None) | Err(_) => def_for(key)
            .map(|d| d.builtin.to_string())
            .unwrap_or_default(),
    }
}

pub async fn active_content(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT content FROM prompt_templates WHERE template_key = ? AND active = 1 LIMIT 1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
}

/// 全部模板行（管理页列表用，按 key/版本排序）。
pub struct TemplateRow {
    pub id: String,
    pub template_key: String,
    pub version: i64,
    pub content: String,
    pub active: bool,
    pub created_at: String,
}

pub async fn list_templates(pool: &SqlitePool) -> Result<Vec<TemplateRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, i64, String, i64, String)>(
        "SELECT id, template_key, version, content, active, created_at \
         FROM prompt_templates ORDER BY template_key ASC, version DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, template_key, version, content, active, created_at)| TemplateRow {
                id,
                template_key,
                version,
                content,
                active: active == 1,
                created_at,
            },
        )
        .collect())
}

/// 新建版本（inactive；下一个版本号 = 该 key 现有最大版本 + 1）。
/// 内容先经占位符校验，非法拒绝。
pub async fn create_version(pool: &SqlitePool, key: &str, content: &str) -> Result<i64, String> {
    validate_template(key, content)?;
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM prompt_templates WHERE template_key = ?",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO prompt_templates (id, template_key, version, content, active, created_at) \
         VALUES (?, ?, ?, ?, 0, ?)",
    )
    .bind(crate::utils::id::new_id())
    .bind(key)
    .bind(next)
    .bind(content)
    .bind(crate::db::models::now_iso())
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(next)
}

/// 激活某版本（同 key 其余版本全部置 inactive），事务保证唯一激活。
pub async fn activate_version(pool: &SqlitePool, key: &str, version: i64) -> Result<(), String> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM prompt_templates WHERE template_key = ? AND version = ? LIMIT 1",
    )
    .bind(key)
    .bind(version)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    if exists.is_none() {
        return Err(format!("版本不存在: {key} v{version}"));
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE prompt_templates SET active = 0 WHERE template_key = ?")
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE prompt_templates SET active = 1 WHERE template_key = ? AND version = ?")
        .bind(key)
        .bind(version)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn seed_writes_builtin_literals_verbatim() {
        let pool = memory_db().await;
        seed_if_empty(&pool).await.unwrap();
        for def in TEMPLATE_DEFS {
            let stored = active_content(&pool, def.key)
                .await
                .unwrap()
                .expect("种子后应有激活行");
            assert_eq!(
                stored, def.builtin,
                "{} 的种子必须与源码字面量逐字节一致",
                def.key
            );
        }
        // 幂等：再次种子不重复写、不改版本
        seed_if_empty(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prompt_templates")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, TEMPLATE_DEFS.len() as i64, "重复种子零新增");
    }

    #[tokio::test]
    async fn activate_switches_and_rolls_back_immediately() {
        let pool = memory_db().await;
        seed_if_empty(&pool).await.unwrap();
        let v2 = create_version(&pool, KEY_RAG_SYSTEM, "测试版系统词 v2")
            .await
            .unwrap();
        assert_eq!(v2, 2);
        // 新版本未激活前仍是 v1
        assert_eq!(
            active_content(&pool, KEY_RAG_SYSTEM)
                .await
                .unwrap()
                .as_deref(),
            Some(def_for(KEY_RAG_SYSTEM).unwrap().builtin)
        );
        activate_version(&pool, KEY_RAG_SYSTEM, v2).await.unwrap();
        assert_eq!(
            active_content(&pool, KEY_RAG_SYSTEM)
                .await
                .unwrap()
                .as_deref(),
            Some("测试版系统词 v2")
        );
        // 回滚 v1 立即生效
        activate_version(&pool, KEY_RAG_SYSTEM, 1).await.unwrap();
        assert_eq!(
            active_content(&pool, KEY_RAG_SYSTEM)
                .await
                .unwrap()
                .as_deref(),
            Some(def_for(KEY_RAG_SYSTEM).unwrap().builtin)
        );
    }

    #[tokio::test]
    async fn load_falls_back_to_builtin_when_table_empty_or_broken() {
        let pool = memory_db().await;
        // 未种子（表空）→ 回退编译期默认
        assert_eq!(
            load(&pool, KEY_RAG_SYSTEM).await,
            def_for(KEY_RAG_SYSTEM).unwrap().builtin
        );
        // 清空激活行 → 回退
        sqlx::query("UPDATE prompt_templates SET active = 0")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            load(&pool, KEY_DEEP_RESEARCH_SYSTEM).await,
            def_for(KEY_DEEP_RESEARCH_SYSTEM).unwrap().builtin
        );
    }

    #[tokio::test]
    async fn invalid_placeholder_templates_are_rejected() {
        let pool = memory_db().await;
        // 未知占位符
        assert!(create_version(&pool, KEY_RAG_SYSTEM, "带 {unknown} 占位符")
            .await
            .is_err());
        // 缺占位符（round0 需要 {query}/{context}）
        assert!(
            create_version(&pool, KEY_DEEP_RESEARCH_ROUND0, "缺少占位符的模板")
                .await
                .is_err()
        );
        // 占位符齐全 → 接受
        assert!(create_version(
            &pool,
            KEY_DEEP_RESEARCH_ROUND0,
            "问题 {query} 上下文 {context}"
        )
        .await
        .is_ok());
        // 未知 key 拒绝
        assert!(create_version(&pool, "no_such_key", "x").await.is_err());
    }

    #[test]
    fn render_replaces_named_placeholders() {
        let out = render(
            "Q:{query} C:{context}",
            &[("query", "问"), ("context", "文")],
        );
        assert_eq!(out, "Q:问 C:文");
        // 无占位符模板原样返回
        assert_eq!(render("固定内容", &[]), "固定内容");
    }
}

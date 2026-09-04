use super::models::*;
use super::project;
use super::repository::WikiRepository;
use crate::core::proxy;
use crate::db::repository::Repository;
use crate::server::event_bridge::EventSink;
use crate::settings_store::SettingsStore;
use crate::utils::text::truncate_utf8;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const MAX_SOURCE_CONTEXT_BYTES: usize = 24_000;
const SOURCE_CONTEXT_TRUNCATION_MARKER: &str = "\n\n[... content truncated ...]";

/// Ingest a source file: read → parse → generate wiki pages via LLM → write to disk+DB.
///
/// FIX-22：失败收敛入口——此前任一步骤 `?` 直接返回，任务行永久停留在
/// running、来源停留在 pending；现在失败统一把两者落 failed 再返回错误。
pub async fn ingest_source(
    events: &EventSink,
    settings: &SettingsStore,
    pool: &sqlx::SqlitePool,
    project_id: &str,
    source_id: &str,
) -> Result<IngestResult, String> {
    let repo = WikiRepository::new(pool.clone());
    let task_id = repo
        .create_task(project_id, Some(source_id), "ingest")
        .await?;
    match ingest_source_inner(events, settings, pool, project_id, source_id, &task_id).await {
        Ok(result) => Ok(result),
        Err(e) => {
            let _ = repo
                .update_task_status(&task_id, "failed", 0, 0, 3, None, Some(&e))
                .await;
            let _ = repo
                .update_source_status(source_id, "failed", 0, Some(&e))
                .await;
            emit_wiki_progress(events, source_id, project_id, "", "error", 0, &e);
            Err(e)
        }
    }
}

async fn ingest_source_inner(
    events: &EventSink,
    settings: &SettingsStore,
    pool: &sqlx::SqlitePool,
    project_id: &str,
    source_id: &str,
    task_id: &str,
) -> Result<IngestResult, String> {
    let repo = WikiRepository::new(pool.clone());
    let db_repo = Arc::new(Repository::new(pool.clone()));

    // 1. Load source record
    let sources = repo.list_sources(project_id).await?;
    let source = sources
        .iter()
        .find(|s| s.id == source_id)
        .ok_or_else(|| format!("Source {} not found", source_id))?;

    // 2. Update task status
    repo.update_task_status(task_id, "running", 0, 0, 3, None, None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "processing",
        0,
        "准备摄入",
    );

    // 3. Read source file content
    let content = if let Some(ref file_path) = source.file_path {
        // Read from disk path
        tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| format!("Failed to read source file: {}", e))?
    } else {
        // Read from project raw/sources/ dir
        let raw = project::read_source_file(project_id, &source.filename)
            .await
            .map_err(|e| format!("Failed to read source: {}", e))?;
        String::from_utf8_lossy(&raw).to_string()
    };

    let source_filename = &source.filename;
    let file_ext = source_filename
        .rsplit('.')
        .next()
        .unwrap_or("txt")
        .to_lowercase();

    // 4. Parse content into sections/chunks
    repo.update_task_status(task_id, "running", 10, 0, 3, None, None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "parsing",
        10,
        "解析文档",
    );
    let sections = parse_content(&content, &file_ext);

    // 5. Get project config for LLM
    let proj = repo.get_project(project_id).await?;
    let ingest_model = proj.ingest_model.as_deref().unwrap_or("gpt-4o");
    let ingest_channel_id = match proj.ingest_channel_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            // Fallback: find first active channel from DB
            let row: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM channels WHERE status = 1 ORDER BY priority DESC LIMIT 1",
            )
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("DB error: {}", e))?;
            let id = row.map(|(id,)| id).ok_or_else(|| "No active channel configured. Please create a channel first or set ingest_channel_id in Wiki project settings.".to_string())?;
            id
        }
    };

    // 6. Generate wiki pages via LLM
    repo.update_task_status(task_id, "running", 30, 1, 3, None, None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "generating",
        30,
        "LLM 生成页面",
    );
    let pages = generate_wiki_pages(
        settings,
        &db_repo,
        ingest_model,
        &ingest_channel_id,
        project_id,
        source_filename,
        &sections,
        proj.schema_text
            .as_deref()
            .unwrap_or(super::repository::DEFAULT_SCHEMA),
    )
    .await?;

    // 7. Write pages to disk + DB
    repo.update_task_status(task_id, "running", 60, 2, 3, None, None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "writing",
        60,
        "写入页面",
    );
    let mut written_pages = Vec::new();
    for page in &pages {
        let page_path = &page.path;

        // Write to disk
        project::write_page(project_id, page_path, &page.content).await?;

        // Compute hash
        let mut hasher = Sha256::new();
        hasher.update(page.content.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let token_count = (page.content.len() / 4) as i64;

        // Extract wikilinks
        let wikilinks = extract_wikilinks(&page.content);
        let wikilinks_json = serde_json::to_string(&wikilinks).unwrap_or_else(|_| "[]".to_string());

        // Extract tags from frontmatter
        let tags = extract_tags_from_frontmatter(&page.content);
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());

        // Upsert into DB
        repo.upsert_page(
            project_id,
            page_path,
            &page.title,
            &page.page_type,
            &hash,
            token_count,
            &wikilinks_json,
            "{}",
            &tags_json,
        )
        .await?;

        written_pages.push(WrittenPage {
            path: page_path.clone(),
            title: page.title.clone(),
            page_type: page.page_type.clone(),
            wikilinks,
        });
    }

    // 8. Update graph edges from wikilinks
    repo.update_task_status(task_id, "running", 80, 2, 3, None, None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "linking",
        80,
        "更新知识图谱",
    );
    update_graph_edges(pool, project_id, &written_pages).await?;

    // 9. Update source status
    repo.update_source_status(source_id, "ingested", written_pages.len() as i64, None)
        .await?;

    // Update project counts
    let page_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_pages WHERE project_id = ? AND status = 'active'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let source_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_sources WHERE project_id = ? AND status = 'ingested'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE wiki_projects SET page_count=?, source_count=?, last_ingest_at=?, updated_at=? WHERE id=?"
    )
    .bind(page_count).bind(source_count).bind(&now).bind(&now).bind(project_id)
    .execute(pool).await.map_err(|e| format!("DB error: {}", e))?;

    // Append log
    let _ = project::append_log(
        project_id,
        &format!(
            "ingest | {} → {} pages",
            source_filename,
            written_pages.len()
        ),
    )
    .await;

    // Update task
    let result_json = serde_json::json!({
        "pages_created": written_pages.len(),
        "source": source_filename,
    })
    .to_string();
    repo.update_task_status(task_id, "done", 100, 3, 3, Some(&result_json), None)
        .await?;
    emit_wiki_progress(
        events,
        source_id,
        project_id,
        &source.filename,
        "done",
        100,
        &format!("完成，生成 {} 个页面", written_pages.len()),
    );

    Ok(IngestResult {
        pages_created: written_pages.len(),
        page_paths: written_pages.iter().map(|p| p.path.clone()).collect(),
    })
}

#[derive(Debug, Clone)]
pub struct IngestResult {
    pub pages_created: usize,
    pub page_paths: Vec<String>,
}

struct GeneratedPage {
    path: String,
    title: String,
    page_type: String,
    content: String,
}

/// Emit wiki source ingest progress event to frontend.
fn emit_wiki_progress(
    events: &EventSink,
    source_id: &str,
    project_id: &str,
    filename: &str,
    stage: &str,
    progress: u8,
    detail: &str,
) {
    events.emit(
        "wiki-source-progress",
        serde_json::json!({
            "source_id": source_id,
            "project_id": project_id,
            "filename": filename,
            "stage": stage,
            "progress": progress,
            "detail": detail,
        }),
    );
}

struct WrittenPage {
    path: String,
    title: String,
    page_type: String,
    wikilinks: Vec<String>,
}

/// Parse content into sections based on file type.
fn parse_content(content: &str, file_ext: &str) -> Vec<ContentSection> {
    match file_ext.as_ref() {
        "md" | "markdown" => parse_markdown(content),
        "txt" => parse_plain_text(content),
        "json" => parse_json(content),
        _ => parse_plain_text(content),
    }
}

#[derive(Debug, Clone)]
struct ContentSection {
    heading: String,
    content: String,
}

fn parse_markdown(content: &str) -> Vec<ContentSection> {
    let mut sections = Vec::new();
    let mut current_heading = String::from("Overview");
    let mut current_content = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("# ") {
            if !current_content.is_empty() {
                sections.push(ContentSection {
                    heading: current_heading.clone(),
                    content: current_content.clone(),
                });
                current_content.clear();
            }
            current_heading = trimmed[2..].to_string();
        } else if trimmed.starts_with("## ") {
            if !current_content.is_empty() {
                sections.push(ContentSection {
                    heading: current_heading.clone(),
                    content: current_content.clone(),
                });
                current_content.clear();
            }
            current_heading = trimmed[3..].to_string();
        } else {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    if !current_content.is_empty() {
        sections.push(ContentSection {
            heading: current_heading,
            content: current_content,
        });
    }

    // If only one small section, split by paragraphs
    if sections.len() <= 1 && content.len() > 2000 {
        return parse_plain_text(content);
    }

    if sections.is_empty() {
        sections.push(ContentSection {
            heading: "Document".to_string(),
            content: content.to_string(),
        });
    }

    sections
}

fn parse_plain_text(content: &str) -> Vec<ContentSection> {
    let mut sections = Vec::new();
    let mut chunk = String::new();
    let mut chunk_idx = 1;
    let max_chunk = 3000;

    for line in content.lines() {
        if chunk.len() + line.len() > max_chunk {
            if !chunk.is_empty() {
                sections.push(ContentSection {
                    heading: format!("Section {}", chunk_idx),
                    content: chunk.clone(),
                });
                chunk_idx += 1;
                chunk.clear();
            }
        }
        chunk.push_str(line);
        chunk.push('\n');
    }

    if !chunk.is_empty() {
        sections.push(ContentSection {
            heading: format!("Section {}", chunk_idx),
            content: chunk,
        });
    }

    if sections.is_empty() {
        sections.push(ContentSection {
            heading: "Document".to_string(),
            content: content.to_string(),
        });
    }

    sections
}

fn parse_json(content: &str) -> Vec<ContentSection> {
    // Try to parse and flatten JSON into readable sections
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(json) => {
            let pretty =
                serde_json::to_string_pretty(&json).unwrap_or_else(|_| content.to_string());
            parse_plain_text(&pretty)
        }
        Err(_) => parse_plain_text(content),
    }
}

/// Generate wiki pages from content sections via LLM.
async fn generate_wiki_pages(
    settings: &SettingsStore,
    db_repo: &Arc<Repository>,
    model: &str,
    channel_id: &str,
    project_id: &str,
    source_filename: &str,
    sections: &[ContentSection],
    schema: &str,
) -> Result<Vec<GeneratedPage>, String> {
    let truncated = build_source_context(sections);

    let system_prompt = format!(
        r#"You are a Wiki maintainer. Read the source document and generate structured wiki pages in Markdown.

## Wiki Schema
{}

## Instructions
1. Analyze the source document and identify key entities, concepts, and topics.
2. For each key item, generate a wiki page in Markdown format.
3. Use `[[wikilinks]]` to connect related pages.
4. Each page should have YAML frontmatter with: title, type (entity/concept/summary), tags, source.
5. Separate pages with a delimiter: ---PAGE---
6. The first line of each page should be the file path (e.g., `entities/my-item.md`).

## Output Format
```
entities/page-name.md
---
title: Page Name
type: entity
tags: [tag1, tag2]
source: {}
---
# Page Name

Content here with [[wikilinks]] to other pages.

## Details
...
---PAGE---
concepts/another-concept.md
---
title: Another Concept
type: concept
tags: [tag1]
source: {}
---
# Another Concept
...
```

Generate 3-8 pages depending on document complexity. Focus on the most important entities and concepts."#,
        schema, source_filename, source_filename
    );

    let user_prompt = format!(
        "Source document: {}\n\nContent:\n{}",
        source_filename, truncated
    );

    let chat_request = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt}
        ],
        "stream": false,
        "temperature": 0.3
    });

    let chat_request_str: String = serde_json::to_string(&chat_request).unwrap_or_default();

    let proxy_result = proxy::handle_request(
        db_repo,
        settings,
        channel_id,
        "Wiki Ingest",
        chat_request,
        false,
        Some(chat_request_str),
        Some(format!("wiki-ingest_{}", project_id)),
        None,
    )
    .await;

    let response_body = match proxy_result {
        Ok(result) => result.body,
        Err((code, msg)) => {
            return Err(format!("LLM request failed ({}): {}", code, msg));
        }
    };

    // Extract text from response
    let raw_text = response_body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if raw_text.is_empty() {
        // Fallback: create a single summary page from the source
        return Ok(vec![GeneratedPage {
            path: format!("summaries/{}.md", sanitize_filename(source_filename)),
            title: source_filename.to_string(),
            page_type: "summary".to_string(),
            content: format!(
                "---\ntitle: {}\ntype: summary\ntags: []\nsource: {}\n---\n\n# {}\n\n{}",
                source_filename, source_filename, source_filename, truncated
            ),
        }]);
    }

    // Parse the LLM output into pages
    Ok(parse_generated_pages(raw_text, source_filename))
}

fn build_source_context(sections: &[ContentSection]) -> String {
    let combined: String = sections
        .iter()
        .map(|s| format!("## {}\n{}", s.heading, s.content))
        .collect::<Vec<_>>()
        .join("\n\n");

    if combined.len() <= MAX_SOURCE_CONTEXT_BYTES {
        return combined;
    }

    let mut truncated = truncate_utf8(&combined, MAX_SOURCE_CONTEXT_BYTES).to_string();
    truncated.push_str(SOURCE_CONTEXT_TRUNCATION_MARKER);
    truncated
}

/// Parse LLM-generated pages from the response text.
fn parse_generated_pages(text: &str, source_filename: &str) -> Vec<GeneratedPage> {
    let mut pages = Vec::new();
    let mut current_path = String::new();
    let mut current_content = String::new();
    let mut in_content = false;

    let mut lines = text.lines().peekable();

    // Skip any preamble before first path
    while let Some(line) = lines.peek() {
        let trimmed = line.trim();
        if trimmed.ends_with(".md") || (trimmed.contains('/') && trimmed.ends_with(".md")) {
            break;
        }
        // Also check for path-like patterns
        if trimmed.contains(".md") && !trimmed.starts_with("#") {
            break;
        }
        lines.next();
    }

    for line in text.lines() {
        let trimmed = line.trim();

        // Check for page delimiter
        if trimmed == "---PAGE---" {
            if !current_content.is_empty() {
                if let Some(path) = extract_path_from_content(&current_content) {
                    pages.push(build_page(&path, &current_content, source_filename));
                }
            }
            current_content.clear();
            current_path.clear();
            in_content = false;
            continue;
        }

        // Check if line looks like a file path (ends with .md and has no spaces in path part)
        if !in_content && (trimmed.ends_with(".md") && trimmed.len() < 200) {
            current_path = trimmed.to_string();
            in_content = true;
            continue;
        }

        if in_content {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Handle last page
    if !current_content.is_empty() {
        if current_path.is_empty() {
            // Try to extract path from content
            if let Some(path) = extract_path_from_content(&current_content) {
                current_path = path;
            }
        }
        if !current_path.is_empty() {
            pages.push(build_page(&current_path, &current_content, source_filename));
        } else if let Some(page) = build_page_from_content(&current_content, source_filename) {
            pages.push(page);
        }
    }

    // Deduplicate by path
    let mut seen = HashSet::new();
    pages.retain(|p| {
        if seen.contains(&p.path) {
            false
        } else {
            seen.insert(p.path.clone());
            true
        }
    });

    if pages.is_empty() {
        // Fallback: create a summary page
        pages.push(GeneratedPage {
            path: format!("summaries/{}.md", sanitize_filename(source_filename)),
            title: source_filename.to_string(),
            page_type: "summary".to_string(),
            content: format!(
                "---\ntitle: {}\ntype: summary\ntags: []\nsource: {}\n---\n\n# {}\n\n{}",
                source_filename,
                source_filename,
                source_filename,
                text.chars().take(8000).collect::<String>()
            ),
        });
    }

    pages
}

fn extract_path_from_content(content: &str) -> Option<String> {
    // Look for first line that looks like a path
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with(".md") && trimmed.len() < 200 {
            // Clean up markdown code fences
            let clean = trimmed
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();
            if clean.ends_with(".md") {
                return Some(clean.to_string());
            }
        }
    }
    None
}

fn build_page(path: &str, raw_content: &str, source_filename: &str) -> GeneratedPage {
    // Remove path line from content if present
    let content = raw_content
        .lines()
        .filter(|l| {
            let t = l.trim();
            !(t.ends_with(".md") && t.len() < 200)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let content = content.trim();

    // Extract title from frontmatter or first heading
    let title = extract_title_from_content(content, path);

    // Determine page type from frontmatter or path
    let page_type = if path.starts_with("entities/") {
        "entity"
    } else if path.starts_with("concepts/") {
        "concept"
    } else if path.starts_with("summaries/") {
        "summary"
    } else if path.ends_with("index.md") {
        "index"
    } else if path.ends_with("log.md") {
        "log"
    } else {
        "entity"
    };

    // Ensure content has frontmatter
    let final_content = if content.starts_with("---") {
        content.to_string()
    } else {
        format!(
            "---\ntitle: {}\ntype: {}\ntags: []\nsource: {}\n---\n\n{}",
            title, page_type, source_filename, content
        )
    };

    GeneratedPage {
        path: path.to_string(),
        title,
        page_type: page_type.to_string(),
        content: final_content,
    }
}

fn build_page_from_content(content: &str, source_filename: &str) -> Option<GeneratedPage> {
    let title = extract_title_from_content(content, "");
    if title.is_empty() {
        return None;
    }
    let path = format!("entities/{}.md", sanitize_filename(&title));
    Some(GeneratedPage {
        path,
        title,
        page_type: "entity".to_string(),
        content: content.to_string(),
    })
}

pub fn extract_title_from_content(content: &str, fallback_path: &str) -> String {
    // Try frontmatter title
    if content.starts_with("---") {
        let end = content[3..].find("---");
        if let Some(e) = end {
            let frontmatter = &content[3..3 + e];
            for line in frontmatter.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("title:") {
                    let title = trimmed[6..].trim().trim_matches('"').trim_matches('\'');
                    if !title.is_empty() {
                        return title.to_string();
                    }
                }
            }
        }
    }
    // Try first heading
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("# ") {
            return trimmed[2..].trim().to_string();
        }
        if trimmed.starts_with("## ") {
            return trimmed[3..].trim().to_string();
        }
    }
    // Fallback to filename from path
    if !fallback_path.is_empty() {
        return fallback_path
            .split('/')
            .last()
            .unwrap_or(fallback_path)
            .trim_end_matches(".md")
            .to_string();
    }
    "Untitled".to_string()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut start = 0;
    loop {
        if let Some(s) = content[start..].find("[[") {
            let s = start + s + 2;
            if let Some(e) = content[s..].find("]]") {
                let link = &content[s..s + e];
                if !link.is_empty() {
                    links.push(link.to_string());
                }
                start = s + e + 2;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    links
}

/// Rebuild graph edges for a project based on current page wikilinks.
/// Called after page save/delete to keep the knowledge graph up-to-date.
/// FIX-27：内存 wikilink 解析表——path/title 一次载入，逐链接解析不再
/// 每条打一次 `resolve_wikilink_to_path` 查询（N+1 消除）。
struct WikilinkIndex {
    /// LOWER(title) → path（与旧实现 LOWER(title)=LOWER(?) 语义一致）。
    titles: HashMap<String, String>,
    paths: HashSet<String>,
}

impl WikilinkIndex {
    fn resolve(&self, link: &str) -> String {
        let link = link.trim();
        // If it already looks like a path, use as-is
        if link.contains('/') && link.ends_with(".md") {
            return link.to_string();
        }
        if let Some(path) = self.titles.get(&link.to_lowercase()) {
            return path.clone();
        }
        normalize_wikilink(link)
    }

    fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }
}

/// 一次载入项目全部活跃页：解析索引 + 逐页 wikilinks 列表。
async fn load_active_pages(
    pool: &sqlx::SqlitePool,
    project_id: &str,
) -> Result<(Vec<(String, Vec<String>)>, WikilinkIndex), String> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT path, wikilinks, title FROM wiki_pages WHERE project_id = ? AND status = 'active'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("DB error: {}", e))?;
    let index = WikilinkIndex {
        titles: rows
            .iter()
            .map(|(path, _, title)| (title.to_lowercase(), path.clone()))
            .collect(),
        paths: rows.iter().map(|(path, _, _)| path.clone()).collect(),
    };
    let pages = rows
        .into_iter()
        .map(|(path, wikilinks_json, _)| {
            let wikilinks: Vec<String> = serde_json::from_str(&wikilinks_json).unwrap_or_default();
            (path, wikilinks)
        })
        .collect();
    Ok((pages, index))
}

/// FIX-27：批量写边——单事务 + 分块多行 INSERT，替代逐行往返。
async fn replace_graph_edges(
    pool: &sqlx::SqlitePool,
    project_id: &str,
    links: Vec<(String, String)>,
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("DB error: {}", e))?;
    sqlx::query("DELETE FROM wiki_graph_edges WHERE project_id = ?")
        .bind(project_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("DB error: {}", e))?;
    let now = chrono::Utc::now().to_rfc3339();
    for chunk in links.chunks(500) {
        let mut sql = String::from(
            "INSERT OR IGNORE INTO wiki_graph_edges (id, project_id, source_page, target_page, edge_type, weight, created_at) VALUES ",
        );
        let groups: Vec<&str> = chunk.iter().map(|_| "(?, ?, ?, ?, 'wikilink', 1.0, ?)").collect();
        sql.push_str(&groups.join(", "));
        let mut query = sqlx::query(&sql);
        for (source, target) in chunk {
            query = query
                .bind(uuid::Uuid::new_v4().to_string())
                .bind(project_id)
                .bind(source)
                .bind(target)
                .bind(&now);
        }
        query
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("DB error: {}", e))?;
    }
    tx.commit().await.map_err(|e| format!("DB error: {}", e))?;
    Ok(())
}

pub async fn rebuild_graph_edges(pool: &sqlx::SqlitePool, project_id: &str) -> Result<(), String> {
    let (pages, index) = load_active_pages(pool, project_id).await?;
    let mut all_links: Vec<(String, String)> = Vec::new();
    for (path, links) in &pages {
        for link in links {
            let target = index.resolve(link);
            if index.contains(&target) {
                all_links.push((path.clone(), target));
            }
        }
    }
    replace_graph_edges(pool, project_id, all_links).await
}

/// Update graph_edges table based on wikilinks in pages.
///
/// FIX-27：新页面在第 7 步已 upsert 入库，这里统一从 DB 一次载入全部
/// 活跃页（含新页）做内存解析 + 批量写入，不再逐链接/逐行打查询。
async fn update_graph_edges(
    pool: &sqlx::SqlitePool,
    project_id: &str,
    pages: &[WrittenPage],
) -> Result<(), String> {
    let _ = pages; // 兼容旧签名保留参数（页面已在库中）
    let (db_pages, index) = load_active_pages(pool, project_id).await?;
    let mut all_links: Vec<(String, String)> = Vec::new();
    for (path, links) in &db_pages {
        for link in links {
            let target = index.resolve(link);
            if index.contains(&target) {
                all_links.push((path.clone(), target));
            }
        }
    }
    replace_graph_edges(pool, project_id, all_links).await
}

/// Extract tags from YAML frontmatter of a wiki page.
pub fn extract_tags_from_frontmatter(content: &str) -> Vec<String> {
    if !content.starts_with("---") {
        return vec![];
    }
    let rest = &content[3..];
    let end = match rest.find("---") {
        Some(e) => e,
        None => return vec![],
    };
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tags:") {
            let tags_part = trimmed[5..].trim();
            // Parse array format: [tag1, tag2] or ["tag1", "tag2"]
            let cleaned = tags_part
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            if cleaned.is_empty() {
                return vec![];
            }
            return cleaned
                .split(',')
                .map(|t| {
                    t.trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .trim()
                        .to_string()
                })
                .filter(|t| !t.is_empty())
                .collect();
        }
    }
    vec![]
}

fn normalize_wikilink(link: &str) -> String {
    let link = link.trim();
    // If it already looks like a path, use as-is
    if link.contains('/') && link.ends_with(".md") {
        return link.to_string();
    }
    // Otherwise, assume it's an entity name
    let slug: String = link
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    format!("entities/{}.md", slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIX-22：摄入失败时任务与来源状态收敛为 failed，不再永久 running。
    /// 用不存在的来源触发第一步失败（无渠道/LLM 依赖，稳定可控）。
    #[tokio::test]
    async fn ingest_failure_converges_task_and_source_to_failed() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let (event_tx, _) = tokio::sync::broadcast::channel(16);
        let events = crate::server::event_bridge::EventSink::headless(event_tx);
        let settings = crate::settings_store::SettingsStore::file(
            std::env::temp_dir().join(format!("waliapi-ingest-test-{}.json", uuid::Uuid::new_v4())),
        );

        // 先建真实项目（wiki_ingest_queue 对 project_id 有外键约束）
        sqlx::query(
            "INSERT INTO wiki_projects (id, name, wiki_dir, created_at, updated_at) VALUES ('proj-fk', 't', '/tmp/w', ?, ?)",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

        let err = ingest_source(&events, &settings, &pool, "proj-fk", "src-none").await;
        assert!(err.is_err(), "不存在的来源必须报错");

        // 任务行存在且已收敛为 failed（带错误信息），不是 running
        let task: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT status, error_message FROM wiki_ingest_queue ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        let (status, error) = task.expect("task row must exist");
        assert_eq!(status, "failed", "任务状态必须收敛 failed");
        assert!(error.is_some(), "失败原因必须落库");
    }

    /// FIX-27：批量图谱重建——标题解析（含大小写不敏感）与边落库正确。
    #[tokio::test]
    async fn rebuild_graph_edges_resolves_titles_and_writes_edges() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        // wiki_pages 对 project_id 有外键约束：先建项目
        sqlx::query(
            "INSERT INTO wiki_projects (id, name, wiki_dir, created_at, updated_at) VALUES ('p1', 't', '/tmp/w', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        for (path, title, wikilinks) in [
            ("a.md", "Alpha", r#"["Beta"]"#),
            ("b.md", "Beta", r#"["ghost"]"#),
            ("docs/c.md", "Gamma", r#"["docs/c.md"]"#),
        ] {
            sqlx::query(
                "INSERT INTO wiki_pages (id, project_id, path, title, page_type, content_hash, token_count, wikilinks, frontmatter, tags, status, created_at, updated_at)
                 VALUES (?, 'p1', ?, ?, 'note', 'h', 1, ?, '{}', '[]', 'active', ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(path)
            .bind(title)
            .bind(wikilinks)
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();
        }

        rebuild_graph_edges(&pool, "p1").await.unwrap();

        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT source_page, target_page FROM wiki_graph_edges WHERE project_id = 'p1' ORDER BY source_page",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        // Alpha→Beta（标题解析）；Gamma→自身（路径直通）；Beta→ghost 不存在 → 无边
        assert!(edges.contains(&("a.md".to_string(), "b.md".to_string())), "edges: {edges:?}");
        assert!(edges.contains(&("docs/c.md".to_string(), "docs/c.md".to_string())), "edges: {edges:?}");
        assert_eq!(edges.len(), 2, "幽灵链接不得建边: {edges:?}");
    }

    #[test]
    fn source_context_truncates_cjk_at_utf8_boundary() {
        let sections = vec![ContentSection {
            heading: String::new(),
            content: "中".repeat(9_000),
        }];

        let context = build_source_context(&sections);
        let content = context
            .strip_suffix(SOURCE_CONTEXT_TRUNCATION_MARKER)
            .unwrap();

        assert!(content.len() <= MAX_SOURCE_CONTEXT_BYTES);
        assert!(content.is_char_boundary(content.len()));
        assert!(context.ends_with(SOURCE_CONTEXT_TRUNCATION_MARKER));
    }

    #[test]
    fn source_context_truncates_emoji_at_utf8_boundary() {
        let sections = vec![ContentSection {
            heading: "Emoji".to_string(),
            content: "😀".repeat(7_000),
        }];

        let context = build_source_context(&sections);
        let content = context
            .strip_suffix(SOURCE_CONTEXT_TRUNCATION_MARKER)
            .unwrap();

        assert!(content.len() <= MAX_SOURCE_CONTEXT_BYTES);
        assert!(content.is_char_boundary(content.len()));
    }

    #[test]
    fn source_context_preserves_content_within_limit() {
        let sections = vec![ContentSection {
            heading: "Overview".to_string(),
            content: "WaLiAPI Wiki".to_string(),
        }];

        assert_eq!(build_source_context(&sections), "## Overview\nWaLiAPI Wiki");
    }

    #[test]
    fn source_context_handles_ascii_at_and_above_byte_limit() {
        let exact = vec![ContentSection {
            heading: "A".to_string(),
            content: "x".repeat(MAX_SOURCE_CONTEXT_BYTES - "## A\n".len()),
        }];
        let over = vec![ContentSection {
            heading: "A".to_string(),
            content: "x".repeat(MAX_SOURCE_CONTEXT_BYTES - "## A\n".len() + 1),
        }];

        let exact_context = build_source_context(&exact);
        assert_eq!(exact_context.len(), MAX_SOURCE_CONTEXT_BYTES);
        assert!(!exact_context.ends_with(SOURCE_CONTEXT_TRUNCATION_MARKER));

        let over_context = build_source_context(&over);
        let over_content = over_context
            .strip_suffix(SOURCE_CONTEXT_TRUNCATION_MARKER)
            .unwrap();
        assert_eq!(over_content.len(), MAX_SOURCE_CONTEXT_BYTES);
    }

    #[test]
    fn source_context_truncates_combined_sections_at_utf8_boundary() {
        let sections = vec![
            ContentSection {
                heading: "Overview".to_string(),
                content: "a".repeat(12_000),
            },
            ContentSection {
                heading: "中文".to_string(),
                content: "界".repeat(5_000),
            },
        ];

        let context = build_source_context(&sections);
        let content = context
            .strip_suffix(SOURCE_CONTEXT_TRUNCATION_MARKER)
            .unwrap();

        assert!(content.len() <= MAX_SOURCE_CONTEXT_BYTES);
        assert!(content.is_char_boundary(content.len()));
        assert!(content.contains("## 中文"));
    }

    /// 回归测试（issue #55，FIX-03 移植适配）：摄入 >24KB 纯中文文档时，
    /// parse_content → 上下文组装的真实崩溃路径不得因字节切片 panic。
    /// 上游 PR #58 的用例直接构造 sections，本用例补齐从原始文档进出的端到端覆盖。
    #[test]
    fn source_context_large_cjk_document_via_parse_content_does_not_panic() {
        // 48000 字节纯中文（单行，保证切点落在多字节字符中间），超过上限。
        let doc = "知识库摄入测试。".repeat(2000);
        assert!(doc.len() > MAX_SOURCE_CONTEXT_BYTES);
        let sections = parse_content(&doc, "txt");
        assert!(!sections.is_empty());
        let context = build_source_context(&sections);
        let content = context
            .strip_suffix(SOURCE_CONTEXT_TRUNCATION_MARKER)
            .unwrap();
        assert!(content.len() <= MAX_SOURCE_CONTEXT_BYTES);
        assert!(content.is_char_boundary(content.len()));
        assert!(std::str::from_utf8(content.as_bytes()).is_ok());
    }
}

use super::import_guard;
use super::models::ImportSourceInput;
use super::parser;
use super::processor;
use super::repository::KbRepository;
use crate::server::event_bridge::EventSink;
use crate::settings_store::SettingsStore;
use sha2::Digest;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

/// url 导入的响应体大小上限（SSRF 探测常用的超大响应/压缩炸弹防御）
const MAX_IMPORT_URL_BYTES: usize = 10 * 1024 * 1024;
/// url 导入允许的重定向跳数上限（同主机内跟随，跨主机一律拒绝）
const MAX_IMPORT_REDIRECTS: usize = 5;

/// Import a Git repository: clone → filter → process files
pub async fn import_git_repo(
    pool: &SqlitePool,
    events: &EventSink,
    kb_id: &str,
    source_id: &str,
    input: &ImportSourceInput,
    settings: &SettingsStore,
    data_dir: &Path,
) -> Result<usize, String> {
    let repo_url = input
        .repo_url
        .as_ref()
        .ok_or("repo_url is required for git import")?;

    // FIX-07 导入加固：scheme 白名单（仅 https，拒绝 ext::/ssh/file 等）+ URL 解析重建
    // （丢弃 userinfo/query/fragment）；凭证经 git -c http.extraHeader 注入，绝不拼进 URL
    // （git 失败时会回显完整 URL，token 进 URL 即随 stderr 泄漏）。
    let clone_url = import_guard::normalize_git_url(repo_url)?;
    let branch = input.branch.as_deref().unwrap_or("main");
    import_guard::validate_branch(branch)?;
    let token = input
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());

    // Clone repo to temp dir
    let temp_dir = std::env::temp_dir().join(format!("kb_import_{}", uuid::Uuid::new_v4()));
    emit_import_progress(events, kb_id, source_id, 0, "Cloning repository...");

    let mut command = tokio::process::Command::new("git");
    if let Some(token) = token {
        command.arg("-c").arg(format!(
            "http.extraHeader={}",
            import_guard::git_auth_header(token)
        ));
    }
    let clone_result = command
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            branch,
            &clone_url,
            temp_dir.to_str().unwrap(),
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run git clone: {}", e))?;

    if !clone_result.status.success() {
        // 回传前剥离任何含凭证的行（明文 token / extraHeader 形态）再截断
        let err =
            import_guard::sanitize_git_error(&String::from_utf8_lossy(&clone_result.stderr), token);
        // Clean up
        std::fs::remove_dir_all(&temp_dir).ok();
        return Err(format!("Git clone failed: {err}"));
    }

    // Process files from cloned repo
    let excluded_dirs = input.excluded_dirs.clone().unwrap_or_default();
    let included_files = input.included_files.clone().unwrap_or_default();
    let max_file_size = input.max_file_size.unwrap_or(1024 * 1024); // 1MB default

    let result = process_directory_files(
        pool,
        events,
        kb_id,
        source_id,
        &temp_dir,
        &excluded_dirs,
        &included_files,
        max_file_size,
        "git",
        Some(&clone_url),
        None,
        settings,
        data_dir,
    )
    .await;

    // Clean up temp dir
    std::fs::remove_dir_all(&temp_dir).ok();

    result
}

/// Import from a URL: fetch content → process
///
/// FIX-07 SSRF 防护：仅 http/https；DNS 解析后拒绝环回/私网/链路本地等内网目标
/// （校验全部解析记录）；禁用自动重定向、仅跟随同主机跳转且每跳重新校验；
/// 响应体流式读取并限制在 10MB 以内。
pub async fn import_url(
    pool: &SqlitePool,
    events: &EventSink,
    kb_id: &str,
    source_id: &str,
    input: &ImportSourceInput,
    settings: &SettingsStore,
    data_dir: &Path,
) -> Result<usize, String> {
    let url_input = input.url.as_ref().ok_or("url is required for url import")?;

    emit_import_progress(events, kb_id, source_id, 0, "Fetching URL...");

    let mut current = import_guard::validate_import_url(url_input).await?;

    // 禁用 reqwest 自动重定向（默认策略会跟到任意主机，可绕过首跳校验），
    // 手动跟随同主机跳转，每跳重新过 scheme + DNS 校验。
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let mut redirects = 0usize;
    let resp = loop {
        let resp = client
            .get(current.clone())
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch URL: {e}"))?;
        if resp.status().is_redirection() {
            redirects += 1;
            if redirects > MAX_IMPORT_REDIRECTS {
                return Err(format!("重定向次数超过上限（{MAX_IMPORT_REDIRECTS}）"));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("重定向响应缺少 Location 头")?;
            let next = import_guard::redirect_allowed(&current, location)?;
            current = import_guard::validate_import_url(next.as_str()).await?;
            continue;
        }
        break resp;
    };

    if !resp.status().is_success() {
        return Err(format!("HTTP {}: {}", resp.status(), current));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();

    // 流式读取响应体并施加大小上限（防超大响应/压缩炸弹撑爆内存）
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut content: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Failed to read body: {e}"))?;
        if content.len() + chunk.len() > MAX_IMPORT_URL_BYTES {
            return Err(format!(
                "响应超过大小上限（{} MB），已中止导入",
                MAX_IMPORT_URL_BYTES / 1024 / 1024
            ));
        }
        content.extend_from_slice(&chunk);
    }

    // Determine filename from URL（用解析后的 path，天然无 query/fragment）
    let filename = current
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("imported")
        .to_string();
    let filename = if filename.contains('.') && !filename.starts_with('.') {
        filename
    } else if content_type.contains("html") {
        format!("{}.html", filename)
    } else if content_type.contains("markdown") || content_type.contains("text/plain") {
        format!("{}.md", filename)
    } else {
        format!("{}.txt", filename)
    };

    let file_type = parser::get_file_type(&filename);

    // Create document record
    let repo = KbRepository::new(pool.clone());
    let hash = sha2::Sha256::digest(&content);
    let hash_hex = hex::encode(hash);

    // Check duplicate
    if let Ok(Some(_)) = repo.find_document_by_hash(kb_id, &hash_hex).await {
        emit_import_progress(events, kb_id, source_id, 100, "URL content already exists");
        return Ok(0);
    }

    let doc = repo
        .create_document_with_source(
            kb_id,
            &filename,
            None,
            &file_type,
            content.len() as i64,
            &hash_hex,
            "url",
            Some(current.as_str()),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

    // Get KB embedding model
    let kb = repo.get_kb(kb_id).await.map_err(|e| e.to_string())?;
    let emb_model = kb.embedding_model.clone();

    emit_import_progress(events, kb_id, source_id, 30, "Processing document...");

    processor::process_document(
        pool,
        events,
        kb_id,
        &doc.id,
        &filename,
        &content,
        emb_model.as_deref(),
        settings,
        data_dir,
    )
    .await?;

    emit_import_progress(events, kb_id, source_id, 100, "URL import complete");
    Ok(1)
}

/// Import from a local directory: scan → filter → process files
///
/// FIX-07 边界约束：默认仅允许数据目录内的路径；数据目录外的目录必须在设置项
/// `kb.import.allowed_roots`（字符串数组）中显式加入白名单。canonicalize 解析
/// 符号链接后做前缀校验——指向允许范围外的符号链接其规范形态必然落在根外而被拒绝。
pub async fn import_local_dir(
    pool: &SqlitePool,
    events: &EventSink,
    kb_id: &str,
    source_id: &str,
    input: &ImportSourceInput,
    settings: &SettingsStore,
    data_dir: &Path,
) -> Result<usize, String> {
    let dir_path = input
        .dir_path
        .as_ref()
        .ok_or("dir_path is required for local_dir import")?;

    let allowed_roots = import_guard::allowed_roots_from_settings(settings);
    let path = import_guard::validate_local_dir(Path::new(dir_path), data_dir, &allowed_roots)?;

    let excluded_dirs = input.excluded_dirs.clone().unwrap_or_default();
    let included_files = input.included_files.clone().unwrap_or_default();
    let max_file_size = input.max_file_size.unwrap_or(1024 * 1024);

    process_directory_files(
        pool,
        events,
        kb_id,
        source_id,
        &path,
        &excluded_dirs,
        &included_files,
        max_file_size,
        "local_dir",
        None,
        Some(dir_path),
        settings,
        data_dir,
    )
    .await
}

/// Common: process all files in a directory with filtering
#[allow(clippy::too_many_arguments)]
async fn process_directory_files(
    pool: &SqlitePool,
    events: &EventSink,
    kb_id: &str,
    source_id: &str,
    dir: &PathBuf,
    excluded_dirs: &[String],
    included_files: &[String],
    max_file_size: usize,
    source_type: &str,
    source_url: Option<&str>,
    _source_path: Option<&str>,
    settings: &SettingsStore,
    data_dir: &Path,
) -> Result<usize, String> {
    emit_import_progress(events, kb_id, source_id, 5, "Scanning directory...");

    let files = scan_directory(dir, excluded_dirs, included_files, max_file_size)?;

    if files.is_empty() {
        emit_import_progress(
            events,
            kb_id,
            source_id,
            100,
            "No files found matching criteria",
        );
        return Ok(0);
    }

    let total = files.len();
    let repo = KbRepository::new(pool.clone());
    let kb = repo.get_kb(kb_id).await.map_err(|e| e.to_string())?;
    let emb_model = kb.embedding_model.clone();

    let mut processed = 0usize;
    let mut skipped = 0usize;

    for (i, file_path) in files.iter().enumerate() {
        let filename = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("file_{}", i));

        let pct = 10 + ((i as f64 / total as f64) * 80.0) as u8;
        emit_import_progress(
            events,
            kb_id,
            source_id,
            pct,
            &format!("Processing {}/{}: {}", i + 1, total, filename),
        );

        // Read file
        let content = match std::fs::read(file_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read file {}: {}", filename, e);
                skipped += 1;
                continue;
            }
        };

        // Hash
        let hash = sha2::Sha256::digest(&content);
        let hash_hex = hex::encode(hash);

        // Check duplicate
        if let Ok(Some(_)) = repo.find_document_by_hash(kb_id, &hash_hex).await {
            skipped += 1;
            continue;
        }

        let file_type = parser::get_file_type(&filename);
        let file_size = content.len() as i64;

        // Create document record with source info
        let rel_path = file_path
            .strip_prefix(dir)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        let doc = match repo
            .create_document_with_source(
                kb_id,
                &filename,
                Some(&file_path.to_string_lossy()),
                &file_type,
                file_size,
                &hash_hex,
                source_type,
                source_url,
                Some(&rel_path),
            )
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to create document record for {}: {}", filename, e);
                skipped += 1;
                continue;
            }
        };

        // Process document
        if let Err(e) = processor::process_document(
            pool,
            events,
            kb_id,
            &doc.id,
            &filename,
            &content,
            emb_model.as_deref(),
            settings,
            data_dir,
        )
        .await
        {
            tracing::warn!("Failed to process document {}: {}", filename, e);
            skipped += 1;
        } else {
            processed += 1;
        }
    }

    // Update KB counts
    repo.update_kb_counts(kb_id).await.ok();

    emit_import_progress(
        events,
        kb_id,
        source_id,
        100,
        &format!("Done: {} processed, {} skipped", processed, skipped),
    );
    Ok(processed)
}

/// Recursively scan directory, applying filters
fn scan_directory(
    dir: &PathBuf,
    excluded_dirs: &[String],
    included_files: &[String],
    max_file_size: usize,
) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();

    // Default excluded dirs
    let default_excluded = vec![
        ".git",
        ".svn",
        ".hg",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        "env",
        ".env",
        "dist",
        "build",
        "target",
        ".next",
        ".nuxt",
        ".output",
        "vendor",
        "vendor",
        ".idea",
        ".vscode",
    ];

    let mut all_excluded: Vec<&str> = default_excluded.iter().copied().collect();
    for d in excluded_dirs {
        all_excluded.push(d.as_str());
    }

    scan_directory_recursive(
        dir,
        &all_excluded,
        included_files,
        max_file_size,
        &mut files,
    )?;

    files.sort();
    Ok(files)
}

fn scan_directory_recursive(
    dir: &PathBuf,
    excluded_dirs: &[&str],
    included_files: &[String],
    max_file_size: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Failed to read dir: {}", e))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() {
            // Check if excluded
            if excluded_dirs.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            scan_directory_recursive(&path, excluded_dirs, included_files, max_file_size, files)?;
        } else if path.is_file() {
            // Check file size
            if let Ok(meta) = entry.metadata() {
                if meta.len() as usize > max_file_size {
                    continue;
                }
            }

            // Check extension
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Supported extensions
            let supported = is_supported_extension(&ext);

            // If included_files is specified, only include matching files
            let included = if included_files.is_empty() {
                true
            } else {
                included_files.iter().any(|f| {
                    let f_lower = f.to_lowercase();
                    name.to_lowercase().contains(&f_lower) || f_lower == ext
                })
            };

            if supported && included {
                files.push(path);
            }
        }
    }

    Ok(())
}

fn is_supported_extension(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown"
            | "txt"
            | "rst"
            | "log"
            | "rs"
            | "go"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "cs"
            | "php"
            | "swift"
            | "kt"
            | "rb"
            | "scala"
            | "clj"
            | "sh"
            | "bash"
            | "vue"
            | "svelte"
            | "sql"
            | "proto"
            | "gradle"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "html"
            | "csv"
            | "env"
            | "ini"
            | "conf"
            | "cfg"
            | "svg"
            | "pdf"
    )
}

fn emit_import_progress(
    events: &EventSink,
    kb_id: &str,
    source_id: &str,
    progress: u8,
    detail: &str,
) {
    events.emit(
        "kb-import-progress",
        serde_json::json!({
            "kb_id": kb_id,
            "source_id": source_id,
            "progress": progress,
            "detail": detail,
        }),
    );
}

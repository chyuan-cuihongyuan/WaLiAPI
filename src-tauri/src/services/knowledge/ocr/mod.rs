//! 知识库 VLM OCR（方案A）：扫描版 PDF 识别子流水线。
//!
//! 入口 `ocr_pdf()`：pdfium 逐页渲染 → 页级缓存 → 并发调 VLM（渠道 failover）
//! → 拼接为带页码锚点的 Markdown。OCR 只产出"文本"，split 之后的流水线无感知。
//!
//! 全局总开关 `ocr.enabled`（默认关）在 processor 中判定：关闭时完全不进入本模块，
//! 不做扫描判定、不调 LLM，行为与历史版本一致。

pub mod cache;
pub mod prompt;
pub mod render;
pub mod vlm;

use std::path::Path;

use futures_util::stream::{self, StreamExt};
use sqlx::SqlitePool;

use crate::db::repository::Repository;
use crate::server::event_bridge::EventSink;
use crate::settings_store::SettingsStore;

/// 每页平均字符数低于该阈值即判定为扫描版（全局常量，后续可在设置中暴露）
const MIN_CHARS_PER_PAGE: usize = 50;

/// 扫描版判定：无法获知页数（page_count=0）时不判定为扫描版。
pub fn is_scanned_pdf(extracted_text: &str, page_count: usize) -> bool {
    if page_count == 0 {
        return false;
    }
    extracted_text.chars().count() / page_count < MIN_CHARS_PER_PAGE
}

/// OCR 错误码。Display 文本以 `OCR_*` 错误码开头，直接作为文档失败原因落库，
/// 前端无需新增错误处理框架。
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("OCR_MODEL_NOT_CONFIGURED: 该 PDF 为扫描版，请先在知识库设置中配置 OCR 视觉模型")]
    ModelNotConfigured,
    #[error("OCR_NO_VISION_CHANNEL: 没有渠道声明支持模型 \"{0}\"。请在渠道管理中为某个渠道添加该视觉模型")]
    NoVisionChannel(String),
    #[error("OCR_PAGE_LIMIT_EXCEEDED: 文档共 {pages} 页，超过上限 {max} 页，可在设置中调整")]
    PageLimitExceeded { pages: usize, max: usize },
    #[error("OCR_RENDER_FAILED: {0}")]
    RenderFailed(String),
    #[error("OCR_TOO_MANY_FAILURES: 共 {total} 页中 {failed} 页识别失败（失败页: {pages}），请检查渠道配额后重建索引")]
    TooManyFailures {
        total: usize,
        failed: usize,
        pages: String,
    },
    #[error("OCR_PAGE_FAILED: 第 {page} 页识别失败: {reason}")]
    PageFailed { page: usize, reason: String },
}

/// OCR 子流水线产物。
pub struct OcrOutcome {
    /// 全文 Markdown（每页前注入 <!-- page: N --> 锚点）
    pub markdown: String,
    /// 每页原始 Markdown（不含锚点），供按页分块；失败页为占位文本
    pub pages: Vec<String>,
    pub page_count: usize,
    pub failed_pages: Vec<usize>,
    pub total_tokens: u64,
}

/// 识别整份 PDF。model 为知识库级 ocr_model（调用方已校验非空）。
#[allow(clippy::too_many_arguments)]
pub async fn ocr_pdf(
    pool: &SqlitePool,
    events: &EventSink,
    doc_id: &str,
    kb_id: &str,
    filename: &str,
    pdf: &[u8],
    model: &str,
    settings: &SettingsStore,
    data_dir: &Path,
    content_hash: &str,
) -> Result<OcrOutcome, OcrError> {
    let max_pages = settings.get_u64("ocr.max_pages", 200) as usize;
    let concurrency = (settings.get_u64("ocr.concurrency", 2) as usize).clamp(1, 4);
    let dpi = settings.get_u64("ocr.dpi", 200) as u32;

    // 1. 打开文档拿页数（同时校验 pdfium 可用、PDF 未加密损坏）
    let page_count = {
        let renderer = render::lock_renderer(data_dir).await?;
        renderer.page_count(pdf)?
    };
    if page_count > max_pages {
        return Err(OcrError::PageLimitExceeded {
            pages: page_count,
            max: max_pages,
        });
    }

    let cache = cache::PageCache::open(data_dir, content_hash, model, dpi);
    let repo = Repository::new(pool.clone());
    let client = vlm::VlmOcrClient::new(&repo, model);

    // 2. 并发识别（buffer_unordered 即并发上限）；渲染经全局 Mutex 串行化
    let mut page_stream = stream::iter(1..=page_count)
        .map(|page_no| {
            let cache = &cache;
            let client = &client;
            async move {
                // 页级缓存命中：跳过渲染与 VLM 调用（重建索引秒级完成的关键）
                if let Some(markdown) = cache.get(page_no) {
                    return (page_no, Ok((markdown, 0u64)));
                }

                let jpeg_result: Result<Vec<u8>, OcrError> = async {
                    let renderer = render::lock_renderer(data_dir).await?;
                    renderer.render_page_jpeg(pdf, page_no, dpi)
                }
                .await;
                let jpeg = match jpeg_result {
                    Ok(j) => j,
                    Err(e) => return (page_no, Err(e)),
                };

                // 失败重试 1 次（ocr_page 内部已做跨渠道 failover）
                let mut attempt = client.ocr_page(&jpeg, page_no).await;
                if attempt.is_err() {
                    tracing::warn!("OCR 第 {} 页首次识别失败，重试一次", page_no);
                    attempt = client.ocr_page(&jpeg, page_no).await;
                }

                match attempt {
                    Ok(result) => {
                        cache.put(page_no, &result.markdown);
                        (page_no, Ok((result.markdown, result.total_tokens)))
                    }
                    Err(e) => (page_no, Err(e)),
                }
            }
        })
        .buffer_unordered(concurrency);

    let mut page_texts: Vec<Option<String>> = vec![None; page_count];
    let mut failed_pages: Vec<usize> = Vec::new();
    let mut total_tokens: u64 = 0;
    let mut done_count = 0usize;

    while let Some((page_no, result)) = page_stream.next().await {
        done_count += 1;
        match result {
            Ok((markdown, tokens)) => {
                page_texts[page_no - 1] = Some(markdown);
                total_tokens += tokens;
            }
            Err(e) => {
                tracing::warn!("OCR 第 {} 页最终失败: {}", page_no, e);
                failed_pages.push(page_no);
            }
        }
        // 3. 进度事件：stage="ocr"，映射到整体进度的 5%–15%（插在 parsing 与 splitting 之间）
        let pct = 5 + ((done_count as f64 / page_count as f64) * 10.0) as u8;
        super::processor::emit_progress(
            events,
            doc_id,
            kb_id,
            filename,
            "ocr",
            pct,
            &format!("OCR 识别 第 {}/{} 页", done_count, page_count),
        );
    }

    // 4. 失败率熔断：过半页面失败则整个文档置 failed
    if failed_pages.len() > page_count / 2 {
        return Err(OcrError::TooManyFailures {
            total: page_count,
            failed: failed_pages.len(),
            pages: failed_pages
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    // 5. 失败页写占位文本，拼接全文（每页前注入页码锚点）
    let pages: Vec<String> = page_texts
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            text.unwrap_or_else(|| format!("[第 {} 页识别失败，请检查后重新索引]", i + 1))
        })
        .collect();
    let markdown = join_pages(&pages);

    Ok(OcrOutcome {
        markdown,
        pages,
        page_count,
        failed_pages,
        total_tokens,
    })
}

/// 拼接全文：每页前注入 <!-- page: N --> 锚点。
fn join_pages(pages: &[String]) -> String {
    let mut out = String::new();
    for (i, text) in pages.iter().enumerate() {
        out.push_str(&format!("<!-- page: {} -->\n\n{}\n\n", i + 1, text));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_scanned_pdf_threshold_boundaries() {
        // 无法获知页数 → 不判定为扫描版
        assert!(!is_scanned_pdf("", 0));
        // 每页恰好 50 字符 → 非扫描版（阈值是严格小于）
        let text = "a".repeat(100);
        assert!(!is_scanned_pdf(&text, 2));
        // 每页 49 字符 → 扫描版
        let text = "a".repeat(98);
        assert!(is_scanned_pdf(&text, 2));
        // 空文本多页 → 扫描版
        assert!(is_scanned_pdf("", 10));
        // 文字版 PDF：大量文本 → 非扫描版
        let text = "字".repeat(10_000);
        assert!(!is_scanned_pdf(&text, 10));
    }

    #[test]
    fn join_pages_injects_page_anchors() {
        let pages = vec!["第一页内容".to_string(), "第二页内容".to_string()];
        let full = join_pages(&pages);
        assert_eq!(
            full,
            "<!-- page: 1 -->\n\n第一页内容\n\n<!-- page: 2 -->\n\n第二页内容"
        );
    }

    #[test]
    fn join_pages_empty() {
        assert_eq!(join_pages(&[]), "");
    }
}

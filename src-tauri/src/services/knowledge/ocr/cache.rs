//! 页级 OCR 结果缓存。
//!
//! 目录结构：`{data_dir}/ocr_cache/{content_hash}/meta.json + page-NNNN.md`
//! 命中条件：meta.json 中 model + dpi + prompt_version 全部一致且对应 page 文件存在。
//! OCR 慢且产生 API 费用，重建索引时命中缓存可从分钟级降到秒级。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::prompt::PROMPT_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CacheMeta {
    model: String,
    dpi: u32,
    prompt_version: u32,
    created_at: String,
}

/// 某文档（按内容哈希）的页级缓存句柄。
pub struct PageCache {
    dir: PathBuf,
    meta: CacheMeta,
    /// 已有 meta 是否与本次识别参数一致；不一致时全部按未命中处理。
    meta_matches: bool,
}

impl PageCache {
    pub fn open(data_dir: &Path, content_hash: &str, model: &str, dpi: u32) -> Self {
        let dir = cache_dir(data_dir, content_hash);
        let meta = CacheMeta {
            model: model.to_string(),
            dpi,
            prompt_version: PROMPT_VERSION,
            created_at: crate::db::models::now_iso(),
        };
        let meta_matches = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<CacheMeta>(&s).ok())
            .map(|m| {
                m.model == meta.model
                    && m.dpi == meta.dpi
                    && m.prompt_version == meta.prompt_version
            })
            .unwrap_or(false);
        Self {
            dir,
            meta,
            meta_matches,
        }
    }

    fn page_path(&self, page_no: usize) -> PathBuf {
        self.dir.join(format!("page-{:04}.md", page_no))
    }

    /// 命中返回缓存的 Markdown，未命中返回 None。
    pub fn get(&self, page_no: usize) -> Option<String> {
        if !self.meta_matches {
            return None;
        }
        std::fs::read_to_string(self.page_path(page_no)).ok()
    }

    /// 写入单页结果并刷新 meta.json（每页落盘，崩溃后已识别页仍可复用）。
    pub fn put(&self, page_no: usize, markdown: &str) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        if std::fs::write(self.page_path(page_no), markdown).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.meta) {
            std::fs::write(self.dir.join("meta.json"), json).ok();
        }
    }
}

/// 文档缓存目录（供删除文档时级联清理）。
pub fn cache_dir(data_dir: &Path, content_hash: &str) -> PathBuf {
    data_dir.join("ocr_cache").join(content_hash)
}

/// 删除某文档的全部缓存页。
pub fn remove_cache(data_dir: &Path, content_hash: &str) {
    std::fs::remove_dir_all(cache_dir(data_dir, content_hash)).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "waliapi_ocr_cache_test_{}_{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_hit_after_put() {
        let dir = temp_dir("hit");
        let cache = PageCache::open(&dir, "hash1", "qwen-vl-max", 200);
        assert!(cache.get(1).is_none());

        cache.put(1, "# 第一页");
        // 重新 open，模拟重建索引时读缓存
        let cache2 = PageCache::open(&dir, "hash1", "qwen-vl-max", 200);
        assert_eq!(cache2.get(1).as_deref(), Some("# 第一页"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_invalidated_by_model_dpi_prompt_version() {
        let dir = temp_dir("invalidate");
        let cache = PageCache::open(&dir, "hash1", "qwen-vl-max", 200);
        cache.put(1, "内容");

        // 模型变化 → 未命中
        let c = PageCache::open(&dir, "hash1", "glm-4v-flash", 200);
        assert!(c.get(1).is_none());
        // dpi 变化 → 未命中
        let c = PageCache::open(&dir, "hash1", "qwen-vl-max", 300);
        assert!(c.get(1).is_none());

        // prompt_version 变化 → 未命中（直接改 meta.json 模拟旧版本缓存）
        let meta_path = cache_dir(&dir, "hash1").join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta["prompt_version"] = serde_json::json!(PROMPT_VERSION + 1);
        std::fs::write(&meta_path, meta.to_string()).unwrap();
        let c = PageCache::open(&dir, "hash1", "qwen-vl-max", 200);
        assert!(c.get(1).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_miss_when_page_file_absent() {
        let dir = temp_dir("absent");
        let cache = PageCache::open(&dir, "hash1", "m", 200);
        cache.put(1, "第一页");
        // meta 匹配但第 2 页文件不存在 → 未命中
        let cache2 = PageCache::open(&dir, "hash1", "m", 200);
        assert!(cache2.get(2).is_none());
        assert_eq!(cache2.get(1).as_deref(), Some("第一页"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_cache_deletes_dir() {
        let dir = temp_dir("remove");
        let cache = PageCache::open(&dir, "hash1", "m", 200);
        cache.put(1, "x");
        assert!(cache_dir(&dir, "hash1").exists());
        remove_cache(&dir, "hash1");
        assert!(!cache_dir(&dir, "hash1").exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}

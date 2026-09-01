//! pdfium 渲染封装：把 PDF 页渲染为 JPEG（内存中完成，不落盘）。
//!
//! pdfium 动态库（pdfium.dll / libpdfium.dylib / libpdfium.so）运行时加载，
//! 取自 bblanchon/pdfium-binaries，打包时放入 tauri.conf.json 的 bundle.resources。
//! pdfium 非线程安全：所有渲染调用都经全局 tokio::sync::Mutex 串行化
//! （渲染很快，不是瓶颈；瓶颈在 VLM 网络调用）。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pdfium_render::prelude::*;

use super::OcrError;

/// pdfium 渲染器（进程内单例，见 lock_renderer）。
pub struct PdfRenderer {
    pdfium: Pdfium,
}

// 安全性：Pdfium 绑定对象不满足 Send（trait object 无 Send 约束），但 PdfRenderer
// 只允许通过全局 Mutex 串行访问，任一时刻只有一个线程持有它，跨线程移交是安全的。
unsafe impl Send for PdfRenderer {}

impl PdfRenderer {
    /// 按候选路径加载 pdfium 动态库，全部失败时返回 OCR_RENDER_FAILED 并提示配置方式。
    pub fn new(data_dir: &Path) -> Result<Self, OcrError> {
        Self::bind_first(&library_candidates(data_dir))
    }

    /// 依次尝试候选路径；全部不存在时报「未找到库」并列出已搜索路径与配置方式。
    fn bind_first(candidates: &[PathBuf]) -> Result<Self, OcrError> {
        let lib_path = candidates.iter().find(|p| p.is_file()).ok_or_else(|| {
            let searched = candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            OcrError::RenderFailed(format!(
                "未找到 pdfium 动态库。已搜索: {}。\
                 请将 {} 放入数据目录的 pdfium/ 子目录，或设置环境变量 WALIAPI_PDFIUM_PATH 指向库文件（或其所在目录）",
                searched,
                platform_library_name()
            ))
        })?;
        let bindings = Pdfium::bind_to_library(&lib_path).map_err(|e| {
            OcrError::RenderFailed(format!(
                "加载 pdfium 动态库失败({}): {}",
                lib_path.display(),
                e
            ))
        })?;
        Ok(Self {
            pdfium: Pdfium::new(bindings),
        })
    }

    /// 打开文档并返回页数；加密/损坏 PDF 在此报 OCR_RENDER_FAILED。
    pub fn page_count(&self, pdf: &[u8]) -> Result<usize, OcrError> {
        let document = self
            .pdfium
            .load_pdf_from_byte_slice(pdf, None)
            .map_err(|e| {
                OcrError::RenderFailed(format!("PDF 渲染失败：文件可能加密或损坏（{}）", e))
            })?;
        Ok(document.pages().len() as usize)
    }

    /// 逐页提取文字层文本（页级扫描判定 + 混合模式下文字页的内容来源）。
    /// 纯图片页得到近空字符串。
    pub fn extract_pages_text(&self, pdf: &[u8]) -> Result<Vec<String>, OcrError> {
        let document = self
            .pdfium
            .load_pdf_from_byte_slice(pdf, None)
            .map_err(|e| {
                OcrError::RenderFailed(format!("PDF 渲染失败：文件可能加密或损坏（{}）", e))
            })?;
        let mut out = Vec::with_capacity(document.pages().len() as usize);
        for page in document.pages().iter() {
            let text = page
                .text()
                .map_err(|e| OcrError::RenderFailed(format!("提取页面文字层失败: {}", e)))?
                .all();
            out.push(text);
        }
        Ok(out)
    }

    /// 渲染单页（1 起）为 JPEG 字节流。200 DPI、quality=80，兼顾清晰度与 token 成本。
    pub fn render_page_jpeg(
        &self,
        pdf: &[u8],
        page_no: usize,
        dpi: u32,
    ) -> Result<Vec<u8>, OcrError> {
        let document = self
            .pdfium
            .load_pdf_from_byte_slice(pdf, None)
            .map_err(|e| {
                OcrError::RenderFailed(format!("PDF 渲染失败：文件可能加密或损坏（{}）", e))
            })?;
        let page = document
            .pages()
            .get((page_no.saturating_sub(1)) as u16)
            .map_err(|e| OcrError::RenderFailed(format!("读取第 {} 页失败: {}", page_no, e)))?;

        // pdfium 页面尺寸单位为 point（1/72 英寸），dpi/72 即缩放系数
        let render_config = PdfRenderConfig::new().scale_page_by_factor(dpi as f32 / 72.0);
        let bitmap = page
            .render_with_config(&render_config)
            .map_err(|e| OcrError::RenderFailed(format!("渲染第 {} 页失败: {}", page_no, e)))?;

        // pdfium 位图为 BGRA，as_image() 已转为 RGBA；JPEG 不支持 alpha，先转 RGB
        let rgb = bitmap.as_image().into_rgb8();
        let mut buf: Vec<u8> = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80);
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| OcrError::RenderFailed(format!("JPEG 编码失败: {}", e)))?;
        Ok(buf)
    }
}

/// 当前平台的 pdfium 动态库文件名。
fn platform_library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// 动态库候选路径，按优先级排列：
/// 1. 环境变量 WALIAPI_PDFIUM_PATH（可指向库文件本体或其所在目录）
/// 2. 可执行文件同目录 pdfium/ 子目录（Windows/Linux 下 Tauri resources 的落点）
/// 3. 可执行文件同目录 Resources/pdfium/（tauri dev 物化 bundle.resources 的位置）
/// 4. macOS .app：Contents/Resources/pdfium/ 以及 glob 保留前缀后的
///    Contents/Resources/resources/pdfium/（0.2.5/0.2.6 安装包实测落点）
/// 5. Linux deb/rpm/AppImage 的 <prefix>/lib/<binary>/pdfium/（tauri-bundler resources 落点）
/// 6. 数据目录 pdfium/ 子目录（Docker/手动部署）
fn library_candidates(data_dir: &Path) -> Vec<PathBuf> {
    let env_path = std::env::var_os("WALIAPI_PDFIUM_PATH").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    library_candidates_from(env_path.as_deref(), exe.as_deref(), data_dir)
}

fn library_candidates_from(
    env_path: Option<&Path>,
    exe: Option<&Path>,
    data_dir: &Path,
) -> Vec<PathBuf> {
    let lib_name = platform_library_name();
    let mut candidates = Vec::new();

    if let Some(custom) = env_path {
        if custom.is_dir() {
            candidates.push(custom.join(lib_name));
        } else {
            candidates.push(custom.to_path_buf());
        }
    }

    if let Some(exe) = exe {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("pdfium").join(lib_name));
            // glob `resources/pdfium/*` 会保留 resources/ 前缀（Windows/Linux 安装包）
            candidates.push(exe_dir.join("resources").join("pdfium").join(lib_name));
            // tauri dev 会把 bundle.resources 物化到 target/<profile>/Resources/
            candidates.push(exe_dir.join("Resources").join("pdfium").join(lib_name));
            candidates.push(
                exe_dir
                    .join("Resources")
                    .join("resources")
                    .join("pdfium")
                    .join(lib_name),
            );
            if let Some(contents) = exe_dir.parent() {
                candidates.push(contents.join("Resources").join("pdfium").join(lib_name));
                // macOS .app：Contents/MacOS/.. → Contents/Resources/resources/pdfium/
                candidates.push(
                    contents
                        .join("Resources")
                        .join("resources")
                        .join("pdfium")
                        .join(lib_name),
                );
            }
            // Linux deb/rpm/AppImage：resources 装在 <prefix>/lib/<binary>/（二进制在 <prefix>/bin/）
            if cfg!(target_os = "linux") {
                if let (Some(prefix), Some(stem)) = (exe_dir.parent(), exe.file_stem()) {
                    let bundled = prefix.join("lib").join(stem);
                    candidates.push(bundled.join("pdfium").join(lib_name));
                    candidates.push(bundled.join("resources").join("pdfium").join(lib_name));
                }
            }
        }
    }

    candidates.push(data_dir.join("pdfium").join(lib_name));
    candidates
}

type SharedRenderer = tokio::sync::Mutex<Option<PdfRenderer>>;

static RENDERER: OnceLock<SharedRenderer> = OnceLock::new();

/// 全局渲染器锁。pdfium 非线程安全，所有渲染必须在持锁期间完成。
/// 锁守卫通过 Deref 直接暴露 &PdfRenderer。
pub struct RendererGuard {
    guard: tokio::sync::MutexGuard<'static, Option<PdfRenderer>>,
}

impl std::ops::Deref for RendererGuard {
    type Target = PdfRenderer;

    fn deref(&self) -> &PdfRenderer {
        // 仅在 lock_renderer 成功构造后存在
        self.guard.as_ref().expect("pdfium renderer initialized")
    }
}

/// 获取全局渲染器（首次调用时按候选路径加载动态库；加载失败不缓存，下次重试）。
pub async fn lock_renderer(data_dir: &Path) -> Result<RendererGuard, OcrError> {
    let mutex = RENDERER.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = mutex.lock().await;
    if guard.is_none() {
        *guard = Some(PdfRenderer::new(data_dir)?);
    }
    Ok(RendererGuard { guard })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_candidates_cover_env_dir_and_data_dir() {
        let dir = PathBuf::from("/tmp/waliapi_test_data");
        let candidates = library_candidates(&dir);
        // 数据目录候选必定存在
        assert!(candidates
            .iter()
            .any(|p| p.ends_with(Path::new("pdfium").join(platform_library_name()))));
    }

    #[test]
    fn macos_app_bundle_searches_glob_prefixed_and_legacy_paths() {
        // 与 0.2.5/0.2.6 安装包、OCR_RENDER_FAILED 弹窗中的搜索列表对齐
        let exe = Path::new("/Applications/WaLiAPI.app/Contents/MacOS/waliapi");
        let data_dir = Path::new("/Users/nelson/Library/Application Support/waliapi.xiaofuge.cn");
        let candidates = library_candidates_from(None, Some(exe), data_dir);
        let lib = platform_library_name();
        let required = [
            Path::new("/Applications/WaLiAPI.app/Contents/MacOS/pdfium").join(lib),
            Path::new("/Applications/WaLiAPI.app/Contents/MacOS/Resources/pdfium").join(lib),
            Path::new("/Applications/WaLiAPI.app/Contents/Resources/pdfium").join(lib),
            Path::new("/Applications/WaLiAPI.app/Contents/Resources/resources/pdfium").join(lib),
            data_dir.join("pdfium").join(lib),
        ];
        for path in &required {
            assert!(
                candidates.iter().any(|p| p == path),
                "缺少候选 {path:?}，实际: {candidates:?}"
            );
        }
    }

    #[test]
    fn installed_app_pdfium_is_first_existing_candidate_when_present() {
        let exe = Path::new("/Applications/WaLiAPI.app/Contents/MacOS/waliapi");
        let data_dir = Path::new("/tmp/waliapi_test_data");
        let candidates = library_candidates_from(None, Some(exe), data_dir);
        let actual = PathBuf::from("/Applications/WaLiAPI.app/Contents/Resources/resources/pdfium")
            .join(platform_library_name());
        if !actual.is_file() {
            return;
        }
        let first_existing = candidates.iter().find(|p| p.is_file());
        assert_eq!(
            first_existing,
            Some(&actual),
            "安装包内真实 dylib 必须是第一个存在的候选，避免误绑其它文件"
        );
        PdfRenderer::bind_first(std::slice::from_ref(&actual))
            .unwrap_or_else(|e| panic!("安装包内 pdfium 应能加载: {e}"));
    }

    #[test]
    fn missing_library_reports_render_failed() {
        // 用肯定不存在的候选路径，确定性覆盖「未找到库」分支——
        // 开发机上 resources/pdfium 已放置真实库（fetch-pdfium.sh）时也不受影响
        let bogus = PathBuf::from("/nonexistent-waliapi-dir/pdfium").join(platform_library_name());
        match PdfRenderer::bind_first(&[bogus]) {
            Err(OcrError::RenderFailed(msg)) => {
                assert!(msg.contains("WALIAPI_PDFIUM_PATH"));
            }
            Err(e) => panic!("unexpected error variant: {}", e),
            Ok(_) => panic!("expected failure when pdfium library is absent"),
        }
    }
}

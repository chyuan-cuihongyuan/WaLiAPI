fn main() {
    // tauri-build 默认会把 Common-Controls v6 清单打进 resource.lib 并只随
    // rustc-link-arg-bins 传给应用 bin。这里显式关闭它，改由下方统一注入同一份清单
    // （内容相同），避免 bin 同时拿到两份 RT_MANIFEST ID 1 资源导致链接失败。
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest()),
    )
    .expect("failed to run tauri-build");

    // Windows 所有产物（应用 bin、测试、示例等）都需要 Common-Controls v6 清单。
    //
    // 依赖图中的 rfd/muda/tao 引用 TaskDialogIndirect、SetWindowSubclass 等
    // comctl32 v6 独有导出；exe 若无 manifest，加载器会把 comctl32 解析到
    // v5.82（System32 副本），进程启动即 STATUS_ENTRYPOINT_NOT_FOUND。
    // tauri-build 默认只覆盖 bin；cargo 的 rustc-link-arg-tests 又不覆盖 lib
    // 单元测试产物（rust-lang/cargo#10937），因此这里用全局 link-arg 统一嵌入。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let rc = manifest_dir.join("tests-manifest.rc");
        let manifest = manifest_dir.join("tests-common-controls.manifest");
        println!("cargo:rerun-if-changed={}", rc.display());
        println!("cargo:rerun-if-changed={}", manifest.display());
        if let Err(e) =
            embed_resource::compile_for_everything(&rc, embed_resource::NONE).manifest_required()
        {
            println!("cargo:warning=Windows Common-Controls 清单嵌入失败（Windows 产物可能无法启动）: {e:?}");
        }
    }
}

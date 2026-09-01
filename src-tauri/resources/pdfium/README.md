# pdfium 动态库放置说明

知识库 VLM OCR（扫描版 PDF 识别）依赖 [pdfium](https://pdfium.googlesource.com/pdfium/) 渲染 PDF 页面。
pdfium 以**动态库**方式在运行时加载，不参与编译链接。

## 获取方式

**推荐：运行仓库根目录的 `scripts/fetch-pdfium.sh`**（发布 CI 与 Dockerfile 打包前自动调用）：

```bash
bash scripts/fetch-pdfium.sh            # 探测当前平台，下载到本目录
bash scripts/fetch-pdfium.sh --dev      # 额外复制到 target/debug/pdfium/（tauri dev 用）
```

脚本从 [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases) 拉取预编译库
（BSD-3-Clause，许可兼容，版本在脚本内固定为 `chromium/8021`），并做体积 + 魔数完整性校验。
库文件已加入 `.gitignore`，不入库。

| 平台 | 文件名 |
| --- | --- |
| Windows (x64) | `pdfium.dll` |
| macOS (arm64/x64) | `libpdfium.dylib` |
| Linux (x64) | `libpdfium.so` |

## 桌面端（Tauri 打包）

`tauri.conf.json` 的 `bundle.resources` 已配置 `resources/pdfium/*`，`pnpm tauri build` 打包后
库文件随安装包分发，运行时按以下顺序解析：

1. 环境变量 `WALIAPI_PDFIUM_PATH`（指向库文件本体或其所在目录）
2. 可执行文件同目录 `pdfium/` 子目录
3. macOS `.app` 包的 `Contents/Resources/pdfium/`，以及 glob `resources/pdfium/*` 保留前缀后的 `Contents/Resources/resources/pdfium/`（0.2.5 安装包实测落点）
4. Linux 安装包的 `<prefix>/lib/<binary>/pdfium/`

数据目录**不在**解析列表内：它是文档上传的落盘根（用户可写），从那里加载动态库会把
上传写穿串联成进程内代码执行（安全审计 FIX-02）。

## Headless / Docker 部署

官方 Docker 镜像已内置 `libpdfium.so`（`/usr/local/lib/waliapi/pdfium/`，由 `WALIAPI_PDFIUM_PATH` 指向）。
手工部署 headless 二进制时，把库放到二进制同目录的 `pdfium/` 子目录（如 `/opt/waliapi/pdfium/libpdfium.so`），
或设置环境变量 `WALIAPI_PDFIUM_PATH=/path/to/libpdfium.so`。

## 注意

- 全局设置 `ocr.enabled` 默认关闭；不放置库文件不影响其他功能，仅在开启 OCR 且处理 PDF 时报 `OCR_RENDER_FAILED`。
- 本目录只需保留实际支持平台的库文件；发布 CI 按目标平台分别拉取。

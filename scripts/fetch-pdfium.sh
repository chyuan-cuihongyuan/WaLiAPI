#!/usr/bin/env bash
# 下载 pdfium 动态库到 src-tauri/resources/pdfium/（知识库 VLM OCR 的渲染依赖）。
#
# 二进制不入库：发布流水线（.github/workflows/release-*.yml）与 Dockerfile 在打包前
# 调用本脚本拉取；tauri.conf.json 的 bundle.resources 已配置 resources/pdfium/*，
# tauri build 会自动打进安装包。运行时库解析顺序见 ocr/render.rs。
#
# 用法：
#   scripts/fetch-pdfium.sh [--platform win-x64|mac-arm64|mac-x64|linux-x64] [--dev]
#     --platform  缺省探测当前宿主平台
#     --dev       额外复制到 src-tauri/target/debug/pdfium/（tauri dev 的 exe 同目录，
#                 使本地开发无需手工放置。注意：数据目录 pdfium/ 不再是加载候选，
#                 见 ocr/render.rs 的候选列表说明）
#
# 幂等：目标文件已存在时跳过下载。升级 pdfium 时修改 PDFIUM_RELEASE 并删除旧文件重跑。
set -euo pipefail

PDFIUM_RELEASE="chromium/8021"
BASE_URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESOURCE_DIR="${SCRIPT_DIR}/../src-tauri/resources/pdfium"
DEBUG_DIR="${SCRIPT_DIR}/../src-tauri/target/debug/pdfium"

platform=""
dev=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --platform) platform="${2:?--platform 需要参数}"; shift 2 ;;
    --dev) dev=1; shift ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$platform" ]]; then
  case "$(uname -s)-$(uname -m)" in
    MINGW*|MSYS*|CYGWIN*) platform="win-x64" ;;
    Darwin-arm64)         platform="mac-arm64" ;;
    Darwin-x86_64)        platform="mac-x64" ;;
    Linux-x86_64)         platform="linux-x64" ;;
    *) echo "无法识别的平台: $(uname -s)-$(uname -m)，请用 --platform 显式指定" >&2; exit 2 ;;
  esac
fi

case "$platform" in
  win-x64)   lib="pdfium.dll" ;;
  mac-*)     lib="libpdfium.dylib" ;;
  linux-*)   lib="libpdfium.so" ;;
  *) echo "未知平台: $platform" >&2; exit 2 ;;
esac

mkdir -p "$RESOURCE_DIR"
dest="${RESOURCE_DIR}/${lib}"

if [[ -f "$dest" ]]; then
  echo "已存在，跳过下载: $dest"
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  url="${BASE_URL}/pdfium-${platform}.tgz"
  echo "下载 ${url}"
  curl -fL --connect-timeout 30 --retry 3 -o "${tmp}/pdfium.tgz" "$url"
  tar -xzf "${tmp}/pdfium.tgz" -C "$tmp"

  # 压缩包结构：Windows 在 bin/，macOS/Linux 在 lib/
  src=""
  for cand in "${tmp}/bin/${lib}" "${tmp}/lib/${lib}"; do
    if [[ -f "$cand" ]]; then src="$cand"; break; fi
  done
  if [[ -z "$src" ]]; then
    echo "压缩包内未找到 ${lib}" >&2; exit 1
  fi
  cp "$src" "$dest"
  rm -rf "$tmp"; trap - EXIT
fi

# 基本完整性校验：体积 >1MB 且魔数正确（MZ / Mach-O / ELF）
size=$(stat -c%s "$dest" 2>/dev/null || stat -f%z "$dest")
if [[ "$size" -lt 1048576 ]]; then
  echo "文件过小（${size} 字节），疑似损坏: $dest" >&2; exit 1
fi
magic=$(head -c 4 "$dest" | od -An -tx1 | tr -d ' \n')
case "$platform" in
  win-x64) [[ "$magic" == 4d5a* ]] || { echo "PE 魔数校验失败: $dest" >&2; exit 1; } ;;
  linux-*) [[ "$magic" == 7f454c46 ]] || { echo "ELF 魔数校验失败: $dest" >&2; exit 1; } ;;
  # Mach-O：feedface / cefaedfe / cafebabe（含 fat 二进制）
  mac-*)   case "$magic" in feedface|cefaedfe|cffaedfe|feedfacf|cafebabe) ;; *) echo "Mach-O 魔数校验失败: $dest" >&2; exit 1 ;; esac ;;
esac

echo "OK: $dest ($((size / 1024 / 1024)) MB)"

if [[ "$dev" -eq 1 ]]; then
  mkdir -p "$DEBUG_DIR"
  cp "$dest" "${DEBUG_DIR}/${lib}"
  echo "已复制到 dev 目录: ${DEBUG_DIR}/${lib}"
fi

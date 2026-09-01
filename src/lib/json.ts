/**
 * JSON 安全解析工具：渲染路径上的 JSON.parse 一律走这里，避免非法 JSON 抛异常导致整页白屏。
 */

/**
 * 安全解析 JSON 字符串。
 * - 输入非字符串或解析失败时返回 `fallback`（默认 null），绝不抛异常。
 * - 典型用法：`safeJsonParse(str, str)` —— 非法 JSON 时回退为显示原文。
 */
export function safeJsonParse<T = unknown>(
  text: string | null | undefined,
  fallback: T | null = null,
): T | null {
  if (typeof text !== "string") return fallback;
  try {
    return JSON.parse(text) as T;
  } catch {
    return fallback;
  }
}

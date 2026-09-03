/**
 * 文本展示工具（C11 预重构：收敛 LogsPage 7 处重复的内联 replace 链）。
 *
 * 日志入库时换行/回车/制表符以字面转义序列（`\n` 两个字符）存储，展示与
 * 复制前需还原为真实控制字符，并把 Windows/Mac 行尾统一为 `\n`。
 * 行为与原内联实现逐字等价：先还原转义，再规范行尾。
 */
export function unescapeText(raw: string): string {
  return raw
    .replace(/\\n/g, "\n")
    .replace(/\\r/g, "\r")
    .replace(/\\t/g, "\t")
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n");
}

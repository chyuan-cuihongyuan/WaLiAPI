# WaLiAPI v0.2.7→v0.2.8 全量审计修复 · 交付文档

> 分支：`fix/v0.2.7-full-audit`（基线 d55df56 / v0.2.7，中途合并 upstream v0.2.8 → 2dadfea）
> 日期：2026-09-04 · 按约定：GitHub 推送 / PR / 评论一律未做，待维护者统一确认。

## 0. 总览

三个阶段共 24 个功能提交（不含 merge）：

| 阶段 | 范围 | 提交区间 |
|:---|:---|:---|
| 移植段（v0.2.6 审计修复移植） | FIX-01~07、GAP-01、#23/#55 | a240f05 … f3d8b97 |
| 修复段（v0.2.7 全量审计） | FIX-08~28、GAP-03~10、#34/#57 | b5c7b52 … bf9cf99 |
| 同步段 | upstream v0.2.8 合并（4 提交） | 2dadfea |

**终验结论（摘要）**：全量 `cargo test` 与 `pnpm build` 结果见 §4；Mimosa 独立扫描未获完整结论（hook 多次 `scanner_enobufs`，MCP 工具本会话不可用），**不宣称安全**，建议在 MCP 可用的会话重跑密封扫描。

---

## 1. 修复项五要素

### FIX-01/06 · KB/Wiki/MCP 端点鉴权恢复（fa41556）

- **开始原因**：v0.2.7 审计发现知识库/ Wiki / MCP 服务端点在特定配置下未强制 token 鉴权，违反三凭证域设计。
- **修复方案**：恢复 `WALIAPI_ADMIN_TOKEN` / `WALIAPI_MCP_TOKEN` 强制校验；未配置 token 时端点 fail-closed（401），绝不无鉴权放行；对齐部署文档。
- **执行过程**：router 层统一挂鉴权中间件；补 CORS 作用域（宽松 CORS 仅数据面）。
- **修复结果**：KB/Wiki/MCP 全部端点无 token/错 token 一律 401；MCP token 与管理 token 互不通用。
- **测试结果**：`server::router` 测试（kb_and_wiki_rest_require_admin_token / mcp_endpoint_requires_mcp_token / service_endpoints_fail_closed_without_configured_tokens / cors_only_covers_data_plane_not_service_routes）全绿。

### FIX-02 · KB 上传文件名与 pdfium 路径（aebbcd6）

- **开始原因**：上传文件名直接落盘存在路径穿越面；pdfium 动态库加载路径过宽。
- **修复方案**：文件名服务端生成（随机 id + 白名单扩展）；pdfium 只从打包目录加载。
- **执行过程**：上传链路改造 + 加载路径收紧。
- **修复结果**：客户端可控文件名不再进入文件系统路径；pdfium 无法被诱导加载任意路径库。
- **测试结果**：KB 上传/OCR 相关测试通过（services::knowledge 全绿）。

### FIX-03 · panic=abort 移除 + Wiki 字符边界截断（#55）（a240f05）

- **开始原因**：release profile `panic=abort` 吞掉崩溃现场；Wiki 摄入按**字节**截断会在多字节字符中间 panic（#55 上报）。
- **修复方案**：移除 panic=abort（恢复 unwind + panic hook）；截断改字符边界。
- **执行过程**：Cargo.toml profile 调整 + truncate_utf8 工具。
- **修复结果**：崩溃可捕获日志；CJK/emoji 内容摄入不再 panic。
- **测试结果**：`utils::text` 边界测试、wiki ingest 截断测试通过。

### FIX-04/05 · 前端全局 ErrorBoundary、日志安全解析、Wiki 渲染 sanitize（#23）（21cea16）

- **开始原因**：单组件异常白屏（#23）；日志页对后端字符串直接 HTML 渲染有 XSS 面；Wiki Markdown 渲染未 sanitize。
- **修复方案**：全局 ErrorBoundary；日志渲染统一安全解析；Wiki HTML 输出 sanitize。
- **执行过程**：前端三处改造。
- **修复结果**：异常降级为可恢复 UI；渲染面收敛。
- **测试结果**：`pnpm build`（tsc strict）通过；人工核验渲染路径。

### FIX-07 · KB 导入加固（f3d8b97）

- **开始原因**：KB git/URL 导入可被构造 `git://` 带凭证 scheme、`local_dir` 越界、内网 URL SSRF。
- **修复方案**：git scheme 白名单 + 凭证剥离；local_dir 边界校验；URL 目标私网/环回拒绝。
- **执行过程**：导入链路逐项设防。
- **修复结果**：导入源不再可触达任意本机路径/内网。
- **测试结果**：KB 导入相关单测通过。

### FIX-08/26 · 流式首帧/空闲超时 + Drop 落账运行时守卫（a4c1ac8）

- **开始原因**：流式请求只设连接超时，首帧/帧间可无限挂死；客户端断开的落账 spawn 在 runtime 关停时 panic。
- **修复方案**：`StreamTimeouts`（首帧 60s / 空闲 120s，可经 `stream.first_frame_timeout_secs` / `stream.idle_timeout_secs` 设置，0=禁用）；Drop 落账加 `Handle::try_current` 守卫。
- **执行过程**：接入 facade 流路径（`next_upstream_item` 统一实现）；v0.2.8 合并时与 upstream 固定 300s 常量方案对撞，保留可配置实现并统一到单一 helper。
- **修复结果**：半死连接按可重试失败切换渠道；空闲超时下发结构化错误事件 + 502 落账；退出期不再 panic。
- **测试结果**：endpoint_executor 全绿（含 upstream 新增空闲超时测试，改为 StreamTimeouts 注入短超时）。

### FIX-09 · 流式落账语义（#57）（b47a175）

- **开始原因**：协议终止事件已到但上游不关连接时，流被记成 499 零用量；取消丢弃已产生计费数据（#57）。
- **修复方案**：协议终止即完成（不等 TCP EOF）；每帧同步落账快照，取消行保留真实用量与已产内容（无用量时本地估算兜底）。
- **执行过程**：泵快照 + finalizer 改造。
- **修复结果**：ModelScope 类延迟关连接上游不再误记 499；取消行计费数据完整。
- **测试结果**：stream_completes_on_protocol_terminal_without_upstream_eof / client_cancel_records_pump_usage_snapshot 通过。

### FIX-10/25 · 多 Key 加权选择边界 + 404 故障切换两轨统一（#34 根因）（e430f66）

- **开始原因**：多 Key 加权随机边界错位（#34 的根因）；404 是否切换渠道在两条转发路径语义不一致。
- **修复方案**：加权选择单一实现 + 边界修正；404 语义按协议族两轨统一判定。
- **执行过程**：core 调整 + 两轨对齐。
- **修复结果**：Key 选择分布正确；404 处理不再随路径漂移。
- **测试结果**：core:: 全绿（125 passed）。

### FIX-11/20 · SSE 解码缓冲上限 + 错误事件 serde 构造（41df4a6）

- **开始原因**：9 处 SSE 解码器 pending 缓冲无上限（恶意上游可 OOM）；错误帧手拼 JSON 字符串在内容含引号时产生非法帧。
- **修复方案**：pending 32MB 共享上限（单一来源）；泵累积内容 4MB 上限 + 截断标记；错误帧全部 `format_stream_error`（serde_json 构造）。
- **执行过程**：responses_codec/anthropic 解码器统一接缝。
- **修复结果**：解码内存有界；错误帧永远合法 JSON。
- **测试结果**：endpoint_executor 78→90 全绿（含三协议回归）。

### FIX-12 · 原生 Anthropic 路径落账对齐（7d212f0）

- **开始原因**：原生路径非流式不经渠道总超时；断开/中断/完成落账口径不一。
- **修复方案**：非流式/count_tokens 改 blocking_client；`NativeStreamFinalizer` 统一 200/499/502 落账 + 部分用量/估算兜底。
- **执行过程**：见提交；usage 解析器共享锁供 Drop 路径取部分用量。
- **修复结果**：原生路径任何退出路径都有日志行。
- **测试结果**：anthropic_handler_tests 全绿。

### GAP-08 · 重试策略设置接入 RoutePlan 主路径（5afed07）

- **开始原因**：重试设置此前只作用于 legacy 路径，主路径不读。
- **修复方案**：`retry_budget_from_settings` + `RoutePlan::apply_retry_budget`（组内=retry_times+1、总量=组内×2、关闭→1,1）。
- **执行过程**：handlers 在 is_stream 分发前应用。
- **修复结果**：设置页重试项对主路径生效。
- **测试结果**：真值表测试 + core:: 全绿。

### FIX-13/23 · 网关密钥列表掩码 + 按需取全量（0fc6a0c）

- **开始原因**：API Key 列表/详情接口返回全量明文密钥，任何 XSS/日志泄露面都直达凭证。
- **修复方案**：`utils::secret::mask_secret`（**字符**边界切片，多字节安全，≤8 字符全掩码）单源实现；新增 `get_api_key_full(id)` 按需取全量。
- **执行过程**：DTO 掩码 + repository.get_api_key_by_id + admin_routes 分发（参数顺序修正）；前端三页（ApiKeys 复制 / Usage 查看 / AppConfig 网关密钥）迁移 id-based。
- **修复结果**：列表/详情只见掩码；显式动作才取全量。
- **测试结果**：utils::secret 矩阵测试、commands:: 31 passed、`pnpm build` 通过。

### FIX-17 · 管理面认证加固（b7a72cf）

- **开始原因**：登录无限速（可暴力破解）；Cookie 无 HttpOnly；用户名枚举有时序差；初始密码文件长期滞留；改密不吊销旧会话；过期会话只惰性删除。
- **修复方案**：`LoginThrottle`（用户/全局两级 + 指数退避：5 次免罚、2^n 秒封顶 1h、15min 滑窗衰减，命中 429+Retry-After）；Cookie HttpOnly；哑 argon2 防枚举；首登成功删 `INITIAL_PASSWORD`；改密吊销全部旧会话（当前会话续期）；会话清扫插入路径 1h 门控顺带执行。
- **执行过程**：admin_auth.rs 重构 + admin_routes 接线 + AppState 字段三处构造点。
- **修复结果**：以上六点全部落地；前端只存响应体 token，HttpOnly 不影响。
- **测试结果**：admin_auth 单测 7 例 + 端到端 admin_login_flow_hardening（限速 429、HttpOnly、文件删除、旧会话 401）通过。

### FIX-16 · 响应侧扫描接入全部转发路径（30a4d83）

- **开始原因**：「响应安全扫描」设置只覆盖 legacy 非流式路径，主路径/流式/原生全部漏扫。
- **修复方案**：`security::merge_response_scan` / `scan_response_into` 单一合并实现；`AuditedRequest` 携带设置快照（零签名穿透）；非流式扫响应体、流式扫泵累积文本（取消/中断行同扫）、原生路径 SSE 有界累积器与 finalizer 共享（完成/断开/中断均扫）；openai 转换流与非流式分支同接入；顺带修复极短响应快照缺失（pump.start/finish 后同步快照）。
- **执行过程**：proxy.rs 收敛到共享实现；设置页标注全覆盖与尽力而为语义。
- **修复结果**：三路径响应扫描全接通；扫描异常不影响转发。
- **测试结果**：合并语义 3 例 + 非流式/流式可观察落账各 1 例 + 原生累积器 1 例通过；endpoint_executor 90 passed。

### FIX-21/22/27 · 复用/状态治理（652a4e9）

- **开始原因**：BPE 词表每次估算重建（毫秒级 × 每请求）；HTTP 客户端每请求重建；Wiki 摄入失败任务永久 running；HTTP 摄入实际同步阻塞；Auth 刷新锁映射泄漏；wikilink 图谱 N+1。
- **修复方案**：cl100k OnceLock 单例；流式客户端全局单例 + 阻塞式按超时分桶（≤32 桶）；摄入失败收敛 failed + HTTP 真后台化（202）；锁清理（无等待者剪除）；内存标题索引 + 单事务分块 INSERT。
- **执行过程**：见提交 652a4e9。
- **修复结果**：高流量下构建开销消除；任务/来源状态不再挂死；图谱重建 O(1) 查询。
- **测试结果**：BPE 构建计数、分桶语义、失败收敛断言、图谱标题解析/幽灵链接测试通过；services:: 81 passed。

### FIX-23（余项）· MCP 会话有界化 + FTS 空 token 防御（e1c62b1）

- **开始原因**：MCP SSE 会话消息通道 unbounded（慢客户端无界积压）+ 会话靠 1 小时定时清扫（断连滞留）；FTS 空/符号查询把原文整段当 MATCH 表达式（空串报错、运算符误匹配）。
- **修复方案**：通道 64 容量 + `try_send` 满即丢；`SseSessionGuard` Drop 即清（立即移除）；FTS 无有效 token 直接返回空结果，`build_fts_query` 改收 token 列表保持引号中和。
- **修复结果**：慢客户端内存有界；断连即释放会话；空查询零误报零报错。
- **测试结果**：token 矩阵/运算符中和/端到端空查询测试通过（掩码部分已在 FIX-13 提交覆盖）。

### FIX-18/GAP-07 · README 威胁模型 + #15 视觉诊断提示（bf9cf99）

- **开始原因**：README 无安全边界声明（明文密钥/DLP 边界/CORS 作用域未告知）；#15（视觉请求打到不支持图片的渠道 400）需要可诊断性。
- **修复方案**：README 新增「安全边界与威胁模型」节；原生渠道 400 且请求含图片块 → 错误 message 追加诊断提示（fail-open，幂等）；`supports_vision` 能力路由作为长期方案文档化（README + PRD 票 03）；AGENTS.md 版本对齐 0.2.8、README 迁移数 23→27。
- **执行过程**：见提交 bf9cf99。
- **修复结果**：边界声明成文且与实现一致；#15 场景有可操作提示。
- **测试结果**：vision_hint 测试（识别矩阵/400 限定/非 400 不动/幂等）通过。

### 其余修复段提交（摘要）

| 提交 | 内容 | 测试结论 |
|:---|:---|:---|
| b5c7b52（GAP-01） | web 发布链 bin 名断裂修复、systemd 命令同步 | workflow 静态核对 |
| 7ccf387（GAP-03/04/09） | /health 版本号、RUST_LOG 级别、桌面日志落数据目录 | server 测试 |
| 6ea29de（FIX-19/GAP-05） | 网关密钥会话级存储、渠道导出明示风险 | pnpm build |
| a453d3f（FIX-14/24/NEW-4） | 会话过期跳登录、SSE 断线重连、token 源收敛 | pnpm build |
| 6007ff8（FIX-15/GAP-06/NEW-2） | 日志过滤防抖+陈旧丢弃+自动刷新、KB 竞态守卫 | pnpm build |
| a330162（GAP-10） | 前端重复工具收敛（转义×7、密钥守卫×4） | pnpm build |
| a015ad0（GAP-07/FIX-28/NEW-1） | AGENTS.md 对齐、死依赖移除、产物清理 | pnpm build |
| 1248f00 | request_log 测试字段补齐 | 集成测试 |

---

## 2. Issues 处置结论

| Issue | 结论 | 证据 |
|:---|:---|:---|
| #23（前端异常/XSS 面） | **已修复** | 21cea16；ErrorBoundary + 安全渲染 |
| #34（多 Key 加权选择异常） | **根因已修** | e430f66；边界修正 + 单一实现 |
| #55（崩溃无现场/CJK panic） | **已修复** | a240f05；panic=abort 移除 + 字符边界截断 |
| #57（流式计费数据丢失） | **已修复** | b47a175 + a4c1ac8；终止早退/取消保量/估算兜底 |
| #15（视觉请求 400 无提示） | **部分处理（按计划）** | bf9cf99；诊断提示落地，`supports_vision` 路由为长期方案（文档化），不在本批实现 |

早期贡献批次（#39/#40、PR #44/#45/#46）此前已合入上游 v0.2.3，不在本批范围。

---

## 3. 提交清单映射（编号 ↔ SHA ↔ 测试结论）

| 编号 | SHA | 测试 |
|:---|:---|:---|
| FIX-01/06 | fa41556 | router 鉴权 4 例 ✓ |
| FIX-02 | aebbcd6 | knowledge 套件 ✓ |
| FIX-03/#55 | a240f05 | text 边界 ✓ |
| FIX-04/05/#23 | 21cea16 | pnpm build ✓ |
| FIX-07 | f3d8b97 | 导入测试 ✓ |
| GAP-01 | b5c7b52 | 静态核对 |
| GAP-07/FIX-28/NEW-1 | a015ad0 | pnpm build ✓ |
| GAP-10 | a330162 | pnpm build ✓ |
| FIX-15/GAP-06/NEW-2 | 6007ff8 | pnpm build ✓ |
| FIX-14/24/NEW-4 | a453d3f | pnpm build ✓ |
| FIX-19/GAP-05 | 6ea29de | pnpm build ✓ |
| FIX-09/#57 | b47a175 | 流式落账 2 例 ✓ |
| GAP-03/04/09 | 7ccf387 | server ✓ |
| FIX-10/25/#34 | e430f66 | core 125 ✓ |
| FIX-08/26 | a4c1ac8 | executor ✓ |
| FIX-11/20 | 41df4a6 | executor 78（时点）✓ |
| FIX-12 | 7d212f0 | anthropic ✓ |
| GAP-08 | 5afed07 | core 125 ✓ |
| v0.2.8 合并 | 2dadfea | executor 88 + core 125 + request_log 5 ✓ |
| FIX-13/23 | 0fc6a0c | utils/commands + pnpm build ✓ |
| FIX-17 | b7a72cf | admin_auth 7 例 + e2e ✓ |
| FIX-16 | 30a4d83 | security 26 + executor 90 + server ✓ |
| FIX-21/22/27 | 652a4e9 | services 81 + adaptor 6 + auth 109 ✓ |
| FIX-23 余项 | e1c62b1 | services FTS/MCP ✓ |
| FIX-18/GAP-07 | bf9cf99 | server 52 ✓ |

---

## 4. 终验记录

- **基线**（会话记忆）：729 passed / 2 known FAILED（`codex_login::export_is_nested_private_backed_up_and_preserves_old_file_if_rename_fails`、`rollout_integration_tests::security_gate_block_zero_upstream`）——均为既有基线，非本批回归。
- **全量 cargo test 终验（2026-09-04）**：`--lib` **777 passed / 2 failed**（恰为上述两条基线，**零新增失败**；日志 `.scratch/final-lib-test2.log`）；集成测试全部通过：channel_migration 21 ✓、auth_repository（见日志）✓、kb_ocr 7 ✓、request_log 5 ✓；doc-tests 0。首轮全量曾见 adaptor 分桶测试因全局映射并行污染偶发失败（`.scratch/final-full-test.log`），已改单向断言后复测稳定通过。
- **前端**：`pnpm build`（tsc strict + vite）通过。
- **Mimosa 独立扫描**：**未获完整结论**——commit hook 多次报 `scanner_enobufs`，本会话 MCP 扫描工具不可用。按纪律不宣称安全；建议 MCP 可用会话重跑 deep 扫描并核对 seal。

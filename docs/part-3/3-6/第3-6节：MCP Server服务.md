# 《WaLiAPI - 本地 LLM API 网关》第3-6节：MCP Server服务

- **本章难度**：★★★★★
- **本章重点**：MCP Streamable HTTP + SSE 传输、13个工具定义、instructions注入、mcp service注册、前端KnowledgeBasePage
- **本节分支**：`xfg-3-6-mcp-server`
- **课程视频**：[]()

---

**版权说明**：©本项目与星球签约合作，受[《中华人民共和国著作权法实施条例》](https://www.gov.cn.gongbao/content/2013/content_2339485.htm) 版权法保护，禁止任何理由和任何方式公开(public)源码、资料、视频等小傅哥发布的星球内容到Github、Gitee等各类平台，违反可追究进一步的法律责任。

作者：小傅哥
<br/>博客：[https://bugstack.cn](https://bugstack.cn)

>沉淀、分享、成长，让自己和他人都能有所收获！😄

大家好，我是技术UP主小傅哥。

前面5节完成了知识库的"内部能力"——数据模型、解析分块、向量化索引、混合检索、RAG问答。这一节把这些能力**通过 MCP 协议对外暴露**，让任意 AI Agent（Claude Desktop、Cursor、Copilot 等）能直接调用知识库工具。

## 一、本章诉求

1. 理解 MCP（Model Context Protocol）的设计目标和传输机制
2. 实现 Streamable HTTP + SSE 双传输通道
3. 实现 13 个 MCP 工具定义（搜索/问答/CRUD/导入/索引管理）
4. 实现 instructions 注入（Agent 首次连接时的系统提示）
5. 实现 McpService 注册到 ServiceRegistry
6. 理解前端 KnowledgeBasePage 与 MCP 的交互方式

## 二、MCP 协议概述

### 2.1 MCP 是什么？

MCP（Model Context Protocol）是 Anthropic 在 2024 年推出的开放协议，让 AI 模型通过标准化接口与外部工具交互。核心概念：

```
┌─────────────────────────────────────────────────────┐
│              MCP Architecture                         │
│                                                      │
│  MCP Host (Claude Desktop / Cursor / AI Agent)       │
│       │                                              │
│       │  JSON-RPC 2.0 over HTTP                      │
│       │                                              │
│  MCP Client (内置在 Host 中)                          │
│       │                                              │
│       │  SSE / Streamable HTTP                       │
│       │                                              │
│  MCP Server (WaLiAPI 知识库服务)                      │
│       │                                              │
│       │  13 tools:                                   │
│       │  search / ask / list_kb / create_kb / ...    │
│       │                                              │
│  本地资源 (SQLite + HNSW 索引)                        │
└─────────────────────────────────────────────────────┘
```

### 2.2 MCP 传输方式

MCP 支持两种传输方式：

| 传输方式 | 连接方向 | 适用场景 |
|----------|---------|---------|
| **Streamable HTTP** | Client → Server (POST JSON-RPC) | 所有请求/响应 |
| **SSE** | Server → Client (GET 事件流) | 长连接、推送通知 |

**WaLiAPI 的实现**：同时支持两种传输，通过同一组路由暴露：

```
POST /mcp          → JSON-RPC 请求/响应（Streamable HTTP）
GET  /mcp/sse      → SSE 事件流（Server → Client 推送）
POST /mcp?session_id=xxx → SSE 模式下的 JSON-RPC 请求
```

### 2.3 MCP JSON-RPC 方法

| 方法 | 说明 |
|------|------|
| `initialize` | 首次连接，交换协议版本和能力 |
| `notifications/initialized` | 客户端确认初始化完成 |
| `tools/list` | 获取工具列表 |
| `tools/call` | 调用指定工具 |
| `ping` | 心跳检查 |

## 三、MCP JSON-RPC 实现

### 3.1 数据结构

```rust
#[derive(Debug, Deserialize)]
pub struct McpRequest {
    pub jsonrpc: String,          // "2.0"
    pub id: Option<serde_json::Value>,
    pub method: String,           // "initialize"/"tools/list"/"tools/call"
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct McpResponse {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<McpError>,
}

#[derive(Debug, Serialize)]
pub struct McpError {
    code: i32,
    message: String,
}
```

### 3.2 dispatch_jsonrpc_async

```rust
async fn dispatch_jsonrpc_async(shared: &SharedState, req: &McpRequest) -> McpResponse {
    match req.method.as_str() {
        "initialize" => {
            McpResponse::success(req.id.clone(), serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "WaLiAPI Knowledge Base",
                    "version": "0.1.0"
                },
                "instructions": MCP_INSTRUCTIONS  // ← 注入系统提示
            }))
        }
        "notifications/initialized" => {
            McpResponse::success(req.id.clone(), serde_json::json!({}))
        }
        "tools/list" => {
            McpResponse::success(req.id.clone(), serde_json::json!({
                "tools": get_tools()
            }))
        }
        "tools/call" => {
            let tool_name = req.params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or_default();
            match handle_tool_call(shared, tool_name, &args).await {
                Ok(result) => McpResponse::success(req.id.clone(), result),
                Err(e) => McpResponse::error(req.id.clone(), -32603, e),
            }
        }
        "ping" => {
            McpResponse::success(req.id.clone(), serde_json::json!({}))
        }
        _ => McpResponse::error(req.id.clone(), -32601, format!("Unknown method: {}", req.method))
    }
}
```

## 四、instructions 注入

MCP 规范允许 Server 在 `initialize` 响应中注入 `instructions`，作为 Agent 的系统提示：

```rust
const MCP_INSTRUCTIONS: &str = r#"# WaLiAPI 知识库 — 本地 RAG + 向量检索

知识库已预建索引：文档已解析、分块、向量化并存入本地 SQLite + HNSW 索引。
所有检索都是本地操作，亚秒级响应。

## 工具使用优先级

1. **ask_knowledge_base** — 首选。直接提问，返回 AI 生成的回答 + 来源引用。
   适合：任何问题、概念理解、代码含义、流程梳理。

2. **search_knowledge_base** — 当需要看原始文本片段，或 ask 回答不够时使用。

3. **list_knowledge_bases** — 首次使用时调用一次，获取可用知识库 ID。

4. **其他工具** — 按需使用（上传文档、管理索引等）。

## 反模式

- ❌ 不要先 search 再自己总结 — 直接用 ask_knowledge_base
- ❌ 不要每次都调 list_knowledge_bases — 缓存第一次的结果
- ❌ 不要对同一问题反复 search 不同关键词

## 代码文件

知识库中的代码文件按符号边界分块（函数/类/方法），每个 chunk 是完整符号。
chunk metadata 包含 symbol_name、symbol_kind、signature，可用于精确过滤。"#;
```

**instructions 的意义**：
- 告诉 Agent **如何正确使用工具**（优先级、反模式）
- 告诉 Agent **知识库的特点**（本地操作、亚秒级、代码符号感知）
- 避免 Agent 常见错误行为（反复搜索、不缓存结果）

## 五、13 个 MCP 工具

### 5.1 工具分类

| 类别 | 工具 | 权限 |
|------|------|------|
| 检索 | search_knowledge_base, ask_knowledge_base | 读取 |
| 信息 | list_knowledge_bases, read_document, get_knowledge_base_stats | 读取 |
| 管理 | create_knowledge_base, update_knowledge_base, delete_knowledge_base | 写入 |
| 文档 | upload_document, delete_document, list_documents | 写入 |
| 索引 | build_index | 写入 |
| 导入 | import_source | 写入 |

### 5.2 工具定义示例

**search_knowledge_base**：

```json
{
    "name": "search_knowledge_base",
    "description": "Semantic search across a local knowledge base. Uses HNSW vector index for O(log n) retrieval. Returns matching text chunks with cosine similarity scores (0-1).",
    "inputSchema": {
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Natural language search query" },
            "kb_id": { "type": "string", "description": "Specific KB ID. If omitted, searches all MCP-enabled KBs." },
            "top_k": { "type": "integer", "description": "Max results (default: 5)", "default": 5 }
        },
        "required": ["query"]
    }
}
```

**ask_knowledge_base**：

```json
{
    "name": "ask_knowledge_base",
    "description": "Ask a question and get an AI-generated answer based on retrieved context (RAG). Returns the answer plus source citations.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "question": { "type": "string", "description": "The question to ask" },
            "kb_id": { "type": "string", "description": "KB ID. If omitted, uses all MCP-enabled KBs." },
            "top_k": { "type": "integer", "description": "Number of chunks to retrieve (default: 5)", "default": 5 },
            "model": { "type": "string", "description": "LLM model for answer generation" }
        },
        "required": ["question"]
    }
}
```

**upload_document**：

```json
{
    "name": "upload_document",
    "description": "上传文档到知识库。文档上传后会自动解析、分块、向量化并建立索引。支持格式: .txt .md .pdf .docx .rs .py .js .ts .go .java 等",
    "inputSchema": {
        "type": "object",
        "properties": {
            "kb_id": { "type": "string", "description": "目标知识库 ID" },
            "filename": { "type": "string", "description": "文档文件名（含扩展名）" },
            "content": { "type": "string", "description": "Base64 编码的文件内容" }
        },
        "required": ["filename", "content"]
    }
}
```

### 5.3 handle_tool_call

```rust
async fn handle_tool_call(shared: &SharedState, tool_name: &str, args: &Value) -> Result<Value, String> {
    let pool = &shared.state.inner().db.pool;
    match tool_name {
        "search_knowledge_base" => {
            let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
            let kb_id = args.get("kb_id").and_then(|k| k.as_str()).unwrap_or("");
            let top_k = args.get("top_k").and_then(|k| k.as_u64()).unwrap_or(5) as usize;
            // ... embed query, hybrid_search, format results
        }
        "ask_knowledge_base" => {
            let question = args.get("question").and_then(|q| q.as_str()).unwrap_or("");
            // ... RAG ask, format answer
        }
        "list_knowledge_bases" => {
            let repo = KbRepository::new(pool.clone());
            let kbs = repo.get_all_mcp_enabled_kbs().await?;
            // Format as MCP tool result
        }
        "create_knowledge_base" => {
            // ... create KB
        }
        // ... 其他 9 个工具
        _ => Err(format!("Unknown tool: {}", tool_name)),
    }
}
```

**MCP 工具返回格式**：

```json
{
    "content": [
        {
            "type": "text",
            "text": "搜索结果:\n1. [来源: handler.rs (0.92)]\n   fn handle_stream..."
        }
    ]
}
```

## 六、SSE 传输实现

### 6.1 SSE Session 管理

```rust
type SessionSender = mpsc::UnboundedSender<String>;

fn sse_sessions() -> &'static Arc<RwLock<HashMap<String, SessionSender>>> {
    static SESSIONS: std::sync::OnceLock<Arc<RwLock<HashMap<String, SessionSender>>>> = std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}
```

每个 SSE 客户端获得唯一 `session_id`，POST 请求通过 `session_id` 将响应推送到对应的 SSE 流。

### 6.2 SSE 端点

```rust
pub async fn handle_mcp_sse(State(_shared): State<SharedState>) -> Response {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    sse_sessions().write().await.insert(session_id.clone(), tx);

    let stream = async_stream::stream! {
        // 1. Send endpoint event — tells client where to POST
        let endpoint_url = format!("/mcp?session_id={}", session_id);
        yield Ok(format!("event: endpoint\ndata: {}\n\n", endpoint_url).into_bytes());

        // 2. Keep-alive + forward JSON-RPC responses
        let mut keepalive = tokio::time::interval(Duration::from_secs(15));
        keepalive.tick().await;

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    yield Ok(format!("data: {}\n\n", msg).into_bytes());
                }
                _ = keepalive.tick() => {
                    yield Ok(b": keepalive\n\n".to_vec());
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}
```

### 6.3 POST 端点（SSE 模式）

```rust
pub async fn handle_mcp(
    State(shared): State<SharedState>,
    Query(params): Query<McpQuery>,
    Json(req): Json<McpRequest>,
) -> Response {
    // Streamable HTTP: direct response
    if params.session_id.is_none() {
        let response = dispatch_jsonrpc_async(&shared, &req).await;
        return Json(response).into_response();
    }

    // SSE mode: push response through the SSE stream
    let session_id = params.session_id.unwrap();
    let sessions = sse_sessions();
    let sessions_read = sessions.read().await;
    if let Some(tx) = sessions_read.get(&session_id) {
        let response = dispatch_jsonrpc_async(&shared, &req).await;
        tx.send(response.to_json_string()).ok();
        return (StatusCode::OK, "Accepted").into_response();
    }

    // Session not found — fall back to direct response
    let response = dispatch_jsonrpc_async(&shared, &req).await;
    Json(response).into_response()
}
```

## 七、McpService 注册

```rust
pub struct McpService;

#[async_trait]
impl Service for McpService {
    fn id(&self) -> &'static str { "mcp" }
    fn name(&self) -> &'static str { "MCP Server" }

    fn routes(&self, _state: Arc<AppState>) -> Router<SharedState> {
        Router::new()
            .route("/mcp", post(handle_mcp).get(handle_mcp_sse))
            .route("/mcp/", post(handle_mcp).get(handle_mcp_sse))
            .route("/mcp/sse", get(handle_mcp_sse).post(handle_mcp))
    }
}
```

**路由设计说明**：
- `/mcp` 和 `/mcp/` 两种路径：有些 MCP Client 发送带尾部斜杠的请求
- `/mcp/sse`：旧版 SSE 端点，保持向后兼容
- GET `/mcp` 和 GET `/mcp/sse`：建立 SSE 连接
- POST `/mcp` 和 POST `/mcp/sse`：发送 JSON-RPC 请求

## 八、前端 KnowledgeBasePage

KnowledgeBasePage 是 WaLiAPI 前端最大的页面（2598行），提供完整的知识库管理界面：

### 8.1 页面功能

| 功能 | 说明 |
|------|------|
| 知识库 CRUD | 创建/编辑/删除知识库 |
| 文档管理 | 上传文件、查看文档状态、删除文档 |
| 多源导入 | Git/URL/本地目录导入 |
| 索引管理 | 构建/重建/删除 HNSW 索引 |
| 搜索 | 向量 + FTS5 混合搜索 |
| RAG 问答 | 多轮对话式问答 |
| MCP 配置 | 启用/禁用 MCP 暴露 |
| 对话历史 | 查看/清除对话记录 |

### 8.2 前端 → 后端交互

```
KnowledgeBasePage (React)
     │
     │  invoke("get_knowledge_bases")        ← Tauri 命令
     │  invoke("create_knowledge_base", ...)  ← Tauri 命令
     │  invoke("ask_knowledge_base", ...)     ← Tauri 命令
     │
     │  fetch("/api/kb/search?query=...")     ← HTTP API
     │  fetch("/api/kb/ask", POST)             ← HTTP API
     │
     ▼
WaLiAPI 后端 (Rust)
```

## 九、验证测试

```bash
npm run tauri dev

# 1. MCP initialize（Streamable HTTP）
curl -X POST http://localhost:3456/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
# → protocolVersion + capabilities + instructions

# 2. MCP tools/list
curl -X POST http://localhost:3456/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
# → 13 个工具定义

# 3. MCP search_knowledge_base
curl -X POST http://localhost:3456/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_knowledge_base","arguments":{"query":"handle stream","top_k":3}}}'
# → 搜索结果

# 4. MCP ask_knowledge_base
curl -X POST http://localhost:3456/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ask_knowledge_base","arguments":{"question":"如何处理流式请求？"}}}'
# → RAG 回答 + 来源引用

# 5. SSE 连接
curl -N http://localhost:3456/mcp/sse
# → event: endpoint
# → data: /mcp?session_id=xxx

# 6. SSE 模式下的 JSON-RPC
curl -X POST "http://localhost:3456/mcp?session_id=xxx" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":5,"method":"ping","params":{}}'
# → SSE 流中推送响应

# 7. Claude Desktop 配置
# 在 claude_desktop_config.json 中添加：
{
  "mcpServers": {
    "waliapi": {
      "url": "http://localhost:3456/mcp"
    }
  }
}
```

## 十、面试问题

1. **MCP 的 Streamable HTTP 和 SSE 两种传输方式分别在什么场景下使用？为什么 WaLiAPI 同时支持两种？**

   > 提示：Streamable HTTP（POST /mcp）适合单次请求-响应，简单直接。SSE（GET /mcp/sse）适合长连接场景——客户端保持 SSE 流，Server 可以主动推送通知（如文档处理完成通知）。同时支持两种确保最大兼容性——不同 MCP Client 实现可能偏好不同传输方式。

2. **instructions 注入在 MCP 中的作用是什么？为什么不把 instructions 写在工具描述里？**

   > 提示：instructions 是全局系统提示，在 Agent 首次连接时一次性注入，提供使用策略（优先级、反模式）。工具描述只描述单个工具的功能和参数，无法提供全局策略。分离 instructions 和工具描述让 Agent 既有大局观又有细粒度操作指引。

3. **SSE Session 管理用 `mpsc::UnboundedChannel`，为什么不用 bounded channel？如果 SSE 客户端断连但 Session 没清理会怎样？**

   > 提示：UnboundedChannel 不会因缓冲区满阻塞发送者——适合 SSE 推送场景（如果阻塞发送者会导致 HTTP handler 卡住）。客户端断连后，rx 被 drop，tx.send() 返回 Err，但 tx 仍在 HashMap 中。当前用超时清理（1小时），更好的方案是在 send 失败时立即清理。

4. **MCP 的 13 个工具中，哪些是只读的、哪些是写入的？如果 MCP Client 只需要检索不需要写入，如何限制？**

   > 提示：只读工具（search/list/read/stats）5个，写入工具（create/update/delete/upload/build/import）8个。MCP 规范目前不支持工具级别的权限控制，但可以通过 `mcp_enabled` 开关控制整个知识库是否暴露。未来可以扩展为 `mcp_read_only` 模式。

5. **`/mcp` 和 `/mcp/` 两种路径都注册了相同的 handler，为什么？这解决了什么兼容性问题？**

   > 提示：部分 MCP Client（如某些 Python SDK）发送请求时会自动添加尾部斜杠（`/mcp/`），而有些不添加。Axum 路由匹配是精确的，`/mcp` 不匹配 `/mcp/`。同时注册两种确保所有 Client 都能正常连接。


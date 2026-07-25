# 《WaLiAPI - 本地 LLM API 网关》第3-5节：知识库检索与RAG问答

- **本章难度**：★★★★★
- **本章重点**：retriever（向量+FTS5混合+符号过滤）、rag（多轮对话+Token降级+来源引用）、handlers+routes、knowledge service注册、commands层
- **本节分支**：`xfg-3-5-kb-retriever-rag`
- **课程视频**：[]()

---

**版权说明**：©本项目与星球签约合作，受[《中华人民共和国著作权法实施条例》](https://www.gov.cn/gongbao/content/2013/content_2339485.htm) 版权法保护，禁止任何理由和任何方式公开(public)源码、资料、视频等小傅哥发布的星球内容到Github、Gitee等各类平台，违反可追究进一步的法律责任。

作者：小傅哥
<br/>博客：[https://bugstack.cn](https://bugstack.cn)

>沉淀、分享、成长，让自己和他人都能有所收获！😄

大家好，我是技术UP主小傅哥。

上一节完成了向量化、HNSW索引和FTS5全文检索的基础设施。这一节把它们组合起来——**检索引擎 + RAG问答**。这是知识库的"输出端"，用户提问后，系统如何检索相关内容并生成答案。

## 一、本章诉求

1. 实现 retriever 模块——HNSW向量搜索 + FTS5关键词搜索 + 混合融合 + 符号过滤
2. 实现 rag 模块——多轮对话 + Token 限制降级 + 来源引用
3. 实现 handlers + routes——HTTP API 端点
4. 实现 knowledge service 注册——Service trait 实现
5. 实现 commands 层——Tauri 命令接口
6. 理解检索 → 生成 的完整链路设计

## 二、Retriever 模块

### 2.1 检索能力层次

```
┌─────────────────────────────────────────────────────────┐
│                  Retrieval Capabilities                  │
│                                                          │
│  ┌──────────────────┐  ┌──────────────────┐             │
│  │  Vector Search    │  │  FTS5 Search     │             │
│  │  (语义相似度)     │  │  (关键词匹配)     │             │
│  │  cosine distance  │  │  rank scoring    │             │
│  └──────────────────┘  └──────────────────┘             │
│           │                     │                         │
│           └──────┬──────────────┘                         │
│                  │                                        │
│          ┌───────▼───────┐                                │
│          │  Hybrid Search │                                │
│          │  加权融合       │                                │
│          │  0.7 * vector  │                                │
│          │  + 0.3 * fts5  │                                │
│          └────────────────┘                                │
│                  │                                        │
│          ┌───────▼───────┐                                │
│          │  Symbol Filter │                                │
│          │  (代码检索增强) │                                │
│          │  symbol_kind   │                                │
│          │  symbol_name   │                                │
│          └────────────────┘                                │
│                                                          │
│  输出: SearchResult[] (chunk_id, content, score, meta)   │
└─────────────────────────────────────────────────────────┘
```

### 2.2 search — 单知识库搜索

```rust
pub async fn search(
    pool: &SqlitePool,
    kb_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());

    // 1. 优先使用 HNSW 索引
    if let Some(index) = load_index(kb_id) {
        if index.dim == query_embedding.len() {
            let hnsw_results = index.search(query_embedding, top_k);
            // 将 HNSW 的 position id 映射回 chunk_id
            let chunks = repo.get_chunks_by_kb(kb_id).await?;
            let mapped = hnsw_results.iter().map(|r| {
                // position → chunk index → chunk data → SearchResult
            }).collect();
            return Ok(mapped);
        }
    }

    // 2. Fallback: 线性扫描（无索引或维度不匹配时）
    let chunks = repo.get_chunks_by_kb(kb_id).await?;
    let mut results: Vec<SearchResult> = chunks.iter()
        .filter_map(|chunk| {
            // 从 BLOB 解码 embedding
            let embedding = decode_embedding(&chunk.embedding, chunk.embedding_dim)?;
            let score = cosine_similarity(query_embedding, &embedding);
            Some(SearchResult { chunk_id: chunk.id, ... score, ... })
        })
        .collect();
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    Ok(results.into_iter().take(top_k).collect())
}
```

### 2.3 search_all — 全局搜索

```rust
/// Search across all MCP-enabled knowledge bases
pub async fn search_all(
    pool: &SqlitePool,
    query_embedding: &[f32],
    top_k: usize,
    mcp_only: bool,
) -> Result<Vec<SearchResult>, String> {
    let repo = KbRepository::new(pool.clone());
    let kbs = repo.get_all_kbs().await?;

    let mut all_results = Vec::new();
    for kb in &kbs {
        if mcp_only && kb.mcp_enabled != 1 { continue; }
        let kb_results = search(pool, &kb.id, query_embedding, top_k).await?;
        all_results.extend(kb_results);
    }

    // Sort all results by score, take top_k
    all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    Ok(all_results.into_iter().take(top_k).collect())
}
```

### 2.4 hybrid_search — 混合检索

```rust
pub async fn hybrid_search(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,               // 关键词查询文本
    query_embedding: &[f32],   // 向量查询
    top_k: usize,
    vector_weight: f32,        // 默认 0.7
    fts_weight: f32,           // 默认 0.3
) -> Result<Vec<SearchResult>, String> {
    // 1. Vector search → top_k*2 candidates
    let vector_results = search(pool, kb_id, query_embedding, top_k * 2).await?;

    // 2. FTS5 search → top_k*2 candidates
    let fts_results = fts5_search(pool, kb_id, query, top_k * 2).await?;

    // 3. Merge by chunk_id with weighted scores
    let mut merged: HashMap<String, f32> = HashMap::new();
    for r in &vector_results {
        let score = r.score * vector_weight;
        merged.entry(r.chunk_id.clone()).and_modify(|s| *s += score).or_insert(score);
    }
    for r in &fts_results {
        let score = r.score * fts_weight;
        merged.entry(r.chunk_id.clone()).and_modify(|s| *s += score).or_insert(score);
    }

    // 4. Sort by merged score, take top_k
    let mut final_results = merged.into_iter()
        .filter_map(|(chunk_id, score)| {
            // Lookup chunk data from vector or fts results
            Some(SearchResult { chunk_id, score, ... })
        })
        .collect::<Vec<_>>();
    final_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    Ok(final_results.into_iter().take(top_k).collect())
}
```

### 2.5 FTS5 搜索实现

```rust
async fn fts5_search(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    // Build FTS5 query with bm25 ranking
    let fts_query = format!(
        "SELECT chunk_id, content, symbol_name, bm25(kb_chunks_fts) as rank
         FROM kb_chunks_fts
         WHERE kb_chunks_fts MATCH ?
         ORDER BY rank
         LIMIT ?",
    );
    // Execute query, map results to SearchResult
    // rank (bm25) → convert to similarity score (0-1 range)
}
```

### 2.6 符号过滤

```rust
/// Filter results by symbol kind (for code search)
pub fn filter_by_symbol(results: &[SearchResult], symbol_kind: &str) -> Vec<SearchResult> {
    results.iter()
        .filter(|r| {
            r.metadata.get("symbol_kind")
                .and_then(|v| v.as_str())
                .map(|k| k == symbol_kind)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}
```

使用场景：用户搜索 "handle_messages 函数"，先 hybrid_search 得到结果，再 `filter_by_symbol(results, "function")` 只保留函数定义，过滤掉注释和 import。

## 三、RAG 问答模块

### 3.1 RAG 流程

```
User Question
     │
     ▼
┌─────────────────────────────────────────────────┐
│  1. embedder::embed([question], model)           │ ← 问题向量化
│  2. retriever::hybrid_search(pool, kb_id, ...)   │ ← 混合检索
│  3. build_context(results)                       │ ← 构建上下文
│  4. build_rag_prompt(context, question, history) │ ← 构建 Prompt
│  5. proxy::handle_request(prompt)                │ ← 调用 LLM 生成答案
│  6. 格式化回答 + 来源引用                         │ ← 输出
└─────────────────────────────────────────────────┘
```

### 3.2 ask 函数

```rust
pub async fn ask(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,
    embedding_model: &str,
    chat_model: &str,
    top_k: usize,
    mcp_only: bool,
    history: &[ConversationMessage],
    app: &AppHandle,
) -> Result<RagAnswer, String> {
    // 1. Embed the query
    let embeddings = embedder::embed(&[query.to_string()], embedding_model, &repo).await?;

    // 2. Hybrid search
    let results = if kb_id.is_empty() {
        retriever::search_all(pool, query_emb, top_k, mcp_only).await?
    } else {
        retriever::hybrid_search(pool, kb_id, query, query_emb, top_k, 0.7, 0.3).await?
    };

    if results.is_empty() {
        return Ok(RagAnswer {
            answer: "知识库中没有找到相关内容。".to_string(),
            sources: vec![],
            usage: None,
        });
    }

    // 3. Build context
    let context = build_context(&results);

    // 4. Build prompt with history
    let prompt = build_rag_prompt(&context, query, history);

    // 5. Token estimation and fallback
    // ... (see §3.3 below)

    // 6. Call LLM via WaLiAPI proxy
    let result = proxy::handle_request(&repo, app, &key_id, &key_name, request_body, false, None, None).await?;

    // 7. Save to conversation history
    kb_repo.add_conversation(kb_id, "user", query, None, Some(chat_model), 0).await.ok();
    kb_repo.add_conversation(kb_id, "assistant", &answer, Some(&sources_json), Some(chat_model), tokens).await.ok();

    Ok(RagAnswer { answer, sources, usage })
}
```

### 3.3 Token 限制降级策略

LLM 有 Token 上下文限制（GPT-4o = 128K, GPT-3.5 = 16K）。如果检索到的上下文 + 对话历史超过模型限制，需要降级：

```
┌─────────────────────────────────────────────────────────┐
│  Token 降级策略（三级）                                    │
│                                                          │
│  Level 0: 全量发送                                        │
│    estimated_tokens < context_limit                       │
│    → 完整上下文 + 完整历史 + 问题                           │
│                                                          │
│  Level 1: 裁剪上下文                                      │
│    estimated_tokens > context_limit                       │
│    → 移除最低分 chunk,保留最高分                            │
│    → 保留完整历史                                          │
│                                                          │
│  Level 2: 裁剪历史                                        │
│    裁剪后仍超限                                            │
│    → 只保留最近 2 条历史                                    │
│    → 保留最高分 chunk                                      │
│                                                          │
│  Level 3: 紧急模式                                        │
│    仍超限                                                  │
│    → 无历史                                                │
│    → 只保留 top-3 chunk                                    │
│    → 简化 prompt                                          │
└─────────────────────────────────────────────────────────┘
```

```rust
let context_limit = (model_limit as f64 * 0.7) as usize; // 30%留给response

let (final_prompt, context_used) = if estimated_tokens > context_limit {
    // Stage 1: Trim context
    let trimmed = trim_context(&results, query, history, context_limit);
    if estimate_tokens(&trimmed.0) > context_limit {
        // Stage 2: Remove history, keep only latest 2
        let no_history = build_rag_prompt(&context, query, &history[history.len().saturating_sub(2)..]);
        if estimate_tokens(&no_history) > context_limit {
            // Stage 3: Emergency mode — no history, top-3 only
            build_rag_prompt(&results[..3].join("\n"), query, &[])
        } else {
            no_history
        }
    } else {
        trimmed
    }
} else {
    (prompt, context.len())
};
```

### 3.4 build_rag_prompt

```rust
fn build_rag_prompt(context: &str, query: &str, history: &[ConversationMessage]) -> String {
    let mut prompt = String::new();

    // System instruction
    prompt.push_str("基于以下知识库内容回答问题。如果知识库内容不足以回答，请明确说明。\n\n");
    prompt.push_str("## 知识库内容\n");
    prompt.push_str(context);
    prompt.push_str("\n\n");

    // History (if any)
    if !history.is_empty() {
        prompt.push_str("## 对话历史\n");
        for msg in history {
            prompt.push_str(&format!("{}: {}\n", msg.role, msg.content));
        }
        prompt.push_str("\n");
    }

    // Current question
    prompt.push_str(&format!("## 问题\n{}", query));

    prompt
}
```

### 3.5 build_context

```rust
fn build_context(results: &[SearchResult]) -> String {
    results.iter().map(|r| {
        let source = &r.filename;
        let score = format!("{:.2}", r.score);
        let content = &r.content;
        format!("[来源: {} (相似度: {})]\n{}", source, score, content)
    }).join("\n---\n")
}
```

## 四、Handlers 层

### 4.1 搜索端点

```rust
pub async fn search(
    State(shared): State<SharedState>,
    Query(params): Query<SearchParams>,
) -> Response {
    let kb_id = params.kb_id.unwrap_or_default();
    let query = params.query.unwrap_or_default();
    let top_k = params.top_k.unwrap_or(5);

    // 1. Get KB embedding model
    let kb_repo = KbRepository::new(shared.state.db.pool.clone());
    let kb = kb_repo.get_kb(&kb_id).await?;
    let model = kb.embedding_model.unwrap_or("text-embedding-3-small");

    // 2. Embed query
    let repo = Repository::new(shared.state.db.pool.clone());
    let embeddings = embedder::embed(&[query.clone()], &model, &repo).await?;

    // 3. Search
    let results = retriever::hybrid_search(&pool, &kb_id, &query, &embeddings[0], top_k, 0.7, 0.3).await?;

    Json(serde_json::json!({ "data": results })).into_response()
}
```

### 4.2 RAG 问答端点

```rust
pub async fn ask(
    State(shared): State<SharedState>,
    Json(input): Json<AskInput>,
) -> Response {
    let kb_id = input.kb_id.unwrap_or_default();
    let kb_repo = KbRepository::new(shared.state.db.pool.clone());
    let kb = kb_repo.get_kb(&kb_id).await?;
    let model = kb.embedding_model.unwrap_or("text-embedding-3-small");

    let result = rag::ask(
        &shared.state.db.pool,
        &kb_id,
        &input.question,
        &model,
        &input.model,
        input.top_k,
        false,
        &input.history.unwrap_or_default(),
        &shared.app,
    ).await;

    match result {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("RAG error: {}", e)).into_response(),
    }
}
```

## 五、Routes 注册

```rust
pub fn create_router(_state: Arc<AppState>) -> Router<SharedState> {
    Router::new()
        // Search & RAG
        .route("/api/kb/search", get(handlers::search))
        .route("/api/kb/ask", post(handlers::ask))
        // ... 其他 CRUD 路由
}
```

## 六、Knowledge Service 注册

KnowledgeService 实现 Service trait（详见 3-2节），这里重点看 `status()` 和 `routes()`：

```rust
async fn status(&self, state: &Arc<AppState>) -> ServiceStatus {
    let pool = &state.db.pool;
    let kb_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_knowledge_bases")
        .fetch_one(pool).await.unwrap_or(0);
    let doc_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_documents")
        .fetch_one(pool).await.unwrap_or(0);
    let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_chunks")
        .fetch_one(pool).await.unwrap_or(0);

    ServiceStatus {
        id: self.id().to_string(),
        name: self.name().to_string(),
        description: self.description().to_string(),
        enabled: true,
        running: true,
        stats: serde_json::json!({
            "knowledge_bases": kb_count,
            "documents": doc_count,
            "chunks": chunk_count,
        }),
    }
}
```

## 七、Commands 层

Tauri 命令层为前端提供原生调用接口：

```rust
#[tauri::command]
pub async fn get_knowledge_bases(state: State<'_, Arc<AppState>>) -> Result<Vec<KbKnowledgeBase>, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.get_all_kbs().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn create_knowledge_base(state: State<'_, Arc<AppState>>, input: CreateKbInput) -> Result<KbKnowledgeBase, String> {
    let repo = KbRepository::new(state.db.pool.clone());
    repo.create_kb(&input).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ask_knowledge_base(
    state: State<'_, Arc<AppState>>,
    kb_id: String,
    question: String,
    model: Option<String>,
    top_k: Option<usize>,
) -> Result<RagAnswer, String> {
    let kb_repo = KbRepository::new(state.db.pool.clone());
    let kb = kb_repo.get_kb(&kb_id).await.map_err(|e| e.to_string())?;
    let embedding_model = kb.embedding_model.unwrap_or("text-embedding-3-small");
    let chat_model = model.unwrap_or("gpt-4o");
    let top_k = top_k.unwrap_or(5);

    rag::ask(&state.db.pool, &kb_id, &question, &embedding_model, &chat_model, top_k, false, &[], &app_handle)
        .await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_service_statuses(state: State<'_, Arc<AppState>>) -> Result<Vec<serde_json::Value>, String> {
    let registry = ServiceRegistry::new();
    let statuses = registry.list_status(state.inner()).await;
    Ok(statuses.into_iter().map(|s| serde_json::to_value(s).unwrap_or_default()).collect())
}
```

## 八、验证测试

```bash
npm run tauri dev

# 1. 创建知识库
curl -X POST http://localhost:3456/api/kb \
  -H "Content-Type: application/json" \
  -d '{"name":"API文档知识库","description":"WaLiAPI 项目文档"}'

# 2. 上传文档
curl -X POST http://localhost:3456/api/kb/{kb_id}/documents \
  -H "Content-Type: application/json" \
  -d '{"filename":"api-guide.md","content":"base64_encoded_markdown"}'

# 3. 构建索引
curl -X POST http://localhost:3456/api/kb/{kb_id}/index

# 4. 语义搜索
curl "http://localhost:3456/api/kb/search?kb_id={kb_id}&query=如何配置渠道&top_k=5"

# 5. RAG 问答（非流式）
curl -X POST http://localhost:3456/api/kb/ask \
  -H "Content-Type: application/json" \
  -d '{"question":"如何添加一个新的API渠道？","kb_id":"{kb_id}","model":"gpt-4o","top_k":5}'

# 6. RAG 问答（带历史）
curl -X POST http://localhost:3456/api/kb/ask \
  -H "Content-Type: application/json" \
  -d '{"question":"具体步骤是什么？","kb_id":"{kb_id}","history":[{"role":"user","content":"如何添加渠道"},{"role":"assistant","content":"通过SettingsPage配置..."}]}'

# 7. 全局搜索（所有 MCP 启用的知识库）
curl "http://localhost:3456/api/kb/search?query=embedding&top_k=10"

# 8. 查看对话历史
curl http://localhost:3456/api/kb/{kb_id}/conversations
```

## 九、面试问题

1. **hybrid_search 的分数融合为什么用加权线性组合而不是 Reciprocal Rank Fusion (RRF)？两者各自的优缺点？**

   > 提示：加权线性组合（0.7×vector + 0.3×fts5）需要分数归一化（不同检索方式的分数范围不同）。RRF 用排名倒数（1/(k+rank))，天然归一化，不需要调权重。但 RRF 丢失了分数粒度（只知道排名不知道分数差距）。桌面级场景数据量小，加权组合更灵活。

2. **RAG 的 Token 降级策略为什么设置 70% 上限（context_limit = model_limit × 0.7）而不是用 100%？**

   > 提示：30% 留给模型生成回答。如果上下文占满全部 Token，模型无法生成完整回答——回答会被截断。70/30 是经验平衡点：足够上下文保证回答质量，足够空间保证完整输出。

3. **对话历史在 RAG 中的作用是什么？为什么不每次都从零开始问答？**

   > 提示：多轮对话中，用户的后续问题通常依赖前文语境（如"具体步骤是什么？"依赖上一轮的"如何添加渠道？"）。历史提供语境让模型理解追问意图。但历史也消耗 Token，所以有降级策略。

4. **`search_all` 跨知识库搜索时，不同知识库可能用不同 embedding 模型（维度不同），如何处理？**

   > 提示：当前实现假设所有知识库用相同的 embedding 模型。如果维度不同，HNSW 索引会拒绝（`index.dim != query_embedding.len()`），fallback 到线性扫描。更完善的方案是为每个 KB 分别 embed，但当前场景用户通常只用一种 embedding 模型。

5. **FTS5 的 bm25 评分和 HNSW 的 cosine similarity 评分范围完全不同（bm25 可能是负数，cosine 在 0-1），如何归一化才能公平融合？**

   > 提示：bm25 的原始值范围不定，需要归一化到 0-1。常用方法：(1) Min-Max 归一化（将最高分映射为 1，最低分映射为 0）；(2) 将 bm25 的绝对值转为相对排名分数。当前实现用 `1.0 / (1.0 + exp(-normalized_bm25))` sigmoid 归一化。


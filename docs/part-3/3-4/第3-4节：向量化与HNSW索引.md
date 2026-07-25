# 《WaLiAPI - 本地 LLM API 网关》第3-4节：向量化与HNSW索引

- **本章难度**：★★★★★
- **本章重点**：embedder（复用渠道调度）、HNSW索引（构建/搜索/持久化）、processor处理流水线、importer多源导入、012 FTS5混合检索
- **本节分支**：`xfg-3-4-kb-embedder-index`
- **课程视频**：[]()

---

**版权说明**：©本项目与星球签约合作，受[《中华人民共和国著作权法实施条例》](https://www.gov.cn/gongbao/content/2013/content_2339485.htm) 版权法保护，禁止任何理由和任何方式公开(public)源码、资料、视频等小傅哥发布的星球内容到Github、Gitee等各类平台，违反可追究进一步的法律责任。

作者：小傅哥
<br/>博客：[https://bugstack.cn](https://bugstack.cn)

>沉淀、分享、成长，让自己和他人都能有所收获！😄

大家好，我是技术UP主小傅哥。

上一节完成了数据模型和文档解析——文档被切成 chunk 存进了数据库。这一节让 chunk "活起来"：**向量化 + 索引构建 + 全文检索**，为下一节的混合检索和 RAG 问答打下基础。

## 一、本章诉求

1. 实现 embedder 模块——复用 WaLiAPI 的渠道调度能力调用 Embeddings API
2. 实现轻量级 HNSW 索引——构建/搜索/持久化/增量更新
3. 实现 processor 流水线——文档处理的完整生命周期
4. 实现 importer 多源导入——Git/URL/本地目录
5. 实现 FTS5 全文索引——为混合检索提供关键词搜索能力
6. 理解向量索引 + FTS5 混合检索的设计动机

## 二、向量化（embedder.rs）

### 2.1 为什么复用渠道调度？

知识库需要调用 Embeddings API 将文本转为向量。但 WaLiAPI 本身就是一个 API 网关，已有完善的渠道调度（Dispatcher）、重试机制、多渠道 fallback。**为什么不直接用这些能力？**

```
┌─────────────────────────────────────────────────────┐
│            embedder 调用流程                          │
│                                                      │
│  texts → embed() → get_enabled_channels()            │
│         → Dispatcher::select_channels(model)          │
│         → try_embed_with_channel() → channel 1       │
│         → 失败？ → try next channel → channel 2      │
│         → 成功 → Vec<Vec<f32>>                        │
│                                                      │
│  不需要额外配置，复用用户已有的渠道设置                  │
└─────────────────────────────────────────────────────┘
```

### 2.2 embed 函数

```rust
pub async fn embed(
    texts: &[String],
    model: &str,
    repo: &Repository,
) -> Result<Vec<Vec<f32>>, String> {
    // 1. 获取启用的渠道
    let channels = repo.get_enabled_channels().await...;
    // 2. 选择支持该模型的渠道
    let selected = Dispatcher::select_channels(&channels, model);
    let candidates = if selected.is_empty() { channels.clone() } else { selected };
    // 3. 逐个尝试
    for channel in &candidates {
        match try_embed_with_channel(texts, model, channel).await {
            Ok(embeddings) => return Ok(embeddings),
            Err(e) => continue, // fallback to next channel
        }
    }
    Err("All channels failed for embedding model".to_string())
}
```

### 2.3 try_embed_with_channel

```rust
async fn try_embed_with_channel(
    texts: &[String],
    model: &str,
    channel: &Channel,
) -> Result<Vec<Vec<f32>>, String> {
    // 1. 构造 OpenAI Embeddings API 请求
    let request = serde_json::json!({
        "model": model,
        "input": texts,
    });

    // 2. 直接发 HTTP 请求到渠道的 base_url + /v1/embeddings
    //    （不走 adaptor，因为 adaptor 硬编码了 /chat/completions URL）
    let url = format!("{}/v1/embeddings", config.base_url);
    let client = reqwest::Client::new();
    let resp = client.post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&request)
        .send().await...;

    // 3. 解析响应：提取 data[].embedding
    let body = resp.json::<Value>().await...;
    let embeddings: Vec<Vec<f32>> = body["data"].as_array()?
        .iter()
        .map(|d| d["embedding"].as_array()?)
        .map(|arr| arr.iter().map(|v| v.as_f64()? as f32).collect())
        .collect();

    Ok(embeddings)
}
```

**关键设计**：不走 adaptor 而直接发 HTTP 请求，因为所有 adaptor 都硬编码了 `/chat/completions` 路径。Embeddings API 用不同的路径 `/v1/embeddings`，所以需要独立处理。

### 2.4 向量维度验证

```rust
if !embeddings.is_empty() {
    tracing::info!(
        "Embedding success: channel={}, model={}, texts={}, dim={}",
        channel.name, model, texts.len(), embeddings[0].len()
    );
}
```

维度不一致是常见问题（不同模型的向量维度不同：text-embedding-3-small=1536，text-embedding-3-large=3072）。构建 HNSW 索引时需要验证所有向量维度一致。

## 三、HNSW 索引（index.rs）

### 3.1 为什么用 HNSW？

| 算法 | 搜索复杂度 | 构建复杂度 | 内存占用 | 适用规模 |
|------|-----------|-----------|---------|---------|
| 线性扫描 | O(n) | O(1) | O(n) | < 10K |
| IVF (倒排) | O(√n) | O(n) | O(n) | 10K-1M |
| HNSW | O(log n) | O(n·log n) | O(n·M) | 1K-100M |

WaLiAPI 是桌面级应用，知识库规模通常在几百到几万 chunk。HNSW 提供 O(log n) 搜索且无需聚类训练，非常适合。

### 3.2 数据结构

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HnswIndex {
    pub nodes: Vec<IndexNode>,      // 所有节点
    pub max_m: usize,               // 最大邻居连接数（默认16）
    pub ef_search: usize,           // 搜索宽度（默认50）
    pub ef_construction: usize,     // 构建宽度（默认200）
    pub dim: usize,                 // 向量维度
    pub entry_point: usize,         // 入口点
    pub initialized: bool,          // 是否已初始化
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexNode {
    pub id: usize,                  // 外部 ID（映射到 chunk 位置）
    pub vector: Vec<f32>,           // 向量数据
    pub neighbours: Vec<usize>,     // 邻居节点 ID
}
```

### 3.3 简化实现说明

WaLiAPI 的 HNSW 是**单层简化实现**（真正的 HNSW 有多层，但桌面规模下单层就够了）：

```
                    ┌─────────┐
                    │ 入口点   │
                    │ Node 0  │
                    └─────────┘
                   /    |    \
              ┌──────┐ ┌──────┐ ┌──────┐
              │Node 1│ │Node 2│ │Node 3│  ← 第一层邻居
              └────────┘ └────────┘ └────────┘
              /  |     /  |     /  |
         ┌──────┐ ┌──────┐ ┌──────┐   ← 第二层邻居
         │Node 4│ │Node 5│ │Node 6│
         └────────┘ └────────┘ └────────┘
```

**构建算法**（greedy insertion）：

```rust
pub fn build_with_progress<F: Fn(usize, usize)>(
    &mut self,
    items: &[(usize, Vec<f32>)],
    callback: F,
) {
    for (i, (id, vector)) in items.iter().enumerate() {
        self.insert(*id, vector.clone());
        if i % 100 == 0 { callback(i, items.len()); }
    }
    self.initialized = true;
}

fn insert(&mut self, id: usize, vector: Vec<f32>) {
    let new_node_idx = self.nodes.len();
    self.nodes.push(IndexNode { id, vector, neighbours: vec![] });

    if new_node_idx == 0 {
        self.entry_point = 0;
        return;
    }

    // Greedy search: find closest neighbors from entry point
    let neighbors = self.search_layer(&vector, self.ef_construction);

    // Connect new node to top-M closest neighbors (bidirectional)
    for (neighbor_idx, _) in neighbors.iter().take(self.max_m) {
        self.nodes[new_node_idx].neighbours.push(*neighbor_idx);
        self.nodes[*neighbor_idx].neighbours.push(new_node_idx);

        // Prune: if neighbor has too many connections, keep only closest
        if self.nodes[*neighbor_idx].neighbours.len() > self.max_m * 2 {
            self.prune_connections(*neighbor_idx);
        }
    }
}
```

### 3.4 搜索算法

```rust
pub fn search(&self, query: &[f32], top_k: usize) -> Vec<SearchResult> {
    let mut visited = HashSet::new();
    let mut candidates = BinaryHeap::new();  // Min-heap by distance
    let mut results = BinaryHeap::new();      // Max-heap (keep worst out)

    // Start from entry point
    let entry_dist = cosine_distance(query, &self.nodes[self.entry_point].vector);
    candidates.push(SearchItem { distance: entry_dist, id: self.entry_point });
    visited.insert(self.entry_point);

    while let Some(candidate) = candidates.pop() {
        if results.len() >= top_k && candidate.distance > results.peek().unwrap().distance {
            break; // All remaining candidates are worse than worst result
        }

        // Explore neighbors
        for &neighbor_idx in &self.nodes[candidate.id].neighbours {
            if visited.contains(&neighbor_idx) { continue; }
            visited.insert(neighbor_idx);

            let dist = cosine_distance(query, &self.nodes[neighbor_idx].vector);
            candidates.push(SearchItem { distance: dist, id: neighbor_idx });
        }

        results.push(candidate);
        if results.len() > top_k {
            results.pop(); // Remove worst result
        }
    }

    results.into_sorted_vec()
}
```

**搜索流程图**：

```
Query Vector
     │
     ▼
┌────────────┐
│ Entry Point │  ← 从入口点开始
└─────┬──────┘
      │
      ▼
┌──────────────────────────────┐
│  Greedy Best-First Search     │
│                               │
│  1. 计算当前节点与 query 的距离 │
│  2. 扩展邻居到 candidate heap  │
│  3. 取距离最小的 candidate     │
│  4. 如果比 results 最差的好    │
│     → 加入 results            │
│  5. 重复直到 candidate 耗尽   │
└──────────────────────────────┘
      │
      ▼
  Top-K Results (按距离排序)
```

### 3.5 持久化

```rust
pub fn save(&self, path: &Path) -> Result<(), String> {
    let bytes = serialize(self).map_err(|e| format!("Serialize error: {}", e))?;
    std::fs::write(path, bytes).map_err(|e| format!("Write error: {}", e))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Self, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Read error: {}", e))?;
    deserialize(&bytes).map_err(|e| format!("Deserialize error: {}", e))?;
    Ok(index)
}
```

**用 bincode 序列化**：比 JSON 快 10 倍以上，体积小 3 倍。索引文件存放在 `~/Library/Application Support/waliapi/hnsw_indexes/kb_{id}.hnsw`。

## 四、Processor 流水线

### 4.1 文档处理生命周期

```
上传文档 → processor::process_document()
     │
     ▼
┌─────────────────────────────────────────────────┐
│  1. parser::parse_document(filename, content)    │ ← 解析文件格式
│  2. splitter::split(text, file_type, config)     │ ← 分块
│  3. code_parser::extract_symbols (代码文件)       │ ← 符号感知
│  4. 逐 chunk 写入数据库                           │ ← 存储分块
│  5. 批量 embedder::embed(chunk_texts, model)     │ ← 向量化
│  6. 更新 chunk 的 embedding BLOB                  │ ← 存储向量
│  7. 更新文档状态: ready                            │ ← 完成标记
│  8. 更新知识库统计: doc_count++, chunk_count++    │ ← 统计同步
└─────────────────────────────────────────────────┘
```

### 4.2 processor 核心流程

```rust
pub async fn process_document(
    pool: &SqlitePool,
    kb_id: &str,
    doc_id: &str,
    filename: &str,
    content: &[u8],
    app: &AppHandle,
) -> Result<(), String> {
    let kb_repo = KbRepository::new(pool.clone());
    let kb = kb_repo.get_kb(kb_id).await?;

    // 1. Parse document
    let parsed = parser::parse_document(filename, content)?;
    let file_type = parsed.file_type;

    // 2. Split into chunks
    let config = SplitConfig {
        chunk_size: kb.chunk_size as usize,
        chunk_overlap: kb.chunk_overlap as usize,
    };
    let base_meta = ChunkMetadata {
        file_path: Some(filename.to_string()),
        language: parsed.language,
        ..Default::default()
    };

    let chunks = if file_type == "code" {
        let symbols = code_parser::extract_symbols(filename, &parsed.text);
        split_code_by_symbols(&parsed.text, &symbols, &config, &base_meta)
    } else {
        split(&parsed.text, &file_type, &config, &base_meta)
    };

    // 3. Store chunks in database
    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_id = new_id();
        let kb_chunk = KbChunk {
            id: chunk_id,
            doc_id: doc_id.to_string(),
            kb_id: kb_id.to_string(),
            chunk_index: i as i64,
            content: chunk.content.clone(),
            token_count: chunk.token_count as i64,
            metadata: serde_json::to_string(&chunk.metadata).unwrap_or_default(),
            created_at: now_iso(),
        };
        kb_repo.create_chunk(&kb_chunk).await?;
    }

    // 4. Embed chunks (batch)
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let repo = Repository::new(pool.clone());
    let embeddings = embedder::embed(&texts, &kb.embedding_model.unwrap_or("text-embedding-3-small"), &repo).await?;

    // 5. Update chunk embeddings
    for (i, emb) in embeddings.iter().enumerate() {
        let chunk_id = ...; // matching chunk_id
        kb_repo.update_chunk_embedding(&chunk_id, emb).await?;
    }

    // 6. Update document status
    kb_repo.update_document_status(doc_id, "ready", chunk_count, token_count).await?;

    // 7. Emit event for frontend
    app.emit("document-processed", serde_json::json!({
        "kb_id": kb_id,
        "doc_id": doc_id,
        "status": "ready",
        "chunk_count": chunks.len(),
    }))?;

    Ok(())
}
```

## 五、Importer 多源导入

### 5.1 支持的导入源

| source_type | 说明 | 参数 |
|-------------|------|------|
| `git` | Git 仓库克隆 | repo_url, branch, token |
| `url` | 单个 URL 下载 | url |
| `local_dir` | 本地目录扫描 | dir_path |

### 5.2 Git 导入流程

```rust
pub async fn import_git(
    pool: &SqlitePool,
    kb_id: &str,
    repo_url: &str,
    branch: Option<&str>,
    token: Option<&str>,
    excluded_dirs: &[String],
    included_files: &[String],
    app: &AppHandle,
) -> Result<(), String> {
    // 1. Clone repo to temp dir
    let temp_dir = clone_repo(repo_url, branch, token)?;

    // 2. Walk files (apply exclude/include filters)
    let files = walk_dir(&temp_dir, excluded_dirs, included_files)?;

    // 3. For each file: process_document()
    for file in &files {
        let content = std::fs::read(&file.path)?;
        process_document(pool, kb_id, &file.id, &file.name, &content, app).await?;
    }

    // 4. Cleanup temp dir
    std::fs::remove_dir_all(&temp_dir)?;

    Ok(())
}
```

### 5.3 URL 导入

```rust
pub async fn import_url(
    pool: &SqlitePool,
    kb_id: &str,
    url: &str,
    app: &AppHandle,
) -> Result<(), String> {
    // 1. Fetch URL content
    let resp = reqwest::get(url).await...;
    let content = resp.bytes().await...;

    // 2. Determine filename from URL
    let filename = url_to_filename(url);

    // 3. Process as document
    process_document(pool, kb_id, &doc_id, filename, &content, app).await?;
    Ok(())
}
```

## 六、FTS5 全文索引（Migration 012）

### 6.1 为什么需要 FTS5？

向量检索擅长语义搜索，但有些场景关键词检索更精准：

| 查询类型 | 向量检索 | FTS5 关键词 |
|----------|---------|-------------|
| "如何处理异常" | ✅ 语义匹配好 | ⚠️ 可能漏掉不带"异常"的文本 |
| "AnthropicStreamState" | ❌ 语义向量不匹配 | ✅ 精确匹配符号名 |
| "fn handle_messages" | ❌ 函数签名不在语义空间 | ✅ 精确匹配代码 |

**混合检索 = 向量搜索（语义） + FTS5搜索（关键词） + 符号过滤（代码）**

### 6.2 012_fts5_hybrid_search.sql

```sql
-- FTS5 全文索引：支持混合检索
CREATE VIRTUAL TABLE IF NOT EXISTS kb_chunks_fts USING fts5(
    chunk_id UNINDEXED,   -- chunk ID（不参与搜索，只用于关联）
    content,               -- 分块文本内容
    symbol_name,           -- 符号名（代码文件）
    tokenize = 'unicode61 remove_diacritics 2'  -- 支持中文/日文/韩文
);

-- 触发器：chunk 插入时自动同步 FTS
CREATE TRIGGER IF NOT EXISTS kb_chunks_ai AFTER INSERT ON kb_chunks BEGIN
    INSERT INTO kb_chunks_fts(chunk_id, content, symbol_name)
    VALUES (NEW.id, NEW.content, COALESCE(NEW.symbol_name, ''));
END;

-- 触发器：chunk 删除时自动同步 FTS
CREATE TRIGGER IF NOT EXISTS kb_chunks_ad AFTER DELETE ON kb_chunks BEGIN
    DELETE FROM kb_chunks_fts WHERE chunk_id = OLD.id;
END;

-- 触发器：chunk 更新时自动同步 FTS（先删旧再插新）
CREATE TRIGGER IF NOT EXISTS kb_chunks_au AFTER UPDATE ON kb_chunks BEGIN
    DELETE FROM kb_chunks_fts WHERE chunk_id = OLD.id;
    INSERT INTO kb_chunks_fts(chunk_id, content, symbol_name)
    VALUES (NEW.id, NEW.content, COALESCE(NEW.symbol_name, ''));
END;
```

**设计要点**：

1. **`chunk_id UNINDEXED`**：chunk ID 不参与搜索，只用于关联回 kb_chunks 表。
2. **`tokenize = 'unicode61 remove_diacritics 2'`**：支持 Unicode 分词，包括中日韩文字。`remove_diacritics 2` 去除变音符号（é→e）。
3. **`symbol_name` 参与搜索**：搜索 "handle_messages" 时能匹配代码符号名。
4. **触发器自动同步**：chunk 的 CRUD 操作自动同步到 FTS 表，无需手动维护。

### 6.3 FTS5 搜索示例

```sql
-- 关键词搜索
SELECT chunk_id FROM kb_chunks_fts WHERE kb_chunks_fts MATCH 'handle_messages' ORDER BY rank;

-- 符号名搜索
SELECT chunk_id FROM kb_chunks_fts WHERE kb_chunks_fts MATCH 'symbol_name:handle_messages';

-- 混合查询（关键词 + 符号）
SELECT chunk_id FROM kb_chunks_fts WHERE kb_chunks_fts MATCH 'handle OR symbol_name:handle';
```

## 七、Retriever 模块预览

retriever 模块负责搜索，是 HNSW + FTS5 混合检索的入口：

```rust
pub async fn search(
    pool: &SqlitePool,
    kb_id: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, String> {
    // 1. 尝试加载 HNSW 索引
    if let Some(index) = load_index(kb_id) {
        // HNSW 搜索 → 得到 (position, score) 列表
        let hnsw_results = index.search(query_embedding, top_k);
        // 从数据库加载对应 chunk 数据
        // ...
    } else {
        // Fallback: 线性扫描（遍历所有 chunk 的 embedding）
        // ...
    }
}

/// 混合检索：向量 + FTS5，带权重融合
pub async fn hybrid_search(
    pool: &SqlitePool,
    kb_id: &str,
    query: &str,               // 关键词查询
    query_embedding: &[f32],   // 向量查询
    top_k: usize,
    vector_weight: f32,        // 默认 0.7
    fts_weight: f32,           // 默认 0.3
) -> Result<Vec<SearchResult>, String> {
    // 1. HNSW 向量搜索 → top_k*2 结果
    // 2. FTS5 关键词搜索 → top_k*2 结果
    // 3. 按 chunk_id 合并，加权分数 = vector_score*0.7 + fts_score*0.3
    // 4. 取 top_k 结果
}
```

## 八、验证测试

```bash
npm run tauri dev

# 1. 创建知识库（指定嵌入模型）
curl -X POST http://localhost:3456/api/kb \
  -H "Content-Type: application/json" \
  -d '{"name":"代码知识库","embedding_model":"text-embedding-3-small"}'

# 2. 上传代码文件
curl -X POST http://localhost:3456/api/kb/{kb_id}/documents \
  -H "Content-Type: application/json" \
  -d '{"filename":"handler.rs","content":"base64_encoded_rust_file"}'

# 3. 构建索引
curl -X POST http://localhost:3456/api/kb/{kb_id}/index

# 4. 查看索引状态
curl http://localhost:3456/api/kb/{kb_id}/index

# 5. 向量搜索
curl "http://localhost:3456/api/kb/search?kb_id={kb_id}&query=handle+stream&top_k=5"

# 6. Git 仓库导入
curl -X POST http://localhost:3456/api/kb/{kb_id}/sources \
  -H "Content-Type: application/json" \
  -d '{"source_type":"git","repo_url":"https://github.com/example/repo","branch":"main"}'

# 7. 验证 FTS5 触发器
sqlite3 ~/Library/Application\ Support/com.waliapi.app/waliapi.db \
  "SELECT chunk_id, content FROM kb_chunks_fts WHERE kb_chunks_fts MATCH 'handler' LIMIT 5;"
```

## 九、面试问题

1. **WaLiAPI 的 HNSW 实现是单层的，真正的 HNSW 有多层（express layer + dense layer）。单层实现在大规模数据下会有什么性能瓶颈？**

   > 提示：单层 HNSW 搜索时从入口点开始贪心遍历，跳数约为 O(log n)，但多层 HNSW 可以从高层快速跳到目标区域再在低层精细搜索。桌面场景（< 100K chunk）单层足够，10 万以上时多层更优。

2. **embedder 复用渠道调度而不是直接调用 Embeddings API，这个设计有什么好处和潜在问题？**

   > 提示：好处是用户只需配置一个渠道（同一个 base_url/api_key），WaLiAPI 自动用它做 chat 和 embedding。潜在问题是渠道可能不支持 embedding（比如某些 OpenAI 兼容服务只实现了 chat completions），需要 fallback 逻辑。

3. **FTS5 的触发器自动同步机制 vs 手动批量同步，各自适用什么场景？触发器在高频写入时有什么性能影响？**

   > 提示：触发器保证一致性（每次 INSERT/DELETE 都自动同步），适合日常操作。批量导入时触发器会有性能开销（每行一次触发），可以用 `INSERT INTO kb_chunks_fts` 批量替代。但触发器更安全——不会遗漏。

4. **hybrid_search 中向量权重 0.7 和 FTS5 权重 0.3 的默认比例是怎么定的？什么场景应该调高 FTS5 权重？**

   > 提示：70/30 的默认比例偏向语义匹配（向量），因为大多数查询是自然语言。代码搜索场景（如搜索 "fn handle_messages"）应该调高 FTS5 权重到 0.5 以上，因为精确关键词匹配更重要。

5. **HNSW 索引持久化用 bincode 而不是 JSON，如果索引文件损坏（进程中断时写入未完成），如何恢复？**

   > 提示：bincode 不像 JSON 有自检能力，损坏后无法加载。恢复策略：(1) `load()` 失败时自动重建索引；(2) 先写临时文件再 rename（原子写入）；(3) `index_status` 字段追踪状态，`corrupted` 状态触发重建。


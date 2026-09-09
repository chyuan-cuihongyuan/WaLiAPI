-- 语义缓存（C-02）：网关侧响应缓存（exact 规范化哈希层 + semantic embedding 余弦层）。
-- 默认关闭（cache.semantic_enabled=false）；TTL 默认 24h；带 tools 的请求永不入缓存。
CREATE TABLE IF NOT EXISTS semantic_cache (
    id TEXT PRIMARY KEY,
    cache_key TEXT NOT NULL UNIQUE,
    model TEXT NOT NULL,
    embedding BLOB,
    answer TEXT NOT NULL,
    ttl_expire_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_semantic_cache_model ON semantic_cache(model, ttl_expire_at);

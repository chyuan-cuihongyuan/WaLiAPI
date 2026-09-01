-- 请求日志缓存命中统计（issue #51：缓存命中、折线图、统计信息、侧边栏收起）。
-- NULL = 上游未上报缓存字段，或该路径本地估算（无法区分命中与否）。
-- 归一化口径（各家字段与 prompt/input 总量的包含关系不同）：
--   OpenAI 系  usage.prompt_tokens_details.cached_tokens -> cache_read_tokens（prompt_tokens 子集）
--   DeepSeek   usage.prompt_cache_hit_tokens              -> cache_read_tokens（miss = prompt - hit，不落列）
--   Anthropic  usage.cache_read_input_tokens              -> cache_read_tokens（与 input_tokens 并列，不混入 prompt）
--              usage.cache_creation_input_tokens          -> cache_creation_tokens
--   Gemini     usageMetadata.cachedContentTokenCount      -> cache_read_tokens（promptTokenCount 子集）
ALTER TABLE request_logs ADD COLUMN cache_read_tokens INTEGER;
ALTER TABLE request_logs ADD COLUMN cache_creation_tokens INTEGER;

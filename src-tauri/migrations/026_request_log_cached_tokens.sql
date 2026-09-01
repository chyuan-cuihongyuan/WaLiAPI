-- Migration 026: Add cached_tokens column to request_logs
-- Stores the number of prompt tokens that hit the upstream provider's cache
-- (e.g. OpenAI prompt_tokens_details.cached_tokens, Anthropic cache_read_input_tokens).
ALTER TABLE request_logs ADD COLUMN cached_tokens INTEGER NOT NULL DEFAULT 0;

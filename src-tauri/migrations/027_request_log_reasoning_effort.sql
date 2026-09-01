-- Migration 027: Add reasoning_effort column to request_logs
-- Stores the canonical reasoning/thinking level used by the provider/protocol.
ALTER TABLE request_logs ADD COLUMN reasoning_effort TEXT;

-- 渠道主动健康探测（C-04）：
-- channels 三列为探测状态（NULL = 从未探测，视为健康参与正常排序）；
-- request_logs.is_probe 标记探测流量，统计/用量口径排除，避免污染用户账单。
ALTER TABLE channels ADD COLUMN last_probe_at TEXT;
ALTER TABLE channels ADD COLUMN last_probe_ok INTEGER;
ALTER TABLE channels ADD COLUMN probe_latency_ms INTEGER;
ALTER TABLE request_logs ADD COLUMN is_probe INTEGER NOT NULL DEFAULT 0;

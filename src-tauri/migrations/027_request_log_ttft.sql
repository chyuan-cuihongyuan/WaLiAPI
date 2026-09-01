-- 首字延迟（TTFT，issue #51 统计信息）。
-- 定义：网关收到下游请求起，至上游首个有效 SSE 帧通过缓冲验证止；
-- 多渠道重试时从最初请求起算（用户感知口径，含失败轮询时间）。
-- NULL = 非流式请求，或请求未到达首帧（失败/断连）。
ALTER TABLE request_logs ADD COLUMN ttft_ms INTEGER;

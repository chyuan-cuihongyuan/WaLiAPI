-- Prompt 模板版本化（C-07）：RAG 系统提示词从源码硬编码迁移为可版本化管理。
-- 启动时按 key 检测空表则把源码字面量逐字节写入 v1（升级后行为不变的硬保证）；
-- 运行时按 key + active 读取，读失败回退编译期默认（双保险）。
CREATE TABLE IF NOT EXISTS prompt_templates (
    id TEXT PRIMARY KEY,
    template_key TEXT NOT NULL,
    version INTEGER NOT NULL,
    content TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    UNIQUE(template_key, version)
);

CREATE INDEX IF NOT EXISTS idx_prompt_templates_active
    ON prompt_templates(template_key, active);

-- 知识库 VLM OCR（方案A）：
-- kb_knowledge_bases.ocr_model 为知识库级视觉模型（NULL/空 = 不启用，且需全局 ocr.enabled 开启才生效）；
-- kb_documents 记录 OCR 产物信息：识别引擎、页数、失败页码 JSON。
ALTER TABLE kb_knowledge_bases ADD COLUMN ocr_model TEXT;
ALTER TABLE kb_documents ADD COLUMN ocr_engine TEXT;
ALTER TABLE kb_documents ADD COLUMN page_count INTEGER DEFAULT 0;
ALTER TABLE kb_documents ADD COLUMN ocr_failed_pages TEXT DEFAULT '[]';

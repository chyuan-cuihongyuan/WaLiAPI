import { useCallback, useEffect, useMemo, useState } from "react";
import { ArrowLeftRight, FileText, Loader2, Plus, Save } from "lucide-react";
import { promptTemplateApi } from "../lib/api";
import type { PromptTemplate } from "../types";

/** Prompt 模板管理（C-07）：按 key 分组列版本、编辑新建版本、一键激活/回滚。
 *  种子 = 源码字面量 v1；激活新版本立即生效，回滚 v1 同理；占位符非法拒绝激活。 */
export default function PromptTemplatesPage() {
  const [templates, setTemplates] = useState<PromptTemplate[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [editingKey, setEditingKey] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setTemplates(await promptTemplateApi.list());
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const grouped = useMemo(() => {
    const map = new Map<string, PromptTemplate[]>();
    for (const t of templates) {
      const list = map.get(t.template_key) ?? [];
      list.push(t);
      map.set(t.template_key, list);
    }
    return [...map.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [templates]);

  const activeOf = (key: string) => grouped.find(([k]) => k === key)?.[1].find(t => t.active);

  const startEdit = (key: string) => {
    setEditingKey(key);
    setDraft(activeOf(key)?.content ?? "");
    setNotice(null);
  };

  const saveNewVersion = async (key: string) => {
    setNotice(null);
    setError(null);
    try {
      const version = await promptTemplateApi.create(key, draft);
      setNotice(`已保存 ${key} v${version}（未激活，预览确认后一键激活）`);
      setEditingKey(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const activate = async (key: string, version: number) => {
    setNotice(null);
    setError(null);
    try {
      await promptTemplateApi.activate(key, version);
      setNotice(`已激活 ${key} v${version}，立即生效`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="mx-auto max-w-5xl p-6">
      <div className="mb-4 flex items-center gap-2">
        <FileText className="h-5 w-5 text-primary" />
        <h1 className="text-xl font-semibold">Prompt 模板</h1>
      </div>
      <p className="mb-6 text-sm text-muted-foreground">
        RAG 问答与深度研究的系统提示词版本化管理：编辑保存生成新版本，一键激活/回滚立即生效；
        升级时自动以源码字面量种子 v1（行为逐字节不变）；含非法占位符的模板会被拒绝。
      </p>

      {error && <div className="mb-4 rounded-2xl border border-red-300 bg-red-50 p-3 text-sm text-red-700">{error}</div>}
      {notice && <div className="mb-4 rounded-2xl border border-emerald-300 bg-emerald-50 p-3 text-sm text-emerald-700">{notice}</div>}
      {loading && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> 加载中…
        </div>
      )}

      <div className="space-y-6">
        {grouped.map(([key, versions]) => {
          const active = versions.find(v => v.active);
          return (
            <div key={key} className="surface rounded-2xl p-4">
              <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
                <div>
                  <div className="font-mono text-sm font-medium">{key}</div>
                  <div className="text-xs text-muted-foreground">
                    当前激活 v{active?.version ?? "-"} · 共 {versions.length} 个版本
                  </div>
                </div>
                <button
                  onClick={() => (editingKey === key ? setEditingKey(null) : startEdit(key))}
                  className="flex items-center gap-1 rounded-full border border-border px-3 py-1.5 text-xs font-medium hover:bg-white/5"
                >
                  <Plus className="h-3.5 w-3.5" />
                  {editingKey === key ? "取消编辑" : "编辑新版本"}
                </button>
              </div>

              {editingKey === key && (
                <div className="mb-4">
                  <textarea
                    value={draft}
                    onChange={e => setDraft(e.target.value)}
                    rows={10}
                    className="w-full rounded-2xl border border-border bg-background/70 p-3 font-mono text-xs focus:outline-none focus:ring-2 focus:ring-primary/20"
                  />
                  <div className="mt-2 flex items-center gap-2">
                    <button
                      onClick={() => void saveNewVersion(key)}
                      className="flex items-center gap-1 rounded-full bg-primary px-4 py-1.5 text-xs font-medium text-primary-foreground hover:opacity-90"
                    >
                      <Save className="h-3.5 w-3.5" /> 保存为新版本
                    </button>
                    <span className="text-xs text-muted-foreground">
                      保存仅校验占位符；激活后才切换生效
                    </span>
                  </div>
                </div>
              )}

              <div className="space-y-2">
                {versions.map(v => (
                  <div
                    key={v.id}
                    className={`flex items-center justify-between gap-3 rounded-xl border px-3 py-2 ${
                      v.active ? "border-emerald-300 bg-emerald-50/50" : "border-border"
                    }`}
                  >
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2 text-xs">
                        <span className="font-mono font-medium">v{v.version}</span>
                        {v.active && (
                          <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-[10px] font-medium text-emerald-700">
                            激活中
                          </span>
                        )}
                        <span className="text-muted-foreground">{v.created_at}</span>
                      </div>
                      <pre className="mt-1 max-h-24 overflow-y-auto whitespace-pre-wrap break-all text-xs text-muted-foreground">
                        {v.content}
                      </pre>
                    </div>
                    {!v.active && (
                      <button
                        onClick={() => void activate(key, v.version)}
                        className="flex shrink-0 items-center gap-1 rounded-full border border-border px-3 py-1.5 text-xs font-medium hover:bg-white/5"
                      >
                        <ArrowLeftRight className="h-3.5 w-3.5" />
                        {v.version === 1 ? "回滚到此版本" : "激活此版本"}
                      </button>
                    )}
                  </div>
                ))}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

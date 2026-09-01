import { Component, type ErrorInfo, type ReactNode } from "react";

interface ErrorBoundaryProps {
  children: ReactNode;
}

interface ErrorBoundaryState {
  error: Error | null;
}

/**
 * 全局错误边界：捕获子树渲染期异常，展示可恢复的错误卡片而非整页白屏。
 * 桌面端（src/App.tsx）与 Web 管理面板（web/src/App.tsx）两入口均在根部挂载。
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("[ErrorBoundary] 渲染异常", error, info.componentStack);
  }

  private handleReset = () => {
    this.setState({ error: null });
  };

  private handleReload = () => {
    window.location.reload();
  };

  render() {
    if (!this.state.error) return this.props.children;

    const message = this.state.error.message || String(this.state.error);

    return (
      <div className="flex min-h-[60vh] items-center justify-center p-6">
        <div className="w-full max-w-lg rounded-2xl border border-red-200 bg-white p-6 shadow-sm">
          <div className="flex items-center gap-2.5">
            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-red-50 text-red-500">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" className="h-5 w-5">
                <path strokeLinecap="round" strokeLinejoin="round" d="M12 9v4m0 4h.01M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z" />
              </svg>
            </div>
            <div>
              <h2 className="text-base font-semibold text-slate-900">页面渲染出错</h2>
              <p className="text-xs text-slate-500">界面发生未预期的异常，可尝试恢复或刷新页面。</p>
            </div>
          </div>
          <pre className="mt-4 max-h-40 overflow-auto rounded-xl border border-slate-200 bg-slate-50 p-3 text-xs leading-relaxed text-red-600 whitespace-pre-wrap break-words">
            {message}
          </pre>
          <div className="mt-4 flex items-center justify-end gap-2">
            <button
              onClick={this.handleReset}
              className="rounded-lg border border-slate-200 bg-white px-3.5 py-1.5 text-sm font-medium text-slate-700 transition-colors hover:bg-slate-50 hover:text-slate-900"
            >
              尝试恢复
            </button>
            <button
              onClick={this.handleReload}
              className="rounded-lg bg-red-500 px-3.5 py-1.5 text-sm font-medium text-white transition-colors hover:bg-red-600"
            >
              刷新页面
            </button>
          </div>
        </div>
      </div>
    );
  }
}

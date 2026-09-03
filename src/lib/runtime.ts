import { invoke as tauriInvoke } from "@tauri-apps/api/core";

export const WEB_UNAUTHORIZED_EVENT = "waliapi:web-unauthorized";

/**
 * Web 管理面板的 Bearer token 读取（FIX-14 token 源收敛）：
 * 主源是登录页写入的 localStorage `waliapi_admin_token`（web/src/lib/auth.ts），
 * 兼容读旧的 sessionStorage 键（WebAdminGate 直连模式写入）。
 * 此前 invoke/listen 只读 sessionStorage——登录页从不写那个键，
 * Web 面板的命令调用实际全靠会话 Cookie 兜底，Bearer 形同虚设。
 */
const WEB_ADMIN_TOKEN_STORAGE_KEY = "waliapi_admin_token";
const WEB_ADMIN_TOKEN_LEGACY_SESSION_KEY = "waliapi.web.admin-token";

export interface RuntimeEvent<T> {
  payload: T;
}

export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export function getWebAdminToken(): string {
  if (typeof window === "undefined") return "";
  return (
    window.localStorage.getItem(WEB_ADMIN_TOKEN_STORAGE_KEY) ||
    window.sessionStorage.getItem(WEB_ADMIN_TOKEN_LEGACY_SESSION_KEY) ||
    ""
  );
}

export function setWebAdminToken(token: string): void {
  if (typeof window === "undefined") return;
  const normalized = token.trim();
  if (normalized) window.sessionStorage.setItem(WEB_ADMIN_TOKEN_LEGACY_SESSION_KEY, normalized);
  else window.sessionStorage.removeItem(WEB_ADMIN_TOKEN_LEGACY_SESSION_KEY);
}

/**
 * Unified command transport. Desktop builds keep using Tauri IPC; browser builds
 * send the exact same command name and argument object to the protected Axum
 * admin endpoint.
 */
export async function invoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isTauriRuntime()) return tauriInvoke<T>(command, args);

  const token = getWebAdminToken();
  const response = await fetch("/admin/api/invoke", {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      "x-requested-with": "XMLHttpRequest",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify({ cmd: command, args }),
  });

  if (response.status === 401) {
    window.dispatchEvent(new CustomEvent(WEB_UNAUTHORIZED_EVENT));
  }

  const text = await response.text();
  let data: unknown = null;
  if (text) {
    try { data = JSON.parse(text); } catch { /* ignore */ }
  }

  if (!response.ok) {
    const errMsg =
      data && typeof data === "object" && "error" in data
        ? String((data as { error: unknown }).error)
        : `管理 API 请求失败（HTTP ${response.status}）`;
    throw new Error(errMsg);
  }

  return data as T;
}

/** Subscribe to native Tauri events or the authenticated headless SSE bridge. */
export async function listen<T>(
  eventName: string,
  handler: (event: RuntimeEvent<T>) => void,
): Promise<() => void> {
  if (isTauriRuntime()) {
    const { listen: tauriListen } = await import("@tauri-apps/api/event");
    return tauriListen<T>(eventName, (event) => handler({ payload: event.payload }));
  }

  const controller = new AbortController();
  const token = getWebAdminToken();
  void (async () => {
    // 断线指数退避重连（FIX-24）：服务重启 / 网络抖动后事件流自动恢复，
    // 渠道测试进度等事件不再静默丢失。连接成功后退避复位，上限 30s。
    let backoffMs = 1_000;
    while (!controller.signal.aborted) {
      let connected = false;
      try {
        const response = await fetch("/admin/api/events", {
          headers: token ? { authorization: `Bearer ${token}` } : {},
          credentials: "same-origin",
          signal: controller.signal,
        });
        if (response.status === 401) {
          window.dispatchEvent(new CustomEvent(WEB_UNAUTHORIZED_EVENT));
        }
        if (!response.ok || !response.body) {
          throw new Error(`事件连接失败（HTTP ${response.status}）`);
        }
        connected = true;

        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (!controller.signal.aborted) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
          let boundary = buffer.indexOf("\n\n");
          while (boundary >= 0) {
            const frame = buffer.slice(0, boundary);
            buffer = buffer.slice(boundary + 2);
            let name = "message";
            const data: string[] = [];
            for (const line of frame.split("\n")) {
              if (line.startsWith("event:")) name = line.slice(6).trim();
              else if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
            }
            if (name === eventName && data.length) {
              try {
                handler({ payload: JSON.parse(data.join("\n")) as T });
              } catch (error) {
                console.warn(`忽略无法解析的事件 ${eventName}`, error);
              }
            }
            boundary = buffer.indexOf("\n\n");
          }
        }
      } catch (error) {
        if (!controller.signal.aborted) console.warn("Web 事件连接已断开", error);
      }
      if (controller.signal.aborted) return;
      if (connected) backoffMs = 1_000;
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, backoffMs);
        controller.signal.addEventListener(
          "abort",
          () => { clearTimeout(timer); resolve(); },
          { once: true },
        );
      });
      backoffMs = Math.min(backoffMs * 2, 30_000);
    }
  })();
  return () => controller.abort();
}

export async function openExternalUrl(url: string): Promise<void> {
  if (isTauriRuntime()) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
    return;
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

export async function writeClipboard(text: string): Promise<void> {
  if (isTauriRuntime()) {
    await tauriInvoke("plugin:clipboard-manager|write_text", { text });
    return;
  }
  if (navigator.clipboard?.writeText && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) throw new Error("浏览器未授予剪贴板权限");
}

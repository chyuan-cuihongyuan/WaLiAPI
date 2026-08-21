import { ReactNode, useState } from "react";
import { Menu } from "lucide-react";
import { Sidebar } from "./Sidebar";

export function Layout({
  children,
  hasUpdate,
  onCheckUpdate,
}: {
  children: ReactNode;
  hasUpdate: boolean;
  onCheckUpdate: () => void;
}) {
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  return (
    <div className="flex h-dvh overflow-hidden bg-transparent text-foreground">
      {mobileNavOpen && (
        <button
          type="button"
          aria-label="关闭导航"
          className="fixed inset-0 z-40 bg-slate-950/35 md:hidden"
          onClick={() => setMobileNavOpen(false)}
        />
      )}
      <Sidebar
        hasUpdate={hasUpdate}
        onCheckUpdate={onCheckUpdate}
        mobileOpen={mobileNavOpen}
        onMobileClose={() => setMobileNavOpen(false)}
      />
      <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <header className="flex h-14 items-center gap-3 border-b border-slate-200 bg-[#eef3f8] px-3 md:hidden">
          <button
            type="button"
            onClick={() => setMobileNavOpen(true)}
            className="flex h-11 w-11 items-center justify-center rounded-xl text-slate-700 active:bg-white"
            aria-label="打开导航"
            aria-expanded={mobileNavOpen}
          >
            <Menu size={21} />
          </button>
          <img src="/logo.png" alt="" className="h-8 w-8 rounded-lg" />
          <span className="text-base font-bold tracking-[-0.03em] text-slate-900">WaLiAPI</span>
        </header>
        <div className="min-h-0 flex-1 overflow-auto bg-[linear-gradient(180deg,rgba(255,255,255,0.02),transparent)]">
          {children}
        </div>
      </main>
    </div>
  );
}

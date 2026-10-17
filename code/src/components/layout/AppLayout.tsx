import type { ReactNode } from "react";

import { Sidebar } from "./Sidebar";
import { TopBar } from "./TopBar";
import { ErrorNotice } from "@/components/ui";
import { useTheme } from "@/hooks/useTheme";
import { useBackendStore } from "@/stores/useBackendStore";

/**
 * The application shell: fixed navigation on the left, scrolling content on the
 * right. Screens are passed in rather than rendered through an `<Outlet/>`,
 * because the screens are mounted once and kept — see `App`.
 */
export function AppLayout({ children }: { children: ReactNode }) {
  useTheme();
  const backendError = useBackendStore((state) => state.error);

  return (
    <div className="flex h-full">
      <Sidebar />

      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar />

        <main className="scroll-pane flex-1 overflow-y-auto">
          <div className="mx-auto max-w-5xl px-6 py-6">
            {/*
             * A backend failure (unreachable database, failed startup check) is
             * shown on every screen, because it affects every screen.
             */}
            {backendError && (
              <div className="mb-6">
                <ErrorNotice error={backendError} />
              </div>
            )}
            {children}
          </div>
        </main>
      </div>
    </div>
  );
}

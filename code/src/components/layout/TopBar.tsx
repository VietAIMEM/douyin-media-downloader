import { Badge, Button } from "@/components/ui";
import { useBackendStore } from "@/stores/useBackendStore";

/**
 * Top bar: backend health at a glance, and the manual refresh that re-reads
 * application info and database status.
 *
 * The current screen's name is not repeated here. The page header under it
 * carries the title; saying it twice in one window is the kind of redundancy
 * that reads as a template rather than a design.
 */
export function TopBar() {
  const appInfo = useBackendStore((state) => state.appInfo);
  const databaseStatus = useBackendStore((state) => state.databaseStatus);
  const loading = useBackendStore((state) => state.loading);
  const refresh = useBackendStore((state) => state.refresh);

  return (
    <header className="flex h-14 shrink-0 items-center justify-between gap-4 border-b border-shell-border bg-surface px-6">
      <div className="min-w-0" />

      <div className="flex items-center gap-3">
        {appInfo && <Badge mono>v{appInfo.version}</Badge>}
        {appInfo?.demoMode && <Badge tone="accent">demo mode</Badge>}
        {databaseStatus && (
          <Badge tone={databaseStatus.healthy ? "success" : "danger"} mono>
            {databaseStatus.healthy ? "storage ready" : "storage unavailable"}
          </Badge>
        )}
        <Button size="sm" loading={loading} onClick={() => void refresh()}>
          Refresh
        </Button>
      </div>
    </header>
  );
}

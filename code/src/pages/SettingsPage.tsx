import { Button, PageHeader } from "@/components/ui";
import { BackendStatusCard } from "@/features/diagnostics/BackendStatusCard";
import { SettingsForm } from "@/features/settings/SettingsForm";
import { UpdateCard } from "@/features/updates/UpdateCard";
import { useBackendStore } from "@/stores/useBackendStore";
import { useSettingsStore } from "@/stores/useSettingsStore";

/**
 * Settings.
 *
 * The grouped editor below covers what a user changes; diagnostics — the
 * application's build identity, its directories, its database, its resolvers —
 * moved here from Home, where they sat in the way of starting a download. This
 * is where a person looks when something is wrong, which is exactly what that
 * card answers.
 */
export function SettingsPage() {
  const loading = useSettingsStore((state) => state.loading);
  const load = useSettingsStore((state) => state.load);
  const appInfo = useBackendStore((state) => state.appInfo);
  const databaseStatus = useBackendStore((state) => state.databaseStatus);

  return (
    <>
      <PageHeader
        title="Settings"
        description="Preferences are stored in the application database, so they survive a restart and are the same values the download pipeline reads."
      >
        <Button
          size="sm"
          loading={loading}
          onClick={() => void load()}
          title="Discard unsaved edits and re-read the stored settings"
        >
          Reload from database
        </Button>
      </PageHeader>

      <SettingsForm />
      <div className="mt-5">
        <UpdateCard />
      </div>
      <div className="mt-5">
        <BackendStatusCard appInfo={appInfo} databaseStatus={databaseStatus} />
      </div>
    </>
  );
}

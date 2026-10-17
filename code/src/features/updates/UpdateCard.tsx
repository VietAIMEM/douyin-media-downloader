/**
 * The update control on the Settings screen.
 *
 * Every state the updater store can be in is rendered as itself, because they
 * ask different things of the user: "checking" and "up to date" are
 * information, "an update is available" is a decision, "downloading" is
 * progress, and a failure needs the cause named so it can be acted on. The one
 * thing this deliberately never does is install anything without being asked —
 * the confirmation is a `ConfirmButton`, so the question is asked in place and
 * Escape backs out of it.
 */

import { Badge, Button, Card, ConfirmButton, ErrorNotice, ProgressBar } from "@/components/ui";
import { formatBytes } from "@/lib/format";
import { useBackendStore } from "@/stores/useBackendStore";
import { useUpdateStore } from "@/services/updater";

export function UpdateCard() {
  const appInfo = useBackendStore((state) => state.appInfo);

  const phase = useUpdateStore((state) => state.phase);
  const version = useUpdateStore((state) => state.version);
  const notes = useUpdateStore((state) => state.notes);
  const downloadedBytes = useUpdateStore((state) => state.downloadedBytes);
  const totalBytes = useUpdateStore((state) => state.totalBytes);
  const error = useUpdateStore((state) => state.error);
  const checkForUpdate = useUpdateStore((state) => state.checkForUpdate);
  const installUpdate = useUpdateStore((state) => state.installUpdate);
  const restart = useUpdateStore((state) => state.restart);

  const busy = phase === "checking" || phase === "downloading" || phase === "installing";
  const current = appInfo?.version;

  // A build with no update channel has nothing to check against. Offering the
  // button anyway would mean the only reachable outcome is a failure, and a
  // failure that always happens is indistinguishable from a broken updater —
  // so this says what is true of the build instead. `null` app info is not
  // "unconfigured", it is "not asked yet", and falls through to the normal
  // card.
  if (appInfo && !appInfo.updateChannelConfigured) {
    return (
      <Card
        title="Updates"
        description="This build carries no update channel, so it never checks for one."
      >
        <div className="space-y-3">
          <p className="text-xs text-content-muted">
            Running version{" "}
            <span className="font-mono text-content-secondary">{current ?? "unknown"}</span>
          </p>
          <p className="text-sm text-content-secondary">
            Updates are applied by running a newer build&rsquo;s installer over this one. Your
            settings, library, download history and downloaded files are not touched, and the
            database is migrated forward on the next launch.
          </p>
        </div>
      </Card>
    );
  }

  return (
    <Card
      title="Updates"
      description="Updates are checked only when you ask. Nothing is downloaded or installed without your confirmation."
    >
      <div className="space-y-4">
        <p className="text-xs text-content-muted">
          Running version{" "}
          <span className="font-mono text-content-secondary">{current ?? "unknown"}</span>
        </p>

        {error && (
          <ErrorNotice
            error={error}
            onDismiss={() => useUpdateStore.getState().dismiss()}
            title="Update"
          />
        )}

        {phase === "idle" && (
          <div className="flex flex-wrap items-center gap-3">
            <Button size="sm" onClick={() => void checkForUpdate()}>
              Check for updates
            </Button>
            <span className="text-xs text-content-muted">
              Asks the release channel whether a newer version exists.
            </span>
          </div>
        )}

        {phase === "checking" && (
          <p role="status" className="text-sm text-content-secondary">
            Checking for updates…
          </p>
        )}

        {phase === "up-to-date" && (
          <div className="flex flex-wrap items-center gap-3">
            <Badge tone="success">Up to date</Badge>
            <span role="status" className="text-sm text-content-secondary">
              {current ? `Version ${current} is the newest available.` : "No newer version was found."}
            </span>
            <Button size="sm" variant="secondary" onClick={() => void checkForUpdate()}>
              Check again
            </Button>
          </div>
        )}

        {phase === "available" && (
          <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-3">
              <Badge tone="brand">Update available</Badge>
              <span className="text-sm text-content-secondary">
                Version <span className="font-mono">{version}</span> is available.
              </span>
            </div>
            {notes && (
              <div className="rounded-lg border border-surface-border bg-surface-raised px-3 py-2">
                <p className="mb-1 text-xs font-medium text-content-primary">What is new</p>
                {/* Plain text, never markup: release notes come from the update
                    feed, which is a network source this application does not
                    control. */}
                <p className="whitespace-pre-wrap text-xs leading-relaxed text-content-secondary">
                  {notes}
                </p>
              </div>
            )}
            <div className="flex flex-wrap items-center gap-3">
              <ConfirmButton
                label="Install update"
                question={`Install version ${version} and restart the application?`}
                detail="The download is verified before it is installed. Your settings, download history, and downloaded files are not affected."
                confirmLabel="Install and restart"
                busy={busy}
                onConfirm={() => void installUpdate()}
              />
              <Button
                size="sm"
                variant="ghost"
                onClick={() => useUpdateStore.getState().dismiss()}
              >
                Not now
              </Button>
            </div>
          </div>
        )}

        {phase === "downloading" && (
          <div className="space-y-2">
            <p role="status" className="text-sm text-content-secondary">
              Downloading version {version}…
            </p>
            <ProgressBar
              downloadedBytes={downloadedBytes}
              totalBytes={totalBytes}
              tone="active"
              label={`Downloading update ${version}`}
            />
            <p className="text-xs text-content-muted">
              {totalBytes === null
                ? formatBytes(downloadedBytes)
                : `${formatBytes(downloadedBytes)} of ${formatBytes(totalBytes)}`}
            </p>
          </div>
        )}

        {phase === "installing" && (
          <p role="status" className="text-sm text-content-secondary">
            Installing the update…
          </p>
        )}

        {phase === "restart-required" && (
          <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-3">
              <Badge tone="success">Installed</Badge>
              <span role="status" className="text-sm text-content-secondary">
                Version <span className="font-mono">{version}</span> is installed. Restart to
                finish.
              </span>
            </div>
            <Button size="sm" onClick={() => void restart()}>
              Restart now
            </Button>
          </div>
        )}
      </div>
    </Card>
  );
}

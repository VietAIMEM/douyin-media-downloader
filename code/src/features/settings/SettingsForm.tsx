import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";

import {
  Badge,
  Button,
  Card,
  ConfirmButton,
  ErrorNotice,
  Field,
  NumberInput,
  Select,
  TextInput,
  Toggle,
} from "@/components/ui";
import { BackendError } from "@/services/ipc";
import { useSettingsStore } from "@/stores/useSettingsStore";
import type { AppSettings, ConflictPolicy, EnumOption, LogLevel, Theme } from "@/types";

/**
 * Editor for the persisted settings.
 *
 * Edits live in a local draft; nothing is written until Save, so an invalid
 * value can never reach the database and a partially typed template is never
 * persisted. The backend validates on save and returns the stored row, which
 * replaces the draft — so what the form shows after saving is exactly what was
 * written, not an optimistic guess.
 */
export function SettingsForm() {
  const settings = useSettingsStore((state) => state.settings);
  const options = useSettingsStore((state) => state.options);
  const saving = useSettingsStore((state) => state.saving);
  const error = useSettingsStore((state) => state.error);
  const savedAt = useSettingsStore((state) => state.savedAt);
  const save = useSettingsStore((state) => state.save);
  const reset = useSettingsStore((state) => state.reset);
  const clearError = useSettingsStore((state) => state.clearError);

  const [draft, setDraft] = useState<AppSettings | null>(settings);
  /** A failure from the folder picker itself, which no backend call produced. */
  const [pickerError, setPickerError] = useState<BackendError | null>(null);

  // Re-seed the draft whenever the store's settings change (initial load, a
  // successful save, or a reset).
  useEffect(() => {
    setDraft(settings);
  }, [settings]);

  if (!draft) {
    return (
      <Card title="Preferences" description={error ? "Could not load settings" : "Loading…"}>
        {error ? (
          <ErrorNotice error={error} onDismiss={clearError} />
        ) : (
          <p className="text-xs text-content-muted">Reading settings from the database…</p>
        )}
      </Card>
    );
  }

  const dirty = settings !== null && !sameSettings(draft, settings);

  function update<K extends keyof AppSettings>(key: K, value: AppSettings[K]) {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
  }

  function onReset() {
    clearError();
    void reset();
  }

  /**
   * Open the native folder picker and write the chosen path into the draft.
   *
   * Cancelling leaves the field exactly as it was — the picker returns `null`,
   * which is not a value the user chose. Nothing here saves: the path still
   * goes through Save and the backend's own validation like anything typed by
   * hand, so the picker cannot introduce a destination the storage layer would
   * otherwise refuse.
   */
  async function chooseFolder() {
    setPickerError(null);
    try {
      const chosen = await open({
        directory: true,
        multiple: false,
        title: "Choose where downloads are saved",
        defaultPath: draft?.downloadDirectory || options?.defaultDownloadDirectory,
      });
      if (typeof chosen === "string" && chosen.length > 0) {
        update("downloadDirectory", chosen);
      }
    } catch (err) {
      setPickerError(
        new BackendError({
          kind: "unknown",
          code: "folder_picker_failed",
          message: "The folder picker could not be opened. Type the path instead.",
          details: err instanceof Error ? err.message : String(err),
        }),
      );
    }
  }

  return (
    <div className="space-y-5">
      {error && <ErrorNotice error={error} onDismiss={clearError} />}

      <Card
        title="Downloads"
        description="Where files are saved and how they are named"
      >
        <div className="space-y-5">
          <Field
            label="Download directory"
            hint={
              options && (
                <>
                  Absolute path. Default:{" "}
                  <span className="font-mono">{options.defaultDownloadDirectory}</span>
                </>
              )
            }
            trailing={
              <>
                <Button
                  type="button"
                  size="sm"
                  className="h-auto shrink-0"
                  onClick={() => void chooseFolder()}
                >
                  Browse…
                </Button>
                {options && (
                  <Button
                    type="button"
                    size="sm"
                    className="h-auto shrink-0"
                    onClick={() =>
                      update("downloadDirectory", options.defaultDownloadDirectory)
                    }
                  >
                    Use default
                  </Button>
                )}
              </>
            }
          >
            <TextInput
              value={draft.downloadDirectory}
              onChange={(event) => update("downloadDirectory", event.target.value)}
              spellCheck={false}
            />
          </Field>
          {pickerError && (
            <ErrorNotice
              error={pickerError}
              onDismiss={() => setPickerError(null)}
              title="Folder picker"
            />
          )}

          <Field
            label="File name template"
            hint={
              <>
                Without the extension. Available placeholders:{" "}
                <span className="font-mono">
                  {(options?.filenamePlaceholders ?? []).map((token) => `{${token}}`).join(" ")}
                </span>
              </>
            }
          >
            <TextInput
              value={draft.filenameTemplate}
              onChange={(event) => update("filenameTemplate", event.target.value)}
              spellCheck={false}
            />
          </Field>

          <Field
            label="Folder template"
            hint="Relative to the download directory. Use `/` between segments; `{type}` becomes Videos, Images, or Audio. Leave empty to save straight into the download directory."
          >
            <TextInput
              value={draft.folderTemplate}
              onChange={(event) => update("folderTemplate", event.target.value)}
              spellCheck={false}
            />
          </Field>

          <div className="space-y-2 border-t border-surface-border pt-4">
            <Toggle
              label="Organise by creator"
              hint="Adds a per-creator folder when the template does not already place one."
              checked={draft.organizeByCreator}
              onCheckedChange={(checked) => update("organizeByCreator", checked)}
            />
            <Toggle
              label="Organise by media type"
              hint="Adds a Videos / Images / Audio folder when the template does not already place one."
              checked={draft.organizeByMediaType}
              onCheckedChange={(checked) => update("organizeByMediaType", checked)}
            />
          </div>

          <Field
            label="If a file already exists"
            hint="Applies when a download would land on a path that is already taken."
          >
            <Select
              value={draft.conflictPolicy}
              options={enumOptions(options?.conflictPolicies)}
              onValueChange={(value) => update("conflictPolicy", value as ConflictPolicy)}
            />
          </Field>
        </div>
      </Card>

      <Card title="Performance" description="Concurrency, retries, and request timeouts">
        <div className="grid gap-5 sm:grid-cols-3">
          <Field
            label="Max concurrent downloads"
            hint={options ? `Between 1 and ${options.maxConcurrentLimit}` : undefined}
          >
            <NumberInput
              value={draft.maxConcurrentDownloads}
              min={1}
              max={options?.maxConcurrentLimit}
              onValueChange={(value) => update("maxConcurrentDownloads", value)}
            />
          </Field>

          <Field
            label="Retry attempts"
            hint={options ? `Between 0 and ${options.maxRetryAttempts}` : undefined}
          >
            <NumberInput
              value={draft.retryMaxAttempts}
              min={0}
              max={options?.maxRetryAttempts}
              onValueChange={(value) => update("retryMaxAttempts", value)}
            />
          </Field>

          <Field
            label="Request timeout (seconds)"
            hint={
              options
                ? `Between ${options.timeoutSecondsMin} and ${options.timeoutSecondsMax}`
                : undefined
            }
          >
            <NumberInput
              value={draft.requestTimeoutSeconds}
              min={options?.timeoutSecondsMin}
              max={options?.timeoutSecondsMax}
              onValueChange={(value) => update("requestTimeoutSeconds", value)}
            />
          </Field>
        </div>
      </Card>

      <Card title="Appearance" description="Theme and logging">
        <div className="space-y-5">
          <div className="grid gap-5 sm:grid-cols-2">
            <Field label="Theme" hint="“System” follows the Windows light/dark setting.">
              <Select
                value={draft.theme}
                options={enumOptions(options?.themes)}
                onValueChange={(value) => update("theme", value as Theme)}
              />
            </Field>

            <Field label="Log level" hint="Applies to new log entries; a restart is not required.">
              <Select
                value={draft.logLevel}
                options={enumOptions(options?.logLevels)}
                onValueChange={(value) => update("logLevel", value as LogLevel)}
              />
            </Field>
          </div>

          <Toggle
            label="Write logs to a file"
            hint="Rotated daily in the log directory shown on the Home screen."
            checked={draft.logToFile}
            onCheckedChange={(checked) => update("logToFile", checked)}
          />

          <Field label="Language" hint="Reserved for later localisation; stored as a tag.">
            <TextInput
              value={draft.language}
              onChange={(event) => update("language", event.target.value)}
              spellCheck={false}
            />
          </Field>
        </div>
      </Card>

      <Card
        title="Advanced"
        description="Behaviour you normally never need to change"
      >
        <Toggle
          label="Browser-assisted resolution"
          hint="Some Douyin pages only hand over a post's media to a real browser. With this on, analyzing such a link opens the page in an installed Chrome or Edge — a fresh, throwaway profile with no sign-in, unless you have turned on the login option in Profile Downloader — reads the media it obtains, and closes again. Media is still downloaded over the ordinary HTTP connection, never by the browser."
          checked={draft.browserAssistedResolution}
          onCheckedChange={(checked) => update("browserAssistedResolution", checked)}
        />
      </Card>

      <div className="flex flex-wrap items-center gap-3">
        <Button
          variant="primary"
          loading={saving}
          disabled={!dirty}
          onClick={() => void save(draft)}
        >
          Save settings
        </Button>
        <ConfirmButton
          label="Restore defaults"
          question="Restore every setting to its default value?"
          detail="Your download folder and naming templates will change."
          confirmLabel="Restore"
          variant="ghost"
          busy={saving}
          onConfirm={onReset}
        />

        {dirty ? (
          <Badge tone="warning">unsaved changes</Badge>
        ) : (
          savedAt !== null && <Badge tone="success">saved</Badge>
        )}
      </div>

      <p className="text-xs leading-relaxed text-content-muted">
        Settings are stored in the application database and checked before they are written.
      </p>
    </div>
  );
}

function enumOptions(options: EnumOption[] | undefined) {
  return (options ?? []).map((option) => ({ value: option.value, label: option.label }));
}

/** Flat value comparison; `AppSettings` contains only primitives. */
function sameSettings(a: AppSettings, b: AppSettings): boolean {
  const keys = Object.keys(a) as Array<keyof AppSettings>;
  return keys.every((key) => a[key] === b[key]);
}

/**
 * The sign-in section of the Profile Downloader.
 *
 * The whole point of it is that signing in is something the user does, in a
 * browser window this application opened but does not drive. Nothing here fills
 * a field in, answers a challenge, or asks for a password — it opens Douyin's
 * own page, waits, and then checks what the browser ended up holding.
 *
 * What it shows is therefore always the result of a check that just happened,
 * never a remembered state: "signed in" here means the browser's profile held a
 * Douyin session a moment ago, not that it once did.
 */

import { useEffect } from "react";

import { Badge, Button, Card, ConfirmButton, ErrorNotice, Toggle, type BadgeTone } from "@/components/ui";
import { useLoginStore, type LoginPhase } from "@/services/login";
import { useSettingsStore } from "@/stores/useSettingsStore";

/** What each phase says, and how it is coloured. */
const PHASES: Record<LoginPhase, { label: string; tone: BadgeTone; hint: string }> = {
  unknown: {
    label: "Not checked",
    tone: "neutral",
    hint: "Press Check status to load Douyin in the saved browser profile and see whether it recognises you.",
  },
  checking: {
    label: "Checking…",
    tone: "brand",
    hint: "Loading Douyin in the application's own browser profile to see whether it recognises you.",
  },
  signedIn: {
    label: "Logged in",
    tone: "success",
    hint: "Douyin's own page shows this profile is signed in. Whether it accepts the session for a particular profile is still up to Douyin.",
  },
  signedOut: {
    label: "Not logged in",
    tone: "neutral",
    hint: "Douyin's page asked for a sign-in. Press Login to open a window and sign in.",
  },
  unverified: {
    label: "Login status unverified",
    tone: "warning",
    hint: "Douyin's page did not say either way — it may not have finished loading, or it answered with something this app will not read as a verdict. Nothing is assumed; press Check status to ask again.",
  },
  unavailable: {
    label: "Login status unavailable",
    tone: "warning",
    hint: "No supported browser was found, so there is nothing to sign in with. Install Chrome or Edge and restart the application.",
  },
  error: {
    label: "Login status unavailable",
    tone: "warning",
    hint: "The browser could not be asked just now. Check status tries again.",
  },
};

export function LoginPanel() {
  const phase = useLoginStore((state) => state.phase);
  const windowOpen = useLoginStore((state) => state.windowOpen);
  const requiredBySource = useLoginStore((state) => state.requiredBySource);
  const profileDirectory = useLoginStore((state) => state.profileDirectory);
  const error = useLoginStore((state) => state.error);
  const refresh = useLoginStore((state) => state.refresh);
  const begin = useLoginStore((state) => state.begin);
  const clear = useLoginStore((state) => state.clear);

  const settings = useSettingsStore((state) => state.settings);
  const saveSettings = useSettingsStore((state) => state.save);
  const enabled = settings?.profileLoginEnabled ?? false;

  // Ask once, on mount, so the section opens with an answer rather than a
  // question mark. It costs a headless browser start and it is the only way to
  // know, which is the trade this whole feature is built on.
  useEffect(() => {
    void refresh();
  }, [refresh]);

  // A window the user is signing in with is a state the section has to show, and
  // it outranks the last check: the check happened before they started.
  const state = windowOpen && phase !== "signedIn" && phase !== "unavailable" ? "signingIn" : phase;

  const badge =
    state === "signingIn"
      ? { label: "Login in progress", tone: "brand" as BadgeTone }
      : PHASES[phase];

  const signedIn = phase === "signedIn";

  return (
    <Card
      title="Login"
      description="Sign in to Douyin in a window this application opens, so profile pages that need an account can be read."
    >
      <div className="space-y-4">
        <div className="flex flex-wrap items-center gap-3">
          <Badge tone={badge.tone}>{badge.label}</Badge>
          {requiredBySource && state !== "signingIn" && (
            <span className="text-xs text-content-muted">
              The last profile load was refused because Douyin wanted a signed-in visitor.
            </span>
          )}
        </div>

        <p className="text-xs leading-relaxed text-content-muted">
          {state === "signingIn"
            ? "Sign in in the window that just opened on douyin.com. When you are done, press Check status — this application never sees or stores what you type."
            : PHASES[phase].hint}
        </p>

        {error && (
          <ErrorNotice
            error={error}
            onDismiss={() => useLoginStore.getState().dismiss()}
            title="Login"
          />
        )}

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" onClick={() => void begin()}>
            Login
          </Button>
          <Button size="sm" variant="secondary" loading={phase === "checking"} onClick={() => void refresh()}>
            Check status
          </Button>
          {/* Clearing asks first: it deletes a sign-in the user may have had to
              complete a challenge to get, and there is no undo. */}
          <ConfirmButton
            label="Clear saved session"
            question="Delete the saved sign-in?"
            detail="This closes the sign-in browser and deletes the application's own browser profile. Your normal browser is not affected. You will need to sign in again to use it."
            confirmLabel="Delete saved sign-in"
            disabled={!signedIn && !windowOpen}
            onConfirm={() => void clear()}
          />
        </div>

        <div className="border-t border-surface-border pt-4">
          <Toggle
            label="Use login session when loading profiles"
            hint="Loads profiles through the saved sign-in above. Off, profiles are loaded as an anonymous visitor, which is what this application has always done."
            checked={enabled}
            onCheckedChange={(checked) => {
              if (!settings) return;
              // Saved immediately rather than with the Settings form: the
              // choice belongs to this screen, and a preference that only takes
              // effect after visiting another screen is a preference that
              // appears not to work.
              void saveSettings({ ...settings, profileLoginEnabled: checked });
            }}
          />
        </div>

        {profileDirectory && (
          <p className="text-xs leading-relaxed text-content-muted">
            The sign-in lives in this application&rsquo;s own browser profile, never in your
            everyday browser:{" "}
            <span className="break-all font-mono text-content-secondary">{profileDirectory}</span>
          </p>
        )}
      </div>
    </Card>
  );
}

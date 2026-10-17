import { useEffect, useRef, useState, type FormEvent } from "react";

import {
  Badge,
  Button,
  Card,
  EmptyState,
  ErrorNotice,
  Field,
  KeyValue,
  KeyValueList,
  TextInput,
} from "@/components/ui";
import { FOCUS_RING } from "@/lib/a11y";
import { formatDateTime } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { TestFixture, UrlClassification, UrlPreview } from "@/types";

type Action = "classify" | "expand" | "resolve";

interface UrlInspectorProps {
  /** The kind of link this screen handles; a different link is flagged. */
  expected: "post" | "profile";
  /** Copy explaining what the screen is for once a link resolves. */
  nextPhaseNote: string;
  nextPhase: string;
}

/**
 * Validate, classify, and (as far as this build can) resolve a Douyin link.
 *
 * All of the Douyin knowledge lives behind these commands — this component only
 * renders what the resolver layer reported. The two project test URLs are
 * offered as quick-fill buttons sourced from `get_test_fixtures`, so they are
 * never duplicated in TypeScript.
 */
export function UrlInspector({ expected, nextPhaseNote, nextPhase }: UrlInspectorProps) {
  const [url, setUrl] = useState("");
  const [fixtures, setFixtures] = useState<TestFixture[]>([]);
  const [classification, setClassification] = useState<UrlClassification | null>(null);
  const [preview, setPreview] = useState<UrlPreview | null>(null);
  const [error, setError] = useState<BackendError | null>(null);
  const [busy, setBusy] = useState<Action | null>(null);

  /** Once the user types, stop prefilling from the fixtures. */
  const userEdited = useRef(false);

  useEffect(() => {
    let cancelled = false;

    void (async () => {
      try {
        const list = await api.getTestFixtures();
        if (cancelled) return;
        setFixtures(list);
        // Prefill the link that matches this screen, so the page is useful
        // before the user has anything on the clipboard. Both shipped links
        // are short links, so the match is on the declared intent rather than
        // on the classification.
        setUrl((current) =>
          current === "" && !userEdited.current
            ? list.find((fixture) => fixture.intent === expected)?.url ?? ""
            : current,
        );
      } catch {
        // Fixtures are a convenience; the inspector works without them.
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [expected]);

  async function submit(action: Action) {
    const trimmed = url.trim();
    if (trimmed === "") return;

    userEdited.current = true;
    setBusy(action);
    setError(null);

    try {
      if (action === "classify") {
        setClassification(await api.classifyUrl(trimmed));
        setPreview(null);
      } else if (action === "expand") {
        setClassification(await api.expandUrl(trimmed));
        setPreview(null);
      } else {
        const resolved = await api.resolveUrlMetadata(trimmed);
        setClassification(resolved.classification);
        setPreview(resolved);
      }
    } catch (raw) {
      // A rejected link is a normal outcome here, not a broken screen: keep the
      // last good result visible and show the reason next to it.
      setError(normalizeError(raw));
    } finally {
      setBusy(null);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    void submit("classify");
  }

  const mismatch =
    classification !== null &&
    classification.kind !== "short_link" &&
    classification.kind !== expected;

  return (
    <div className="space-y-5">
      <Card>
        <form onSubmit={onSubmit} className="space-y-4">
          <Field
            label="Douyin link"
            hint={
              expected === "profile"
                ? "A creator profile link, or a short link that expands to one."
                : "A single video or image post link, or a short link that expands to one."
            }
          >
            <TextInput
              value={url}
              onChange={(event) => {
                userEdited.current = true;
                setUrl(event.target.value);
              }}
              placeholder="https://v.douyin.com/…"
              spellCheck={false}
              autoComplete="off"
            />
          </Field>

          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" variant="primary" loading={busy === "classify"}>
              Validate link
            </Button>
            <Button
              type="button"
              loading={busy === "expand"}
              disabled={classification !== null && !classification.isShortLink}
              onClick={() => void submit("expand")}
              title="Follow the short link to its real target"
            >
              Expand short link
            </Button>
            <Button
              type="button"
              loading={busy === "resolve"}
              onClick={() => void submit("resolve")}
            >
              Resolve metadata
            </Button>
          </div>

          {fixtures.length > 0 && (
            <div className="flex flex-wrap items-center gap-2 border-t border-surface-border pt-3">
              <span className="text-xs text-content-muted">Project test links</span>
              {fixtures.map((fixture) => (
                <button
                  key={fixture.url}
                  type="button"
                  onClick={() => {
                    userEdited.current = true;
                    setUrl(fixture.url);
                    setClassification(null);
                    setPreview(null);
                    setError(null);
                  }}
                  className={`rounded-md border border-surface-border px-2 py-0.5 text-xs
                    text-content-secondary transition-colors hover:bg-surface-overlay
                    hover:text-content-primary ${FOCUS_RING}`}
                >
                  {fixture.label}
                  <span className="ml-1.5 font-mono text-[10px] text-content-muted">
                    {fixture.expectedKind}
                  </span>
                </button>
              ))}
            </div>
          )}
        </form>
      </Card>

      <ErrorNotice error={error} onDismiss={() => setError(null)} />

      {classification && (
        <Card
          title="Classification"
          description="What the resolver layer made of this link"
          action={
            <div className="flex items-center gap-2">
              <Badge mono tone={classification.platform === "douyin" ? "brand" : "neutral"}>
                {classification.platform}
              </Badge>
              <Badge tone={mismatch ? "warning" : "success"}>{classification.label}</Badge>
            </div>
          }
        >
          <div className="space-y-4">
            {mismatch && (
              <p className="rounded-lg border border-warning/30 bg-warning/5 px-3 py-2 text-xs text-content-secondary">
                This is a <span className="font-medium text-content-primary">{classification.kind}</span>{" "}
                link, but this screen handles{" "}
                <span className="font-medium text-content-primary">{expected}</span> links. It was
                still classified correctly — use the other screen to work with it.
              </p>
            )}

            <KeyValueList>
              <KeyValue label="Submitted" mono>
                {classification.original}
              </KeyValue>
              <KeyValue label="Canonical" mono>
                {classification.canonical}
              </KeyValue>
              <KeyValue label="Platform id" mono>
                {classification.id ?? "—"}
              </KeyValue>
              <KeyValue label="Short link">
                {classification.isShortLink ? "yes" : "no"}
              </KeyValue>
              <KeyValue label="Needs expansion">
                {classification.requiresExpansion ? "yes" : "no"}
              </KeyValue>
              <KeyValue label="Resolver" mono>
                {classification.resolver ?? "—"}
              </KeyValue>
            </KeyValueList>
          </div>
        </Card>
      )}

      {preview && <ResolvedMetadata preview={preview} />}

      {classification && !preview && (
        <EmptyState
          title={nextPhaseNote}
          description={`Links are validated and classified here. Everything beyond that is ${nextPhase} work, so this screen reports only what it can actually determine rather than pretending to succeed.`}
          phase={nextPhase}
        />
      )}
    </div>
  );
}

function ResolvedMetadata({ preview }: { preview: UrlPreview }) {
  const { post, creator, resolutionStage } = preview;

  return (
    <Card
      title="Resolved metadata"
      description="Domain data the resolver produced for this link"
      action={<Badge tone="accent" mono>{resolutionStage}</Badge>}
    >
      {post && (
        <KeyValueList>
          <KeyValue label="Post id" mono>
            {post.platformPostId}
          </KeyValue>
          <KeyValue label="Media type">{post.mediaType}</KeyValue>
          <KeyValue label="Status">{post.status}</KeyValue>
          <KeyValue label="Caption">{post.caption ?? "—"}</KeyValue>
          <KeyValue label="Media items">{post.mediaItems.length}</KeyValue>
        </KeyValueList>
      )}

      {creator && (
        <KeyValueList>
          <KeyValue label="Creator id" mono>
            {creator.platformId}
          </KeyValue>
          <KeyValue label="Display name">{creator.displayName}</KeyValue>
          <KeyValue label="Handle">{creator.handle ?? "—"}</KeyValue>
          <KeyValue label="Verified">{creator.verified ? "yes" : "no"}</KeyValue>
          <KeyValue label="Posts">{creator.postCount ?? "unknown"}</KeyValue>
          <KeyValue label="Profile URL" mono>
            {creator.profileUrl}
          </KeyValue>
          <KeyValue label="Updated" mono>
            {formatDateTime(creator.updatedAt)}
          </KeyValue>
        </KeyValueList>
      )}
    </Card>
  );
}

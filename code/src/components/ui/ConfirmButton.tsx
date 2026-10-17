import { useEffect, useRef, useState } from "react";

import { Button } from "./Button";

interface ConfirmButtonProps {
  /** Text on the button that starts the action. */
  label: string;
  /** The question asked before it runs, in the user's own terms. */
  question: string;
  /** Extra detail: what will happen, and what will not. */
  detail?: string;
  /** Text on the button that actually performs it. */
  confirmLabel?: string;
  onConfirm: () => void;
  variant?: "primary" | "secondary" | "ghost" | "danger";
  disabled?: boolean;
  busy?: boolean;
}

/**
 * A button that asks before it does something irreversible.
 *
 * Deliberately inline rather than a modal. The application has no dialog
 * primitive and adding one — a focus trap, an overlay, a portal, an escape
 * handler — for two actions is a lot of machinery to get wrong. Asking in place
 * keeps the question next to the thing it is about, needs no focus management to
 * be correct, and cannot leave the page in a state where a modal was dismissed
 * but the page behind it is unreachable.
 *
 * What it does take seriously is the keyboard. Escape cancels, because that is
 * what Escape means; focus moves to the confirming button when the question
 * appears and returns to the trigger when it is answered, so a keyboard user is
 * never left with focus on an element that no longer exists.
 */
export function ConfirmButton({
  label,
  question,
  detail,
  confirmLabel = "Confirm",
  onConfirm,
  variant = "ghost",
  disabled = false,
  busy = false,
}: ConfirmButtonProps) {
  const [asking, setAsking] = useState(false);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLDivElement>(null);

  // Focus follows the question. Without this the confirming button appears
  // somewhere the keyboard is not, and a keyboard user has to hunt for it.
  useEffect(() => {
    if (asking) confirmRef.current?.focus();
  }, [asking]);

  const cancel = () => setAsking(false);

  // Focus returns to the trigger once the question is gone. Doing it here
  // rather than inside `cancel` is what makes it reliable: at the moment
  // `cancel` runs, the trigger has not been rendered again yet, so there would
  // be nothing to focus.
  const wasAsking = useRef(false);
  useEffect(() => {
    if (wasAsking.current && !asking) {
      triggerRef.current?.querySelector("button")?.focus();
    }
    wasAsking.current = asking;
  }, [asking]);

  // The wrapper is rendered in both states so the ref is stable, and the Escape
  // handler lives here — the key event bubbles up from whichever button inside
  // has focus.
  return (
    <div
      ref={triggerRef}
      onKeyDown={(event) => {
        if (asking && event.key === "Escape") {
          event.stopPropagation();
          cancel();
        }
      }}
      className={asking ? "inline-flex" : undefined}
    >
      {asking ? (
        <div
          role="group"
          aria-label={question}
          className="inline-flex flex-wrap items-center gap-2 rounded-lg border border-warning/40
            bg-warning/5 px-3 py-2"
        >
          <span className="text-xs text-content-primary">
            {question}
            {detail && <span className="ml-1 text-content-muted">{detail}</span>}
          </span>
          <Button
            ref={confirmRef}
            variant="danger"
            size="sm"
            loading={busy}
            onClick={() => {
              setAsking(false);
              onConfirm();
            }}
          >
            {confirmLabel}
          </Button>
          <Button variant="ghost" size="sm" onClick={cancel} disabled={busy}>
            Cancel
          </Button>
        </div>
      ) : (
        <Button variant={variant} onClick={() => setAsking(true)} disabled={disabled}>
          {label}
        </Button>
      )}
    </div>
  );
}

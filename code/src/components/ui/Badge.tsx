import type { ReactNode } from "react";

export type BadgeTone =
  | "neutral"
  | "brand"
  | "accent"
  | "success"
  | "warning"
  | "danger";

const TONES: Record<BadgeTone, string> = {
  neutral: "bg-surface-overlay text-content-secondary border-surface-border",
  brand: "bg-brand-subtle text-brand border-brand/30",
  accent: "bg-accent-subtle text-accent border-accent/30",
  success: "bg-success-subtle text-success border-success/30",
  warning: "bg-warning-subtle text-warning border-warning/30",
  danger: "bg-danger-subtle text-danger border-danger/30",
};

interface BadgeProps {
  tone?: BadgeTone;
  children: ReactNode;
  /** Use the monospace face for machine-readable values (ids, codes). */
  mono?: boolean;
  /** Layout escape hatch, e.g. `mr-1.5` when rendered inline in a sentence. */
  className?: string;
}

/**
 * A small status chip.
 *
 * Statuses are read from tone *and* text together: a badge's word is what a
 * screen reader says and what a colour-blind user matches against the legend
 * in their head, so the tone is emphasis and the word is the fact.
 */
export function Badge({
  tone = "neutral",
  mono = false,
  className = "",
  children,
}: BadgeProps) {
  return (
    <span
      className={`inline-flex items-center rounded-md border px-2 py-0.5 text-xs font-medium
        ${mono ? "font-mono" : ""} ${TONES[tone]} ${className}`}
    >
      {children}
    </span>
  );
}

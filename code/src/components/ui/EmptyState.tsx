import type { ReactNode } from "react";

interface EmptyStateProps {
  title: string;
  description?: string;
  action?: ReactNode;
  /**
   * `phase` marks a feature that is intentionally not built yet, so the UI can
   * say so plainly instead of implying something is broken.
   */
  phase?: string;
}

/**
 * An empty result, stated plainly.
 *
 * Bordered rather than dashed: a dashed border reads as a drop target, and
 * nothing here accepts a drop. Whatever brought the user here is described in
 * the title and, where there is a next step, the action is offered.
 */
export function EmptyState({ title, description, action, phase }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center rounded-xl border border-surface-border bg-surface-raised px-6 py-12 text-center">
      <p className="text-sm font-medium text-content-primary">{title}</p>
      {description && (
        <p className="mt-2 max-w-md text-xs leading-relaxed text-content-muted">{description}</p>
      )}
      {phase && (
        <p className="mt-3 text-[11px] uppercase tracking-wide text-content-muted">
          Planned for {phase}
        </p>
      )}
      {action && <div className="mt-5">{action}</div>}
    </div>
  );
}

import type { ReactNode } from "react";

interface CardProps {
  title?: ReactNode;
  description?: ReactNode;
  /** Rendered on the right of the header (buttons, badges). */
  action?: ReactNode;
  /** Render the body with no padding, for tables. */
  flush?: boolean;
  /** Layout escape hatch, e.g. `lg:col-span-2` inside a grid. */
  className?: string;
  children: ReactNode;
}

export function Card({
  title,
  description,
  action,
  flush = false,
  className = "",
  children,
}: CardProps) {
  return (
    <section className={`rounded-xl border border-surface-border bg-surface-raised ${className}`}>
      {(title || action) && (
        <header className="flex items-start justify-between gap-4 border-b border-surface-border px-5 py-3.5">
          <div className="min-w-0">
            {title && (
              <h2 className="text-sm font-semibold text-content-primary">{title}</h2>
            )}
            {description && (
              <p className="mt-1 text-xs leading-relaxed text-content-muted">{description}</p>
            )}
          </div>
          {action && <div className="shrink-0">{action}</div>}
        </header>
      )}
      <div className={flush ? "" : "px-5 py-4"}>{children}</div>
    </section>
  );
}

/** Definition list for label/value pairs, used on the diagnostics screens. */
export function KeyValueList({ children }: { children: ReactNode }) {
  return <dl className="divide-y divide-surface-border">{children}</dl>;
}

interface KeyValueProps {
  label: string;
  children: ReactNode;
  mono?: boolean;
}

export function KeyValue({ label, children, mono = false }: KeyValueProps) {
  return (
    <div className="flex items-baseline justify-between gap-6 py-2 first:pt-0 last:pb-0">
      <dt className="shrink-0 text-xs text-content-muted">{label}</dt>
      <dd
        className={`min-w-0 break-all text-right text-xs text-content-primary ${
          mono ? "font-mono" : ""
        }`}
      >
        {children}
      </dd>
    </div>
  );
}

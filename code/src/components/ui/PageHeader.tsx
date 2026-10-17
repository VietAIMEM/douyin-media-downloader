import type { ReactNode } from "react";

interface PageHeaderProps {
  title: string;
  description?: string;
  /** Badges or buttons shown beside the title. */
  children?: ReactNode;
}

/**
 * The heading of a screen.
 *
 * One h1 per screen, stated once; the TopBar no longer repeats it. The
 * description is capped so a long paragraph cannot stretch the page, and the
 * header stays aligned with the content column below it.
 */
export function PageHeader({ title, description, children }: PageHeaderProps) {
  return (
    <header className="mb-6">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-xl font-semibold tracking-tight text-content-primary">{title}</h1>
        {children}
      </div>
      {description && (
        <p className="mt-1.5 max-w-3xl text-sm leading-relaxed text-content-secondary">
          {description}
        </p>
      )}
    </header>
  );
}

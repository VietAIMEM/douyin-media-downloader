import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md";

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  loading?: boolean;
  icon?: ReactNode;
}

const VARIANTS: Record<Variant, string> = {
  primary:
    "bg-brand text-content-inverse hover:bg-brand-hover focus-visible:outline-brand disabled:bg-brand/50",
  secondary:
    "bg-surface-raised text-content-primary border border-surface-border hover:bg-surface-overlay focus-visible:outline-brand",
  ghost:
    "bg-transparent text-content-secondary hover:bg-surface-overlay hover:text-content-primary focus-visible:outline-brand",
  danger:
    "bg-transparent text-danger border border-danger/40 hover:bg-danger/10 focus-visible:outline-danger",
};

const SIZES: Record<Size, string> = {
  sm: "h-8 px-3 text-xs gap-1.5",
  md: "h-10 px-4 text-sm gap-2",
};

/**
 * The classes a button of this shape uses.
 *
 * Exported so a non-`<button>` element that must *look* like one — a router
 * link that navigates to another screen, say — can share the exact same
 * styling instead of keeping a second copy of it in step.
 */
export function buttonClasses(
  variant: Variant = "secondary",
  size: Size = "md",
  className = "",
): string {
  return `inline-flex items-center justify-center rounded-lg font-medium transition-colors
    focus-visible:outline focus-2 focus-visible:outline-offset-2
    disabled:cursor-not-allowed disabled:opacity-60
    ${VARIANTS[variant]} ${SIZES[size]} ${className}`;
}

/**
 * A button.
 *
 * Forwards its ref so a caller can move focus to it — `ConfirmButton` does, to
 * put the keyboard where the question is.
 */
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "secondary",
    size = "md",
    loading = false,
    icon,
    className = "",
    disabled,
    children,
    ...rest
  },
  ref,
) {
  return (
    <button
      {...rest}
      ref={ref}
      disabled={disabled || loading}
      className={buttonClasses(variant, size, className)}
    >
      {loading ? <Spinner /> : icon}
      {children}
    </button>
  );
});

function Spinner() {
  return (
    <span
      aria-hidden
      className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
    />
  );
}

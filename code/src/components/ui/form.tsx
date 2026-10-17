import {
  cloneElement,
  isValidElement,
  useId,
  type InputHTMLAttributes,
  type ReactElement,
  type ReactNode,
  type SelectHTMLAttributes,
} from "react";

const CONTROL = `w-full rounded-lg border border-surface-border bg-surface px-3 py-2 text-sm
  text-content-primary placeholder:text-content-muted
  focus:border-brand focus:outline-none focus:ring-1 focus:ring-brand
  disabled:cursor-not-allowed disabled:opacity-60`;

interface FieldProps {
  label: string;
  /** Explains the setting; also where validation limits are stated. */
  hint?: ReactNode;
  /** Validation message from the backend, when this field was rejected. */
  error?: string;
  children: ReactNode;
  /**
   * Controls that sit *beside* the field's control — a browse button, a reset.
   *
   * A prop rather than something the caller wraps around the control, because
   * the label is wired to the child and a wrapper would take its place.
   */
  trailing?: ReactNode;
}

/**
 * A labelled form control.
 *
 * The control is linked to its own label, hint, and error message by id rather
 * than by wrapping it in a `<label>`. Wrapping works visually but gives a screen
 * reader the label as the control's name and nothing else: the hint and the
 * error go unannounced, because an error is a *sibling* the control has no
 * relationship to. `aria-describedby` is that relationship.
 *
 * A rejected field is also marked `aria-invalid`, so the state is available
 * without having to hear the message first.
 */
export function Field({ label, hint, error, children, trailing }: FieldProps) {
  const id = useId();
  const hintId = `${id}-hint`;
  const errorId = `${id}-error`;

  // The control is the only child in every use in this application. Passing the
  // wiring down this way keeps the call sites unchanged: a caller writes
  // `<Field label="..."><TextInput .../></Field>` and gets the association for
  // free, rather than having to invent an id and repeat it on both elements —
  // which is exactly the bookkeeping that gets skipped.
  //
  // The id is set even when there is nothing to describe. A `<label for>` that
  // points at no element is not a label at all: the field would have no
  // accessible name, which is the one thing a form control cannot do without.
  //
  // Which is also why `trailing` exists as a prop rather than a caller wrapping
  // the control in its own `<div>` to sit something beside it: the label is
  // wired to *this* child, and a wrapper silently takes its place, leaving the
  // control unnamed. Anything that belongs next to the control but is not the
  // control goes here.
  const describedBy = error ? errorId : hint ? hintId : undefined;
  const control = isValidElement(children)
    ? cloneElement(children as ReactElement<Record<string, unknown>>, {
        id,
        ...(describedBy ? { "aria-describedby": describedBy } : {}),
        ...(error ? { "aria-invalid": true } : {}),
      })
    : children;

  return (
    <div className="block">
      <label htmlFor={id} className="text-xs font-medium text-content-primary">
        {label}
      </label>
      <div className="mt-1.5">
        {trailing ? (
          <div className="flex gap-2">
            {control}
            {trailing}
          </div>
        ) : (
          control
        )}
      </div>
      {error ? (
        <p id={errorId} role="alert" className="mt-1.5 text-xs text-danger">
          {error}
        </p>
      ) : (
        hint && (
          <p id={hintId} className="mt-1.5 text-xs leading-relaxed text-content-muted">
            {hint}
          </p>
        )
      )}
    </div>
  );
}

export function TextInput(props: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={`${CONTROL} ${props.className ?? ""}`} />;
}

interface NumberInputProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "onChange"> {
  value: number;
  onValueChange: (value: number) => void;
  min?: number;
  max?: number;
}

/**
 * Numeric input that reports `NaN`-free values.
 *
 * An empty or unparseable box reports the last valid number rather than zero, so
 * clearing the field to retype does not momentarily push an invalid value into
 * state.
 */
export function NumberInput({ value, onValueChange, min, max, ...rest }: NumberInputProps) {
  return (
    <input
      {...rest}
      type="number"
      min={min}
      max={max}
      value={Number.isFinite(value) ? value : ""}
      onChange={(event) => {
        const parsed = Number.parseInt(event.target.value, 10);
        if (!Number.isNaN(parsed)) onValueChange(parsed);
        else if (event.target.value === "") onValueChange(min ?? 0);
      }}
      className={`${CONTROL} ${rest.className ?? ""}`}
    />
  );
}

interface SelectOption {
  value: string;
  label: string;
}

interface SelectProps extends Omit<SelectHTMLAttributes<HTMLSelectElement>, "onChange"> {
  value: string;
  options: SelectOption[];
  onValueChange: (value: string) => void;
}

export function Select({ value, options, onValueChange, ...rest }: SelectProps) {
  return (
    <select
      {...rest}
      value={value}
      onChange={(event) => onValueChange(event.target.value)}
      className={`${CONTROL} ${rest.className ?? ""}`}
    >
      {options.map((option) => (
        <option key={option.value} value={option.value}>
          {option.label}
        </option>
      ))}
    </select>
  );
}

interface ToggleProps {
  label: string;
  hint?: string;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}

export function Toggle({ label, hint, checked, onCheckedChange }: ToggleProps) {
  return (
    <label className="flex items-start justify-between gap-4 py-1">
      <span className="min-w-0">
        <span className="block text-xs font-medium text-content-primary">{label}</span>
        {hint && (
          <span className="mt-1 block text-xs leading-relaxed text-content-muted">{hint}</span>
        )}
      </span>
      <input
        type="checkbox"
        checked={checked}
        onChange={(event) => onCheckedChange(event.target.checked)}
        className="mt-0.5 h-4 w-4 shrink-0 accent-brand"
      />
    </label>
  );
}

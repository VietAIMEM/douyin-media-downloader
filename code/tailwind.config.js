/**
 * Colour tokens resolve to CSS variables declared in `src/index.css`.
 *
 * The palette is semantic rather than literal (`surface-raised`, not
 * `grey-900`) and the variables are swapped per theme, so components never
 * carry a light/dark branch. `rgb(var(--x) / <alpha-value>)` keeps Tailwind's
 * opacity modifiers (`bg-danger/10`, `border-brand/30`) working.
 *
 * `shell` is the navigation chrome; `nav-*` carries the sidebar's own text
 * scale so the chrome can read well on its darker ground in both themes.
 */
const token = (name) => `rgb(var(--color-${name}) / <alpha-value>)`;

/** @type {import('tailwindcss').Config} */
export default {
  darkMode: "class",
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        shell: {
          DEFAULT: token("shell"),
          raised: token("shell-raised"),
          border: token("shell-border"),
        },
        surface: {
          DEFAULT: token("surface"),
          raised: token("surface-raised"),
          overlay: token("surface-overlay"),
          border: token("surface-border"),
        },
        brand: {
          DEFAULT: token("brand"),
          hover: token("brand-hover"),
          subtle: token("brand-subtle"),
        },
        accent: {
          DEFAULT: token("accent"),
          subtle: token("accent-subtle"),
        },
        success: { DEFAULT: token("success"), subtle: token("success-subtle") },
        warning: { DEFAULT: token("warning"), subtle: token("warning-subtle") },
        danger: { DEFAULT: token("danger"), subtle: token("danger-subtle") },
        content: {
          primary: token("content-primary"),
          secondary: token("content-secondary"),
          muted: token("content-muted"),
          inverse: token("content-inverse"),
        },
        nav: {
          primary: token("nav-primary"),
          secondary: token("nav-secondary"),
          active: token("nav-active"),
          "active-subtle": token("nav-active-subtle"),
        },
      },
      fontFamily: {
        sans: ["Inter", "Segoe UI", "system-ui", "sans-serif"],
        mono: ["Cascadia Code", "Consolas", "monospace"],
      },
    },
  },
  plugins: [],
};

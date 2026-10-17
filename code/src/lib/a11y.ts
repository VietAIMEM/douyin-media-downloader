/**
 * Shared interaction-quality class strings.
 *
 * The `<Button>` component carries the application's focus treatment in one
 * place, but a few controls are deliberately not buttons from that component:
 * the view tabs, the test-link chips, and a notice's dismiss control. They are
 * ordinary `<button>` elements with their own shape, and each of them had
 * forgotten the focus ring — which is invisible to anyone using a mouse and
 * leaves everyone else unable to tell where they are.
 *
 * A constant rather than a component because these controls share nothing else:
 * their sizes, colours, and behaviour differ, and wrapping them in a common
 * component purely to share one class would be a worse trade.
 */

/**
 * The focus indicator for a control that is not `<Button>`.
 *
 * `focus-visible` rather than `focus`, so it appears for keyboard navigation and
 * not on a mouse click. The outline colour follows the theme's brand token, so
 * it stays legible in both light and dark.
 */
export const FOCUS_RING =
  "focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand";

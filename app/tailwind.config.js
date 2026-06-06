/**
 * Tailwind config for the plan-ai-usb dashboard.
 *
 * Consumes ONLY the Tailwind layer of plan-ai-design: the color tokens below
 * mirror the `--c-*` CSS variables that `third_party/plan-ai-design/assets/input.css`
 * defines under `:root`/`.dark`, so the design system's `@apply bg-surface`,
 * `text-fg-muted`, etc. resolve. No Rust/Dioxus is involved.
 */
const c = (v) => `rgb(var(--c-${v}) / <alpha-value>)`;

module.exports = {
  darkMode: 'class',
  // Scan our renderer markup + the design crate's components so every class the
  // design emits is generated and the look matches the rest of plan.ai.
  content: [
    './renderer/**/*.{html,js}',
    '../third_party/plan-ai-design/src/**/*.rs',
  ],
  theme: {
    extend: {
      colors: {
        canvas: c('bg'),
        surface: {
          DEFAULT: c('surface'),
          2: c('surface-2'),
          3: c('surface-3'),
          strong: c('surface-strong'),
        },
        fg: {
          DEFAULT: c('fg'),
          muted: c('fg-muted'),
          faint: c('fg-faint'),
          strong: c('fg-strong'),
          invert: c('fg-invert'),
        },
        brand: { DEFAULT: c('brand'), soft: c('brand-soft'), strong: c('brand-strong') },
        accent: { DEFAULT: c('accent'), soft: c('accent-soft'), strong: c('accent-strong') },
        danger: { DEFAULT: c('danger'), soft: c('danger-soft'), strong: c('danger-strong') },
        success: { DEFAULT: c('success'), soft: c('success-soft') },
        warn: { DEFAULT: c('warn'), soft: c('warn-soft'), strong: c('warn-strong') },
        info: { DEFAULT: c('info'), soft: c('info-soft') },
        line: { DEFAULT: c('line'), soft: c('line-soft') },
      },
      // Custom elevation tokens the design system references (defined in the
      // consumer's config upstream; mirrored here to match the hairline look).
      boxShadow: {
        card: '0 1px 2px 0 rgb(0 0 0 / 0.04), 0 1px 3px 0 rgb(0 0 0 / 0.06)',
        pop: '0 8px 24px -6px rgb(0 0 0 / 0.18), 0 2px 6px -2px rgb(0 0 0 / 0.12)',
      },
    },
  },
  plugins: [],
};

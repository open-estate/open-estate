# Open Estate — standing instructions for agents

These are product preferences the owner has asked us to remember. Apply them to every change.

## Generated videos follow the viewer's system theme; report modals match the report page
- The video tour (`assets/report_motion.js`, canvas + modal chrome), exported `.webm` files, artifact canvases
  and standalone video/story surfaces must match the viewer's OS color scheme via `prefers-color-scheme`:
  a light theme when the system is light, a dark theme when it is dark.
- React to live changes (`matchMedia('(prefers-color-scheme: dark)').addEventListener('change', …)`); never
  hard-code an always-dark (or always-light) theme. Exports use the theme active when the export starts.
- Light themes are real light designs (white/frosted cards, dark ink, frosted light modal washes instead of
  dark navy scrims over light pages). Keep both themes readable over aerial imagery and verify both with
  `browser.newContext({ colorScheme: 'light' | 'dark' })` screenshots.
- The walkthrough slide deck (`#walkthroughModal`) belongs to the light report page, so it stays light with a
  frosted light wash backdrop rather than switching on `prefers-color-scheme: dark`.

## Data honesty
- Every number shown comes from the report; when data is missing, leave it out — never invent it.

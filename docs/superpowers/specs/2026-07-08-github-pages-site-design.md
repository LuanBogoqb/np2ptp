# NP2PTP GitHub Pages Site — Design

## Problem

NP2PTP currently only presents itself through its GitHub README — plain text,
no visual identity, nothing a non-technical visitor would land on and quickly
understand. The goal is a small, fast, visually distinct landing page hosted
on GitHub Pages, giving the project a proper front door.

## Scope

A single static landing page. No live data, no backend, no build step —
GitHub Pages serves the files as-is. Content is drawn from the existing
README/docs (already written this session), re-presented visually rather than
duplicated with new claims.

Out of scope: a docs mirror, a blog, multi-page navigation, any client-side
fetch to the tracker or any other live service.

## Hosting

- Served from the `web` branch (already created as an orphan branch — no
  shared history with `main`/`dev`, contains only site files, nothing from the
  Rust workspace).
- GitHub Pages configured to build from `web`, root folder, via `gh api`
  (`POST /repos/{owner}/{repo}/pages` to create it if not yet enabled, or
  `PUT` to update the source branch if it already exists) — done once as
  part of implementation, confirmed working by checking the Pages API
  response and the resulting URL.
- Planning artifacts (this spec, the implementation plan) live on `dev`'s
  `docs/superpowers/` tree, matching this project's established convention —
  keeps `web` limited to publishable site content only, per direct instruction.

## Visual Design (approved via the visual-companion mockups)

**Direction: "Warm Paper."** Cream/off-white background in light mode, a warm
dark background in dark mode, switching automatically via
`prefers-color-scheme` — no manual toggle, so nobody is stuck in the wrong
mode.

- **Typography:** serif for the main headline (system serif stack — Georgia /
  Times New Roman fallback, no webfont download), sans-serif for body text
  (system UI stack — `-apple-system, "Segoe UI", Roboto, sans-serif`).
- **Accent color:** warm amber/rust. `#b5562b` on the light background,
  `#e8834f` on the dark background (brighter, so it doesn't wash out against
  the darker surface — same hue family, tuned per-mode contrast).
- **Backgrounds:** light `#f7f2ea` (cream), dark `#1c1815` (warm near-black,
  not pure black). Body text: `#2b2620` light / `#efe6d8` dark.
- No webfonts, no icon font, no JS framework — keeps first paint fast and the
  page workable with JS disabled (progressive: the only JS is smooth-scroll
  navigation and a mobile nav toggle, both non-essential to reading the page).

## Page Structure (top to bottom)

0. **Nav bar** (sticky, thin) — "NP2PTP" wordmark on the left; in-page anchor
   links (How it works, Examples, Research) in the middle; a GitHub mark-link
   and a **"Download"** button on the right. This is the main repeated
   "get back to the repo" affordance — collapses to a hamburger menu on
   narrow viewports (the one place `script.js`'s mobile-nav toggle applies).
1. **Hero** — "NP2PTP" headline (serif), one-line pitch, and two CTA buttons:
   **"View on GitHub"** (links to the repo) and **"Download latest release"**
   (links to `https://github.com/LuanBogoqb/np2ptp/releases/latest` — the
   GitHub releases page itself, not a specific asset file, so it always shows
   whatever platforms/assets the latest tag actually published; the page
   already lists Windows/Linux clearly, so there is no need to guess the
   visitor's OS or hardcode a filename that could drift from what CI
   produces).
2. **Problems it targets** — the three pain points from the README (NAT
   traversal, permanence/incentives, integrity/dedup), presented as three
   short cards, not a numbered list.
3. **How it works** — a plain-language pass over content addressing
   (BLAKE3 + Merkle), content-defined chunking, RaptorQ, and relay/NAT
   traversal — no code, no jargon dump, aimed at someone who has never seen
   the README.
4. **Example commands** — `pack` / `serve` / `fetch` shown as styled code
   blocks (static text, not a live terminal), pulled from `docs/USAGE.md`'s
   existing examples.
5. **Research numbers** — the `np2ptp-sim` results table (dedup %, permanence,
   free-riding, FEC cost) from `docs/EXAMPLES.md`, presented as a small stat
   grid rather than a raw markdown table.
6. **Teaser** — a short, low-key section hinting at a native GUI (C#) in the
   works, without committing to a timeline or feature list — something like
   "the CLI is just the start — a proper desktop app is brewing." No mockups,
   no roadmap, no signup form; this is a wink, not an announcement, and it
   should read as one (light, a little playful, not a marketing promise).
7. **Footer** — links to the GitHub repo, releases, and the docs
   (USAGE/EXAMPLES/BUILDING) already published on `main` — the third and
   last repo-link touchpoint, after the nav bar and the hero.

## File Structure

All at the root of the `web` branch (GitHub Pages serves root by default):

- `index.html` — the whole page; sections above as one document (no
  multi-page routing needed for a single landing page).
- `style.css` — all styling, including both color-scheme variants via
  `@media (prefers-color-scheme: dark)`.
- `script.js` — smooth-scroll for the in-page nav links and a mobile nav
  toggle; nothing else. The page must be fully readable with this file
  absent (progressive enhancement only).
- `favicon.svg` — a simple monogram (an "N" mark, or an abstract node/mesh
  glyph) in the amber/rust accent color. SVG favicons need no build step and
  are supported by every current browser; no PNG fallback is needed for this
  project's audience (developers).
- `.gitignore` — ignore OS cruft (`.DS_Store`, etc.) since this branch has no
  inherited one (it is orphaned).
- `CNAME` — **not** included; the project does not have a custom domain today
  (uses the default `<owner>.github.io/np2ptp` URL). Add later if that
  changes.

## Responsiveness

Single-column, fluid layout using flexbox/grid and relative units; a handful
of media query breakpoints for the stat grid and problem cards collapsing to
one column on narrow viewports. No separate mobile template.

## Testing / Verification

There is no automated test suite for a static HTML page. Verification is
manual, done during implementation:

- Open `index.html` locally in a browser (no server needed — plain
  file:// load must work, since GitHub Pages just serves static files).
- Confirm both light and dark mode render correctly (toggle the OS/browser
  color-scheme preference, or use the browser devtools' rendering-emulation
  panel).
- Confirm the page is usable with JavaScript disabled (devtools).
- Confirm all links (GitHub repo, releases, docs) resolve to the right,
  already-published targets on `main`.
- After deploying, load the live GitHub Pages URL and repeat the same checks
  there.

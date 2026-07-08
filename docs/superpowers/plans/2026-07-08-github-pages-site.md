# NP2PTP GitHub Pages Site Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and publish a static GitHub Pages landing page for NP2PTP on the `web` branch — "Warm Paper" visual direction, light/dark auto-switching, plain HTML/CSS/minimal-JS, no build step.

**Architecture:** A single `index.html` (nav, hero, problems, how-it-works, examples, research stats, GUI teaser, footer) styled by one `style.css` using CSS custom properties for the two color-scheme variants, plus a small `script.js` for the mobile nav toggle and in-page smooth scroll. All content is static; nothing calls any backend.

**Tech Stack:** Plain HTML5, CSS3 (custom properties, flexbox/grid, `prefers-color-scheme`), vanilla JS (no framework, no bundler). Hosted by GitHub Pages directly from the `web` branch root.

## Global Constraints

- No JS framework, no CSS framework, no build step, no webfonts — system font stacks only. GitHub Pages must serve the files exactly as committed.
- The page must be fully readable and navigable with JavaScript disabled (the only JS is the mobile nav toggle and smooth-scroll — both non-essential).
- Colors: light `--bg: #f7f2ea`, `--text: #2b2620`, `--text-muted: #5c5346`, `--accent: #b5562b`, `--bg-card: #ffffff`, `--border: #e8dfd0`. Dark (via `prefers-color-scheme: dark`): `--bg: #1c1815`, `--text: #efe6d8`, `--text-muted: #b3a693`, `--accent: #e8834f`, `--bg-card: #262019`, `--border: #3a332a`. No manual light/dark toggle.
- Fonts: headings `Georgia, "Times New Roman", serif`; body `-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif`; code `"SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace`.
- All external links point at `https://github.com/LuanBogoqb/np2ptp` (repo), `https://github.com/LuanBogoqb/np2ptp/releases/latest` (download), and the three docs already published on `main` (`docs/USAGE.md`, `docs/EXAMPLES.md`, `docs/BUILDING.md`).
- Work happens on the `web` branch (an orphan branch already created, containing nothing but site files — do not bring in any Rust workspace files). Planning artifacts (this plan, the spec) live on `dev`, not `web`.
- Spec: `docs/superpowers/specs/2026-07-08-github-pages-site-design.md` (on the `dev` branch) — re-read it if anything below is ambiguous.

---

### Task 1: Build the complete static site

**Files:**
- Create: `index.html` (repo root, `web` branch)
- Create: `style.css` (repo root, `web` branch)
- Create: `script.js` (repo root, `web` branch)
- Create: `favicon.svg` (repo root, `web` branch)
- Create: `.gitignore` (repo root, `web` branch)
- Create: `.nojekyll` (repo root, `web` branch) — empty file; tells GitHub Pages to serve files as-is instead of running Jekyll processing, which this project doesn't use and doesn't need.

**Interfaces:**
- Consumes: nothing (this is the first task; no earlier code to integrate with).
- Produces: the whole page. No later task depends on any Rust/CLI interface — this is pure static content.

- [ ] **Step 1: Create `index.html`**

Working directory must be the `web` branch checkout. Write exactly:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>NP2PTP — New Peer-To-Peer Transfer Protocol</title>
  <meta name="description" content="A research prototype rethinking BitTorrent: content-addressed, permanent, verified.">
  <link rel="icon" type="image/svg+xml" href="favicon.svg">
  <link rel="stylesheet" href="style.css">
</head>
<body>
  <nav class="navbar">
    <div class="nav-inner">
      <a href="#" class="nav-brand">NP2PTP</a>
      <button class="nav-toggle" aria-label="Toggle menu" aria-expanded="false">☰</button>
      <div class="nav-links">
        <a href="#how-it-works">How it works</a>
        <a href="#examples">Examples</a>
        <a href="#research">Research</a>
        <a href="https://github.com/LuanBogoqb/np2ptp" aria-label="GitHub repository">GitHub</a>
        <a href="https://github.com/LuanBogoqb/np2ptp/releases/latest" class="nav-download">Download</a>
      </div>
    </div>
  </nav>

  <header class="hero">
    <h1>NP2PTP</h1>
    <p class="hero-tagline">Torrents, rebuilt: dedup, permanence, and integrity that actually hold up.</p>
    <div class="hero-ctas">
      <a href="https://github.com/LuanBogoqb/np2ptp" class="btn btn-primary">View on GitHub</a>
      <a href="https://github.com/LuanBogoqb/np2ptp/releases/latest" class="btn btn-secondary">Download latest release</a>
    </div>
  </header>

  <section class="problems">
    <h2>What it fixes</h2>
    <div class="card-grid">
      <div class="problem-card">
        <h3>NAT &amp; connectivity</h3>
        <p>Too many peers can't accept inbound connections. NP2PTP falls back to a public relay automatically — no port forwarding, no configuration.</p>
      </div>
      <div class="problem-card">
        <h3>Permanence &amp; incentives</h3>
        <p>Content dies when seeders leave, and seeding earns nothing. Erasure coding survives churn; a persistent reputation ledger rewards contribution.</p>
      </div>
      <div class="problem-card">
        <h3>Integrity &amp; dedup</h3>
        <p>Coarse verification and no cross-content deduplication. Every chunk is content-addressed and hash-verified — a lying peer is caught immediately.</p>
      </div>
    </div>
  </section>

  <section id="how-it-works" class="how-it-works">
    <h2>How it works</h2>
    <div class="how-grid">
      <div class="how-item">
        <h3>Content addressing</h3>
        <p>Every file is split into content-defined chunks and hashed with BLAKE3 into a Merkle tree. The root of that tree <em>is</em> the content's identity — a link like <code>np2ptp:e0cf...</code> is all you ever need to fetch and verify it.</p>
      </div>
      <div class="how-item">
        <h3>Deduplication</h3>
        <p>Because chunk boundaries are defined by content, not fixed offsets, an edited file mostly reuses its old chunks. Identical files anywhere — even across unrelated downloads — are stored and transferred exactly once.</p>
      </div>
      <div class="how-item">
        <h3>Permanence</h3>
        <p>RaptorQ erasure coding turns a file into redundant symbols: any sufficiently large subset reconstructs the whole thing, so content survives seeders leaving without anyone re-uploading it whole.</p>
      </div>
      <div class="how-item">
        <h3>Reachability</h3>
        <p>Built on libp2p/QUIC with DHT discovery. Behind CGNAT or a closed router? NP2PTP tries UPnP, then NAT-PMP, then falls back to a public relay automatically.</p>
      </div>
    </div>
  </section>

  <section id="examples" class="examples">
    <h2>Three commands</h2>
    <p class="section-sub">Link a file, seed it, fetch it — that's the whole workflow.</p>
    <div class="code-stack">
      <div class="code-block">
        <div class="code-label">Link what you want to share</div>
        <pre><code>np2ptp pack myfile.zip --out myfile.nptp</code></pre>
      </div>
      <div class="code-block">
        <div class="code-label">Make it available on the network</div>
        <pre><code>np2ptp serve myfile.nptp</code></pre>
      </div>
      <div class="code-block">
        <div class="code-label">Download it, anywhere</div>
        <pre><code>np2ptp fetch np2ptp:abc123... --out ./downloaded</code></pre>
      </div>
    </div>
    <a href="https://github.com/LuanBogoqb/np2ptp/blob/main/docs/USAGE.md" class="link-more">Full usage guide →</a>
  </section>

  <section id="research" class="research">
    <h2>Does it actually work better?</h2>
    <p class="section-sub">Measured with a research harness that compares against a plain-chunk baseline — not just claimed.</p>
    <div class="stat-grid">
      <div class="stat">
        <div class="stat-value">~49%</div>
        <div class="stat-label">chunks deduplicated on a lightly-edited file</div>
      </div>
      <div class="stat">
        <div class="stat-value">Survives</div>
        <div class="stat-label">seeder departure — only with re-sharing enabled</div>
      </div>
      <div class="stat">
        <div class="stat-value">Cut off</div>
        <div class="stat-label">a free-riding leech, under the reputation choke</div>
      </div>
      <div class="stat">
        <div class="stat-value">~110ms</div>
        <div class="stat-label">erasure-coded download vs. ~107ms plain chunk (1MB)</div>
      </div>
    </div>
  </section>

  <section class="teaser">
    <p>The CLI is just the start — a proper desktop app is brewing. 👀</p>
  </section>

  <footer class="footer">
    <div class="footer-links">
      <a href="https://github.com/LuanBogoqb/np2ptp">GitHub</a>
      <a href="https://github.com/LuanBogoqb/np2ptp/releases/latest">Releases</a>
      <a href="https://github.com/LuanBogoqb/np2ptp/blob/main/docs/USAGE.md">Usage</a>
      <a href="https://github.com/LuanBogoqb/np2ptp/blob/main/docs/EXAMPLES.md">Examples</a>
      <a href="https://github.com/LuanBogoqb/np2ptp/blob/main/docs/BUILDING.md">Building</a>
    </div>
    <p class="footer-note">A research prototype. Not a production client.</p>
  </footer>

  <script src="script.js"></script>
</body>
</html>
```

- [ ] **Step 2: Create `style.css`**

Write exactly:

```css
:root {
  --bg: #f7f2ea;
  --bg-card: #ffffff;
  --text: #2b2620;
  --text-muted: #5c5346;
  --accent: #b5562b;
  --border: #e8dfd0;
  --font-serif: Georgia, "Times New Roman", serif;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  --font-mono: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
  --max-width: 960px;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1c1815;
    --bg-card: #262019;
    --text: #efe6d8;
    --text-muted: #b3a693;
    --accent: #e8834f;
    --border: #3a332a;
  }
}

* { box-sizing: border-box; }

body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: var(--font-sans);
  line-height: 1.6;
}

h1, h2, h3 { font-family: var(--font-serif); font-weight: 700; margin: 0 0 0.5em; color: var(--text); }
h2 { font-size: 1.8rem; }
h3 { font-size: 1.15rem; }
p { margin: 0 0 1em; color: var(--text-muted); }
a { color: var(--accent); }
code, pre { font-family: var(--font-mono); }

/* Nav */
.navbar {
  position: sticky;
  top: 0;
  z-index: 10;
  background: var(--bg);
  border-bottom: 1px solid var(--border);
}
.nav-inner {
  max-width: var(--max-width);
  margin: 0 auto;
  padding: 14px 24px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  position: relative;
}
.nav-brand { font-family: var(--font-serif); font-weight: 700; font-size: 1.2rem; text-decoration: none; color: var(--text); }
.nav-links { display: flex; align-items: center; gap: 24px; }
.nav-links a { text-decoration: none; color: var(--text-muted); font-size: 0.9rem; }
.nav-links a:hover { color: var(--text); }
.nav-download {
  background: var(--accent);
  color: #fff !important;
  padding: 6px 14px;
  border-radius: 6px;
}
.nav-toggle { display: none; background: none; border: none; font-size: 1.4rem; color: var(--text); cursor: pointer; }

@media (max-width: 720px) {
  .nav-toggle { display: block; }
  .nav-links {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    background: var(--bg);
    border-bottom: 1px solid var(--border);
    flex-direction: column;
    align-items: flex-start;
    padding: 16px 24px;
    gap: 16px;
    display: none;
  }
  .nav-links.open { display: flex; }
}

/* Hero */
.hero { max-width: var(--max-width); margin: 0 auto; padding: 96px 24px 64px; text-align: center; }
.hero h1 { font-size: 3rem; }
.hero-tagline { font-size: 1.15rem; max-width: 560px; margin: 0 auto 32px; }
.hero-ctas { display: flex; gap: 16px; justify-content: center; flex-wrap: wrap; }
.btn { display: inline-block; padding: 12px 24px; border-radius: 8px; text-decoration: none; font-weight: 600; font-size: 0.95rem; }
.btn-primary { background: var(--text); color: var(--bg); }
.btn-secondary { border: 1.5px solid var(--accent); color: var(--accent); }

/* Sections */
section { max-width: var(--max-width); margin: 0 auto; padding: 64px 24px; }
section h2 { text-align: center; margin-bottom: 12px; }
.section-sub { text-align: center; max-width: 480px; margin: 0 auto 40px; }

.card-grid, .how-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 24px; margin-top: 40px; }
.how-grid { grid-template-columns: repeat(2, 1fr); }
.problem-card, .how-item { background: var(--bg-card); border: 1px solid var(--border); border-radius: 10px; padding: 24px; }

@media (max-width: 720px) {
  .card-grid, .how-grid { grid-template-columns: 1fr; }
}

.code-stack { display: flex; flex-direction: column; gap: 16px; margin-top: 32px; }
.code-block { background: var(--bg-card); border: 1px solid var(--border); border-radius: 10px; overflow: hidden; }
.code-label { padding: 10px 18px; font-size: 0.85rem; color: var(--text-muted); border-bottom: 1px solid var(--border); }
.code-block pre { margin: 0; padding: 16px 18px; overflow-x: auto; }
.code-block code { color: var(--accent); font-size: 0.9rem; }
.link-more { display: block; text-align: center; margin-top: 24px; font-weight: 600; text-decoration: none; }

.stat-grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 24px; margin-top: 16px; text-align: center; }
.stat-value { font-family: var(--font-serif); font-size: 1.6rem; font-weight: 700; color: var(--accent); }
.stat-label { font-size: 0.85rem; margin-top: 6px; }

@media (max-width: 720px) {
  .stat-grid { grid-template-columns: repeat(2, 1fr); }
}

.teaser { text-align: center; padding: 40px 24px; border-top: 1px solid var(--border); border-bottom: 1px solid var(--border); }
.teaser p { font-style: italic; font-size: 1rem; margin: 0; }

.footer { text-align: center; padding: 48px 24px; }
.footer-links { display: flex; justify-content: center; gap: 20px; flex-wrap: wrap; margin-bottom: 16px; }
.footer-links a { text-decoration: none; font-size: 0.9rem; }
.footer-note { font-size: 0.8rem; }
```

- [ ] **Step 3: Create `script.js`**

Write exactly:

```js
document.addEventListener("DOMContentLoaded", () => {
  const toggle = document.querySelector(".nav-toggle");
  const links = document.querySelector(".nav-links");
  if (toggle && links) {
    toggle.addEventListener("click", () => {
      const isOpen = links.classList.toggle("open");
      toggle.setAttribute("aria-expanded", String(isOpen));
    });
  }

  document.querySelectorAll('a[href^="#"]').forEach((link) => {
    link.addEventListener("click", (e) => {
      const targetId = link.getAttribute("href");
      // Skip the bare "#" on the nav brand — not a real anchor, and
      // `document.querySelector("#")` throws (invalid selector).
      if (!targetId || targetId === "#") {
        return;
      }
      const target = document.querySelector(targetId);
      if (target) {
        e.preventDefault();
        target.scrollIntoView({ behavior: "smooth" });
        if (links) {
          links.classList.remove("open");
        }
      }
    });
  });
});
```

- [ ] **Step 4: Create `favicon.svg`**

Write exactly:

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
  <rect width="32" height="32" rx="7" fill="#b5562b"/>
  <text x="16" y="22" font-family="Georgia, serif" font-size="18" font-weight="700" fill="#f7f2ea" text-anchor="middle">N</text>
</svg>
```

- [ ] **Step 5: Create `.gitignore`**

Write exactly:

```
.DS_Store
Thumbs.db
```

- [ ] **Step 6: Create `.nojekyll`**

Create an empty file named `.nojekyll` at the repo root (zero bytes — its mere presence is what matters to GitHub Pages).

- [ ] **Step 7: Manual verification (no automated test suite exists for a static page)**

Run:
```sh
git branch --show-current
```
Expected: `web` (if not, STOP — do not proceed on the wrong branch).

Open `index.html` directly in a browser via `file://` (double-click it, or `start index.html` on Windows / `open index.html` on macOS). Confirm:
- The page renders with no console errors (open devtools, check the Console tab).
- Nav bar, hero, all three "What it fixes" cards, all four "How it works" items, all three example command blocks, all four research stats, the teaser line, and the footer links are all visible and readable.
- Resize the browser window down to ~375px wide: the nav collapses to a hamburger (☰); clicking it reveals the links; the card grids and stat grid collapse to fewer columns; nothing overflows horizontally.
- Using devtools' rendering emulation (or your OS's dark/light toggle), confirm both `prefers-color-scheme: light` and `prefers-color-scheme: dark` render with the colors specified in Global Constraints — text stays readable in both, the accent color is legible against both backgrounds.
- Disable JavaScript (devtools → Settings → Debugger → "Disable JavaScript", or via `about:preferences` in Firefox) and reload: every section is still visible and readable; only the mobile nav toggle and smooth-scroll behavior stop working (anchor links still jump, just without the smooth animation).
- Click every link (GitHub, Download, the three docs links, the in-page nav anchors) and confirm each resolves to the correct already-published target.

- [ ] **Step 8: Commit**

```bash
git add index.html style.css script.js favicon.svg .gitignore .nojekyll
git commit -m "Add the NP2PTP landing page (Warm Paper, light/dark, no build step)

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: Deploy to GitHub Pages

**Files:** none (this task is git/GitHub configuration, not code).

**Interfaces:**
- Consumes: Task 1's committed `index.html` + `style.css` + `script.js` + `favicon.svg` on the `web` branch.
- Produces: a live GitHub Pages URL (`https://<owner>.github.io/np2ptp/` unless a custom domain is configured, which is explicitly out of scope per the spec).

- [ ] **Step 1: Push the `web` branch**

```bash
git push origin web
```

Expected: `web` branch created on `origin` (first push) or fast-forwarded (subsequent pushes).

- [ ] **Step 2: Enable GitHub Pages via the API**

Check whether Pages is already configured:
```bash
gh api repos/LuanBogoqb/np2ptp/pages 2>&1
```

If that 404s (Pages not yet enabled), create it:
```bash
gh api repos/LuanBogoqb/np2ptp/pages -X POST -f "source[branch]=web" -f "source[path]=/"
```

If it already exists (e.g. from a previous run of this task) but points at a different branch, update it instead:
```bash
gh api repos/LuanBogoqb/np2ptp/pages -X PUT -f "source[branch]=web" -f "source[path]=/"
```

- [ ] **Step 3: Verify the API accepted it**

```bash
gh api repos/LuanBogoqb/np2ptp/pages 2>&1
```

Expected: JSON response with `"source":{"branch":"web","path":"/"}` and a `"html_url"` field — that URL is the live site.

- [ ] **Step 4: Wait for the first build and verify live**

GitHub Pages builds asynchronously after enabling/updating. Poll:
```bash
gh api repos/LuanBogoqb/np2ptp/pages/builds/latest 2>&1
```
Expected: `"status":"built"` (if `"status":"building"`, wait ~30s and re-check; if `"status":"errored"`, read the `"error"` field and fix the reported problem before re-running this step).

Once built, open the `html_url` from Step 3 in a browser and repeat the same checks from Task 1 Step 7 (rendering, responsive collapse, light/dark, JS-disabled, all links) against the **live** URL — a local `file://` check can pass while the deployed version has a path or MIME-type issue a local load wouldn't surface.

- [ ] **Step 5: Report the live URL**

No commit in this step (nothing to add — the site content was already committed in Task 1, and Pages configuration is a GitHub repo setting, not a file in the repo). Record the confirmed live URL in your final report so the human partner has it without needing to re-derive it.

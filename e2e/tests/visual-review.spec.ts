/**
 * Nightly visual review: screenshots every route scout.spec.ts already knows
 * how to reach, at a mobile and a desktop viewport, so an agent can look at
 * each one and report what a deterministic assertion cannot catch — clipped
 * content, an unlabelled control, a raw decimal where a formatted amount
 * belongs. This file only captures; scripts/visual-review.mjs does the
 * looking, against the manifest written below.
 *
 * Advisory-only, not part of the gating suite: ci.yml's `e2e` job runs
 * `npx playwright test` with no filter, so without the gate below this file
 * would run on every push and capture screenshots nobody reviews. Run it
 * explicitly with E2E_VISUAL_REVIEW=true, as the scheduled workflow does.
 */
import { test as base, expect, type Page } from '@playwright/test';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { setupVirtualAuthenticator, register } from '../fixtures/auth';
import { createStoreAndOpen } from '../fixtures/stores';
import {
  UNAUTHENTICATED_ROUTES,
  AUTHENTICATED_ROUTES,
  TAB_STATES,
  STORE_DETAIL,
  tabCaptureName,
  captureGaps,
  expectedCaptures,
  unstyledReason,
} from '../fixtures/visual-review-routes';

const RUN = process.env.E2E_VISUAL_REVIEW === 'true';
const SKIP_REASON = 'Nightly-only: set E2E_VISUAL_REVIEW=true to run (see scripts/visual-review.mjs)';

const VISUAL_DIR = path.join('test-results', 'visual');

const VIEWPORTS = [
  { name: 'mobile', width: 375, height: 812 },
  { name: 'desktop', width: 1280, height: 720 },
] as const;

// The routes scout.spec.ts is already known to reach unauthenticated and
// after a single passkey registration. Hand-listed rather than imported —
// scout's paths live inline in each test body, not behind an export — but
// route-coverage.spec.ts reads scout.spec.ts's source and fails if a route it
// reaches is missing from these lists, so the two can drift for a commit but
// not past CI.

interface ManifestEntry {
  route: string;
  path: string;
  viewport: string;
  file: string | null;
  error: string | null;
  /** Set when the captured view names classes the stylesheet never defines. */
  unstyled?: string;
}

const manifest: ManifestEntry[] = [];

let sharedPage: Page;

const test = base.extend({});
test.describe.configure({ mode: 'serial' });

test.beforeAll(async ({ browser }) => {
  // `test.skip(!RUN, ...)` inside each test body is a runtime skip — Playwright
  // still runs the describe's beforeAll/afterAll either way. Without this guard,
  // every unfiltered `npx playwright test` (i.e. ci.yml's `e2e` job, on every
  // push) opens a browser context and writes an empty manifest.json into
  // test-results/visual/, contradicting "a screenshot pass nobody reviews has
  // no business slowing down every push."
  if (!RUN) return;
  fs.mkdirSync(VISUAL_DIR, { recursive: true });
  const ctx = await browser.newContext();
  sharedPage = await ctx.newPage();
});

test.afterAll(async () => {
  if (!RUN) return;
  fs.writeFileSync(path.join(VISUAL_DIR, 'manifest.json'), JSON.stringify(manifest, null, 2));
  // The denominator for the judge: how many routes this run set out to
  // capture, so a partial review cannot read as a complete one.
  fs.writeFileSync(path.join(VISUAL_DIR, 'expected-routes.json'), JSON.stringify({ routes: expectedCaptures() }, null, 2));
  console.log(`\n=== VISUAL REVIEW: ${manifest.length} capture(s), ${manifest.filter(m => m.error).length} error(s) ===`);
  for (const m of manifest) {
    console.log(m.error ? `  [ERROR] ${m.route} (${m.viewport}): ${m.error}` : `  ${m.route} (${m.viewport}) -> ${m.file}`);
  }
  await sharedPage?.context().close();
});

// One navigation, one screenshot. Never throws — a route that fails to load
// is a manifest entry with an error, not a reason to abandon the rest of the
// walk. Serial mode skips every later test after a thrown error, and this
// spec exists to name problems, not to stop at the first one.
async function capture(route: string, urlPath: string) {
  for (const vp of VIEWPORTS) {
    const file = `${route}-${vp.name}.png`;
    const entry: ManifestEntry = { route, path: urlPath, viewport: vp.name, file: null, error: null };
    try {
      await sharedPage.setViewportSize({ width: vp.width, height: vp.height });
      await sharedPage.goto(urlPath);
      await sharedPage.waitForLoadState('networkidle', { timeout: 15_000 }).catch(() => {});
      await sharedPage.screenshot({ path: path.join(VISUAL_DIR, file), fullPage: true });
      entry.file = file;
    } catch (err) {
      entry.error = err instanceof Error ? err.message : String(err);
    }
    manifest.push(entry);
  }
}

test.describe('Unauthenticated routes', () => {
  test('capture login and register', async () => {
    test.skip(!RUN, SKIP_REASON);
    for (const [route, urlPath] of UNAUTHENTICATED_ROUTES) {
      await capture(route, urlPath);
    }
  });
});

test.describe('Authenticated routes', () => {
  // One registration, reused for every authenticated capture — the same
  // shape as scout's `register with passkey` test. Registering once per
  // route the way some of the older specs do hits the auth-tier rate limit
  // (5 req/min/IP) against a remote origin; see e2e/README.md.
  test('register once, then capture the sidebar routes', async () => {
    test.skip(!RUN, SKIP_REASON);

    try {
      await setupVirtualAuthenticator(sharedPage);
      await register(sharedPage);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      // 'register-flow', not 'register': the /register *page* capture above
      // already owns the 'register' key, and groupByRoute buckets by that
      // string alone. Reusing it here would make a flaky passkey ceremony
      // read, in the report, as a defect on the /register page itself.
      manifest.push({
        route: 'register-flow',
        path: '/register',
        viewport: 'n/a',
        file: null,
        error: message,
      });
      // Registration gates every authenticated capture below — record each
      // route as uncaptured rather than letting it vanish from the manifest.
      // A vanished route and a clean route both read as "nothing to report";
      // an explicit error entry is the only way to tell them apart.
      for (const [route, urlPath] of AUTHENTICATED_ROUTES) {
        manifest.push({
          route,
          path: urlPath,
          viewport: 'n/a',
          file: null,
          error: `not captured — passkey registration failed: ${message}`,
        });
      }
      return;
    }

    for (const [route, urlPath] of AUTHENTICATED_ROUTES) {
      await capture(route, urlPath);
    }

    // States behind one URL: a sidebar crawl lands on the default tab and
    // never sees the others. Each tab is clicked and captured at both viewports.
    await captureStoreTabs();
  });
});

// Opens the page that owns a tab group and returns its URL. A TAB_STATES entry
// for a route with no opener here fails with that message, rather than being
// promised by expectedCaptures() and then never taken.
async function openTabOwner(route: string): Promise<string> {
  if (route !== STORE_DETAIL) throw new Error(`no opener is defined for '${route}' - add one to openTabOwner`);
  await sharedPage.setViewportSize({ width: VIEWPORTS[1].width, height: VIEWPORTS[1].height });
  await createStoreAndOpen(sharedPage, `Visual Review ${Date.now()}`);
  return sharedPage.url();
}

async function captureStoreTabs() {
  const urls = new Map<string, string>();
  for (const t of TAB_STATES) {
    const name = tabCaptureName(t);
    if (!urls.has(t.route)) {
      try {
        urls.set(t.route, await openTabOwner(t.route));
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        urls.set(t.route, '');
        manifest.push({ route: name, path: t.route, viewport: 'n/a', file: null, error: `not captured - could not open the page: ${message}` });
        continue;
      }
    }
    const url = urls.get(t.route)!;
    if (!url) {
      manifest.push({ route: name, path: t.route, viewport: 'n/a', file: null, error: 'not captured - the page this tab belongs to could not be opened' });
      continue;
    }
    for (const vp of VIEWPORTS) {
      const file = `${name}-${vp.name}.png`;
      const entry: ManifestEntry = { route: name, path: url, viewport: vp.name, file: null, error: null };
      try {
        await sharedPage.setViewportSize({ width: vp.width, height: vp.height });
        await sharedPage.goto(url);
        // Exact label: a substring match would also hit a tab whose label merely contains it.
        const tab = sharedPage.locator('.store-tab').filter({ hasText: new RegExp(`^\\s*${t.label}\\s*$`) });
        await tab.click();
        // A click that lands on nothing would photograph the default tab under this one's name.
        await expect(tab, `'${t.label}' did not become the active tab`).toHaveClass(/\bactive\b/);
        await sharedPage.waitForLoadState('networkidle', { timeout: 15_000 }).catch(() => {});
        const { classes, selectors } = await sharedPage.evaluate(() => {
          const panel = document.querySelector('.store-tab-content');
          const classes = panel ? [panel, ...panel.querySelectorAll('*')].flatMap((e) => [...e.classList]) : [];
          const selectors: string[] = [];
          for (const sheet of document.styleSheets) {
            try {
              for (const rule of sheet.cssRules) selectors.push((rule as CSSStyleRule).selectorText ?? rule.cssText);
            } catch {
              // cross-origin sheet: its rules are unreadable, not absent
            }
          }
          return { classes, selectors: selectors.join(' ') };
        });
        if (classes.length === 0) throw new Error('the tab panel rendered no elements');
        entry.unstyled = unstyledReason(classes, selectors) ?? undefined;
        await sharedPage.screenshot({ path: path.join(VISUAL_DIR, file), fullPage: true });
        entry.file = file;
      } catch (err) {
        entry.error = err instanceof Error ? err.message : String(err);
      }
      manifest.push(entry);
    }
  }
}

// Coverage gates; the model's judgement of what it sees does not. Every
// capture this suite promises must exist at both viewports, so a run that
// reached nothing (or only three pages) fails instead of reporting no findings.
test.describe('Coverage', () => {
  test('every promised capture was taken', () => {
    test.skip(!RUN, SKIP_REASON);
    const gaps = captureGaps(manifest, VIEWPORTS.map((v) => v.name));
    if (gaps.length > 0) throw new Error(`${gaps.length} promised capture(s) missing: ${gaps.join(', ')}`);
  });

  // Whether a view is styled is a question with a right answer, so it gates
  // too; how the styled view looks stays the model's advisory judgement.
  test('no captured view is unstyled', () => {
    test.skip(!RUN, SKIP_REASON);
    const bad = manifest.filter((m) => m.unstyled).map((m) => `${m.route}@${m.viewport}: ${m.unstyled}`);
    if (bad.length > 0) throw new Error(`unstyled view(s): ${bad.join('; ')}`);
  });
});

// The static "does scout.spec.ts's walk still match these route lists" check
// used to live here, but this file's `beforeAll` above requests the `browser`
// fixture unconditionally — Playwright launches Chromium to satisfy that the
// moment it's named as a parameter, even on a run where every test below
// skips itself. A check with no browser dependency of its own has no business
// dragging that launch in just by sharing a file, so it now lives in
// route-coverage.spec.ts, which imports these route lists instead of
// reading them out of a file it would otherwise have to load.

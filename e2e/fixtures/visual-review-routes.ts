/**
 * Route lists shared between the nightly visual-review capture
 * (visual-review.spec.ts) and the static check that keeps them in sync with
 * scout.spec.ts's walk (route-coverage.spec.ts). Kept in their own module
 * rather than defined in either spec file: visual-review.spec.ts's
 * `test.beforeAll` requests the `browser` fixture, which Playwright resolves
 * — and launches Chromium for — the moment it's named as a parameter, even
 * though the hook body returns immediately when E2E_VISUAL_REVIEW isn't set.
 * Importing that file from the coverage check would drag that launch along
 * for a test that has no browser dependency of its own and needs none to run
 * at PR time.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';

export const UNAUTHENTICATED_ROUTES: [string, string][] = [
  ['login', '/login'],
  ['register', '/register'],
];

export const AUTHENTICATED_ROUTES: [string, string][] = [
  ['dashboard', '/evm'],
  ['stores', '/evm/stores'],
  ['invoices', '/evm/invoices'],
  ['payments', '/evm/payments'],
  ['wallets', '/evm/wallets'],
  ['settings', '/evm/settings'],
];

// ---------------------------------------------------------------------------
// Router-derived coverage
//
// The lists above are what the sidebar offers. The client's router declares
// more pages than that, and some views are not routes at all, so coverage is
// checked against the router's own list rather than against what a crawl found.
// ---------------------------------------------------------------------------

/** Every route the client router declares (a copy of the client's checked-in list). */
export function readClientRoutes(): string[] {
  return fs
    .readFileSync(path.join(__dirname, 'client-routes.txt'), 'utf8')
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('#'));
}

/** Capture name of the store detail page, which owns the tab states below. */
export const STORE_DETAIL = 'store-detail';

/**
 * In-page states that sit behind one URL. Hand-written, because unlike routes
 * there is nothing to derive them from, and deliberately incomplete: tabs,
 * accordions and modals elsewhere are not listed. Add an entry here to capture
 * another; it is not a statement that nothing else exists.
 */
export const TAB_STATES: { route: string; tab: string; label: string }[] = [
  { route: STORE_DETAIL, tab: 'general', label: 'General' },
  { route: STORE_DETAIL, tab: 'payment-methods', label: 'Payment Methods' },
  { route: STORE_DETAIL, tab: 'webhooks', label: 'Webhooks' },
  { route: STORE_DETAIL, tab: 'settings', label: 'Settings' },
];

/**
 * Classes in `classes` that no selector in `selectorText` mentions. `selectorText`
 * is every selector of every loaded stylesheet, joined.
 */
export function classesWithoutRules(classes: string[], selectorText: string): string[] {
  return [...new Set(classes)].filter((c) => {
    const escaped = c.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    return !new RegExp(`\\.${escaped}(?![\\w-])`).test(selectorText);
  });
}

/**
 * A captured view whose markup names classes the stylesheet never defines is
 * unstyled, whatever the model judging the screenshot makes of it. Deterministic
 * so it can gate: a view must name at least three distinct classes before it is
 * judged at all (a sparse view proves nothing), and is unstyled when more than
 * half of them have no rule. Returns the reason, or null when the view is styled.
 */
export function unstyledReason(classes: string[], selectorText: string): string | null {
  const distinct = [...new Set(classes)];
  if (distinct.length < 3) return null;
  const missing = classesWithoutRules(distinct, selectorText);
  if (missing.length * 2 <= distinct.length) return null;
  return `${missing.length} of ${distinct.length} classes have no stylesheet rule: ${missing.slice(0, 8).join(', ')}`;
}

/** Names of the captures made for a route's tabs, as they appear in the manifest. */
export const tabCaptureName = (t: { route: string; tab: string }) => `${t.route}-${t.tab}`;

/**
 * How each router path is covered: the capture name it produces, or the reason
 * it is not captured. A router path in neither state fails the static check, so
 * adding a page forces a decision instead of leaving it silently unvisited.
 */
export const ROUTE_DISPOSITION: Record<string, { captured: string } | { uncovered: string }> = {
  '/login': { captured: 'login' },
  '/register': { captured: 'register' },
  '/': { captured: 'dashboard' },
  '/evm': { captured: 'dashboard' },
  '/evm/invoices': { captured: 'invoices' },
  '/evm/payments': { captured: 'payments' },
  '/evm/stores': { captured: 'stores' },
  '/evm/stores/:id': { captured: `${STORE_DETAIL}-general` },
  '/evm/wallets': { captured: 'wallets' },
  '/evm/settings': { captured: 'settings' },
  '/checkout/:id': { uncovered: 'needs a live invoice id; scout.spec.ts walks it with a stand-in' },
  '/evm/invoices/:id': { uncovered: 'needs an invoice created by a fixture' },
  '/evm/payments/:id': { uncovered: 'needs a payment, which needs a funded chain' },
  '/evm/wallets/:id': { uncovered: 'needs a wallet created by a fixture' },
  '/evm/plugins/:id/:path': { uncovered: 'exists only when a plugin declares a page' },
};

/** Router paths with neither a capture nor a stated reason, and dispositions naming no router path. */
export function dispositionGaps(routes: string[]): { undecided: string[]; stale: string[] } {
  return {
    undecided: routes.filter((r) => !(r in ROUTE_DISPOSITION)),
    stale: Object.keys(ROUTE_DISPOSITION).filter((r) => !routes.includes(r)),
  };
}

/** Every capture name this suite promises to produce. */
export function expectedCaptures(): string[] {
  return [
    ...new Set([
      ...UNAUTHENTICATED_ROUTES.map(([n]) => n),
      ...AUTHENTICATED_ROUTES.map(([n]) => n),
      ...TAB_STATES.map(tabCaptureName),
    ]),
  ];
}

/**
 * Expected (capture, viewport) pairs the manifest holds no screenshot for. An
 * empty manifest misses everything, so a run that captured nothing fails here
 * instead of reporting no findings.
 */
export function captureGaps(
  manifest: { route: string; viewport: string; file: string | null }[],
  viewports: readonly string[],
): string[] {
  const have = new Set(manifest.filter((m) => m.file).map((m) => `${m.route}@${m.viewport}`));
  return expectedCaptures().flatMap((r) => viewports.map((v) => `${r}@${v}`)).filter((k) => !have.has(k));
}

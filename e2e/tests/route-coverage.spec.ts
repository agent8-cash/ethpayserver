/**
 * Static, no browser: reads scout.spec.ts's source and checks every route it
 * navigates to is also captured by visual-review.spec.ts. Split into its own
 * file (rather than living in visual-review.spec.ts, which is where this
 * check used to be) so it has no fixture dependency of its own — see
 * fixtures/visual-review-routes.ts's doc comment for why importing
 * visual-review.spec.ts directly would have dragged a Chromium launch along
 * for a check that needs none. That split is also what lets this run in the
 * e2e-static-checks CI job at PR time rather than only once a branch lands on
 * main/testnet or a release tag.
 */
import { test, expect } from '@playwright/test';
import * as fs from 'node:fs';
import * as path from 'node:path';
import {
  UNAUTHENTICATED_ROUTES,
  AUTHENTICATED_ROUTES,
  ROUTE_DISPOSITION,
  TAB_STATES,
  STORE_DETAIL,
  readClientRoutes,
  dispositionGaps,
  expectedCaptures,
  captureGaps,
} from '../fixtures/visual-review-routes';

test('scout.spec.ts route coverage stays in sync with visual-review.spec.ts', () => {
  const scoutSrc = fs.readFileSync(path.join('tests', 'scout.spec.ts'), 'utf8');
  const reached = new Set([...scoutSrc.matchAll(/goto(?:Authed)?\('([^']+)'\)/g)].map((m) => m[1]));

  // A regex that matches nothing (formatter switches quote style, a route
  // becomes a template literal, scout.spec.ts gets renamed) makes `reached`
  // empty and `missing` trivially [] — the same "0 missing" result as
  // actually being in sync. Assert the parse actually found routes before
  // trusting its diff, so a broken extractor fails loudly instead of
  // reading as nothing-to-add.
  expect(reached.size, 'route extraction from scout.spec.ts found nothing — the regex no longer matches').toBeGreaterThan(0);

  // Not nav routes: /checkout/:id is a per-invoice page (there is no generic
  // "the" checkout page to screenshot), and /evm/nonexistent is scout's
  // deliberate 404 check, not a page this review should judge on its merits.
  reached.delete('/checkout/00000000-0000-0000-0000-000000000000');
  reached.delete('/evm/nonexistent');

  const known = new Set([...UNAUTHENTICATED_ROUTES, ...AUTHENTICATED_ROUTES].map(([, p]) => p));
  const missing = [...reached].filter((p) => !known.has(p));
  expect(
    missing,
    `scout.spec.ts reaches ${missing.join(', ')} but visual-review.spec.ts's route lists do not — add it there`,
  ).toHaveLength(0);
});

test('every route the client router declares is captured or has a stated reason it is not', () => {
  const routes = readClientRoutes();
  // An empty list would make both differences empty - the same result as being in sync.
  expect(routes.length, 'fixtures/client-routes.txt yielded no routes').toBeGreaterThanOrEqual(10);
  const { undecided, stale } = dispositionGaps(routes);
  expect(undecided, `routes with no capture and no stated reason: add them to ROUTE_DISPOSITION`).toEqual([]);
  expect(stale, `ROUTE_DISPOSITION names routes the router no longer declares`).toEqual([]);

  // A claimed capture must be one this suite actually makes.
  const made = new Set(expectedCaptures());
  for (const [route, d] of Object.entries(ROUTE_DISPOSITION)) {
    if ('captured' in d) expect(made.has(d.captured), `${route} claims capture '${d.captured}' which nothing makes`).toBe(true);
  }
});

test('an empty or short manifest is a coverage failure, not a pass', () => {
  const vps = ['mobile', 'desktop'];
  const all = expectedCaptures().length * vps.length;
  expect(captureGaps([], vps)).toHaveLength(all);

  const complete = expectedCaptures().flatMap((route) => vps.map((viewport) => ({ route, viewport, file: 'x.png' })));
  expect(captureGaps(complete, vps)).toEqual([]);
  // One screenshot missing, or taken but failed (no file), must be named.
  expect(captureGaps(complete.slice(1), vps)).toHaveLength(1);
  expect(captureGaps([{ ...complete[0], file: null }, ...complete.slice(1)], vps)).toHaveLength(1);
});

test('store detail tabs are all in the capture list', () => {
  expect(TAB_STATES.filter((t) => t.route === STORE_DETAIL).map((t) => t.label)).toEqual([
    'General',
    'Payment Methods',
    'Webhooks',
    'Settings',
  ]);
});

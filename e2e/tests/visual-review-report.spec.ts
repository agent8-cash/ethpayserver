/**
 * Unit tests for the pure shaping logic behind the nightly visual review:
 * scripts/visual-review-report.mjs decides what counts as an error vs a
 * finding and how the two render. Nothing here exercises a browser or the
 * Anthropic API, so — like route-coverage.spec.ts — it isn't gated behind
 * E2E_VISUAL_REVIEW and runs both whenever the `e2e` job does (pushes to
 * main/testnet and release tags) and, since neither file needs a browser or a
 * live server, in the e2e-static-checks job at PR time (see ci.yml).
 *
 * The property that matters most: a run that broke and a run that came back
 * clean must never render as the same string. That's the one thing this
 * report exists to guarantee, and it's the one thing a human skimming a
 * nightly issue has no other way to notice if it silently stopped holding.
 */
import { test, expect } from '@playwright/test';
import { groupByRoute, renderReport, crashOutputs, renderPublicSummary } from '../scripts/visual-review-report.mjs';

test.describe('groupByRoute', () => {
  test('buckets a captured shot by route and pushes a failed capture into errors, not routes', () => {
    const errors: unknown[] = [];
    const manifest = [
      { route: 'dashboard', path: '/evm', viewport: 'mobile', file: 'dashboard-mobile.png', error: null },
      { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: null, error: 'timeout' },
    ];

    const routes = groupByRoute(manifest, errors);

    expect(routes.has('dashboard')).toBe(true);
    expect(routes.has('wallets')).toBe(false);
    expect(errors).toEqual([{ route: 'wallets', viewport: 'mobile', reason: 'capture failed: timeout' }]);
  });

  test('a route with one captured viewport and one failed viewport keeps the capture and records the failure', () => {
    const errors: unknown[] = [];
    const manifest = [
      { route: 'settings', path: '/evm/settings', viewport: 'mobile', file: 'settings-mobile.png', error: null },
      { route: 'settings', path: '/evm/settings', viewport: 'desktop', file: null, error: null },
    ];

    const routes = groupByRoute(manifest, errors);

    expect(routes.get('settings')?.shots).toHaveLength(1);
    expect(routes.get('settings')?.shots[0].viewport).toBe('mobile');
    expect(errors).toEqual([{ route: 'settings', viewport: 'desktop', reason: 'capture failed' }]);
  });
});

test.describe('renderReport', () => {
  test('a clean run and an errors-only run never render as the same report', () => {
    const clean = renderReport([], [], 6);
    const incomplete = renderReport([], [{ route: 'wallets', viewport: 'mobile', reason: 'capture failed' }], 5);

    expect(clean).not.toBe(incomplete);
    expect(clean).toContain('No issues found across 6 route(s).');
    expect(incomplete).not.toContain('No issues found');
    expect(incomplete).toContain('could not be reviewed — treat this run as incomplete, not clean');
    expect(incomplete).toContain('No findings among the 5 route(s) that were reviewed.');
  });

  // The defect this count exists for: with no reviewed count to print, the
  // errors-only branch said "No findings among the routes that were
  // reviewed" on nineteen consecutive nightly runs that reviewed nothing.
  test('a run that reviewed nothing says so, and never reads as a run with no findings', () => {
    const nothing = renderReport([], [{ route: null, viewport: null, reason: 'the review did not run' }], 0);
    const clean = renderReport([], [], 6);
    const partial = renderReport([], [{ route: 'wallets', viewport: 'mobile', reason: 'capture failed' }], 5);

    expect(nothing).toContain('No route was reviewed');
    expect(nothing).not.toContain('No issues found');
    expect(nothing).not.toContain('No findings among');
    expect(nothing).not.toBe(clean);
    expect(nothing).not.toBe(partial);
  });

  test('findings and errors together render both, not one instead of the other', () => {
    const report = renderReport(
      [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' }],
      [{ route: 'settings', viewport: 'desktop', reason: 'capture failed' }],
      5,
    );

    expect(report).toContain('could not be reviewed — treat this run as incomplete, not clean');
    expect(report).toContain('settings (desktop): capture failed');
    expect(report).toContain('1 finding(s) across 1 route(s).');
    expect(report).toContain('/evm/wallets (wallets)');
    expect(report).toContain('raw decimal shown for balance');
  });

  test('a pipeline-level error with no route still renders distinctly from a clean run', () => {
    const report = renderReport([], [{ route: null, viewport: null, reason: 'no manifest — review did not run' }], 0);

    expect(report).toContain('pipeline: no manifest — review did not run');
    expect(report).not.toContain('No issues found');
  });
});

test.describe('renderPublicSummary', () => {
  test('a clean run with nothing to report is null, not an empty issue body', () => {
    expect(renderPublicSummary([], [], 'https://example.invalid/run')).toBeNull();
  });

  test('carries what_is_wrong text — the rubric only covers cosmetic UI defects, not vulnerabilities', () => {
    const summary = renderPublicSummary(
      [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' }],
      [],
      'https://example.invalid/run',
      6,
    );

    expect(summary).not.toBeNull();
    expect(summary).toContain('raw decimal shown for balance');
    expect(summary).toContain('1 finding(s) across 1 route(s)');
    expect(summary).toContain('/evm/wallets (wallets)');
  });

  test('groups findings per route without collapsing distinct routes', () => {
    const summary = renderPublicSummary(
      [
        { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'x' },
        { route: 'wallets', path: '/evm/wallets', viewport: 'desktop', what_is_wrong: 'y' },
        { route: 'settings', path: '/evm/settings', viewport: 'mobile', what_is_wrong: 'z' },
      ],
      [],
      'https://example.invalid/run',
      6,
    );

    expect(summary).toContain('3 finding(s) across 2 route(s)');
    expect(summary).toContain('/evm/wallets (wallets)');
    expect(summary).toContain('/evm/settings (settings)');
  });

  test('an errors-only run still reports something, distinct from a clean run', () => {
    const summary = renderPublicSummary([], [{ route: 'wallets', viewport: 'mobile', reason: 'capture failed' }], 'https://example.invalid/run', 5);

    expect(summary).not.toBeNull();
    expect(summary).toContain('could not be reviewed — treat this run as incomplete, not clean');
    expect(summary).toContain('wallets (mobile): capture failed');
    expect(summary).toContain('No findings among the 5 route(s) that were reviewed.');
  });

  // The issue body is the only thing anyone reads, and nineteen of them were
  // text backed by no judgement. A run that reviewed nothing publishes
  // NOTHING - not its errors either; the step fails and the private log
  // carries the reason.
  test('a run that reviewed nothing publishes nothing at all', () => {
    expect(
      renderPublicSummary(
        [],
        [{ route: null, viewport: null, reason: 'CLAUDE_CODE_OAUTH_TOKEN is not set — the review did not run' }],
        'https://example.invalid/run',
        0,
      ),
    ).toBeNull();

    // Every capture failing is the same nothing by a different route.
    expect(
      renderPublicSummary(
        [],
        [{ route: 'wallets', viewport: 'mobile', reason: 'capture failed' }],
        'https://example.invalid/run',
        0,
      ),
    ).toBeNull();
  });

  test('the run link is labelled when a note is given, so a 404 is not mistaken for a bad link', () => {
    const findings = [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'x' }];
    const labelled = renderPublicSummary(findings, [], 'https://example.invalid/run/42', 6, 'Private to the ops repository.');
    const bare = renderPublicSummary(findings, [], 'https://example.invalid/run/42', 6);

    expect(labelled).toContain('Run: https://example.invalid/run/42');
    expect(labelled).toContain('Private to the ops repository.');
    expect(bare).toContain('Run: https://example.invalid/run/42');
    expect(bare).not.toContain('Private to the ops repository.');
  });

  test('carries the run URL so the issue still points back to the run that produced it', () => {
    const summary = renderPublicSummary([{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'x' }], [], 'https://example.invalid/run/42', 6);

    expect(summary).toContain('https://example.invalid/run/42');
  });
});

test.describe('crashOutputs', () => {
  test('turns an unexpected throw into a non-empty error, never an empty findings.json', () => {
    const { findings, errors } = crashOutputs(new Error('fetch is not defined'));

    expect(findings).toEqual([]);
    expect(errors).toHaveLength(1);
    expect(errors[0].reason).toContain('visual-review.mjs crashed');
    expect(errors[0].reason).toContain('fetch is not defined');

    const report = renderReport(findings, errors, 0);
    expect(report).not.toContain('No issues found');
    expect(report).toContain('treat this run as incomplete, not clean');
  });

  test('handles a thrown non-Error value without losing the crash signal', () => {
    const { findings, errors } = crashOutputs('manifest.json is not valid JSON');

    expect(findings).toEqual([]);
    expect(errors[0].reason).toContain('manifest.json is not valid JSON');
  });
});

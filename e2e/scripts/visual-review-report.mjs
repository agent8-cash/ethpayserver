/**
 * The pure shaping logic behind visual-review.mjs's report: which manifest
 * entries are findings vs errors, and how the two render. Split out so it can
 * be unit-tested without importing visual-review.mjs itself, which runs
 * main() (an API call) as a module-load side effect.
 */

// One route per call, both viewports in the same request — half the calls of
// reviewing each screenshot alone, and the model can tell what changed
// between mobile and desktop rather than judging each in isolation.
export function groupByRoute(manifest, errors) {
  const routes = new Map();
  for (const entry of manifest) {
    if (!entry.file) {
      // Capture failed — nothing to look at, but say so instead of letting
      // the route disappear from the report as if it had never been listed.
      errors.push({
        route: entry.route,
        viewport: entry.viewport,
        reason: entry.error ? `capture failed: ${entry.error}` : 'capture failed',
      });
      continue;
    }
    if (!routes.has(entry.route)) routes.set(entry.route, { path: entry.path, shots: [] });
    routes.get(entry.route).shots.push(entry);
  }
  return routes;
}

// Errors are not findings, but they must never be invisible: a route that
// could not be captured or reviewed and a route that was reviewed and found
// clean must not render identically, or the only signal that the pipeline
// broke is a console line nobody watches on a nightly run.
export function renderReport(findings, errors, reviewed) {
  const lines = ['# Nightly visual review', ''];
  if (errors.length > 0) {
    lines.push(
      `**${errors.length} route(s) could not be reviewed — treat this run as incomplete, not clean:**`,
      '',
    );
    for (const e of errors) {
      const label = e.route ? `${e.route}${e.viewport && e.viewport !== 'n/a' ? ` (${e.viewport})` : ''}` : 'pipeline';
      lines.push(`- ${label}: ${e.reason}`);
    }
    lines.push('');
  }

  if (findings.length === 0) {
    // "No findings" is a claim about routes that were looked at, so it may
    // never be printed without the count it is out of. Nineteen consecutive
    // nightly issues said "No findings among the routes that were reviewed"
    // over a run that reviewed none, because this line could be reached
    // without knowing that — the review step had no credential and the
    // sentence read the same as a clean night.
    if (reviewed === 0) {
      lines.push('**No route was reviewed** — this run says nothing about the UI either way.');
    } else if (errors.length > 0) {
      lines.push(`No findings among the ${reviewed} route(s) that were reviewed.`);
    } else {
      lines.push(`No issues found across ${reviewed} route(s).`);
    }
    return lines.join('\n') + '\n';
  }

  const byRoute = new Map();
  for (const f of findings) {
    if (!byRoute.has(f.route)) byRoute.set(f.route, { path: f.path, items: [] });
    byRoute.get(f.route).items.push(f);
  }
  lines.push(`${findings.length} finding(s) across ${byRoute.size} route(s).`, '');
  for (const [route, { path: routePath, items }] of byRoute) {
    lines.push(`## ${routePath} (${route})`);
    for (const item of items) {
      lines.push(`- **${item.viewport}**: ${item.what_is_wrong}`);
    }
    lines.push('');
  }
  return lines.join('\n');
}

// visual-review.mjs's top-level main().catch() must turn an unexpected throw
// into the same shape writeOutputs() always produces — an empty findings
// array and a non-empty errors array — never an empty findings.json, which
// would render identically to a clean run. Pulled out so that invariant is
// checkable without actually crashing main() end to end.
export function crashOutputs(err) {
  const reason = `visual-review.mjs crashed: ${err instanceof Error ? err.stack : err}`;
  return { findings: [], errors: [{ route: null, viewport: null, reason }] };
}

// The one output this pipeline actually publishes: a public, durable GitHub
// issue. The rubric (see visual-review.mjs) only ever asks about layout/UX
// defects — clipped content, an unlabelled control, a raw decimal — never a
// security or fund-movement issue, and every one of those is already
// visible to anyone loading the page. So there is nothing here that needs
// keeping off a public issue: this is the same renderReport() a human reads
// locally, plus the run URL. Delegating to renderReport rather than
// re-deriving the route grouping is what keeps a route from silently
// disappearing from the published report undetected.
// Returns null for a clean run with nothing to report, so the caller can
// skip opening or commenting on an issue at all.
export function renderPublicSummary(findings, errors, runUrl, reviewed = 0, runNote = '') {
  // NOTHING IS PUBLISHED FOR A RUN THAT JUDGED NO ROUTE. Not even the errors
  // that say why: a run with nothing behind it has no business adding text to
  // a public issue, and the nineteen void comments on that issue are what
  // happens when it does. The caller fails the step in that case, so the
  // reason is in the (private) job log, where a broken pipeline belongs.
  // This guard lives here rather than in the workflow's `if:` because a
  // condition in YAML is editable without any of these tests running.
  if (reviewed === 0) return null;
  if (findings.length === 0 && errors.length === 0) return null;

  // The run link is labelled, not bare: the job lives in a private
  // repository, so an outside reader following it gets a 404 that is
  // indistinguishable from a broken link. Saying which it is costs one line.
  const run = runNote ? `Run: ${runUrl}\n${runNote}\n` : `Run: ${runUrl}\n`;

  return (
    renderReport(findings, errors, reviewed).trimEnd() +
    '\n\n' +
    run +
    '\n' +
    'This is advisory — a model judging layout will produce false positives.\n'
  );
}

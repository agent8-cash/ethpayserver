#!/usr/bin/env node
/**
 * Builds the body of the nightly visual-review GitHub issue from
 * findings.json, using renderPublicSummary() so the workflow's issue-filing
 * step never has to re-derive the route grouping itself — a hand-rolled
 * duplicate was previously how a route could go untested and silently
 * disappear from the public issue without anything failing.
 *
 *   node scripts/render-public-summary.mjs <out.md> <findings.json> <runUrl> [runNote]
 *
 * Writes nothing and prints why when there is nothing to report, so the
 * workflow step can treat "no output file" as "skip filing an issue". That
 * covers a run which judged no route at all: it publishes nothing, because
 * text on a public issue backed by no judgement is what nineteen void
 * nightly comments were made of.
 *
 * <runNote> is one line printed under the run link - the job runs in a
 * private repository, so an outside reader following that link gets a 404
 * they cannot tell from a broken URL.
 */
import * as fs from 'node:fs';
import { renderPublicSummary } from './visual-review-report.mjs';

const [, , outPath, findingsPath, runUrl, runNote = ''] = process.argv;

let findings = [];
let errors = [];
// Zero unless the review step says otherwise, so a report can never claim
// "no findings" over routes it never counted — see renderReport.
let reviewed = 0;
if (fs.existsSync(findingsPath)) {
  ({ findings = [], errors = [], reviewed = 0 } = JSON.parse(fs.readFileSync(findingsPath, 'utf8')));
} else {
  errors = [{ route: null, viewport: null, reason: 'findings.json missing — the review step produced no output at all' }];
}

const body = renderPublicSummary(findings, errors, runUrl, reviewed, runNote);
if (body === null) {
  // Two different nothings, and they must not print the same line: a run that
  // looked at every route and found nothing wrong, and a run that looked at
  // no route at all. The second publishes nothing either (the review step
  // fails and the private log says why) but it is not good news.
  console.log(
    reviewed === 0
      ? 'No route was reviewed — publishing nothing. The review step fails in this case; see its log.'
      : `Visual review ran cleanly across ${reviewed} route(s) and found nothing to report.`,
  );
} else {
  fs.writeFileSync(outPath, body);
  console.log(`wrote ${outPath} (${findings.length} finding(s), ${errors.length} error(s), ${reviewed} route(s) reviewed)`);
}

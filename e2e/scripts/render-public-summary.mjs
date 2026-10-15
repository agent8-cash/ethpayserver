#!/usr/bin/env node
/**
 * Builds the body of the nightly visual-review GitHub issue from
 * findings.json, using renderPublicSummary() so the workflow's issue-filing
 * step never has to re-derive the route grouping itself — a hand-rolled
 * duplicate was previously how a route could go untested and silently
 * disappear from the public issue without anything failing.
 *
 *   node scripts/render-public-summary.mjs <out.md> <findings.json> <runUrl>
 *
 * Writes nothing and prints why when there is nothing to report, so the
 * workflow step can treat "no output file" as "skip filing an issue".
 */
import * as fs from 'node:fs';
import { renderPublicSummary } from './visual-review-report.mjs';

const [, , outPath, findingsPath, runUrl] = process.argv;

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

const body = renderPublicSummary(findings, errors, runUrl, reviewed);
if (body === null) {
  console.log('Visual review ran cleanly and found nothing to report.');
} else {
  fs.writeFileSync(outPath, body);
  console.log(`wrote ${outPath} (${findings.length} finding(s), ${errors.length} error(s), ${reviewed} route(s) reviewed)`);
}

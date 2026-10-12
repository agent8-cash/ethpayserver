#!/usr/bin/env node
/**
 * Look at every screenshot tests/visual-review.spec.ts just took and report
 * what is wrong with it — the layout bugs a deterministic assertion cannot
 * see: content overflowing its container, an unlabelled control, a raw
 * decimal where a formatted amount belongs. A human found every one of these
 * on a phone; this is the agent that replaces that phone, not a new CI gate.
 *
 *   ANTHROPIC_API_KEY=... node scripts/visual-review.mjs
 *
 * Reads test-results/visual/manifest.json (written by the spec), writes
 * test-results/visual/findings.json ({ findings, errors }) and
 * test-results/visual/report.md. Advisory only: findings never fail it, and a
 * crash or a model error exits 0. The one exception is a missing key under
 * E2E_VISUAL_REVIEW=true, which exits 1 — the pipeline is broken, not the page.
 * A model
 * judging layout will produce false positives, and a check that can fail on
 * a false positive gets disabled — see scripts/health-gate.sh's history for
 * what that looks like.
 *
 * "errors" is not "findings": a route that could not be captured or
 * reviewed goes there, so it never renders the same as a route that was
 * looked at and found clean.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { groupByRoute, renderReport, crashOutputs } from './visual-review-report.mjs';

const VISUAL_DIR = path.join('test-results', 'visual');
const MANIFEST_PATH = path.join(VISUAL_DIR, 'manifest.json');
const MODEL = process.env.VISUAL_REVIEW_MODEL || 'claude-sonnet-5-5';
// Overridable so a test can point this at a local stand-in instead of the
// real Anthropic API — see visual-review-main.spec.ts.
const API_URL = process.env.VISUAL_REVIEW_API_URL || 'https://api.anthropic.com/v1/messages';

const RUBRIC = `You are doing a nightly visual QA pass on a non-custodial crypto payment
processor's merchant dashboard. You are shown full-page screenshots of one
route: first at a mobile viewport (375x812), then at a desktop viewport
(1280x720) — a screenshot may be missing if that capture failed, in which
case just review the one you have.

Look only for concrete, visible defects — the kind a deterministic test
cannot catch:
- content clipped or overflowing its container; horizontal scroll on the page body
- a control with no label, or a label that does not say what it does
- a raw decimal where a formatted amount belongs (e.g. "0.500000000000000000")
- an identifier shown where a name was promised
- text overlapping, or a badge breaking a row's baseline
- an empty state that reads as a failure rather than as "nothing here yet"
- a disabled control with no explanation of why

An empty page because the account has no data yet is correct, not a defect —
only report it if it *looks* broken (an error-shaped box, missing copy, a
spinner that never resolved). Do not invent a severity score; that is not
yours to judge here. If a viewport is fine, do not report anything for it.
Call report_findings with an empty array if both viewports are fine.`;

const FINDINGS_TOOL = {
  name: 'report_findings',
  description: 'Report visual defects found in the supplied screenshots.',
  input_schema: {
    type: 'object',
    properties: {
      findings: {
        type: 'array',
        items: {
          type: 'object',
          properties: {
            viewport: { type: 'string', enum: ['mobile', 'desktop'] },
            what_is_wrong: { type: 'string' },
          },
          required: ['viewport', 'what_is_wrong'],
        },
      },
    },
    required: ['findings'],
  },
};

function loadManifest() {
  if (!fs.existsSync(MANIFEST_PATH)) return null;
  return JSON.parse(fs.readFileSync(MANIFEST_PATH, 'utf8'));
}

async function reviewRoute(apiKey, route, { path: routePath, shots }) {
  const content = [{ type: 'text', text: `Route: ${routePath}` }];
  for (const shot of shots) {
    const data = fs.readFileSync(path.join(VISUAL_DIR, shot.file)).toString('base64');
    content.push({ type: 'text', text: `Viewport: ${shot.viewport}` });
    content.push({ type: 'image', source: { type: 'base64', media_type: 'image/png', data } });
  }

  const resp = await fetch(API_URL, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
    },
    body: JSON.stringify({
      model: MODEL,
      max_tokens: 1024,
      system: RUBRIC,
      messages: [{ role: 'user', content }],
      tools: [FINDINGS_TOOL],
      tool_choice: { type: 'tool', name: 'report_findings' },
    }),
  });

  if (!resp.ok) {
    // Deliberately not the response body: same reasoning as the missing-
    // tool_use case below — this message flows into `errors`, which the
    // scheduled workflow publishes, so it must never carry API-authored
    // text, only the status line.
    throw new Error(`${resp.status} ${resp.statusText}`);
  }

  const data = await resp.json();
  const toolUse = data.content?.find((b) => b.type === 'tool_use' && b.name === 'report_findings');
  if (!toolUse) {
    // Deliberately not `JSON.stringify(data)`: a response with no tool_use
    // block is exactly the shape a policy refusal takes, and a refusal's
    // freeform text is where the model would put a prose description of the
    // screenshot — i.e. a leaked finding. This reason flows into `errors`,
    // which the scheduled workflow does publish (unlike `findings`), so it
    // must never carry model-authored text — only block *types*, which
    // cannot.
    const blockTypes = (data.content ?? []).map((b) => b.type).join(',') || 'none';
    throw new Error(`no report_findings tool call in response (stop_reason=${data.stop_reason ?? 'unknown'}, blocks=[${blockTypes}])`);
  }

  return (toolUse.input?.findings ?? []).map((f) => ({ route, path: routePath, ...f }));
}

function writeOutputs(findings, errors) {
  fs.mkdirSync(VISUAL_DIR, { recursive: true });
  fs.writeFileSync(path.join(VISUAL_DIR, 'findings.json'), JSON.stringify({ findings, errors }, null, 2));
  const report = renderReport(findings, errors);
  fs.writeFileSync(path.join(VISUAL_DIR, 'report.md'), report);
  // Not the full report: on the scheduled run this log is a public repo's
  // job log, exactly as downloadable as the issue findings.json feeds — see
  // .github/workflows/visual-review-scheduled.yml's header comment. Counts
  // only; report.md on disk (not uploaded anywhere) carries the rest.
  console.log(`${findings.length} finding(s), ${errors.length} error(s) — see report.md`);
}

async function main() {
  fs.mkdirSync(VISUAL_DIR, { recursive: true });

  const findings = [];
  const errors = [];

  const apiKey = process.env.ANTHROPIC_API_KEY;
  if (!apiKey) {
    // The scheduled workflow sets E2E_VISUAL_REVIEW and always passes the
    // secret, so an unset key there is a broken pipeline, not a run with
    // nothing to do: skipping quietly is how the review went unrun with a
    // green step. Fail the step, and leave an error behind so the report
    // cannot read as a clean pass. Anywhere else (normal CI has no key and
    // no manifest) the skip stays silent.
    if (process.env.E2E_VISUAL_REVIEW === 'true') {
      errors.push({ route: null, viewport: null, reason: 'ANTHROPIC_API_KEY is not set — the review did not run' });
      writeOutputs(findings, errors);
      console.error('ANTHROPIC_API_KEY is not set in a scheduled visual review — failing rather than skipping');
      process.exitCode = 1;
      return;
    }
    console.log('ANTHROPIC_API_KEY unset — skipping visual review');
    return;
  }

  const manifest = loadManifest();
  if (!manifest || manifest.length === 0) {
    errors.push({ route: null, viewport: null, reason: `no manifest at ${MANIFEST_PATH} — review did not run` });
    writeOutputs(findings, errors);
    return;
  }

  const routes = groupByRoute(manifest, errors);
  if (routes.size === 0) {
    errors.push({ route: null, viewport: null, reason: 'every capture in the manifest failed — nothing to review' });
    writeOutputs(findings, errors);
    return;
  }

  for (const [route, group] of routes) {
    try {
      findings.push(...(await reviewRoute(apiKey, route, group)));
    } catch (err) {
      const reason = `review failed: ${err instanceof Error ? err.message : err}`;
      // Not `reason` itself in the console log: writeOutputs()'s "counts
      // only" comment applies here too. `reason` is safe to publish in
      // `errors` because both throw sites above already strip it down to a
      // status line / block-type list, never API- or model-authored text.
      console.error(`${route}: review failed`);
      errors.push({ route, viewport: null, reason });
    }
  }

  writeOutputs(findings, errors);
}

main().catch((err) => {
  // Advisory tooling: log and exit 0 rather than failing a job that exists
  // to produce a report a human skims, not a gate anyone depends on being green.
  // Still write findings.json — an empty one would read as "reviewed, clean",
  // which is exactly the outcome a crash must not produce.
  const { findings, errors } = crashOutputs(err);
  console.error(errors[0].reason);
  try {
    writeOutputs(findings, errors);
  } catch (writeErr) {
    console.error(`could not even write the failure report: ${writeErr instanceof Error ? writeErr.stack : writeErr}`);
  }
});

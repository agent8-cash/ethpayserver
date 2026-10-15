#!/usr/bin/env node
/**
 * Look at every screenshot tests/visual-review.spec.ts just took and report
 * what is wrong with it — the layout bugs a deterministic assertion cannot
 * see: content overflowing its container, an unlabelled control, a raw
 * decimal where a formatted amount belongs. A human found every one of these
 * on a phone; this is the agent that replaces that phone, not a new CI gate.
 *
 *   CLAUDE_CODE_OAUTH_TOKEN=... node scripts/visual-review.mjs
 *
 * The looking is done by the Claude Code CLI (`claude -p`), which reads the
 * PNGs off disk itself. Not a direct api.anthropic.com call with an
 * ANTHROPIC_API_KEY, which is what this script did for its first nineteen
 * nightly runs — every one of which captured screenshots and judged none,
 * because that key existed in no repository and nobody had to create one for
 * the step to go green. CLAUDE_CODE_OAUTH_TOKEN is already provisioned for
 * the agent jobs, so the credential this needs is one that demonstrably
 * exists.
 *
 * Reads test-results/visual/manifest.json (written by the spec), writes
 * test-results/visual/findings.json ({ findings, errors, reviewed }) and
 * test-results/visual/report.md. Advisory only: findings never fail it, and a
 * crash or a model error exits 0. The one exception is a missing credential
 * under E2E_VISUAL_REVIEW=true, which exits 1 — the pipeline is broken, not
 * the page. A model judging layout will produce false positives, and a check
 * that can fail on a false positive gets disabled — see
 * scripts/health-gate.sh's history for what that looks like.
 *
 * "errors" is not "findings": a route that could not be captured or
 * reviewed goes there, so it never renders the same as a route that was
 * looked at and found clean. "reviewed" is how many routes were actually
 * judged, so a report can never say "no findings" without saying what that
 * is out of — nineteen nightly issues said exactly that over zero routes.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { spawn } from 'node:child_process';
import { groupByRoute, renderReport, crashOutputs } from './visual-review-report.mjs';

const VISUAL_DIR = path.join('test-results', 'visual');
const MANIFEST_PATH = path.join(VISUAL_DIR, 'manifest.json');
const MODEL = process.env.VISUAL_REVIEW_MODEL || 'claude-sonnet-5-5';
// Overridable so a test can point this at a stand-in script instead of the
// real CLI — see visual-review-main.spec.ts.
const CLAUDE_BIN = process.env.VISUAL_REVIEW_CLAUDE_BIN || 'claude';
// Per route, not per run. A CLI that never returns would otherwise hold the
// job until its own 20-minute timeout kills it with nothing written at all.
const TIMEOUT_MS = Number(process.env.VISUAL_REVIEW_TIMEOUT_MS || 300_000);

const VIEWPORTS = new Set(['mobile', 'desktop']);

const RUBRIC = `You are doing a nightly visual QA pass on a non-custodial crypto payment
processor's merchant dashboard. You are shown full-page screenshots of one
route: a mobile viewport (375x812) and a desktop viewport (1280x720) — a
screenshot may be missing if that capture failed, in which case just review
the one you have.

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
yours to judge here. If a viewport is fine, report nothing for it.

Read each screenshot with the Read tool, then print ONE JSON object and
nothing else — no prose before or after, no code fence:

{"findings": [{"viewport": "mobile", "what_is_wrong": "..."}]}

"viewport" is exactly "mobile" or "desktop". An empty findings array is the
right answer when both viewports are fine.`;

function loadManifest() {
  if (!fs.existsSync(MANIFEST_PATH)) return null;
  return JSON.parse(fs.readFileSync(MANIFEST_PATH, 'utf8'));
}

function runClaude(prompt) {
  return new Promise((resolve, reject) => {
    const child = spawn(CLAUDE_BIN, ['-p', '--model', MODEL, '--permission-mode', 'bypassPermissions'], {
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, TIMEOUT_MS);
    child.stdout.on('data', (c) => (stdout += c));
    child.stderr.on('data', (c) => (stderr += c));
    child.once('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.once('close', (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr, timedOut });
    });
    child.stdin.end(prompt);
  });
}

// Deliberately byte counts and never the output itself: this reason flows
// into `errors`, which the scheduled workflow publishes to a public issue,
// and unparseable output is exactly the shape a refusal takes — i.e. prose
// describing the screenshot, i.e. a leaked finding that skipped the rubric.
function parseFindings(stdout) {
  const start = stdout.indexOf('{');
  const end = stdout.lastIndexOf('}');
  if (start === -1 || end <= start) {
    throw new Error(`no JSON object in the review output (${stdout.length} bytes)`);
  }
  let parsed;
  try {
    parsed = JSON.parse(stdout.slice(start, end + 1));
  } catch {
    throw new Error(`unparseable JSON in the review output (${stdout.length} bytes)`);
  }
  if (!Array.isArray(parsed.findings)) {
    throw new Error(`review output carries no findings array (${stdout.length} bytes)`);
  }
  // A malformed entry fails the whole route rather than being dropped: a
  // silently shortened list is a partial review rendering as a complete one,
  // which is the failure this script's whole error/finding split exists for.
  for (const f of parsed.findings) {
    if (!VIEWPORTS.has(f?.viewport) || typeof f?.what_is_wrong !== 'string') {
      throw new Error('a finding entry in the review output is malformed');
    }
  }
  return parsed.findings;
}

async function reviewRoute(route, { path: routePath, shots }) {
  const shotLines = shots.map((s) => `- ${s.viewport}: ${path.resolve(VISUAL_DIR, s.file)}`);
  const prompt = `${RUBRIC}\n\nRoute: ${routePath}\nScreenshots:\n${shotLines.join('\n')}\n`;

  const { code, stdout, stderr, timedOut } = await runClaude(prompt);
  if (timedOut) throw new Error(`review timed out after ${TIMEOUT_MS}ms`);
  if (code !== 0) {
    // stderr goes to the job log (private repo) but never into `errors`: the
    // CLI prints model-adjacent text there and `errors` is published.
    if (stderr) console.error(`${route}: claude stderr: ${stderr.slice(0, 500)}`);
    throw new Error(`claude exited ${code}`);
  }

  return parseFindings(stdout).map((f) => ({ route, path: routePath, ...f }));
}

function writeOutputs(findings, errors, reviewed) {
  fs.mkdirSync(VISUAL_DIR, { recursive: true });
  fs.writeFileSync(
    path.join(VISUAL_DIR, 'findings.json'),
    JSON.stringify({ findings, errors, reviewed }, null, 2),
  );
  const report = renderReport(findings, errors, reviewed);
  fs.writeFileSync(path.join(VISUAL_DIR, 'report.md'), report);
  console.log(
    `${findings.length} finding(s), ${errors.length} error(s), ${reviewed} route(s) reviewed — see report.md`,
  );
}

async function main() {
  fs.mkdirSync(VISUAL_DIR, { recursive: true });

  const findings = [];
  const errors = [];

  const token = process.env.CLAUDE_CODE_OAUTH_TOKEN;
  if (!token) {
    // The scheduled workflow sets E2E_VISUAL_REVIEW and always passes the
    // credential, so an unset one there is a broken pipeline, not a run with
    // nothing to do: skipping quietly is how the review went unrun with a
    // green step for nineteen nights. Fail the step, and leave an error
    // behind so the report cannot read as a clean pass. Anywhere else
    // (normal CI has no credential and no manifest) the skip stays silent.
    if (process.env.E2E_VISUAL_REVIEW === 'true') {
      errors.push({ route: null, viewport: null, reason: 'CLAUDE_CODE_OAUTH_TOKEN is not set — the review did not run' });
      writeOutputs(findings, errors, 0);
      console.error('CLAUDE_CODE_OAUTH_TOKEN is not set in a scheduled visual review — failing rather than skipping');
      process.exitCode = 1;
      return;
    }
    console.log('CLAUDE_CODE_OAUTH_TOKEN unset — skipping visual review');
    return;
  }

  const manifest = loadManifest();
  if (!manifest || manifest.length === 0) {
    errors.push({ route: null, viewport: null, reason: `no manifest at ${MANIFEST_PATH} — review did not run` });
    writeOutputs(findings, errors, 0);
    return;
  }

  const routes = groupByRoute(manifest, errors);
  if (routes.size === 0) {
    errors.push({ route: null, viewport: null, reason: 'every capture in the manifest failed — nothing to review' });
    writeOutputs(findings, errors, 0);
    return;
  }

  let reviewed = 0;
  for (const [route, group] of routes) {
    try {
      findings.push(...(await reviewRoute(route, group)));
      reviewed += 1;
    } catch (err) {
      const reason = `review failed: ${err instanceof Error ? err.message : err}`;
      // Not `reason` itself in the console log: `reason` is safe to publish
      // in `errors` because every throw site above strips it to a status
      // line or a byte count, never model-authored text.
      console.error(`${route}: review failed`);
      errors.push({ route, viewport: null, reason });
    }
  }

  writeOutputs(findings, errors, reviewed);
}

main().catch((err) => {
  // Advisory tooling: log and exit 0 rather than failing a job that exists
  // to produce a report a human skims, not a gate anyone depends on being green.
  // Still write findings.json — an empty one would read as "reviewed, clean",
  // which is exactly the outcome a crash must not produce.
  const { findings, errors } = crashOutputs(err);
  console.error(errors[0].reason);
  try {
    writeOutputs(findings, errors, 0);
  } catch (writeErr) {
    console.error(`could not even write the failure report: ${writeErr instanceof Error ? writeErr.stack : writeErr}`);
  }
});

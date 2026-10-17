/**
 * Exercises visual-review.mjs's own main() — the wiring visual-review-report
 * .spec.ts's unit tests never reach: which env var and file it reads first,
 * what the no-credential / no-manifest / all-captures-failed early returns
 * actually write, that every route's result lands in one combined
 * findings.json rather than N separate writes, and that a route whose review
 * failed is counted as not reviewed. Runs the real script as a subprocess
 * against a throwaway cwd, with a stand-in script in place of the Claude
 * Code CLI (VISUAL_REVIEW_CLAUDE_BIN), so — like visual-review-report
 * .spec.ts — it needs neither a browser nor a live credential and belongs in
 * e2e-static-checks rather than behind E2E_VISUAL_REVIEW.
 */
import { test, expect } from '@playwright/test';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { spawn } from 'node:child_process';

const SCRIPT = path.resolve('scripts', 'visual-review.mjs');

function freshDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'visual-review-main-'));
}

function visualDir(dir: string): string {
  return path.join(dir, 'test-results', 'visual');
}

function writeManifest(dir: string, manifest: unknown[]): void {
  fs.mkdirSync(visualDir(dir), { recursive: true });
  fs.writeFileSync(path.join(visualDir(dir), 'manifest.json'), JSON.stringify(manifest));
}

function readFindings(dir: string): {
  findings: { route: string; path: string; viewport: string; what_is_wrong: string }[];
  errors: { route: string | null; viewport: string | null; reason: string }[];
  reviewed: number;
} {
  return JSON.parse(fs.readFileSync(path.join(visualDir(dir), 'findings.json'), 'utf8'));
}

// A stand-in for `claude -p`: reads the prompt on stdin exactly as the real
// CLI does, so a test can answer per route. `body` is the script body with
// `prompt` in scope.
function fakeClaude(dir: string, body: string): string {
  const binPath = path.join(dir, 'fake-claude.mjs');
  fs.writeFileSync(
    binPath,
    `#!/usr/bin/env node
import * as fs from 'node:fs';
const prompt = fs.readFileSync(0, 'utf8');
${body}
`,
  );
  fs.chmodSync(binPath, 0o755);
  return binPath;
}

// Not spawnSync: the stand-in CLI is a child of this child, and a
// synchronous spawn blocks this process's event loop for as long as the
// whole tree runs.
function run(dir: string, env: Record<string, string>): Promise<{ status: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn('node', [SCRIPT], { cwd: dir, env: { ...process.env, ...env } });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (c: Buffer) => (stdout += c));
    child.stderr.on('data', (c: Buffer) => (stderr += c));
    child.once('error', reject);
    child.once('close', (status) => resolve({ status, stdout, stderr }));
  });
}

// The second half of "a broken run files nothing": whatever the review step
// wrote, this is the script the workflow asks for an issue body, and it must
// produce no file for a run that judged nothing.
function renderSummary(dir: string): Promise<{ status: number | null; stdout: string }> {
  const script = path.resolve('scripts', 'render-public-summary.mjs');
  return new Promise((resolve, reject) => {
    const child = spawn(
      'node',
      [script, 'test-results/visual/public-summary.md', 'test-results/visual/findings.json', 'https://example.invalid/run/7'],
      { cwd: dir },
    );
    let stdout = '';
    child.stdout.on('data', (c: Buffer) => (stdout += c));
    child.once('error', reject);
    child.once('close', (status) => resolve({ status, stdout }));
  });
}

test('no credential: skips silently, writing no findings.json at all', async () => {
  const dir = freshDir();
  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: '', E2E_VISUAL_REVIEW: '' });

  expect(result.stdout).toContain('CLAUDE_CODE_OAUTH_TOKEN unset — skipping visual review');
  expect(fs.existsSync(path.join(visualDir(dir), 'findings.json'))).toBe(false);
});

test('no credential in the scheduled run: fails the step and records an error, never a silent skip', async () => {
  const dir = freshDir();
  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: '', E2E_VISUAL_REVIEW: 'true' });

  expect(result.status).toBe(1);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0].reason).toContain('CLAUDE_CODE_OAUTH_TOKEN is not set');
  expect(reviewed).toBe(0);
});

test('no manifest: a pipeline error, not the empty findings.json a clean run would leave', async () => {
  const dir = freshDir();
  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token' });

  expect(result.status).toBe(0);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0].reason).toContain('no manifest');
  expect(reviewed).toBe(0);
});

test('every capture in the manifest failed: still a pipeline error, never a silent clean run', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: null, error: 'timeout' }]);
  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token' });

  expect(result.status).toBe(0);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors.some((e) => e.reason.includes('nothing to review'))).toBe(true);
  expect(reviewed).toBe(0);
});

test('a scheduled run with nothing to review is red, not an advisory green', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: null, error: 'timeout' }]);

  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token', E2E_VISUAL_REVIEW: 'true' });

  expect(result.status).toBe(1);
  expect(readFindings(dir).reviewed).toBe(0);
});

// The failure chef named: a credential that is present but dead. Every route's
// CLI call fails, so nothing is judged - and an advisory green saying "no route
// was reviewed" would be the nineteen-night failure rebuilt one layer up.
test('a credential that is present but rejected is red and publishes nothing', async () => {
  const dir = freshDir();
  writeManifest(dir, [
    { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null },
    { route: 'settings', path: '/evm/settings', viewport: 'mobile', file: 'settings-mobile.png', error: null },
  ]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));
  fs.writeFileSync(path.join(visualDir(dir), 'settings-mobile.png'), Buffer.from([0]));

  const bin = fakeClaude(
    dir,
    `process.stderr.write('Invalid API key · Please run /login');
process.exit(1);`,
  );

  const result = await run(dir, {
    CLAUDE_CODE_OAUTH_TOKEN: 'revoked-token',
    E2E_VISUAL_REVIEW: 'true',
    VISUAL_REVIEW_CLAUDE_BIN: bin,
  });

  expect(result.status).toBe(1);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(2);
  expect(reviewed).toBe(0);

  // ...and nothing reaches the public issue: no body file to post.
  const summary = await renderSummary(dir);
  expect(summary.status).toBe(0);
  expect(summary.stdout).toContain('No route was reviewed');
  expect(fs.existsSync(path.join(visualDir(dir), 'public-summary.md'))).toBe(false);
});

test('reviews every route in the manifest and writes their results in one combined findings.json', async () => {
  const dir = freshDir();
  writeManifest(dir, [
    { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null },
    { route: 'settings', path: '/evm/settings', viewport: 'mobile', file: 'settings-mobile.png', error: null },
  ]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));
  fs.writeFileSync(path.join(visualDir(dir), 'settings-mobile.png'), Buffer.from([0]));

  const bin = fakeClaude(
    dir,
    `if (prompt.includes('/evm/wallets')) {
  process.stdout.write(JSON.stringify({ findings: [{ viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' }] }));
} else {
  process.stderr.write('leaky-stderr-should-not-be-published');
  process.exit(1);
}`,
  );

  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token', VISUAL_REVIEW_CLAUDE_BIN: bin });

  expect(result.status).toBe(0);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([
    { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' },
  ]);
  // The route whose review failed is an error and is NOT counted as
  // reviewed: the count is what the published report says "no findings" out
  // of, so a failed route inflating it is the same lie in a smaller font.
  expect(errors).toHaveLength(1);
  expect(errors[0].route).toBe('settings');
  expect(errors[0].reason).toBe('review failed: claude exited 1');
  expect(errors[0].reason).not.toContain('leaky-stderr-should-not-be-published');
  expect(reviewed).toBe(1);
  // One write covering both routes, not one per route.
  expect(result.stdout).toContain('1 finding(s), 1 error(s), 1 route(s) reviewed');

  // One route judged is an advisory run, so it DOES publish - the failed
  // route appears as an error beside the finding, not instead of it.
  const summary = await renderSummary(dir);
  const body = fs.readFileSync(path.join(visualDir(dir), 'public-summary.md'), 'utf8');
  expect(summary.status).toBe(0);
  expect(body).toContain('raw decimal shown for balance');
  expect(body).toContain('could not be reviewed');
});

test('output that is not the asked-for JSON is an error carrying its size, never its text', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null }]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));

  // Exactly the shape a refusal takes: exit 0, prose instead of findings.
  // That prose is a description of the screenshot that skipped the rubric,
  // and `errors` is published, so none of it may appear there.
  const bin = fakeClaude(dir, `process.stdout.write('I cannot help with analysing this screenshot.');`);

  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token', VISUAL_REVIEW_CLAUDE_BIN: bin });

  expect(result.status).toBe(0);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0].reason).toContain('no JSON object in the review output');
  expect(errors[0].reason).toContain('bytes');
  expect(errors[0].reason).not.toContain('cannot help');
  expect(reviewed).toBe(0);
});

// The CLI prints diagnostics to stdout, and some carry JSON of their own -
// an unrecognized model id prints {"model":"...","query_source":"sdk"} before
// the answer. Spanning from the first brace in the stream to the last would
// parse as nothing, which is how a whole night's review turns into "no route
// was reviewed" over a working credential.
test('a JSON-carrying diagnostic line before the answer does not break the parse', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null }]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));

  const bin = fakeClaude(
    dir,
    `process.stdout.write('"claude-sonnet-9" isn\\'t described by this version\\'s model catalog\\n');
process.stdout.write('[claude-code:unrecognized_model] {"model":"claude-sonnet-9","query_source":"sdk"}\\n');
process.stdout.write(JSON.stringify({ findings: [{ viewport: 'mobile', what_is_wrong: 'badge breaks the row baseline' }] }));`,
  );

  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token', VISUAL_REVIEW_CLAUDE_BIN: bin });

  expect(result.status).toBe(0);
  const { findings, errors, reviewed } = readFindings(dir);
  expect(errors).toEqual([]);
  expect(reviewed).toBe(1);
  expect(findings).toEqual([
    { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'badge breaks the row baseline' },
  ]);
});

test('a malformed finding fails its route rather than being quietly dropped', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null }]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));

  const bin = fakeClaude(
    dir,
    `process.stdout.write(JSON.stringify({ findings: [
  { viewport: 'mobile', what_is_wrong: 'a real one' },
  { viewport: 'watch', what_is_wrong: 'not a viewport we capture' },
] }));`,
  );

  const result = await run(dir, { CLAUDE_CODE_OAUTH_TOKEN: 'test-token', VISUAL_REVIEW_CLAUDE_BIN: bin });

  const { findings, errors, reviewed } = readFindings(dir);
  expect(result.status).toBe(0);
  expect(findings).toEqual([]);
  expect(errors[0].reason).toContain('malformed');
  expect(reviewed).toBe(0);
});

test('a CLI that never returns is killed and reported, not left to hold the job', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null }]);
  fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));

  const bin = fakeClaude(dir, `setInterval(() => {}, 1000);`);

  const result = await run(dir, {
    CLAUDE_CODE_OAUTH_TOKEN: 'test-token',
    VISUAL_REVIEW_CLAUDE_BIN: bin,
    VISUAL_REVIEW_TIMEOUT_MS: '1500',
  });

  expect(result.status).toBe(0);
  const { errors, reviewed } = readFindings(dir);
  expect(errors[0].reason).toContain('timed out');
  expect(reviewed).toBe(0);
});

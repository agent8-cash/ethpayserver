/**
 * Exercises visual-review.mjs's own main() — the wiring visual-review-report
 * .spec.ts's unit tests never reach: which env var and file it reads first,
 * what the no-key / no-manifest / all-captures-failed early returns actually
 * write, and that every route's result lands in one combined findings.json
 * rather than N separate writes. Runs the real script as a subprocess
 * against a throwaway cwd and a stand-in HTTP server in place of the
 * Anthropic API (VISUAL_REVIEW_API_URL), so — like visual-review-report
 * .spec.ts — it needs neither a browser nor a live key and belongs in
 * e2e-static-checks rather than behind E2E_VISUAL_REVIEW.
 */
import { test, expect } from '@playwright/test';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
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

function readFindings(dir: string): { findings: { route: string; path: string; viewport: string; what_is_wrong: string }[]; errors: { route: string | null; viewport: string | null; reason: string }[] } {
  return JSON.parse(fs.readFileSync(path.join(visualDir(dir), 'findings.json'), 'utf8'));
}

// Not spawnSync: a stand-in server for the Anthropic API runs in this same
// process (see the two tests below that use one), and a synchronous spawn
// blocks this process's event loop for as long as the child runs — which is
// also exactly how long that server needs to answer the child's request.
// That's a deadlock, not a slow test; it only surfaces once a case actually
// needs the server, so the first three tests below would pass with
// spawnSync too, right up until someone added one that didn't.
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

test('no API key: skips silently, writing no findings.json at all', async () => {
  const dir = freshDir();
  const result = await run(dir, { ANTHROPIC_API_KEY: '', E2E_VISUAL_REVIEW: '' });

  expect(result.stdout).toContain('ANTHROPIC_API_KEY unset — skipping visual review');
  expect(fs.existsSync(path.join(visualDir(dir), 'findings.json'))).toBe(false);
});

test('no API key in the scheduled run: fails the step and records an error, never a silent skip', async () => {
  const dir = freshDir();
  const result = await run(dir, { ANTHROPIC_API_KEY: '', E2E_VISUAL_REVIEW: 'true' });

  expect(result.status).toBe(1);
  const { findings, errors } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0].reason).toContain('ANTHROPIC_API_KEY is not set');
});

test('no manifest: a pipeline error, not the empty findings.json a clean run would leave', async () => {
  const dir = freshDir();
  const result = await run(dir, { ANTHROPIC_API_KEY: 'test-key' });

  expect(result.status).toBe(0);
  const { findings, errors } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0].reason).toContain('no manifest');
});

test('every capture in the manifest failed: still a pipeline error, never a silent clean run', async () => {
  const dir = freshDir();
  writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: null, error: 'timeout' }]);
  const result = await run(dir, { ANTHROPIC_API_KEY: 'test-key' });

  expect(result.status).toBe(0);
  const { findings, errors } = readFindings(dir);
  expect(findings).toEqual([]);
  expect(errors.some((e) => e.reason.includes('nothing to review'))).toBe(true);
});

test('reviews every route in the manifest and writes their results in one combined findings.json', async () => {
  const server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => chunks.push(c));
    req.on('end', () => {
      const payload = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      const routeLine = payload.messages[0].content.find((b: { type: string; text?: string }) => b.type === 'text')?.text ?? '';
      if (routeLine.includes('/evm/wallets')) {
        res.setHeader('content-type', 'application/json');
        res.end(
          JSON.stringify({
            content: [
              {
                type: 'tool_use',
                name: 'report_findings',
                input: { findings: [{ viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' }] },
              },
            ],
          }),
        );
      } else {
        res.statusCode = 503;
        res.end();
      }
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = (server.address() as AddressInfo).port;

  try {
    const dir = freshDir();
    writeManifest(dir, [
      { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null },
      { route: 'settings', path: '/evm/settings', viewport: 'mobile', file: 'settings-mobile.png', error: null },
    ]);
    fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));
    fs.writeFileSync(path.join(visualDir(dir), 'settings-mobile.png'), Buffer.from([0]));

    const result = await run(dir, {
      ANTHROPIC_API_KEY: 'test-key',
      VISUAL_REVIEW_API_URL: `http://127.0.0.1:${port}/v1/messages`,
    });

    expect(result.status).toBe(0);
    const { findings, errors } = readFindings(dir);
    expect(findings).toEqual([
      { route: 'wallets', path: '/evm/wallets', viewport: 'mobile', what_is_wrong: 'raw decimal shown for balance' },
    ]);
    expect(errors).toHaveLength(1);
    expect(errors[0].route).toBe('settings');
    expect(errors[0].reason).toContain('503');
    // One write covering both routes, not one per route: the totals in the
    // console summary already reflect the failed route alongside the found one.
    expect(result.stdout).toContain('1 finding(s), 1 error(s)');
  } finally {
    server.close();
  }
});

test("an API failure's status line reaches errors, but the response body never does", async () => {
  let server: Server | undefined;
  try {
    server = createServer((req, res) => {
      req.on('data', () => {});
      req.on('end', () => {
        res.statusCode = 500;
        res.end('sensitive-body-should-not-leak');
      });
    });
    await new Promise<void>((resolve, reject) => {
      server!.once('error', reject);
      server!.listen(0, '127.0.0.1', resolve);
    });
    const port = (server.address() as AddressInfo).port;

    const dir = freshDir();
    writeManifest(dir, [{ route: 'wallets', path: '/evm/wallets', viewport: 'mobile', file: 'wallets-mobile.png', error: null }]);
    fs.writeFileSync(path.join(visualDir(dir), 'wallets-mobile.png'), Buffer.from([0]));

    const result = await run(dir, {
      ANTHROPIC_API_KEY: 'test-key',
      VISUAL_REVIEW_API_URL: `http://127.0.0.1:${port}/v1/messages`,
    });

    expect(result.status).toBe(0);
    const { errors } = readFindings(dir);
    expect(errors).toHaveLength(1);
    expect(errors[0].reason).toBe('review failed: 500 Internal Server Error');
    expect(errors[0].reason).not.toContain('sensitive-body-should-not-leak');
  } finally {
    server?.close();
  }
});

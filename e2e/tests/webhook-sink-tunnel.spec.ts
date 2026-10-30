/**
 * The quick tunnel's failure classification, against a stand-in `cloudflared`.
 *
 * The synthetic payment spec only meets a Cloudflare outage on a runner holding
 * secrets, so the rule that an outage reads as "infrastructure unavailable"
 * while every other failure stays ordinary is otherwise untested. No network.
 */
import { test, expect } from '@playwright/test';
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { TunnelUnavailableError, startQuickTunnelWithRetry } from '../fixtures/webhook-sink';

function fakeCloudflared(body: string): { bin: string; attempts: () => number } {
  const dir = mkdtempSync(join(tmpdir(), 'fake-cloudflared-'));
  const log = join(dir, 'runs');
  const bin = join(dir, 'cloudflared');
  writeFileSync(bin, `#!/bin/sh\necho run >> '${log}'\n${body}\n`);
  chmodSync(bin, 0o755);
  return {
    bin,
    attempts: () => {
      try {
        return readFileSync(log, 'utf8').trim().split('\n').length;
      } catch {
        return 0;
      }
    },
  };
}

test.describe('quick tunnel failure classification', () => {
  test.afterEach(() => {
    delete process.env.E2E_CLOUDFLARED_BIN;
    delete process.env.E2E_TUNNEL_URL_TIMEOUT_MS;
  });

  test('a provisioning 409 that exits early is unavailable, and retried once', async () => {
    const fake = fakeCloudflared(
      'echo "quick tunnel provisioning failed with status 409: [10000] Tunnelstore request failed" >&2; exit 1',
    );
    process.env.E2E_CLOUDFLARED_BIN = fake.bin;
    const err = await startQuickTunnelWithRetry(1).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(TunnelUnavailableError);
    expect((err as Error).message).toContain('status 409');
    expect(fake.attempts()).toBe(2);
  });

  // The shape of the reported outage: cloudflared prints the refusal and then
  // stays alive without ever giving a URL, so only the timeout ends it.
  test('a tunnel that never reports a URL times out as unavailable, and is retried', async () => {
    const fake = fakeCloudflared(
      'echo "quick tunnel provisioning failed with status 409" >&2; exec sleep 30',
    );
    process.env.E2E_CLOUDFLARED_BIN = fake.bin;
    process.env.E2E_TUNNEL_URL_TIMEOUT_MS = '300';
    const err = await startQuickTunnelWithRetry(1).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(TunnelUnavailableError);
    expect((err as Error).message).toContain('did not report a URL');
    expect(fake.attempts()).toBe(2);
  });

  test('an early exit with unrelated output is an ordinary failure and is not retried', async () => {
    const fake = fakeCloudflared('echo "unknown flag: --bogus" >&2; exit 2');
    process.env.E2E_CLOUDFLARED_BIN = fake.bin;
    const err = await startQuickTunnelWithRetry(1).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(Error);
    expect(err).not.toBeInstanceOf(TunnelUnavailableError);
    expect(fake.attempts()).toBe(1);
  });

  test('a missing binary is an ordinary failure and is not retried', async () => {
    process.env.E2E_CLOUDFLARED_BIN = '/nonexistent/cloudflared';
    const err = await startQuickTunnelWithRetry(1).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(Error);
    expect(err).not.toBeInstanceOf(TunnelUnavailableError);
  });

  test('a tunnel that reports a URL resolves', async () => {
    const fake = fakeCloudflared('echo "https://abc-def.trycloudflare.com" >&2; exec sleep 30');
    process.env.E2E_CLOUDFLARED_BIN = fake.bin;
    const started = await startQuickTunnelWithRetry(1);
    started.proc.kill('SIGTERM');
    expect(started.url).toBe('https://abc-def.trycloudflare.com');
    expect(fake.attempts()).toBe(1);
  });
});

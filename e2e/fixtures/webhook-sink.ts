/**
 * Local webhook sink with a publicly-reachable URL.
 *
 * `PUT /stores/{id}/webhook` rejects anything that is not `https://` or
 * `http://localhost` (`server/src/api/stores/webhooks.rs`), and the server runs
 * in a container, so its `localhost` is not ours. To assert delivery against the
 * live deployment the sink therefore needs a real HTTPS URL: by default we open
 * a cloudflared quick tunnel (cloudflared already fronts the deployment — it is what
 * fronts testnet.random.cash), and `E2E_WEBHOOK_PUBLIC_URL` overrides it with a
 * stable tunnel when one exists.
 */
import { spawn, type ChildProcess } from 'node:child_process';
import { createHmac, timingSafeEqual } from 'node:crypto';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

export interface ReceivedWebhook {
  headers: Record<string, string>;
  /** Exact bytes as delivered — the HMAC is over this, not over a re-serialisation. */
  raw: string;
  body: Record<string, unknown>;
}

/**
 * The quick-tunnel service did not hand out a URL in time.
 *
 * Thrown only from the tunnel's own provisioning — the 60s no-URL timeout,
 * which is how a `409` from Cloudflare's free provisioning API surfaces, or
 * cloudflared exiting early with that same refusal. A
 * missing binary, or anything after the sink has a URL, is a normal failure:
 * this type means "a third party we do not control was unavailable", so a
 * caller that catches it broadly would relabel real defects as an outage.
 */
export class TunnelUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'TunnelUnavailableError';
  }
}

/** Quick-tunnel attempts before giving up; each can burn the full 60s. */
const TUNNEL_ATTEMPTS = 2;

export class WebhookSink {
  private constructor(
    private readonly server: Server,
    private readonly tunnel: ChildProcess | null,
    readonly publicUrl: string,
    readonly port: number,
    readonly received: ReceivedWebhook[],
  ) {}

  /**
   * Start the sink and resolve a public URL for it.
   *
   * @param path path the server should POST to, e.g. `/webhook`
   */
  static async start(path = '/webhook'): Promise<WebhookSink> {
    const received: ReceivedWebhook[] = [];
    const server = createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on('data', (c: Buffer) => chunks.push(c));
      req.on('end', () => {
        const raw = Buffer.concat(chunks).toString('utf8');
        let body: Record<string, unknown> = {};
        try {
          body = JSON.parse(raw) as Record<string, unknown>;
        } catch {
          // Keep the raw bytes; the assertion will report the malformed body.
        }
        received.push({ headers: req.headers as Record<string, string>, raw, body });
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end('{"ok":true}');
      });
    });

    const fixedPort = process.env.E2E_WEBHOOK_PORT ? Number(process.env.E2E_WEBHOOK_PORT) : 0;
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(fixedPort, '0.0.0.0', resolve);
    });
    const port = (server.address() as AddressInfo).port;

    let tunnel: ChildProcess | null = null;
    let publicUrl: string;
    if (process.env.E2E_WEBHOOK_PUBLIC_URL) {
      publicUrl = process.env.E2E_WEBHOOK_PUBLIC_URL.replace(/\/$/, '') + path;
    } else {
      const started = await startQuickTunnelWithRetry(port).catch((err: Error) => {
        server.close();
        throw err;
      });
      tunnel = started.proc;
      publicUrl = started.url + path;
    }

    return new WebhookSink(server, tunnel, publicUrl, port, received);
  }

  /**
   * Wait for a webhook whose `event_type` matches, or reject on timeout.
   *
   * `match` narrows further, and a run that pays more than one invoice needs
   * it: unfiltered this returns the *first* delivery of that type the sink ever
   * took, so the second payment would be handed the first payment's webhook and
   * assert against it — green while attributing every payment to one invoice,
   * which is precisely the failure the multi-invoice run looks for.
   */
  async waitFor(
    eventType: string,
    timeoutMs: number,
    match: (hook: ReceivedWebhook) => boolean = () => true,
  ): Promise<ReceivedWebhook> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const hit = this.received.find((w) => w.body.event_type === eventType && match(w));
      if (hit) return hit;
      if (Date.now() >= deadline) {
        // Invoice ids too: with a filter in play, "received: payment_confirmed"
        // on its own reads as a server that never delivered, when in fact it
        // delivered for a different invoice.
        const seen =
          this.received
            .map((w) => `${String(w.body.event_type)}(${String(w.body.invoice_id ?? 'no invoice_id')})`)
            .join(', ') || 'none';
        throw new Error(
          `No '${eventType}' webhook matching the filter within ${timeoutMs}ms at ` +
            `${this.publicUrl} (received: ${seen})`,
        );
      }
      await new Promise((r) => setTimeout(r, 1_000));
    }
  }

  async stop(): Promise<void> {
    this.tunnel?.kill('SIGTERM');
    // `close()` stops accepting but waits on live sockets, and cloudflared holds
    // keep-alives — so without dropping them this can hang past the job timeout
    // and turn an already-passing test into a failure (it runs in a `finally`).
    await new Promise<void>((resolve) => {
      this.server.close(() => resolve());
      this.server.closeAllConnections();
    });
  }
}

/**
 * Verify the `X-Webhook-Signature` header against the store's webhook secret.
 *
 * Mirrors `WebhookService::sign_payload` — HMAC-SHA256 over the delivered body,
 * hex-encoded, prefixed with `sha256=`.
 */
export function verifySignature(hook: ReceivedWebhook, secret: string): boolean {
  const header = hook.headers['x-webhook-signature'];
  if (!header) return false;
  const expected = `sha256=${createHmac('sha256', secret).update(hook.raw).digest('hex')}`;
  const a = Buffer.from(header);
  const b = Buffer.from(expected);
  return a.length === b.length && timingSafeEqual(a, b);
}

/**
 * The free provisioning API is best-effort and rate-limited, so one refusal
 * says little; retry only the unavailable case, never a spawn failure.
 */
export async function startQuickTunnelWithRetry(
  port: number,
): Promise<{ proc: ChildProcess; url: string }> {
  for (let attempt = 1; ; attempt++) {
    try {
      return await startQuickTunnel(port);
    } catch (err) {
      if (!(err instanceof TunnelUnavailableError) || attempt >= TUNNEL_ATTEMPTS) throw err;
      console.log(
        `quick tunnel attempt ${attempt}/${TUNNEL_ATTEMPTS} failed, retrying: ${err.message}`,
      );
    }
  }
}

/** What cloudflared prints when the quick-tunnel API refuses to hand out a tunnel. */
const PROVISIONING_REFUSED = /quick tunnel provisioning failed|Tunnelstore request failed/i;

/** Spawn `cloudflared tunnel --url` and resolve the assigned trycloudflare host. */
function startQuickTunnel(port: number): Promise<{ proc: ChildProcess; url: string }> {
  const bin = process.env.E2E_CLOUDFLARED_BIN || 'cloudflared';
  const proc = spawn(bin, ['tunnel', '--no-autoupdate', '--url', `http://localhost:${port}`], {
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  return new Promise((resolve, reject) => {
    let output = '';
    const timeoutMs = Number(process.env.E2E_TUNNEL_URL_TIMEOUT_MS) || 60_000;
    const timer = setTimeout(() => {
      proc.kill('SIGTERM');
      reject(
        new TunnelUnavailableError(
          `cloudflared did not report a URL within ${timeoutMs / 1000}s. Output:\n${output}`,
        ),
      );
    }, timeoutMs);

    const onChunk = (chunk: Buffer) => {
      output += chunk.toString();
      const match = output.match(/https:\/\/[a-z0-9-]+\.trycloudflare\.com/);
      if (match) {
        clearTimeout(timer);
        proc.stdout?.off('data', onChunk);
        proc.stderr?.off('data', onChunk);
        resolve({ proc, url: match[0] });
      }
    };
    proc.stdout?.on('data', onChunk);
    proc.stderr?.on('data', onChunk);

    // `close`, not `exit`: it fires after the pipes drain, so the provisioning
    // error text is in `output` by the time we classify.
    proc.on('close', (code, signal) => {
      clearTimeout(timer);
      const detail = `cloudflared exited (${signal ?? `code ${code}`}) before reporting a URL. Output:\n${output}`;
      // Only Cloudflare refusing to provision is "unavailable". A crash, a bad
      // flag or a wrong-architecture binary also exits early and is our fault.
      reject(
        PROVISIONING_REFUSED.test(output) ? new TunnelUnavailableError(detail) : new Error(detail),
      );
    });

    proc.on('error', (err) => {
      clearTimeout(timer);
      reject(
        new Error(
          `Failed to spawn '${bin}': ${err.message}. Install cloudflared or set ` +
            `E2E_WEBHOOK_PUBLIC_URL to a tunnel that already forwards to this runner.`,
        ),
      );
    });
  });
}

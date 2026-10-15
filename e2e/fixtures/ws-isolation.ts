/**
 * Helpers for `tests/ws-tenant-isolation.spec.ts`: an authenticated `/ws`
 * client that records every frame, and the checks the spec rests on.
 *
 * The checks are pure and live here so `tests/ws-isolation-helpers.spec.ts`
 * can prove, with no network, that they go red when a foreign event arrives.
 * A negative assertion that has never been seen failing proves nothing: a
 * socket that is simply dead passes "did not receive the other tenant's
 * event" just as well as a socket that is correctly filtered.
 */
import { wsUrl } from './api';

/** One text frame off the socket, in arrival order. */
export interface Frame {
  /** Position in this socket's arrival order, 0-based. */
  seq: number;
  type: string;
  invoice_id?: string;
  status?: string;
}

/** Frames that say nothing about any store. */
const CONTROL_TYPES = new Set(['connected', 'ping']);

/** Frames that carry a store's data, i.e. everything but the control frames. */
export function dataFrames(frames: Frame[]): Frame[] {
  return frames.filter((f) => !CONTROL_TYPES.has(f.type));
}

/** Arrival position of the first frame about `invoiceId`, or -1. */
export function firstSeen(frames: Frame[], invoiceId: string): number {
  return frames.findIndex((f) => f.invoice_id === invoiceId);
}

/**
 * Positive control: the socket was told about every invoice it should have
 * been. Without this a dead socket would pass every negative check below.
 */
export function expectReceived(label: string, frames: Frame[], invoiceIds: string[]): void {
  for (const id of invoiceIds) {
    if (firstSeen(frames, id) < 0) {
      throw new Error(
        `${label}: never received its own invoice ${id}; got [${describe(frames)}]. ` +
          `A socket that hears nothing proves nothing about isolation.`,
      );
    }
  }
}

/**
 * Isolation: every data frame the socket received is about one of its own
 * invoices. Any other invoice id, or a data frame naming no invoice at all,
 * is a leak.
 */
export const FOREIGN_FRAME_MESSAGE = 'outside its stores';

export function expectNothingForeign(label: string, frames: Frame[], ownInvoiceIds: string[]): void {
  const own = new Set(ownInvoiceIds);
  const foreign = dataFrames(frames).filter((f) => !f.invoice_id || !own.has(f.invoice_id));
  if (foreign.length > 0) {
    throw new Error(
      `${label}: received ${foreign.length} frame(s) for invoices ${FOREIGN_FRAME_MESSAGE}: ` +
        `[${describe(foreign)}]`,
    );
  }
}

/** One socket's evidence: what it heard, and which invoices are its own. */
export interface SocketEvidence {
  label: string;
  frames: Frame[];
  own: string[];
}

/**
 * Decide the isolation result from both sockets, in an order that keeps the
 * three outcomes apart. A foreign frame is a leak whether or not any own event
 * ever arrived, so it is checked first and reported as a failure. Only when
 * nothing leaked does a missing own event matter, and then the isolation
 * claim is unproven rather than false: that is `NOT VERIFIED`, a state
 * distinct from a failed isolation check.
 */
export function judgeIsolation(evidence: SocketEvidence[]): void {
  for (const e of evidence) expectNothingForeign(e.label, e.frames, e.own);
  for (const e of evidence) {
    try {
      expectReceived(e.label, e.frames, e.own);
    } catch (err) {
      throw new Error(
        `NOT VERIFIED - isolation could not run: the precondition (the socket receiving its own events) ` +
          `was not established. No foreign frame was seen, but that proves nothing. ` +
          `This is not an isolation result. ${(err as Error).message}`,
      );
    }
  }
}

function describe(frames: Frame[]): string {
  return frames.map((f) => `${f.type}:${f.invoice_id ?? '-'}:${f.status ?? '-'}`).join(', ') || 'nothing';
}

/** An open, authenticated `/ws` connection that remembers everything it hears. */
export interface RecordingSocket {
  frames: Frame[];
  /** Resolves once a frame about `invoiceId` has arrived, rejects after `timeoutMs`. */
  waitForInvoice(invoiceId: string, timeoutMs: number): Promise<Frame>;
  close(): void;
}

/**
 * Open `/ws`, send the auth frame, and resolve once the server has answered
 * `connected` - the point at which the server has accepted the session and its
 * gate is live. Events published before that are not owed to this socket.
 */
export async function openAuthenticatedSocket(sessionToken: string, timeoutMs = 15_000): Promise<RecordingSocket> {
  const frames: Frame[] = [];
  const waiters = new Set<() => void>();
  const socket = new WebSocket(wsUrl('/ws'));

  socket.onmessage = (event: { data: unknown }) => {
    const parsed = JSON.parse(String(event.data)) as Omit<Frame, 'seq'>;
    frames.push({ ...parsed, seq: frames.length });
    for (const wake of waiters) wake();
  };

  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('no `connected` frame from /ws after auth')), timeoutMs);
    socket.onerror = () => {
      clearTimeout(timer);
      reject(new Error('/ws connection failed'));
    };
    socket.onclose = () => {
      clearTimeout(timer);
      reject(new Error('/ws closed before it acknowledged the session'));
    };
    socket.onopen = () => socket.send(JSON.stringify({ type: 'auth', token: sessionToken }));
    waiters.add(function check() {
      if (frames.some((f) => f.type === 'connected')) {
        clearTimeout(timer);
        waiters.delete(check);
        resolve();
      }
    });
  });
  socket.onerror = null;
  // A close after the handshake used to be dropped, so a socket the proxy cut
  // looked identical to one the server never wrote to. Record it so a timeout
  // says which it was.
  let closed: string | null = null;
  const openedAt = Date.now();
  socket.onclose = (event: { code?: number }) => {
    closed = `socket closed by peer after ${Math.round((Date.now() - openedAt) / 1000)}s (code ${event.code ?? '?'})`;
  };

  return {
    frames,
    waitForInvoice(invoiceId, waitMs) {
      return new Promise<Frame>((resolve, reject) => {
        const found = () => frames.find((f) => f.invoice_id === invoiceId);
        const now = found();
        if (now) return resolve(now);
        const timer = setTimeout(() => {
          waiters.delete(check);
          reject(new Error(`no frame for invoice ${invoiceId} within ${waitMs}ms; got [${describe(frames)}]; ${closed ?? 'socket still open'}`));
        }, waitMs);
        function check() {
          const hit = found();
          if (!hit) return;
          clearTimeout(timer);
          waiters.delete(check);
          resolve(hit);
        }
        waiters.add(check);
      });
    },
    close() {
      socket.close();
    },
  };
}

/**
 * Unit coverage for the checks `ws-tenant-isolation.spec.ts` rests on.
 *
 * The live spec can only ever pass against a correct server, so on its own it
 * cannot show that its negative assertions are able to fail. These run with no
 * network and feed the checks a fake event source: a clean timeline passes, and
 * a timeline with one foreign event, or with the own event missing, goes red.
 */
import { test, expect } from '@playwright/test';

import {
  dataFrames,
  expectNothingForeign,
  expectReceived,
  FOREIGN_FRAME_MESSAGE,
  firstSeen,
  judgeIsolation,
  type Frame,
} from '../fixtures/ws-isolation';

function timeline(...entries: Array<[string, string?, string?]>): Frame[] {
  return entries.map(([type, invoice_id, status], seq) => ({ seq, type, invoice_id, status }));
}

const OWN = 'inv-own';
const OTHER = 'inv-other';

test.describe('isolation checks', () => {
  test('a socket that heard only its own store passes both checks', () => {
    const frames = timeline(['connected'], ['invoice_status', OWN, 'expired']);
    expect(() => expectReceived('A', frames, [OWN])).not.toThrow();
    expect(() => expectNothingForeign('A', frames, [OWN])).not.toThrow();
  });

  test('one foreign event fails the isolation check, wherever it sits', () => {
    const before = timeline(['connected'], ['invoice_status', OTHER, 'expired'], ['invoice_status', OWN, 'expired']);
    const after = timeline(['connected'], ['invoice_status', OWN, 'expired'], ['invoice_status', OTHER, 'expired']);
    for (const frames of [before, after]) {
      expect(() => expectNothingForeign('A', frames, [OWN])).toThrow(new RegExp(`${FOREIGN_FRAME_MESSAGE}.*inv-other`));
    }
  });

  test('a foreign payment update fails the check as well', () => {
    const frames = timeline(['connected'], ['payment_update', OTHER, 'confirmed']);
    expect(() => expectNothingForeign('A', frames, [OWN])).toThrow(new RegExp(FOREIGN_FRAME_MESSAGE));
  });

  test('a data frame naming no invoice is not waved through', () => {
    const frames = timeline(['connected'], ['invoice_status', undefined, 'expired']);
    expect(() => expectNothingForeign('A', frames, [OWN])).toThrow(new RegExp(FOREIGN_FRAME_MESSAGE));
  });

  test('a silent socket fails the positive control, so it cannot pass as isolated', () => {
    const frames = timeline(['connected']);
    expect(() => expectNothingForeign('A', frames, [OWN])).not.toThrow();
    expect(() => expectReceived('A', frames, [OWN])).toThrow(/never received its own invoice inv-own/);
  });

  test('control frames are not data and do not count as a leak', () => {
    const frames = timeline(['connected'], ['ping'], ['invoice_status', OWN, 'expired']);
    expect(dataFrames(frames)).toHaveLength(1);
    expect(() => expectNothingForeign('A', frames, [OWN])).not.toThrow();
  });

  test('arrival order is observable, which is what makes "own came after foreign" meaningful', () => {
    const frames = timeline(['connected'], ['invoice_status', OTHER, 'expired'], ['invoice_status', OWN, 'expired']);
    expect(firstSeen(frames, OTHER)).toBeLessThan(firstSeen(frames, OWN));
    expect(firstSeen(frames, 'inv-absent')).toBe(-1);
  });

  test('a leak is reported as a failure even when no own event ever arrived', () => {
    const a = { label: 'A', frames: timeline(['connected'], ['invoice_status', OTHER, 'expired']), own: [OWN] };
    expect(() => judgeIsolation([a])).toThrow(new RegExp(FOREIGN_FRAME_MESSAGE));
    expect(() => judgeIsolation([a])).not.toThrow(/NOT VERIFIED/);
  });

  test('a silent socket with no leak is NOT VERIFIED, distinct from a failure', () => {
    const a = { label: 'A', frames: timeline(['connected']), own: [OWN] };
    expect(() => judgeIsolation([a])).toThrow(/NOT VERIFIED - isolation could not run/);
  });

  test('a clean socket that heard its own event passes the verdict', () => {
    const a = { label: 'A', frames: timeline(['connected'], ['invoice_status', OWN, 'expired']), own: [OWN] };
    expect(() => judgeIsolation([a])).not.toThrow();
  });

  test('the leak check covers every socket before any socket is judged NOT VERIFIED', () => {
    const silentA = { label: 'A', frames: timeline(['connected']), own: [OWN] };
    const leakyB = {
      label: 'B',
      frames: timeline(['connected'], ['invoice_status', OTHER, 'expired']),
      own: ['inv-b'],
    };
    expect(() => judgeIsolation([silentA, leakyB])).toThrow(new RegExp(FOREIGN_FRAME_MESSAGE));
    expect(() => judgeIsolation([silentA, leakyB])).not.toThrow(/NOT VERIFIED/);
  });

  test('a wrong-status product failure is not relabelled NOT VERIFIED by a missing event elsewhere', () => {
    const silentA = { label: 'A', frames: timeline(['connected']), own: [OWN] };
    const failure = ['B was told pending rather than expiry'];
    expect(() => judgeIsolation([silentA], failure)).toThrow(/B was told pending/);
    expect(() => judgeIsolation([silentA], failure)).not.toThrow(/NOT VERIFIED/);
  });

  test('a recorded cause is carried into NOT VERIFIED, and alone it prevents a clean pass', () => {
    const silentA = { label: 'A', frames: timeline(['connected']), own: [OWN] };
    expect(() => judgeIsolation([silentA], [], 'B: socket closed by peer')).toThrow(/NOT VERIFIED.*socket closed by peer/);
    const clean = { label: 'A', frames: timeline(['connected'], ['invoice_status', OWN, 'expired']), own: [OWN] };
    expect(() => judgeIsolation([clean], [], 'A: socket closed by peer')).toThrow(/NOT VERIFIED/);
  });
});

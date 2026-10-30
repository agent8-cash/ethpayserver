/**
 * The refusal must fire before any write. These feed the guard an explicit
 * environment, so they run with no server and no network.
 */
import { test, expect } from '@playwright/test';

import { assertSafeTarget, OPT_IN_VARIABLE, RESERVED_PORTS_VARIABLE } from '../fixtures/target-guard';

// An arbitrary port inside the local range, reserved only for these tests.
//
// EVERY REFUSAL TEST BELOW INJECTS ITS OWN LIST, so none of them exercises the
// set the harness actually runs with. They passed while the built-in set was
// empty and the no-override refusal could not fire at all - a fixture holding
// the tests outside the condition they are named for. The three cases at the
// bottom of this file cover the REAL default instead, and must be read as the
// ones that fail if the built-in set is emptied again.
const RESERVED = 3077;
const reserved = { [RESERVED_PORTS_VARIABLE]: `3001, ${RESERVED}` };
const live = `http://localhost:${RESERVED}`;

test('the live service port is refused in local mode, even on loopback', () => {
  expect(() => assertSafeTarget({ ...reserved, E2E_API_URL: live })).toThrow(/live service's port/);
  expect(() => assertSafeTarget({ ...reserved, E2E_API_URL: `http://127.0.0.1:${RESERVED}` })).toThrow(
    /live service's port/,
  );
});

test('the opt-in does not unlock the live service port', () => {
  expect(() => assertSafeTarget({ ...reserved, E2E_API_URL: live, [OPT_IN_VARIABLE]: 'true' })).toThrow(
    /live service's port/,
  );
});

test('a non-loopback host or an out-of-range port needs the opt-in', () => {
  for (const url of ['https://testnet.example.com', 'http://10.0.0.5:3000', 'http://localhost:4000']) {
    expect(() => assertSafeTarget({ E2E_API_URL: url })).toThrow(/refusing to run in local mode/);
    expect(() => assertSafeTarget({ E2E_API_URL: url, [OPT_IN_VARIABLE]: 'true' })).not.toThrow();
  }
});

test('the frontend URL is guarded too', () => {
  expect(() => assertSafeTarget({ E2E_BASE_URL: 'https://testnet.example.com' })).toThrow();
});

test('E2E_REMOTE=false is still local mode', () => {
  expect(() => assertSafeTarget({ ...reserved, E2E_REMOTE: 'false', E2E_API_URL: live })).toThrow();
});

test('the harness defaults and CI targets pass', () => {
  expect(() => assertSafeTarget({})).not.toThrow();
  expect(() =>
    assertSafeTarget({ E2E_API_URL: 'http://localhost:3000', E2E_BASE_URL: 'http://localhost:8080' }),
  ).not.toThrow();
});

test('remote mode is untouched', () => {
  expect(() =>
    assertSafeTarget({ E2E_REMOTE: 'true', E2E_API_URL: 'https://testnet.example.com' }),
  ).not.toThrow();
});

test('an IPv6 loopback target inside the range passes without the opt-in', () => {
  expect(() => assertSafeTarget({ E2E_API_URL: 'http://[::1]:3000' })).not.toThrow();
});

// ---------------------------------------------------------------------------
// THE REAL DEFAULT, with no E2E_RESERVED_PORTS at all. The live service's port
// is the one proxied to in docker/nginx/example.conf; the guard must refuse
// it with nothing configured, because an environment that forgets the variable
// is exactly the one that needs the refusal.
const LIVE = 3100;

test('the live service port is refused with no E2E_RESERVED_PORTS set', () => {
  expect(() => assertSafeTarget({ E2E_API_URL: `http://localhost:${LIVE}` })).toThrow(
    /live service's port/,
  );
  expect(() => assertSafeTarget({ E2E_API_URL: `http://127.0.0.1:${LIVE}` })).toThrow(
    /live service's port/,
  );
});

test('the opt-in cannot unlock the live service port with nothing configured', () => {
  // Without the built-in set this does not merely weaken: the port falls
  // through to the loopback/range check, where the opt-in DOES let it pass. So
  // this case is the difference between an unconditional refusal and an
  // overridable one, and it is the one to break when checking this file.
  expect(() =>
    assertSafeTarget({ E2E_API_URL: `http://localhost:${LIVE}`, [OPT_IN_VARIABLE]: 'true' }),
  ).toThrow(/live service's port/);
});

test('E2E_RESERVED_PORTS adds to the built-in set and cannot empty it', () => {
  const extra = { [RESERVED_PORTS_VARIABLE]: `${RESERVED}` };
  // the variable's own port is refused ...
  expect(() => assertSafeTarget({ ...extra, E2E_API_URL: `http://localhost:${RESERVED}` })).toThrow(
    /live service's port/,
  );
  // ... and the built-in one still is, which a replacing implementation breaks.
  expect(() => assertSafeTarget({ ...extra, E2E_API_URL: `http://localhost:${LIVE}` })).toThrow(
    /live service's port/,
  );
  // An explicitly empty variable must not disarm it either.
  expect(() =>
    assertSafeTarget({ [RESERVED_PORTS_VARIABLE]: '', E2E_API_URL: `http://localhost:${LIVE}` }),
  ).toThrow(/live service's port/);
});

test('an unparseable E2E_RESERVED_PORTS entry throws rather than being silently dropped', () => {
  for (const value of ['3100x', 'abc', `${RESERVED};3001`]) {
    expect(() =>
      assertSafeTarget({ [RESERVED_PORTS_VARIABLE]: value, E2E_API_URL: 'http://localhost:3000' }),
    ).toThrow(/unparseable/);
  }
});

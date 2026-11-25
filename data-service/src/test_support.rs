//! Shared database access for tests.
//!
//! A test that needs Postgres is run on purpose (`#[ignore]`d, and CI passes
//! `--run-ignored`). If the database is missing or down, that run must fail:
//! a helper that quietly returns `None` makes "11 passed" and "11 skipped"
//! the same output, and the suite goes green by not existing.
//!
//! Every database test connects through here. `scripts/check-test-db-skips.sh`
//! rejects the old `env::var("DATABASE_URL").ok()?` / `.connect(..).await.ok()?`
//! shapes anywhere else.
//!
//! A skip that is genuinely wanted (a test that is not ignored but can use a
//! database when one is present) goes through [`database_url_or_skip`], which
//! prints a fixed `SKIPPED:` line so skips can be counted in a CI log.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::PgDataService;

/// The connection string for the test database. Panics when it is unset.
#[track_caller]
pub fn database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "DATABASE_URL is not set: this test needs a database and is run on purpose, \
             so a missing database is a failure rather than a skip"
        )
    })
}

/// The connection string, or `None` after announcing the skip.
///
/// For a non-ignored test that can use a database when one is present. The
/// announcement has a fixed form, `SKIPPED: <test> (DATABASE_URL not set)`, so
/// `grep -c SKIPPED` counts skips. It is written to the process's stderr
/// handle directly, which gets past libtest's per-test capture. nextest
/// captures each test process's output and hides it for a test that passes, so
/// `.config/nextest.toml` sets `success-output = "final"`: the line then shows
/// in the run summary without `--nocapture`.
pub fn database_url_or_skip(test: &str) -> Option<String> {
    use std::io::Write;

    match std::env::var("DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "{}", skip_line(test));
            None
        }
    }
}

/// The announcement [`database_url_or_skip`] prints. One fixed form, so a CI
/// log can be searched for `SKIPPED:` and the hits counted.
fn skip_line(test: &str) -> String {
    format!("SKIPPED: {test} (DATABASE_URL not set)")
}

/// Connect a pool to `url`. Panics, naming `DATABASE_URL`, when it cannot.
///
/// A refused connection is probed first and fails at once: sqlx retries a
/// refused connect until its acquire timeout (30s by default), which would
/// make every test in a run against a dead database take half a minute to
/// report the same thing. The probe blocks its own test's thread for at most
/// three seconds; every test has a runtime of its own, so nothing else waits.
pub async fn pool_for(url: &str, max_connections: u32) -> PgPool {
    use std::net::ToSocketAddrs;
    use std::str::FromStr;
    use std::time::Duration;

    if let Ok(opts) = sqlx::postgres::PgConnectOptions::from_str(url) {
        // Unix-socket hosts are paths; leave those to sqlx.
        let host = opts.get_host();
        let port = opts.get_port();
        if !host.starts_with('/') {
            let reachable = (host, port).to_socket_addrs().is_ok_and(|mut addrs| {
                addrs.any(|a| {
                    std::net::TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok()
                })
            });
            assert!(
                reachable,
                "DATABASE_URL is set but the database is unreachable: nothing accepts \
                 connections at {host}:{port}"
            );
        }
    }
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but the database is unreachable: {e}"))
}

/// A pool on the test database. Panics when `DATABASE_URL` is unset or the
/// database cannot be reached.
///
/// Also installs the test-only xpub encryption key
/// (`xpub_encryption::init_insecure_test_default`), idempotently: any test
/// that reaches a real database by this route and then touches a wallet
/// needs a key provider installed first, same as the real server does at
/// startup, and this is the one place in the test harness nearly all of them
/// pass through. Never do this outside a test binary - production startup
/// installs the real provider itself, before this would ever run.
pub async fn pg_pool(max_connections: u32) -> PgPool {
    crate::xpub_encryption::init_insecure_test_default();
    pool_for(&database_url(), max_connections).await
}

/// A [`PgDataService`] on the test database, with the same failure behaviour
/// as [`pg_pool`].
pub async fn pg_service() -> PgDataService {
    PgDataService::new(pg_pool(10).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_is_announced_in_one_fixed_greppable_form() {
        assert_eq!(
            skip_line("some::test"),
            "SKIPPED: some::test (DATABASE_URL not set)"
        );
    }

    /// The failure this module exists for: a database that is not there must
    /// fail the run, and the message must name the variable.
    #[tokio::test]
    #[should_panic(expected = "DATABASE_URL is set but the database is unreachable")]
    async fn a_closed_port_fails_instead_of_skipping() {
        pool_for("postgres://x:x@127.0.0.1:1/x", 1).await;
    }
}

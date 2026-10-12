//! Schema-per-plugin storage: creation, migrations, and a scoped connection
//! handle.
//!
//! Copies BTCPay's `BaseDbContextFactory<T>` shape: each plugin gets its own
//! Postgres schema and ships its own migrations, which the host runs on
//! install and upgrade. A plugin never gets a connection to `public` — see
//! [`PluginSchema::acquire`] for what that scoping does and does not buy.
//!
//! ## For plugin authors: migration checksums are baked in at compile time
//!
//! `sqlx` records a SHA-384 of each migration file's *bytes* the first time
//! it applies, and refuses to run again if that checksum ever changes — this
//! is the same trap documented for the host's own migrations in this repo's
//! `CLAUDE.md`, and it applies identically to a plugin's own migrations
//! directory. Once a plugin build has shipped a migration and it has run
//! against even one installation, editing that file — including something
//! as small as fixing a comment or reformatting whitespace — breaks every
//! future install or upgrade against a database where it already applied.
//! Add a new migration instead of editing an old one; renaming an
//! unapplied file is free, changing its bytes is not.

use std::path::Path;

use payserver_plugin_api::PluginId;
use sqlx::Transaction;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgPool, Postgres};

/// Postgres's identifier length limit (`NAMEDATALEN` 64, minus the
/// terminator). A schema name past this is silently truncated by Postgres,
/// which could make two differently-named plugins collide on one schema —
/// refused up front instead of discovered later as data corruption.
const MAX_IDENTIFIER_LEN: usize = 63;

/// How long one plugin statement may run before Postgres cancels it.
///
/// Matches the host's own default call deadline: a plugin gets the same
/// budget whether it spends it computing or waiting on a query.
const STATEMENT_TIMEOUT_MS: u64 = 2_000;

/// How long a plugin may sit inside an open transaction doing nothing.
/// Longer than a statement, short enough that an abandoned transaction does
/// not hold locks past a request.
const IDLE_TX_TIMEOUT_MS: u64 = 5_000;

/// Connections one plugin may hold at once. Two, not one: a pool that cannot
/// open a second connection serialises every call through the first, and a
/// plugin that is merely slow would then look like a plugin that is stuck.
const ROLE_CONNECTION_LIMIT: u32 = 2;

/// Bytes of randomness behind a generated role password.
const ROLE_PASSWORD_BYTES: usize = 24;

/// A fresh password for a plugin's login role.
///
/// Alphanumeric because `CREATE ROLE` takes no bind parameters, so the value
/// is interpolated into SQL and must carry nothing that could end the quoted
/// literal. Drawn from the OS CSPRNG, not a seeded generator.
///
/// # Errors
/// If the OS random source is unavailable. Returned rather than panicked:
/// this runs during a plugin install, and failing that install is a far
/// better outcome than either taking the process down or - much worse -
/// falling back to something predictable for a credential.
pub fn generate_role_password() -> Result<String, PluginStorageError> {
    let mut bytes = [0u8; ROLE_PASSWORD_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| PluginStorageError::Randomness(e.to_string()))?;
    // Hex rather than base64: alphanumeric by construction, with no
    // characters to escape and no padding to strip.
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Why a plugin storage operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PluginStorageError {
    /// A role password that is not alphanumeric. Only reachable by bypassing
    /// [`generate_role_password`]; refused rather than escaped, because a
    /// password is interpolated into `CREATE ROLE` and escaping is the part
    /// that gets subtly wrong.
    #[error("a plugin role password must be non-empty and alphanumeric")]
    InvalidRolePassword,

    /// The OS random source refused. A plugin install fails rather than
    /// proceeding with a credential that is not unpredictable.
    #[error("could not generate a plugin role password: {0}")]
    Randomness(String),

    /// `plugin_<id>` is longer than Postgres identifiers allow.
    #[error(
        "plugin id {0:?} produces a schema name longer than {MAX_IDENTIFIER_LEN} bytes; \
         Postgres would silently truncate it"
    )]
    SchemaNameTooLong(String),

    #[error("plugin storage database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("plugin migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// The Postgres schema name for `id`: `plugin_<id>`. The prefix keeps it out
/// of the way of `public` and of any core schema that might exist later.
/// The login role a plugin's own statements run as.
///
/// The same identifier as its schema, deliberately: `provision_role` grants
/// against the schema of this name, and `pools` authenticates as the role of
/// this name. Deriving both from one function is what stops a connection
/// authenticating as a role whose grants point at a different schema.
///
/// Takes the id as `&str` rather than `&PluginId` so a caller holding only
/// the stored string does not have to re-parse it.
///
/// # Errors
/// If the resulting name exceeds Postgres's identifier limit.
pub fn role_name(plugin_id: &str) -> Result<String, PluginStorageError> {
    let name = format!("plugin_{plugin_id}");
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(PluginStorageError::SchemaNameTooLong(name));
    }
    Ok(name)
}

fn schema_name(id: &PluginId) -> Result<String, PluginStorageError> {
    role_name(id.as_str())
}

/// Double-quote a Postgres identifier, escaping embedded quotes.
///
/// Every caller here builds `name` from [`schema_name`], which in turn
/// builds it from a [`PluginId`] — already restricted to ASCII
/// alphanumerics, `.`, `-` and `_` by [`PluginId::new`], so there is never
/// actually a quote to escape. Escaping anyway costs nothing and keeps this
/// correct if that charset ever loosens.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Schema-per-plugin storage, backed by the same database as core data.
#[derive(Clone)]
pub struct PluginStorage {
    pool: PgPool,
}

impl PluginStorage {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create `id`'s schema if it does not already exist, then run the
    /// migrations at `migrations_dir` against it.
    ///
    /// Serves both install and upgrade: schema creation is idempotent, and
    /// sqlx tracks applied versions inside the plugin's own schema, so
    /// calling this again after a fixed migration resumes rather than
    /// reapplying what already succeeded.
    ///
    /// A failed migration returns `Err`. Whatever migrations already
    /// committed before the failing one stay applied — each runs in its own
    /// transaction — but this method does not report success, and the
    /// caller must not register the plugin as installed. That is what keeps
    /// a failed migration from leaving the plugin half-installed instead of
    /// not installed.
    pub async fn install(
        &self,
        id: &PluginId,
        migrations_dir: &Path,
    ) -> Result<PluginSchema, PluginStorageError> {
        let name = schema_name(id)?;

        sqlx::query(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(&name)
        ))
        .execute(&self.pool)
        .await?;

        let migrator = Migrator::new(migrations_dir).await?;
        let mut conn = self.pool.acquire().await?;
        sqlx::query(&format!("SET search_path TO {}", quote_ident(&name)))
            .execute(&mut *conn)
            .await?;
        let migration_result = migrator.run(&mut *conn).await;

        // `conn` is a physical connection borrowed from the shared pool, and
        // `SET` (unlike `SET LOCAL`) changes it for the rest of the
        // connection's life, not just this borrow — sqlx does not reset
        // session state on release. Left alone, the next unrelated query
        // that happens to reuse this connection (including a core query
        // that assumes the default `public` search_path) would run scoped
        // to this plugin's schema instead. Reset unconditionally, whether
        // the migration succeeded or not, before deciding what to return.
        let reset_result = sqlx::query("RESET search_path").execute(&mut *conn).await;
        migration_result?;
        reset_result?;

        Ok(PluginSchema {
            pool: self.pool.clone(),
            name,
        })
    }

    /// Uninstall `id`. Dropping the schema is opt-in via `drop_schema`
    /// because an admin who uninstalls a plugin to debug it
    /// should not lose its data — the billing plugin's subscription records
    /// most of all. When `drop_schema` is `false` this leaves the schema and
    /// its data in place for a future reinstall to resume against.
    pub async fn uninstall(
        &self,
        id: &PluginId,
        drop_schema: bool,
    ) -> Result<(), PluginStorageError> {
        if !drop_schema {
            return Ok(());
        }
        let name = schema_name(id)?;
        sqlx::query(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&name)
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Create the login role `id`'s own statements run as, and grant it its
    /// schema and nothing else.
    ///
    /// This is what turns schema-per-plugin from an organising convention
    /// into a boundary Postgres enforces. The host keeps ownership of the
    /// schema and every table in it; the role gets DML and no `CREATE`, so a
    /// plugin cannot alter its own shape at runtime and its schema stays
    /// exactly what its migrations describe.
    ///
    /// Three limits are set on the role rather than trusted to the caller:
    ///
    /// - `statement_timeout`, because a plugin's call deadline does **not**
    ///   cover time spent inside a host call - wasmtime's epoch interruption
    ///   traps wasm execution, and a database query is not wasm execution.
    ///   Without this a hung query holds its thread past any deadline the
    ///   host believes it set. Postgres cancels it instead.
    /// - `idle_in_transaction_session_timeout`, so a plugin that opens a
    ///   transaction and stops cannot hold locks indefinitely.
    /// - `CONNECTION LIMIT`, so one plugin cannot exhaust the server's
    ///   connections and take every other plugin - and the core - down with
    ///   it.
    ///
    /// `password` must be alphanumeric: it is interpolated, because Postgres
    /// accepts no bind parameters in `CREATE ROLE`, and
    /// [`generate_role_password`] is the only intended source.
    pub async fn provision_role(
        &self,
        id: &PluginId,
        password: &str,
    ) -> Result<(), PluginStorageError> {
        if password.is_empty() || !password.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(PluginStorageError::InvalidRolePassword);
        }
        let name = schema_name(id)?;
        let quoted = quote_ident(&name);

        // Idempotent: a re-install, or an install retried after a failure
        // partway through, must converge rather than refuse. The password is
        // reset either way, so the stored credential is always the live one.
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_roles WHERE rolname = $1")
            .bind(&name)
            .fetch_optional(&self.pool)
            .await?;
        let create_or_alter = if exists.is_some() { "ALTER" } else { "CREATE" };
        sqlx::query(&format!(
            "{create_or_alter} ROLE {quoted} LOGIN PASSWORD '{password}'"
        ))
        .execute(&self.pool)
        .await?;

        for statement in [
            format!("ALTER ROLE {quoted} SET statement_timeout = '{STATEMENT_TIMEOUT_MS}ms'"),
            format!(
                "ALTER ROLE {quoted} SET idle_in_transaction_session_timeout = '{IDLE_TX_TIMEOUT_MS}ms'"
            ),
            format!("ALTER ROLE {quoted} CONNECTION LIMIT {ROLE_CONNECTION_LIMIT}"),
            // Its own schema, and the objects the host created in it. The
            // `ALTER DEFAULT PRIVILEGES` lines are not redundant with the
            // `ALL TABLES` ones: a grant on "all tables" covers the tables
            // that exist now, and says nothing about the ones a later
            // migration adds.
            format!("GRANT USAGE ON SCHEMA {quoted} TO {quoted}"),
            format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {quoted} TO {quoted}"
            ),
            format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {quoted} TO {quoted}"),
            format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {quoted} GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {quoted}"
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {quoted} GRANT USAGE, SELECT ON SEQUENCES TO {quoted}"
            ),
            // Defence in depth, and worth being precise about what it does
            // and does not do. These revoke privileges granted *directly* to
            // this role; they cannot revoke what the role inherits from
            // PUBLIC, which every role holds unconditionally. On a database
            // created the usual way PUBLIC holds `USAGE` on `public`, so a
            // plugin role can see that the schema exists.
            //
            // What actually keeps core data out of reach is that no core
            // table grants anything to PUBLIC: schema `USAGE` without a
            // table privilege reaches nothing. These two lines exist so that
            // a direct grant added later - by a migration, or by hand during
            // an incident - does not quietly survive. They are not the
            // boundary; `a_plugin_role_cannot_read_core_tables` tests the
            // boundary, against a database shaped like production.
            format!("REVOKE ALL ON SCHEMA public FROM {quoted}"),
            format!("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM {quoted}"),
        ] {
            sqlx::query(&statement).execute(&self.pool).await?;
        }

        Ok(())
    }

    /// Drop `id`'s login role.
    ///
    /// `DROP OWNED BY` first, and it does not touch the plugin's data: the
    /// host owns the schema and its tables, so what the role owns is only the
    /// privileges granted to it. Postgres refuses to drop a role while any
    /// such grant still references it, which is the failure this ordering
    /// avoids.
    ///
    /// The caller must have closed the role's connection pool first -
    /// `DROP ROLE` fails while a session is authenticated as it.
    pub async fn drop_role(&self, id: &PluginId) -> Result<(), PluginStorageError> {
        let name = schema_name(id)?;
        let quoted = quote_ident(&name);

        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_roles WHERE rolname = $1")
            .bind(&name)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Ok(());
        }

        sqlx::query(&format!("DROP OWNED BY {quoted}"))
            .execute(&self.pool)
            .await?;
        sqlx::query(&format!("DROP ROLE IF EXISTS {quoted}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A handle to `id`'s own schema for the host API. Does not require `id`
    /// to have just been installed — a caller that already knows a plugin is
    /// installed can go straight here without redoing `install`.
    pub fn schema(&self, id: &PluginId) -> Result<PluginSchema, PluginStorageError> {
        Ok(PluginSchema {
            pool: self.pool.clone(),
            name: schema_name(id)?,
        })
    }
}

/// A handle scoped to one plugin's own schema: the host API's item 3, a
/// plugin can reach its own tables through here and nothing else.
#[derive(Clone)]
pub struct PluginSchema {
    pool: PgPool,
    name: String,
}

impl PluginSchema {
    /// The schema's unquoted name, e.g. `plugin_cash.random.billing`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A transaction whose `search_path` is this schema alone (no
    /// `public`), so an unqualified table name in a query the plugin issues
    /// can only resolve inside its own schema. The caller must `commit` it
    /// for any writes to persist; dropping it without committing rolls it
    /// back, same as any other sqlx transaction.
    ///
    /// This hands back a transaction rather than a bare pooled connection
    /// so the scoping can use `SET LOCAL` instead of `SET`: `SET LOCAL`
    /// reverts automatically when the transaction ends, by commit or by
    /// drop-triggered rollback, whereas plain `SET` changes the physical
    /// connection for the rest of its life. Since connections here are
    /// borrowed from the same pool the host's own core queries draw from,
    /// a `SET` that outlived this call would leak this plugin's search
    /// path into whatever unrelated query reuses the connection next.
    ///
    /// This is a default, not an enforcement boundary: a query that
    /// qualifies a table explicitly (`public.invoices`) still reaches it,
    /// because the connection runs as the same database role as the rest of
    /// the host. Real enforcement would mean a per-plugin database role with
    /// grants revoked on every other schema, which is out of scope here and
    /// unnecessary for the threat this actually defends against: the admin
    /// installing a plugin already trusts it. What
    /// schema-scoping buys is blast radius and upgradability: a plugin that
    /// only ever names its own tables cannot be broken by a core migration,
    /// and core migrations stay free to change core tables however they
    /// need to.
    pub async fn acquire(&self) -> Result<Transaction<'static, Postgres>, PluginStorageError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(&format!(
            "SET LOCAL search_path TO {}",
            quote_ident(&self.name)
        ))
        .execute(&mut *tx)
        .await?;
        Ok(tx)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// A migrations directory with nothing in it. The role tests are about
    /// grants, not schema contents, and create whatever tables they need as
    /// the host afterwards - which is also the case `ALTER DEFAULT
    /// PRIVILEGES` exists to cover.
    fn no_migrations() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Drop whatever a previous run left behind for `id`.
    ///
    /// These tests share one Postgres instance and run with
    /// `--test-threads=1`, so a test that fails part-way leaves its schema
    /// and role in place and the *next* run fails on that residue instead of
    /// on the thing under test - which is how a real failure gets mistaken
    /// for a dirty fixture. Cleaning up at the start rather than only at the
    /// end makes every one of them re-runnable regardless of how the last
    /// run ended.
    async fn reset(storage: &PluginStorage, id: &PluginId) {
        storage.drop_role(id).await.expect("drop any leftover role");
        storage
            .uninstall(id, true)
            .await
            .expect("drop any leftover schema");
    }

    fn plugin_id(id: &str) -> PluginId {
        PluginId::new(id).unwrap()
    }

    /// A pool authenticated as `role`, against the same database
    /// `DATABASE_URL` names.
    ///
    /// Connecting *as the role* is the whole point: every assertion about
    /// what a plugin can and cannot reach has to be made on a connection
    /// Postgres has authenticated as that role, not on the host's connection
    /// with a `SET ROLE` applied. A session can always `RESET ROLE` back to
    /// the role it authenticated as, so a test written that way would be
    /// asserting against an escape hatch the plugin itself could take.
    async fn pool_as_role(role: &str, password: &str) -> PgPool {
        let url = data_service::test_support::database_url();
        let rest = url.split_once("://").expect("scheme").1;
        let host_and_db = rest.rsplit_once('@').map_or(rest, |(_, after)| after);
        let as_role = format!(
            "postgres://{}:{}@{}",
            urlencode(role),
            password,
            host_and_db
        );
        PgPoolOptions::new()
            .max_connections(1)
            .connect(&as_role)
            .await
            .expect("connect as the plugin role")
    }

    /// Percent-encodes the few characters a role name can carry that a URL
    /// userinfo field cannot. Plugin ids contain dots, which are fine, but
    /// the schema prefix makes the name long enough to be worth doing
    /// properly rather than assuming.
    fn urlencode(value: &str) -> String {
        value
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }

    async fn test_pool() -> PgPool {
        data_service::test_support::pg_pool(3).await
    }

    /// A fresh migrations directory with one migration that creates a table,
    /// so a test can install a plugin without shipping a fixture on disk.
    fn write_migration(dir: &Path, version: &str, sql: &str) {
        std::fs::write(dir.join(format!("{version}_test.sql")), sql).unwrap();
    }

    /// Ticket item 1 + 2: install creates the plugin's own schema and runs
    /// its migrations there, not against `public`.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn install_creates_schema_and_runs_migrations_scoped_to_it() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.install");

        let dir = tempfile::tempdir().unwrap();
        write_migration(
            dir.path(),
            "1",
            "CREATE TABLE widgets (id INT PRIMARY KEY);",
        );

        let schema = storage.install(&id, dir.path()).await.unwrap();

        // The table exists inside the plugin's schema...
        let in_schema: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = 'widgets')",
        )
        .bind(schema.name())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            in_schema.0,
            "widgets table should exist in {}",
            schema.name()
        );

        // ...and not in public.
        let in_public: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = 'widgets')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!in_public.0, "widgets table must not leak into public");

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Ticket item 2: a migration that fails blocks the install — the error
    /// propagates rather than being swallowed, so a caller cannot mistake
    /// this for a successful install.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn a_failing_migration_returns_an_error_instead_of_succeeding() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.failing");

        let dir = tempfile::tempdir().unwrap();
        write_migration(dir.path(), "1", "THIS IS NOT VALID SQL;");

        let result = storage.install(&id, dir.path()).await;
        assert!(
            result.is_err(),
            "a broken migration must not report success"
        );

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Ticket item 3: a connection from `PluginSchema::acquire` only sees the
    /// plugin's own tables for an unqualified name.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn acquired_connection_is_scoped_to_the_plugin_schema() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.scoped");

        let dir = tempfile::tempdir().unwrap();
        write_migration(
            dir.path(),
            "1",
            "CREATE TABLE widgets (id INT PRIMARY KEY); INSERT INTO widgets VALUES (1);",
        );
        let schema = storage.install(&id, dir.path()).await.unwrap();

        let mut tx = schema.acquire().await.unwrap();
        let row = sqlx::query("SELECT id FROM widgets")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(row.get::<i32, _>("id"), 1);
        tx.commit().await.unwrap();

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Regression test for the search_path leak the automated review caught:
    /// with the pool pinned to a single physical connection, `install`'s
    /// `SET search_path` must not survive past the migration, or the very
    /// next unrelated query borrowing this connection (a core query
    /// assuming the default `public` path, say) would inherit it.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn install_does_not_leak_search_path_to_the_next_pool_borrower() {
        let pool = data_service::test_support::pg_pool(1).await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.installleak");

        let baseline: (String,) = sqlx::query_as("SHOW search_path")
            .fetch_one(&pool)
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        write_migration(
            dir.path(),
            "1",
            "CREATE TABLE widgets (id INT PRIMARY KEY);",
        );
        storage.install(&id, dir.path()).await.unwrap();

        let after: (String,) = sqlx::query_as("SHOW search_path")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after.0, baseline.0,
            "install must reset search_path before releasing its connection back to the pool"
        );

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Same regression, for `PluginSchema::acquire`: its `SET LOCAL` must
    /// not survive past the transaction it was set in, whether the caller
    /// commits or just drops it.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn acquire_does_not_leak_search_path_to_the_next_pool_borrower() {
        let pool = data_service::test_support::pg_pool(1).await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.acquireleak");

        let dir = tempfile::tempdir().unwrap();
        write_migration(
            dir.path(),
            "1",
            "CREATE TABLE widgets (id INT PRIMARY KEY);",
        );
        // Baseline captured before any plugin operation touches the pool,
        // so this isolates acquire's own leak from install's — install runs
        // its own SET search_path too, and asserting against a baseline
        // taken after install would only prove the two leaks match, not
        // that either was absent.
        let baseline: (String,) = sqlx::query_as("SHOW search_path")
            .fetch_one(&pool)
            .await
            .unwrap();

        let schema = storage.install(&id, dir.path()).await.unwrap();

        let mut tx = schema.acquire().await.unwrap();
        sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();

        let after: (String,) = sqlx::query_as("SHOW search_path")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after.0, baseline.0,
            "acquire's SET LOCAL must not survive past its transaction"
        );

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Ticket item 5: uninstalling without asking to drop the schema leaves
    /// its data behind.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL"]
    async fn uninstall_without_drop_schema_keeps_the_data() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.storagetest.keepdata");

        let dir = tempfile::tempdir().unwrap();
        write_migration(
            dir.path(),
            "1",
            "CREATE TABLE widgets (id INT PRIMARY KEY);",
        );
        let schema = storage.install(&id, dir.path()).await.unwrap();

        storage.uninstall(&id, false).await.unwrap();

        let still_there: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = 'widgets')",
        )
        .bind(schema.name())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still_there.0, "opt-in uninstall must not drop the schema");

        // Clean up for real so the shared test database doesn't accumulate
        // schemas across runs.
        storage.uninstall(&id, true).await.unwrap();
    }

    #[test]
    fn schema_name_prefixes_and_preserves_the_plugin_id() {
        let id = plugin_id("cash.random.billing");
        assert_eq!(schema_name(&id).unwrap(), "plugin_cash.random.billing");
    }

    #[test]
    fn schema_name_refuses_an_id_that_would_be_truncated() {
        let long_id = "a".repeat(60);
        let id = plugin_id(&long_id);
        assert!(matches!(
            schema_name(&id),
            Err(PluginStorageError::SchemaNameTooLong(_))
        ));
    }

    /// The property the whole role design exists for.
    ///
    /// A plugin's schema was only ever a `search_path` default: on the host's
    /// connection a query naming `public.installed_plugins` reaches it, and
    /// on our deployments the host connects as a superuser. This asserts that
    /// a connection authenticated as the plugin's own role cannot.
    ///
    /// The `GRANT USAGE ... TO PUBLIC` is not incidental setup - it is what
    /// makes this test mean anything. A database created the usual way
    /// already grants PUBLIC `USAGE` on `public`, and testnet does; the
    /// fixture database this runs against happens to have had it revoked.
    /// Without this line the test would pass on the fixture for a reason that
    /// does not hold in production, and would therefore not be testing the
    /// boundary at all. Granting it first means the denial proven here is the
    /// table-level one - the only one that is actually load-bearing.
    #[tokio::test]
    #[ignore]
    async fn a_plugin_role_cannot_read_core_tables() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.rolescope.core");
        let password = generate_role_password().unwrap();

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage.provision_role(&id, &password).await.unwrap();
        sqlx::query("GRANT USAGE ON SCHEMA public TO PUBLIC")
            .execute(&pool)
            .await
            .unwrap();

        let as_plugin = pool_as_role(&schema_name(&id).unwrap(), &password).await;
        let result = sqlx::query("SELECT id FROM public.installed_plugins LIMIT 1")
            .fetch_optional(&as_plugin)
            .await;

        assert!(
            result.is_err(),
            "a plugin role read a core table; the schema boundary is not enforced"
        );
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("permission denied"),
            "expected a privilege refusal, got: {message}"
        );

        as_plugin.close().await;
        storage.drop_role(&id).await.unwrap();
        storage.uninstall(&id, true).await.unwrap();
    }

    /// No `CREATE`, so a plugin's shape is exactly what its migrations say
    /// and an upgrade's migrations cannot meet a schema that drifted under
    /// them.
    #[tokio::test]
    #[ignore]
    async fn a_plugin_role_cannot_create_tables_in_its_own_schema() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.rolescope.ddl");
        let password = generate_role_password().unwrap();
        let schema = schema_name(&id).unwrap();

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage.provision_role(&id, &password).await.unwrap();

        let as_plugin = pool_as_role(&schema, &password).await;
        let result = sqlx::query(&format!(
            "CREATE TABLE {}.snuck_in (x int)",
            quote_ident(&schema)
        ))
        .execute(&as_plugin)
        .await;

        assert!(
            result.is_err(),
            "a plugin created a table; its schema no longer matches its migrations"
        );

        as_plugin.close().await;
        storage.drop_role(&id).await.unwrap();
        storage.uninstall(&id, true).await.unwrap();
    }

    /// A plugin can use its own tables — the boundary has to let the intended
    /// traffic through, or the previous two tests would pass on a role that
    /// simply cannot do anything.
    ///
    /// Also covers the subtle half: the table is created *after* the role was
    /// provisioned, so it is `ALTER DEFAULT PRIVILEGES` that makes it usable,
    /// not the `GRANT ... ON ALL TABLES`. A later migration adding a table is
    /// exactly this case, and without the default privileges the plugin
    /// breaks on upgrade rather than on install.
    #[tokio::test]
    #[ignore]
    async fn a_plugin_role_can_use_a_table_created_after_it_was_provisioned() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.rolescope.dml");
        let password = generate_role_password().unwrap();
        let schema = schema_name(&id).unwrap();

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage.provision_role(&id, &password).await.unwrap();

        // Host creates the table, after provisioning.
        sqlx::query(&format!(
            "CREATE TABLE {}.subscriptions (account_id text primary key, paid_until timestamptz)",
            quote_ident(&schema)
        ))
        .execute(&pool)
        .await
        .unwrap();

        let as_plugin = pool_as_role(&schema, &password).await;
        sqlx::query(&format!(
            "INSERT INTO {}.subscriptions (account_id, paid_until) VALUES ($1, now())",
            quote_ident(&schema)
        ))
        .bind("acct-7")
        .execute(&as_plugin)
        .await
        .expect("a plugin must be able to write its own tables");

        let row: (String,) = sqlx::query_as(&format!(
            "SELECT account_id FROM {}.subscriptions",
            quote_ident(&schema)
        ))
        .fetch_one(&as_plugin)
        .await
        .expect("a plugin must be able to read its own tables");
        assert_eq!(row.0, "acct-7");

        as_plugin.close().await;
        storage.drop_role(&id).await.unwrap();
        storage.uninstall(&id, true).await.unwrap();
    }

    /// Dropping the role must not take the plugin's data with it. The host
    /// owns the tables, so `DROP OWNED BY` removes only the grants — an
    /// uninstall-to-debug keeps the subscription records.
    #[tokio::test]
    #[ignore]
    async fn dropping_the_role_leaves_the_plugins_data_intact() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.rolescope.dropdata");
        let schema = schema_name(&id).unwrap();

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage
            .provision_role(&id, &generate_role_password().unwrap())
            .await
            .unwrap();
        sqlx::query(&format!(
            "CREATE TABLE {}.kept (x int)",
            quote_ident(&schema)
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "INSERT INTO {}.kept VALUES (1)",
            quote_ident(&schema)
        ))
        .execute(&pool)
        .await
        .unwrap();

        storage.drop_role(&id).await.unwrap();

        let count: (i64,) = sqlx::query_as(&format!(
            "SELECT count(*) FROM {}.kept",
            quote_ident(&schema)
        ))
        .fetch_one(&pool)
        .await
        .expect("the table must still exist after the role is gone");
        assert_eq!(count.0, 1, "dropping the role destroyed the plugin's data");

        storage.uninstall(&id, true).await.unwrap();
    }

    /// Install is retried after a partial failure, and a reinstall happens.
    /// Neither may refuse because the role is already there.
    #[tokio::test]
    #[ignore]
    async fn provisioning_the_same_role_twice_converges() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool.clone());
        let id = plugin_id("cash.random.rolescope.twice");

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage
            .provision_role(&id, &generate_role_password().unwrap())
            .await
            .unwrap();

        let second = generate_role_password().unwrap();
        storage
            .provision_role(&id, &second)
            .await
            .expect("provisioning must be idempotent");

        // The second password is the live one, so the stored credential and
        // the role never disagree.
        let as_plugin = pool_as_role(&schema_name(&id).unwrap(), &second).await;
        sqlx::query("SELECT 1").execute(&as_plugin).await.unwrap();

        as_plugin.close().await;
        storage.drop_role(&id).await.unwrap();
        storage.uninstall(&id, true).await.unwrap();
    }

    /// The plugin host cannot cancel a thread blocked in a host call, so the
    /// only bound on a stalled query is the one Postgres enforces on the role.
    /// A query that sleeps far past the timeout must be cancelled by the
    /// server, on a connection authenticated as the plugin's own role.
    #[tokio::test]
    #[ignore]
    async fn a_plugin_role_query_is_cancelled_by_the_statement_timeout() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool);
        let id = plugin_id("cash.random.rolescope.timeout");
        let password = generate_role_password().unwrap();

        reset(&storage, &id).await;
        let migrations = no_migrations();
        storage.install(&id, migrations.path()).await.unwrap();
        storage.provision_role(&id, &password).await.unwrap();

        let as_plugin = pool_as_role(&schema_name(&id).unwrap(), &password).await;
        let started = std::time::Instant::now();
        let result = sqlx::query("SELECT pg_sleep(30)").execute(&as_plugin).await;
        let elapsed = started.elapsed();

        let message = result
            .expect_err("a 30s query must not complete")
            .to_string();
        assert!(
            message.contains("statement timeout"),
            "expected Postgres to cancel the query, got: {message}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(STATEMENT_TIMEOUT_MS * 3),
            "query ran {elapsed:?}, far past the {STATEMENT_TIMEOUT_MS}ms role timeout"
        );

        as_plugin.close().await;
        storage.drop_role(&id).await.unwrap();
        storage.uninstall(&id, true).await.unwrap();
    }

    /// Dropping a role that was never created is not an error: uninstall runs
    /// against plugins installed before roles existed.
    #[tokio::test]
    #[ignore]
    async fn dropping_a_role_that_does_not_exist_is_fine() {
        let pool = test_pool().await;
        let storage = PluginStorage::new(pool);
        storage
            .drop_role(&plugin_id("cash.random.rolescope.absent"))
            .await
            .expect("dropping an absent role must not fail");
    }

    #[test]
    fn a_generated_role_password_is_alphanumeric_and_unique() {
        let a = generate_role_password().unwrap();
        let b = generate_role_password().unwrap();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(a.len(), ROLE_PASSWORD_BYTES * 2);
    }

    #[tokio::test]
    async fn a_non_alphanumeric_role_password_is_refused_rather_than_escaped() {
        // The password is refused before any query is sent, so this needs no
        // database: a lazy pool never connects, and if the check were removed
        // the call would fail on the connection instead of on the password.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        let storage = PluginStorage::new(pool);
        let result = storage
            .provision_role(
                &plugin_id("cash.random.rolescope.inject"),
                "a'; DROP ROLE x--",
            )
            .await;
        assert!(matches!(
            result,
            Err(PluginStorageError::InvalidRolePassword)
        ));
    }
}

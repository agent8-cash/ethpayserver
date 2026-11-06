#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A plugin installed below the minimum version the operator configured must be
//! refused by the real boot path and disabled exactly like any other load
//! failure, so a stale build cannot come back after a restart. Driven through
//! `load_installed_plugins` with an in-memory store and a deliberately stale
//! fixture manifest; no database is needed.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use data_service::{InstalledPlugin, InstalledPluginReader, InstalledPluginWriter, NewPluginEvent};
use payserver_plugin_api::{PluginId, Version};
use payserver_plugin_host::{PluginArtifacts, PluginHost, host_version};
use server::services::plugins::{
    DEFAULT_CALL_DEADLINE, DEFAULT_MAX_FAILURES, DeferredCapabilities, load_installed_plugins,
};
use types::{RepositoryError, RepositoryResult};

const PLUGIN_ID: &str = "cash.random.billing";

/// The smallest module this host will load: the ABI exports and a `call` that
/// hands its argument straight back. Only has to load, not run.
fn echo_module() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 1024))
            (func (export "alloc") (param $len i32) (result i32)
                (local $ptr i32)
                (local.set $ptr (global.get $next))
                (global.set $next (i32.add (global.get $next) (local.get $len)))
                (local.get $ptr))
            (func (export "call") (param $ptr i32) (param $len i32) (result i64)
                (i64.or
                    (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
                    (i64.extend_i32_u (local.get $len))))
        )
        "#,
    )
    .unwrap()
}

/// A manifest this host accepts at `version`, built against the running host
/// version so a crate version bump does not turn this into a version-rejection test.
fn manifest_toml(id: &str, version: &str) -> String {
    let host = host_version();
    format!(
        r#"
        id = "{id}"
        version = "{version}"
        dependencies = ["ethpayserver:^{}.{}"]
        kind = "action"
        "#,
        host.major, host.minor
    )
}

fn row(version: &str, sha: &str) -> InstalledPlugin {
    InstalledPlugin {
        id: PLUGIN_ID.to_string(),
        version: version.to_string(),
        manifest_toml: manifest_toml(PLUGIN_ID, version),
        artifact_sha256: sha.to_string(),
        enabled: true,
        disabled_reason: None,
        db_role_password: None,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// The installed-plugin tables, in memory.
#[derive(Default)]
struct FakeStore {
    rows: Mutex<Vec<InstalledPlugin>>,
    events: Mutex<Vec<NewPluginEvent>>,
}

impl FakeStore {
    fn with_row(row: InstalledPlugin) -> Self {
        Self {
            rows: Mutex::new(vec![row]),
            events: Mutex::new(Vec::new()),
        }
    }

    fn row(&self, id: &str) -> InstalledPlugin {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .expect("row should still exist")
    }

    fn event_kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind.as_str().to_string())
            .collect()
    }
}

#[async_trait]
impl InstalledPluginReader for FakeStore {
    async fn list_installed_plugins(&self) -> RepositoryResult<Vec<InstalledPlugin>> {
        Ok(self.rows.lock().unwrap().clone())
    }

    async fn get_installed_plugin(&self, id: &str) -> RepositoryResult<Option<InstalledPlugin>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned())
    }

    async fn plugin_events(
        &self,
        _id: &str,
        _limit: i64,
    ) -> RepositoryResult<Vec<data_service::PluginEvent>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl InstalledPluginWriter for FakeStore {
    async fn upsert_installed_plugin(
        &self,
        _plugin: &data_service::NewInstalledPlugin,
    ) -> RepositoryResult<()> {
        Err(RepositoryError::InvalidData("not needed here".to_string()))
    }

    async fn set_plugin_enabled(
        &self,
        id: &str,
        enabled: bool,
        reason: Option<&str>,
    ) -> RepositoryResult<bool> {
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows.iter_mut().find(|r| r.id == id) else {
            return Ok(false);
        };
        row.enabled = enabled;
        row.disabled_reason = if enabled {
            None
        } else {
            reason.map(str::to_string)
        };
        Ok(true)
    }

    async fn remove_installed_plugin(&self, _id: &str) -> RepositoryResult<bool> {
        Err(RepositoryError::InvalidData("not needed here".to_string()))
    }

    async fn record_plugin_event(&self, event: &NewPluginEvent) -> RepositoryResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[tokio::test]
async fn load_one_refuses_a_plugin_below_the_configured_minimum_version() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = PluginArtifacts::new(dir.path());
    let id = PluginId::new(PLUGIN_ID).unwrap();
    let sha = artifacts.write(&id, "0.1.0", &echo_module()).unwrap();

    let store = FakeStore::with_row(row("0.1.0", &sha));
    let floors = HashMap::from([(id.clone(), Version::parse("0.2.0").unwrap())]);
    let host = PluginHost::new(host_version(), DEFAULT_MAX_FAILURES, DEFAULT_CALL_DEADLINE)
        .with_minimum_versions(floors);

    let report = load_installed_plugins(
        &store,
        Some(&host),
        &artifacts,
        None,
        &DeferredCapabilities::default(),
    )
    .await
    .unwrap();

    assert!(report.loaded.is_empty(), "a stale plugin must not load");
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].disabled);
    assert!(host.status(&id).is_none());
    assert!(
        report.failed[0].reason.contains("0.1.0") && report.failed[0].reason.contains("0.2.0"),
        "the recorded reason should name both versions, got {:?}",
        report.failed[0].reason
    );

    assert!(!store.row(PLUGIN_ID).enabled, "disabled for the next boot");
    assert_eq!(store.event_kinds(), vec!["load_failed".to_string()]);
}

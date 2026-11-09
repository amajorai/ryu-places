//! Who is calling, and which shared map they belong to.
//!
//! Sharing follows organization membership: everyone Core stamps with the same
//! `x-ryu-caller-org-id` sees one map and one set of trips, and each user in that
//! org is a member with their own marks. A caller with a user but no org gets a
//! personal map; an unstamped HTTP caller on an unmanaged node (a single-user
//! local node or a standalone run) is the node-local map. On a managed node
//! (`RYU_MANAGED_NODE`) an unstamped request fails closed instead. Each tenant is
//! a separate SQLite file, so one org can never read another's rows through a
//! missed filter.
//!
//! The MCP server is a separate lane. Core forwards no caller into a stdio MCP
//! server, so it never guesses one: it serves only the single tenant that turned
//! agent access on (see [`AgentLane`]), acting as the [`AGENT_MEMBER_ID`] member.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use tokio::sync::Mutex;

use crate::models::now_ms;
use crate::seal_keys::PluginKeys;
use crate::store::{PlaceStore, AGENT_MEMBER_ID, LOCAL_MEMBER_ID};

pub const CALLER_USER_ID_HEADER: &str = "x-ryu-caller-user-id";
pub const CALLER_ORG_ID_HEADER: &str = "x-ryu-caller-org-id";
pub const LOCAL_TENANT: &str = "local";
const MANAGED_NODE_ENV: &str = "RYU_MANAGED_NODE";
const MAX_ID_LEN: usize = 128;
const SYSTEM_ACTOR: &str = "system";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub tenant: String,
    pub member: String,
}

impl Caller {
    pub fn local() -> Self {
        Self {
            tenant: LOCAL_TENANT.to_owned(),
            member: LOCAL_MEMBER_ID.to_owned(),
        }
    }

    /// The MCP lane's caller: the tenant that granted agent access, with every
    /// write attributed to the agent member rather than to a person.
    pub fn agent(tenant: String) -> Self {
        Self {
            tenant,
            member: AGENT_MEMBER_ID.to_owned(),
        }
    }
}

/// Whether a tenant is an organization's shared map (as opposed to the
/// node-local map or one user's personal map).
pub fn is_org_tenant(tenant: &str) -> bool {
    tenant.starts_with("org:")
}

/// The request carries no usable verified caller (403 `tenant_required`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this request has no verified Ryu caller")]
pub struct TenantRequired;

fn header(headers: &HeaderMap, name: &str) -> Result<Option<String>, TenantRequired> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| TenantRequired)?.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_ID_LEN || value.chars().any(char::is_control) {
        return Err(TenantRequired);
    }
    Ok(Some(value.to_owned()))
}

/// Match Core's managed-node truthy values, so a request with no verified
/// caller fails closed on a managed node instead of landing on the local map.
pub fn is_managed_node() -> bool {
    std::env::var(MANAGED_NODE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// Core stamps these after verifying the caller; the bearer gate in `main.rs`
/// guarantees the request came through Core, so the stamps can be trusted.
/// Malformed stamps, an org without a user, a user id that collides with a
/// reserved member, and (on a managed node) no stamps at all are rejected.
pub fn caller_from_headers(
    headers: &HeaderMap,
    managed_node: bool,
) -> Result<Caller, TenantRequired> {
    let user = header(headers, CALLER_USER_ID_HEADER)?;
    let org = header(headers, CALLER_ORG_ID_HEADER)?;
    if user
        .as_deref()
        .is_some_and(|user| user == LOCAL_MEMBER_ID || user == AGENT_MEMBER_ID)
    {
        return Err(TenantRequired);
    }
    match (user, org) {
        (Some(user), Some(org)) => Ok(Caller {
            tenant: format!("org:{org}"),
            member: user,
        }),
        (Some(user), None) => Ok(Caller {
            tenant: format!("user:{user}"),
            member: user,
        }),
        (None, Some(_)) => Err(TenantRequired),
        (None, None) if managed_node => Err(TenantRequired),
        (None, None) => Ok(Caller::local()),
    }
}

/// Agent access could not be turned on (409).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentLaneError {
    #[error("Another map on this device already uses the Places Guide agent. It has to turn agent access off first.")]
    HeldElsewhere,
    #[error("This device serves an organization's shared map, so a personal map cannot use the Places Guide agent.")]
    OrgBound,
}

/// The MCP lane has no tenant to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Places Guide is not connected. Turn on agent access in the Places app (open Members and your profile) to let the agent use a map.")]
pub struct AgentNotConnected;

/// One tenant's view of the agent lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccess {
    /// This tenant holds the lane.
    pub enabled: bool,
    /// This tenant could turn it on now (or already has).
    pub available: bool,
}

/// The node-level record of which single tenant the MCP lane serves.
///
/// Agent access starts off and is an explicit opt-in. At most one tenant
/// holds it, and no tenant can take it from another. The first request from
/// an organization's map marks the node as organization-bound: that drops
/// access still held by a personal (`local` or `user:`) map and keeps personal
/// maps from taking the lane afterwards, so an organization's agents are never
/// served the owner's personal map. The record is a small SQLite file next to
/// the maps because the HTTP sidecar and the MCP child are separate processes.
#[derive(Clone)]
struct AgentLane {
    root: Option<PathBuf>,
    conn: Arc<Mutex<Option<Connection>>>,
    /// In-process fast path: this process already recorded the org binding.
    org_noted: Arc<AtomicBool>,
}

impl AgentLane {
    fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            conn: Arc::default(),
            org_noted: Arc::default(),
        }
    }

    fn open(&self) -> Result<Connection> {
        let conn = match &self.root {
            None => Connection::open_in_memory()?,
            Some(root) => {
                std::fs::create_dir_all(root)
                    .with_context(|| format!("creating {}", root.display()))?;
                let path = crate::paths::agent_db_path(root);
                Connection::open(&path)
                    .with_context(|| format!("opening agent lane at {}", path.display()))?
            }
        };
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS agent_lane (
               id INTEGER PRIMARY KEY CHECK (id = 1),
               tenant TEXT,
               org_bound INTEGER NOT NULL DEFAULT 0 CHECK (org_bound IN (0, 1)),
               updated_at INTEGER NOT NULL,
               updated_by TEXT NOT NULL
             );
             INSERT OR IGNORE INTO agent_lane (id, tenant, org_bound, updated_at, updated_by)
               VALUES (1, NULL, 0, 0, 'system');",
        )
        .context("creating agent lane schema")?;
        Ok(conn)
    }

    async fn with<T>(&self, run: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.open()?);
        }
        let conn = guard.as_mut().context("agent lane is not open")?;
        run(conn)
    }

    async fn holder(&self) -> Result<Option<String>> {
        self.with(|conn| {
            Ok(conn
                .query_row("SELECT tenant FROM agent_lane WHERE id = 1", [], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .optional()?
                .flatten())
        })
        .await
    }

    async fn note_org(&self) -> Result<()> {
        if self.org_noted.load(Ordering::Acquire) {
            return Ok(());
        }
        self.with(|conn| {
            let changed = conn.execute(
                "UPDATE agent_lane
                 SET org_bound = 1,
                     tenant = CASE WHEN tenant LIKE 'org:%' THEN tenant ELSE NULL END,
                     updated_at = ?1, updated_by = ?2
                 WHERE id = 1
                   AND (org_bound = 0 OR (tenant IS NOT NULL AND tenant NOT LIKE 'org:%'))",
                params![now_ms(), SYSTEM_ACTOR],
            )?;
            if changed > 0 {
                tracing::info!("places: node is organization-bound; personal agent access dropped");
            }
            Ok(())
        })
        .await?;
        self.org_noted.store(true, Ordering::Release);
        Ok(())
    }

    async fn access(&self, tenant: &str) -> Result<AgentAccess> {
        self.with(|conn| {
            let (holder, org_bound) = conn.query_row(
                "SELECT tenant, org_bound FROM agent_lane WHERE id = 1",
                [],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?)),
            )?;
            Ok(access_for(tenant, holder.as_deref(), org_bound))
        })
        .await
    }

    async fn set(&self, tenant: &str, actor: &str, enabled: bool) -> Result<AgentAccess> {
        let tenant = tenant.to_owned();
        let actor = actor.to_owned();
        self.with(move |conn| {
            // IMMEDIATE takes the write lock up front, so the MCP child and the
            // HTTP sidecar cannot both see the lane free and both claim it.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (holder, org_bound) = tx.query_row(
                "SELECT tenant, org_bound FROM agent_lane WHERE id = 1",
                [],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?)),
            )?;
            let holds = holder.as_deref() == Some(tenant.as_str());
            let next = if enabled {
                if holder.is_some() && !holds {
                    return Err(AgentLaneError::HeldElsewhere.into());
                }
                if org_bound && !is_org_tenant(&tenant) {
                    return Err(AgentLaneError::OrgBound.into());
                }
                Some(tenant.clone())
            } else if holds {
                None
            } else {
                holder.clone()
            };
            if next != holder {
                tx.execute(
                    "UPDATE agent_lane SET tenant = ?1, updated_at = ?2, updated_by = ?3 WHERE id = 1",
                    params![next, now_ms(), actor],
                )?;
            }
            tx.commit()?;
            Ok(access_for(&tenant, next.as_deref(), org_bound))
        })
        .await
    }
}

fn access_for(tenant: &str, holder: Option<&str>, org_bound: bool) -> AgentAccess {
    let enabled = holder == Some(tenant);
    let available = enabled || (holder.is_none() && (is_org_tenant(tenant) || !org_bound));
    AgentAccess { enabled, available }
}

fn file_stem(tenant: &str) -> String {
    tenant
        .bytes()
        .fold(String::with_capacity(tenant.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[derive(Clone)]
pub struct Stores {
    root: Option<PathBuf>,
    open: Arc<Mutex<HashMap<String, PlaceStore>>>,
    lane: AgentLane,
    /// The sealing keys every map opens with (`None`: resolved from the
    /// environment at first use, see [`crate::seal_keys`]).
    keys: Option<PluginKeys>,
}

impl Stores {
    /// `root` is this app's plugin data dir (`paths::data_dir()`): the local map
    /// stays at `places.db`, and each other tenant lives at `tenants/<hex tenant>.db`.
    /// The agent lane record is `agent.db` beside them.
    pub fn on_disk(root: PathBuf) -> Self {
        Self {
            lane: AgentLane::new(Some(root.clone())),
            root: Some(root),
            open: Arc::default(),
            keys: None,
        }
    }

    /// [`Self::on_disk`] with explicit sealing keys (the sidecar's resolved
    /// keys, or the keyless MCP process's).
    pub fn on_disk_with_keys(root: PathBuf, keys: PluginKeys) -> Self {
        Self {
            keys: Some(keys),
            ..Self::on_disk(root)
        }
    }

    pub fn memory() -> Self {
        Self {
            root: None,
            open: Arc::default(),
            lane: AgentLane::new(None),
            keys: None,
        }
    }

    fn keys(&self, root: &std::path::Path) -> Result<PluginKeys> {
        match &self.keys {
            Some(keys) => Ok(keys.clone()),
            None => PluginKeys::resolve(crate::state::PLUGIN_ID, root),
        }
    }

    /// Record that an organization's map is served from this node (see
    /// [`AgentLane`]). Cheap after the first call in a process.
    pub async fn note_tenant(&self, tenant: &str) -> Result<()> {
        if is_org_tenant(tenant) {
            self.lane.note_org().await?;
        }
        Ok(())
    }

    /// The tenant the MCP lane serves, if one turned agent access on.
    pub async fn agent_tenant(&self) -> Result<Option<String>> {
        self.lane.holder().await
    }

    pub async fn agent_access(&self, tenant: &str) -> Result<AgentAccess> {
        self.lane.access(tenant).await
    }

    /// Turn agent access on or off for `tenant`. Turning it on fails with
    /// [`AgentLaneError`] when another tenant holds the lane, or when a personal
    /// map asks on an organization-bound node.
    pub async fn set_agent_access(
        &self,
        tenant: &str,
        actor: &str,
        enabled: bool,
    ) -> Result<AgentAccess> {
        self.lane.set(tenant, actor, enabled).await
    }

    /// Every map opened so far (health).
    pub async fn open_maps(&self) -> Vec<(String, PlaceStore)> {
        let open = self.open.lock().await;
        open.iter()
            .map(|(tenant, store)| (tenant.clone(), store.clone()))
            .collect()
    }

    pub async fn get(&self, tenant: &str) -> Result<PlaceStore> {
        let mut open = self.open.lock().await;
        if let Some(store) = open.get(tenant) {
            return Ok(store.clone());
        }
        // Each map file seals under its own store name, so a value never opens
        // in another tenant's map.
        let store = match &self.root {
            None => PlaceStore::open_memory()?,
            Some(root) if tenant == LOCAL_TENANT => PlaceStore::open_with_keys(
                crate::paths::local_db_path(root),
                crate::store::STORE,
                &self.keys(root)?,
            )?,
            Some(root) => {
                let stem = file_stem(tenant);
                PlaceStore::open_with_keys(
                    crate::paths::tenant_db_path(root, &stem),
                    &crate::store::tenant_store_name(&stem),
                    &self.keys(root)?,
                )?
            }
        };
        open.insert(tenant.to_owned(), store.clone());
        Ok(store)
    }
}

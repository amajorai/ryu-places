use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

use ryu_crypto::content_sqlite::{ColumnPlan, KeyKind};

use crate::seal_keys::{KeySource, PluginKeys, StoreSeal};
use crate::seal_sql::{collect_readable, open_store, opened, opened_opt, shown, SqlSeal};

use crate::models::{
    now_ms, Activity, ListPatch, Mark, MarkInput, MarkStatus, Member, MemberPatch, MemberStats,
    NewList, NewPlace, Place, PlaceFilters, PlaceList, PlacePatch, Summary,
};
use crate::validation::{
    list_emoji, list_name, member_name, normalize_category, normalize_color, normalized_mark,
    normalized_new, parse_status, patch_text, validate_coordinates, validate_patch,
    ValidationError,
};

pub const DEFAULT_LIMIT: i64 = 500;
pub const MAX_LIMIT: i64 = 2_000;
pub const ACTIVITY_LIMIT: i64 = 50;
/// The member id used when Core stamps no caller (a single-user local node or a
/// standalone run).
pub const LOCAL_MEMBER_ID: &str = "local";
/// The member the MCP lane writes as. It is not a person: it never carries
/// marks and never pays for or shares in a trip expense.
pub const AGENT_MEMBER_ID: &str = "agent:places";

/// Distinct, legible pin colors handed to members in join order. A member can
/// change theirs; this only keeps two new members from starting identical.
const MEMBER_COLORS: &[&str] = &[
    "#e4572e", "#2e86ab", "#4caf50", "#9c27b0", "#f2a541", "#00897b", "#d81b60", "#5c6bc0",
];

const PLACE_COLUMNS: &str = "id, ryu_open('places.name', id, name) AS name, ryu_open('places.address', id, address) AS address, lat, lng, category, ryu_open('places.url', id, url) AS url, ryu_open('places.note', id, note) AS note, list_id, external_id, added_by, created_at, updated_at";
/// The raw column list, for writes.
const PLACE_INSERT_COLUMNS: &str = "id, name, address, lat, lng, category, url, note, list_id, external_id, added_by, created_at, updated_at";
/// A member row with its sealed name opened.
pub(crate) const MEMBER_COLUMNS: &str =
    "id, ryu_open('members.name', id, name) AS name, color, joined_at, updated_at";

#[derive(Debug, thiserror::Error)]
pub enum NotFound {
    #[error("place not found")]
    Place,
    #[error("trip not found")]
    Trip,
    #[error("trip item not found")]
    TripItem,
    #[error("list not found")]
    List,
    #[error("no mark to clear")]
    Mark,
}

#[derive(Clone)]
pub struct PlaceStore {
    pub(crate) conn: Arc<Mutex<Connection>>,
    seal: SqlSeal,
}

/// The registered content-store name of the node-local map (part of every key
/// derivation; never rename). Each other tenant's map is its own store,
/// [`tenant_store_name`].
pub const STORE: &str = "places.db";

/// The store name of one tenant's map file (`tenants/<stem>.db`): each map
/// seals under its own key.
pub fn tenant_store_name(stem: &str) -> String {
    format!("places/tenants/{stem}.db")
}

/// The sealed columns keyed by a TEXT id (`encb:v1:`, bound to
/// `<table>.<column>` and the row id). The `activity` copies (keyed by their
/// INTEGER id) are added in [`column_plans`]; `marks.note` (keyed by place and
/// member) is sealed by [`migrate_marks`].
const SEALED_TEXT_COLUMNS: &[(&str, &str)] = &[
    ("members", "name"),
    ("lists", "name"),
    ("places", "name"),
    ("places", "address"),
    ("places", "url"),
    ("places", "note"),
    ("trips", "name"),
    ("trips", "destination"),
    ("trips", "note"),
    ("trip_stops", "name"),
    ("trip_stops", "note"),
    ("trip_reservations", "title"),
    ("trip_reservations", "location"),
    ("trip_reservations", "confirmation"),
    ("trip_reservations", "note"),
    ("trip_expenses", "title"),
    ("trip_checklist", "text"),
];

/// Every column plan of the open-time migration.
pub fn column_plans() -> Result<Vec<ColumnPlan>> {
    let mut plans = SEALED_TEXT_COLUMNS
        .iter()
        .map(|(table, column)| ColumnPlan::text(table, column, "id", KeyKind::Text))
        .collect::<Result<Vec<_>>>()?;
    for column in ["place_name", "detail"] {
        plans.push(ColumnPlan::text(
            "activity",
            column,
            "id",
            KeyKind::Integer,
        )?);
    }
    Ok(plans)
}

/// The AAD id of a mark's note: its place and member (ids never hold a newline).
pub fn mark_note_id(place_id: &str, member_id: &str) -> String {
    format!("{place_id}\n{member_id}")
}

/// Seal legacy `marks.note` values (keyed by `(place_id, member_id)`, so not
/// a cursor-paginated column plan). Idempotent: only values that need a
/// reseal are rewritten, each with a compare-and-swap. Returns how many.
pub fn migrate_marks(conn: &mut Connection, cipher: &ryu_crypto::ContentCipher) -> Result<u64> {
    ryu_crypto::content_sqlite::require_secure_delete(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows: Vec<(String, String, String)> = {
        let mut statement =
            tx.prepare("SELECT place_id, member_id, note FROM marks WHERE note IS NOT NULL")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut sealed = 0;
    for (place_id, member_id, note) in rows {
        let context =
            ryu_crypto::ContentContext::for_id("marks.note", mark_note_id(&place_id, &member_id));
        let Ok(opened) = cipher.open_text(&note, &context) else {
            continue; // unreadable: left untouched (reads skip and count it)
        };
        if !opened.needs_reseal {
            continue;
        }
        let value = cipher.seal_text(&opened.value, &context)?;
        sealed += tx.execute(
            "UPDATE marks SET note = ?1 WHERE place_id = ?2 AND member_id = ?3 AND note IS ?4",
            params![value, place_id, member_id, note],
        )? as u64;
    }
    tx.commit()?;
    Ok(sealed)
}

impl PlaceStore {
    /// Open the node-local map, resolving this process's sealing key with the
    /// store's directory as the plugin data dir (see [`crate::seal_keys`]).
    pub fn open(path: PathBuf) -> Result<Self> {
        let dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let keys = PluginKeys::resolve(crate::state::PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, STORE, &keys)
    }

    /// Open one map file sealed as `store` under `keys`.
    pub fn open_with_keys(path: PathBuf, store: &str, keys: &PluginKeys) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", path.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening places db at {}", path.display()))?;
        Self::from_connection(conn, store, keys, Some(&path))
    }

    /// An in-memory map (tests) sealing under a random key, so every test runs
    /// through the sealed read and write paths.
    pub fn open_memory() -> Result<Self> {
        Self::open_memory_with_keys(&ephemeral_keys()?)
    }

    /// An in-memory map under `keys` (tests).
    pub fn open_memory_with_keys(keys: &PluginKeys) -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, STORE, keys, None)
    }

    /// The sealing health item: `{state, detail?, unreadable_rows}`.
    pub fn seal_health(&self) -> serde_json::Value {
        self.seal.health()
    }

    /// A keyless process sharing the map with the keyed sidecar: stop writing
    /// plaintext once the sidecar has sealed it.
    pub async fn refresh_seal(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        self.seal.refresh_compat(&conn)
    }

    fn from_connection(
        mut conn: Connection,
        store: &str,
        keys: &PluginKeys,
        path: Option<&std::path::Path>,
    ) -> Result<Self> {
        // Before any statement that could rewrite a cell.
        ryu_crypto::content_sqlite::enable_secure_delete(&conn)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS members (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               color TEXT NOT NULL,
               joined_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS lists (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               emoji TEXT,
               created_by TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS places (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               address TEXT,
               lat REAL NOT NULL,
               lng REAL NOT NULL,
               category TEXT NOT NULL,
               url TEXT,
               note TEXT,
               list_id TEXT REFERENCES lists(id) ON DELETE SET NULL,
               external_id TEXT,
               added_by TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_places_external
               ON places(external_id) WHERE external_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS idx_places_updated ON places(updated_at DESC);
             CREATE TABLE IF NOT EXISTS marks (
               place_id TEXT NOT NULL REFERENCES places(id) ON DELETE CASCADE,
               member_id TEXT NOT NULL,
               status TEXT NOT NULL CHECK (status IN ('want', 'been')),
               visited_on TEXT,
               rating INTEGER,
               note TEXT,
               updated_at INTEGER NOT NULL,
               PRIMARY KEY (place_id, member_id)
             );
             CREATE INDEX IF NOT EXISTS idx_marks_member ON marks(member_id, status);
             CREATE TABLE IF NOT EXISTS activity (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               member_id TEXT NOT NULL,
               kind TEXT NOT NULL,
               place_id TEXT,
               place_name TEXT,
               detail TEXT,
               at INTEGER NOT NULL
             );",
        )
        .context("creating places schema")?;
        crate::trips::migrate(&conn)?;
        let seal = open_store(&mut conn, keys, store, &column_plans()?, path)
            .with_context(|| format!("sealing {store}"))?;
        if let StoreSeal::Sealing(cipher) = seal.current() {
            let sealed = migrate_marks(&mut conn, &cipher)?;
            if sealed > 0 {
                tracing::info!(count = sealed, "{store}: sealed legacy mark notes");
                if let Some(path) = path {
                    if let Err(error) = ryu_crypto::content_sqlite::finish_store(&conn, path, store)
                    {
                        tracing::warn!("{store}: scrubbing freed pages failed: {error:#}");
                    }
                }
            }
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            seal,
        })
    }

    // ── Members ────────────────────────────────────────────────────────────

    /// Return the caller's member row, creating it on first contact. Joining is
    /// implicit: anyone Core lets through with `places.view` into this tenant is a member.
    pub async fn ensure_member(&self, id: &str) -> Result<Member> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, id)
    }

    pub async fn members(&self) -> Result<Vec<Member>> {
        let conn = self.conn.lock().await;
        let mut statement = conn.prepare(&format!(
            "SELECT {MEMBER_COLUMNS} FROM members ORDER BY joined_at, id"
        ))?;
        let rows = statement.query_map([], row_to_member)?;
        collect_readable(rows).map_err(Into::into)
    }

    pub async fn update_member(&self, id: &str, patch: MemberPatch) -> Result<Member> {
        if patch.name.is_none() && patch.color.is_none() {
            return Err(ValidationError::EmptyPatch.into());
        }
        let name = patch.name.as_deref().map(member_name).transpose()?;
        let color = patch.color.as_deref().map(normalize_color).transpose()?;
        let conn = self.conn.lock().await;
        let current = ensure_member_locked(&conn, id)?;
        let next_name = name.unwrap_or(current.name.clone());
        let next_color = color.unwrap_or(current.color);
        conn.execute(
            "UPDATE members SET name = ryu_seal('members.name', ?4, ?1), color = ?2, updated_at = ?3 WHERE id = ?4",
            params![next_name, next_color, now_ms(), id],
        )?;
        if next_name != current.name {
            log_locked(&conn, id, "renamed", None, None, Some(&next_name))?;
        }
        ensure_member_locked(&conn, id)
    }

    // ── Lists ──────────────────────────────────────────────────────────────

    pub async fn lists(&self) -> Result<Vec<PlaceList>> {
        let conn = self.conn.lock().await;
        let mut statement = conn.prepare(
            "SELECT l.id, ryu_open('lists.name', l.id, l.name) AS name, l.emoji, l.created_by, l.created_at, l.updated_at,
                    (SELECT COUNT(*) FROM places p WHERE p.list_id = l.id)
             FROM lists l ORDER BY l.created_at, l.id",
        )?;
        let rows = statement.query_map([], row_to_list)?;
        collect_readable(rows).map_err(Into::into)
    }

    pub async fn create_list(&self, actor: &str, input: NewList) -> Result<PlaceList> {
        let name = list_name(&input.name)?;
        let emoji = list_emoji(input.emoji)?;
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        conn.execute(
            "INSERT INTO lists (id, name, emoji, created_by, created_at, updated_at)
             VALUES (?1, ryu_seal('lists.name', ?1, ?2), ?3, ?4, ?5, ?6)",
            params![id, name, emoji, actor, now, now],
        )?;
        log_locked(&conn, actor, "list_created", None, None, Some(&name))?;
        get_list_locked(&conn, &id)?.context("inserted list disappeared")
    }

    pub async fn update_list(&self, actor: &str, id: &str, patch: ListPatch) -> Result<PlaceList> {
        if patch.name.is_none() && patch.emoji.is_none() {
            return Err(ValidationError::EmptyPatch.into());
        }
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let current = get_list_locked(&conn, id)?.ok_or(NotFound::List)?;
        let name = match patch.name.as_deref() {
            Some(name) => list_name(name)?,
            None => current.name,
        };
        let emoji = match patch.emoji {
            Some(emoji) => list_emoji(emoji)?,
            None => current.emoji,
        };
        conn.execute(
            "UPDATE lists SET name = ryu_seal('lists.name', ?4, ?1), emoji = ?2, updated_at = ?3 WHERE id = ?4",
            params![name, emoji, now_ms(), id],
        )?;
        get_list_locked(&conn, id)?.context("updated list disappeared")
    }

    /// Delete a list. Its places stay on the map, unfiled — a list is a lens, and
    /// removing one must never silently erase where someone has been.
    pub async fn delete_list(&self, actor: &str, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let list = get_list_locked(&conn, id)?.ok_or(NotFound::List)?;
        conn.execute(
            "UPDATE places SET list_id = NULL WHERE list_id = ?",
            params![id],
        )?;
        conn.execute("DELETE FROM lists WHERE id = ?", params![id])?;
        log_locked(&conn, actor, "list_deleted", None, None, Some(&list.name))?;
        Ok(())
    }

    // ── Places ─────────────────────────────────────────────────────────────

    pub async fn list_places(&self, actor: &str, filters: PlaceFilters) -> Result<Vec<Place>> {
        let limit = filters.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        // Sealed columns are opened by `ryu_open` (aliased to their names);
        // search below matches the opened text, after decrypt.
        let mut sql = format!("SELECT {PLACE_COLUMNS} FROM places p WHERE 1 = 1");
        let mut values: Vec<Value> = Vec::new();

        let status = match filters.status.as_deref().map(str::trim) {
            None | Some("" | "any" | "all") => None,
            Some(value) => Some(parse_status(value)?),
        };
        let member = match filters.member.as_deref().map(str::trim) {
            None | Some("" | "all" | "everyone") => None,
            Some("me") => Some(actor.to_owned()),
            Some(value) => Some(value.to_owned()),
        };
        match (status, member) {
            (Some(status), Some(member)) => {
                sql.push_str(" AND EXISTS (SELECT 1 FROM marks m WHERE m.place_id = p.id AND m.status = ? AND m.member_id = ?)");
                values.push(Value::Text(status.as_str().to_owned()));
                values.push(Value::Text(member));
            }
            (Some(status), None) => {
                sql.push_str(
                    " AND EXISTS (SELECT 1 FROM marks m WHERE m.place_id = p.id AND m.status = ?)",
                );
                values.push(Value::Text(status.as_str().to_owned()));
            }
            (None, Some(member)) => {
                sql.push_str(" AND (p.added_by = ? OR EXISTS (SELECT 1 FROM marks m WHERE m.place_id = p.id AND m.member_id = ?))");
                values.push(Value::Text(member.clone()));
                values.push(Value::Text(member));
            }
            (None, None) => {}
        }
        if let Some(list) = filters
            .list
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if list == "none" {
                sql.push_str(" AND p.list_id IS NULL");
            } else {
                sql.push_str(" AND p.list_id = ?");
                values.push(Value::Text(list.to_owned()));
            }
        }
        if let Some(category) = filters
            .category
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            sql.push_str(" AND p.category = ?");
            values.push(Value::Text(normalize_category(category)?));
        }
        if let Some(query) = filters
            .q
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sql.push_str(" AND (ryu_open('places.name', p.id, p.name) LIKE ? ESCAPE '\\' OR COALESCE(ryu_open('places.address', p.id, p.address), '') LIKE ? ESCAPE '\\' OR COALESCE(ryu_open('places.note', p.id, p.note), '') LIKE ? ESCAPE '\\')");
            let pattern = format!("%{}%", escape_like(query));
            values.push(Value::Text(pattern.clone()));
            values.push(Value::Text(pattern.clone()));
            values.push(Value::Text(pattern));
        }
        sql.push_str(" ORDER BY p.updated_at DESC, p.id LIMIT ?");
        values.push(Value::Integer(limit));

        let conn = self.conn.lock().await;
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), row_to_place)?;
        let mut places = collect_readable(rows)?;
        attach_marks_locked(&conn, &mut places)?;
        Ok(places)
    }

    pub async fn get_place(&self, id: &str) -> Result<Place> {
        let conn = self.conn.lock().await;
        get_place_locked(&conn, id)?.ok_or_else(|| NotFound::Place.into())
    }

    /// Save a place. A geocoder hit another member already saved is not
    /// duplicated: the existing pin is returned (with the caller's mark applied),
    /// so two people saving the same restaurant converge on one shared place.
    pub async fn add_place(&self, actor: &str, input: NewPlace) -> Result<Place> {
        let input = normalized_new(input)?;
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        if let Some(list_id) = input.list_id.as_deref() {
            get_list_locked(&conn, list_id)?.ok_or(NotFound::List)?;
        }
        let existing = match input.external_id.as_deref() {
            Some(external) => conn
                .query_row(
                    "SELECT id FROM places WHERE external_id = ?",
                    params![external],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
            None => None,
        };
        let now = now_ms();
        let id = if let Some(id) = existing {
            id
        } else {
            let id = Uuid::new_v4().to_string();
            conn.execute(
                &format!("INSERT INTO places ({PLACE_INSERT_COLUMNS}) VALUES (?1, ryu_seal('places.name', ?1, ?2), ryu_seal('places.address', ?1, ?3), ?4, ?5, ?6, ryu_seal('places.url', ?1, ?7), ryu_seal('places.note', ?1, ?8), ?9, ?10, ?11, ?12, ?13)"),
                params![
                    id,
                    input.name,
                    input.address,
                    input.lat,
                    input.lng,
                    input.category,
                    input.url,
                    input.note,
                    input.list_id,
                    input.external_id,
                    actor,
                    now,
                    now,
                ],
            )?;
            log_locked(&conn, actor, "saved", Some(&id), Some(&input.name), None)?;
            id
        };
        if let Some(status) = input.status {
            let mark = MarkInput {
                status,
                visited_on: input.visited_on,
                rating: input.rating,
                note: None,
            };
            write_mark_locked(&conn, actor, &id, &mark)?;
        }
        get_place_locked(&conn, &id)?.context("saved place disappeared")
    }

    pub async fn update_place(&self, actor: &str, id: &str, patch: PlacePatch) -> Result<Place> {
        validate_patch(&patch)?;
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let current = get_place_locked(&conn, id)?.ok_or(NotFound::Place)?;
        let lat = patch.lat.unwrap_or(current.lat);
        let lng = patch.lng.unwrap_or(current.lng);
        validate_coordinates(lat, lng)?;
        let name = patch
            .name
            .map_or(current.name, |name| name.trim().to_owned());
        let category = match patch.category.as_deref() {
            Some(category) => normalize_category(category)?,
            None => current.category,
        };
        let list_id = match patch.list_id {
            Some(Some(list_id)) if !list_id.trim().is_empty() => {
                let list_id = list_id.trim().to_owned();
                get_list_locked(&conn, &list_id)?.ok_or(NotFound::List)?;
                Some(list_id)
            }
            Some(_) => None,
            None => current.list_id,
        };
        conn.execute(
            "UPDATE places SET name = ryu_seal('places.name', ?10, ?1), address = ryu_seal('places.address', ?10, ?2), lat = ?3, lng = ?4, category = ?5, url = ryu_seal('places.url', ?10, ?6),
             note = ryu_seal('places.note', ?10, ?7), list_id = ?8, updated_at = ?9 WHERE id = ?10",
            params![
                name,
                patch_text("address", patch.address, current.address)?,
                lat,
                lng,
                category,
                patch_text("url", patch.url, current.url)?,
                patch_text("note", patch.note, current.note)?,
                list_id,
                now_ms(),
                id,
            ],
        )?;
        log_locked(&conn, actor, "edited", Some(id), Some(&name), None)?;
        get_place_locked(&conn, id)?.context("updated place disappeared")
    }

    pub async fn delete_place(&self, actor: &str, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let place = get_place_locked(&conn, id)?.ok_or(NotFound::Place)?;
        conn.execute("DELETE FROM marks WHERE place_id = ?", params![id])?;
        // Trip stops keep their copied name and coordinates; only the link goes.
        conn.execute(
            "UPDATE trip_stops SET place_id = NULL WHERE place_id = ?",
            params![id],
        )?;
        conn.execute("DELETE FROM places WHERE id = ?", params![id])?;
        log_locked(&conn, actor, "deleted", None, Some(&place.name), None)?;
        Ok(())
    }

    // ── Marks ──────────────────────────────────────────────────────────────

    pub async fn set_mark(&self, actor: &str, place_id: &str, input: MarkInput) -> Result<Place> {
        let input = normalized_mark(input)?;
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        get_place_locked(&conn, place_id)?.ok_or(NotFound::Place)?;
        write_mark_locked(&conn, actor, place_id, &input)?;
        get_place_locked(&conn, place_id)?.context("marked place disappeared")
    }

    pub async fn clear_mark(&self, actor: &str, place_id: &str) -> Result<Place> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let place = get_place_locked(&conn, place_id)?.ok_or(NotFound::Place)?;
        let changed = conn.execute(
            "DELETE FROM marks WHERE place_id = ? AND member_id = ?",
            params![place_id, actor],
        )?;
        if changed == 0 {
            return Err(NotFound::Mark.into());
        }
        log_locked(
            &conn,
            actor,
            "unmarked",
            Some(place_id),
            Some(&place.name),
            None,
        )?;
        get_place_locked(&conn, place_id)?.context("unmarked place disappeared")
    }

    // ── Activity and summary ───────────────────────────────────────────────

    pub async fn activity(&self, limit: Option<i64>) -> Result<Vec<Activity>> {
        let limit = limit.unwrap_or(ACTIVITY_LIMIT).clamp(1, 200);
        let conn = self.conn.lock().await;
        let mut statement = conn.prepare(
            "SELECT a.id, a.member_id, COALESCE(ryu_open('members.name', m.id, m.name), a.member_id), a.kind, a.place_id,
                    ryu_open('activity.place_name', a.id, a.place_name), ryu_open('activity.detail', a.id, a.detail), a.at
             FROM activity a LEFT JOIN members m ON m.id = a.member_id
             ORDER BY a.id DESC LIMIT ?",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(Activity {
                id: row.get(0)?,
                member_id: row.get(1)?,
                member_name: shown(row.get(2)?),
                kind: row.get(3)?,
                place_id: row.get(4)?,
                place_name: row.get::<_, Option<String>>(5)?.map(shown),
                detail: row.get::<_, Option<String>>(6)?.map(shown),
                at: row.get(7)?,
            })
        })?;
        collect_readable(rows).map_err(Into::into)
    }

    pub async fn summary(&self) -> Result<Summary> {
        let conn = self.conn.lock().await;
        let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0));
        let places = count("SELECT COUNT(*) FROM places")?;
        let want = count("SELECT COUNT(DISTINCT place_id) FROM marks WHERE status = 'want'")?;
        let been = count("SELECT COUNT(DISTINCT place_id) FROM marks WHERE status = 'been'")?;
        let lists = count("SELECT COUNT(*) FROM lists")?;
        let mut statement = conn.prepare(
            "SELECT m.id, ryu_open('members.name', m.id, m.name), m.color,
                    (SELECT COUNT(*) FROM marks k WHERE k.member_id = m.id AND k.status = 'want'),
                    (SELECT COUNT(*) FROM marks k WHERE k.member_id = m.id AND k.status = 'been')
             FROM members m ORDER BY m.joined_at, m.id",
        )?;
        let members = statement
            .query_map([], |row| {
                Ok(MemberStats {
                    member_id: row.get(0)?,
                    member_name: shown(row.get(1)?),
                    member_color: row.get(2)?,
                    want: row.get(3)?,
                    been: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Summary {
            places,
            want,
            been,
            lists,
            members,
        })
    }

    pub async fn counts(&self) -> Result<i64> {
        let conn = self.conn.lock().await;
        conn.query_row("SELECT COUNT(*) FROM places", [], |row| row.get(0))
            .map_err(Into::into)
    }
}

/// Whether a place exists, for callers outside this module that link to one.
pub(crate) fn get_place_for_link(conn: &Connection, id: &str) -> Result<Option<Place>> {
    get_place_locked(conn, id)
}

pub(crate) fn ensure_member_locked(conn: &Connection, id: &str) -> Result<Member> {
    if let Some(member) = conn
        .query_row(
            &format!("SELECT {MEMBER_COLUMNS} FROM members WHERE id = ?"),
            params![id],
            row_to_member,
        )
        .optional()?
    {
        return Ok(member);
    }
    let existing: i64 = conn.query_row("SELECT COUNT(*) FROM members", [], |row| row.get(0))?;
    let index = usize::try_from(existing).unwrap_or(0);
    let color = MEMBER_COLORS[index % MEMBER_COLORS.len()];
    let name = if id == LOCAL_MEMBER_ID {
        "Owner".to_owned()
    } else if id == AGENT_MEMBER_ID {
        "Places Guide".to_owned()
    } else {
        format!("Member {}", index + 1)
    };
    let now = now_ms();
    conn.execute(
        "INSERT INTO members (id, name, color, joined_at, updated_at) VALUES (?1, ryu_seal('members.name', ?1, ?2), ?3, ?4, ?5)",
        params![id, name, color, now, now],
    )?;
    log_locked(conn, id, "joined", None, None, None)?;
    Ok(Member {
        id: id.to_owned(),
        name,
        color: color.to_owned(),
        joined_at: now,
        updated_at: now,
    })
}

fn write_mark_locked(
    conn: &Connection,
    actor: &str,
    place_id: &str,
    mark: &MarkInput,
) -> Result<()> {
    let previous = conn
        .query_row(
            "SELECT status FROM marks WHERE place_id = ? AND member_id = ?",
            params![place_id, actor],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let now = now_ms();
    conn.execute(
        "INSERT INTO marks (place_id, member_id, status, visited_on, rating, note, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ryu_seal('marks.note', ?1 || char(10) || ?2, ?6), ?7)
         ON CONFLICT(place_id, member_id) DO UPDATE SET
           status = excluded.status, visited_on = excluded.visited_on,
           rating = excluded.rating, note = excluded.note, updated_at = excluded.updated_at",
        params![
            place_id,
            actor,
            mark.status.as_str(),
            mark.visited_on,
            mark.rating,
            mark.note,
            now,
        ],
    )?;
    conn.execute(
        "UPDATE places SET updated_at = ? WHERE id = ?",
        params![now, place_id],
    )?;
    // Re-rating a place you have already been to is an edit, not news for the feed.
    if previous.as_deref() != Some(mark.status.as_str()) {
        let name: String = conn.query_row(
            "SELECT ryu_open('places.name', id, name) FROM places WHERE id = ?",
            params![place_id],
            |row| row.get(0),
        )?;
        let detail = mark.rating.map(|rating| format!("{rating}/5"));
        log_locked(
            conn,
            actor,
            mark.status.as_str(),
            Some(place_id),
            Some(&name),
            detail.as_deref(),
        )?;
    }
    Ok(())
}

pub(crate) fn log_locked(
    conn: &Connection,
    member_id: &str,
    kind: &str,
    place_id: Option<&str>,
    place_name: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    // The row id is not known before the insert: insert without the copies of
    // user text, then seal them bound to the new id (never stored in plaintext).
    conn.execute(
        "INSERT INTO activity (member_id, kind, place_id, place_name, detail, at)
         VALUES (?, ?, ?, NULL, NULL, ?)",
        params![member_id, kind, place_id, now_ms()],
    )?;
    if place_name.is_some() || detail.is_some() {
        let id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE activity SET place_name = ryu_seal('activity.place_name', ?1, ?2),
             detail = ryu_seal('activity.detail', ?1, ?3) WHERE id = ?1",
            params![id, place_name, detail],
        )?;
    }
    Ok(())
}

fn get_place_locked(conn: &Connection, id: &str) -> Result<Option<Place>> {
    let place = conn
        .query_row(
            &format!("SELECT {PLACE_COLUMNS} FROM places WHERE id = ?"),
            params![id],
            row_to_place,
        )
        .optional()?;
    let Some(place) = place else {
        return Ok(None);
    };
    let mut places = vec![place];
    attach_marks_locked(conn, &mut places)?;
    Ok(places.pop())
}

fn get_list_locked(conn: &Connection, id: &str) -> Result<Option<PlaceList>> {
    conn.query_row(
        "SELECT l.id, ryu_open('lists.name', l.id, l.name) AS name, l.emoji, l.created_by, l.created_at, l.updated_at,
                (SELECT COUNT(*) FROM places p WHERE p.list_id = l.id)
         FROM lists l WHERE l.id = ?",
        params![id],
        row_to_list,
    )
    .optional()
    .map_err(Into::into)
}

fn attach_marks_locked(conn: &Connection, places: &mut [Place]) -> Result<()> {
    if places.is_empty() {
        return Ok(());
    }
    let mut by_place: HashMap<String, Vec<Mark>> = HashMap::new();
    // SQLite caps bound parameters; chunk so a large map still loads.
    for chunk in places.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT k.place_id, k.member_id, COALESCE(ryu_open('members.name', m.id, m.name), k.member_id), COALESCE(m.color, '#888888'),
                    k.status, k.visited_on, k.rating, ryu_open('marks.note', k.place_id || char(10) || k.member_id, k.note), k.updated_at
             FROM marks k LEFT JOIN members m ON m.id = k.member_id
             WHERE k.place_id IN ({placeholders})
             ORDER BY m.joined_at, k.member_id"
        );
        let mut statement = conn.prepare(&sql)?;
        let ids = chunk.iter().map(|place| Value::Text(place.id.clone()));
        let rows = statement.query_map(params_from_iter(ids), |row| {
            let status: String = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?,
                Mark {
                    member_id: row.get(1)?,
                    member_name: shown(row.get(2)?),
                    member_color: row.get(3)?,
                    status: MarkStatus::parse(&status).unwrap_or(MarkStatus::Want),
                    visited_on: row.get(5)?,
                    rating: row.get(6)?,
                    note: row.get::<_, Option<String>>(7)?.map(shown),
                    updated_at: row.get(8)?,
                },
            ))
        })?;
        for row in rows {
            let (place_id, mark) = row?;
            by_place.entry(place_id).or_default().push(mark);
        }
    }
    for place in places.iter_mut() {
        place.marks = by_place.remove(&place.id).unwrap_or_default();
    }
    Ok(())
}

/// Keys over a random root (in-memory test maps).
pub(crate) fn ephemeral_keys() -> Result<PluginKeys> {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut root = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut root);
    Ok(PluginKeys::sealing(
        ryu_crypto::PluginCipher::from_subkey_b64(
            crate::state::PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode(root),
        )?,
        KeySource::Delivered,
    ))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn row_to_member(row: &Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        id: row.get(0)?,
        name: opened(row, 1)?,
        color: row.get(2)?,
        joined_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn row_to_list(row: &Row<'_>) -> rusqlite::Result<PlaceList> {
    Ok(PlaceList {
        id: row.get(0)?,
        name: opened(row, 1)?,
        emoji: row.get(2)?,
        created_by: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        place_count: row.get(6)?,
    })
}

fn row_to_place(row: &Row<'_>) -> rusqlite::Result<Place> {
    Ok(Place {
        id: row.get(0)?,
        name: opened(row, 1)?,
        address: opened_opt(row, 2)?,
        lat: row.get(3)?,
        lng: row.get(4)?,
        category: row.get(5)?,
        url: opened_opt(row, 6)?,
        note: opened_opt(row, 7)?,
        list_id: row.get(8)?,
        external_id: row.get(9)?,
        added_by: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        marks: Vec::new(),
    })
}

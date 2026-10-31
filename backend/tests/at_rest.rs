//! Places data at rest: free text sealed per value in `places.db` and every
//! `tenants/<stem>.db` (each map under its own store key), legacy plaintext
//! sealed once on open, unreadable rows skipped and counted, other keys
//! refused, coordinates left readable.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ryu_crypto::{ContentCipher, PluginCipher};
use ryu_places::{
    models::{MarkInput, MarkStatus, NewPlace, PlaceFilters},
    seal_keys::{KeySource, PluginKeys},
    state::PLUGIN_ID,
    store::{column_plans, STORE},
    tenancy::Stores,
    trips::{NewChecklistItem, NewReservation, NewTrip},
};
use serde_json::json;

const NAME: &str = "Zebra Noodle Bar";
const NOTE: &str = "quokka-place-note";
const MARK_NOTE: &str = "wombat-mark-note";
const CONFIRMATION: &str = "NUMBAT-CONF-77";
const CHECK: &str = "pack the dingo charger";

fn keys(n: u8) -> PluginKeys {
    PluginKeys::sealing(
        PluginCipher::from_subkey_b64(
            PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode([n; 32]),
        )
        .unwrap(),
        KeySource::Delivered,
    )
}

fn root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ryu-places-at-rest-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn all_db_bytes(root: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut files = vec![root.join("places.db"), root.join("places.db-wal")];
    if let Ok(entries) = std::fs::read_dir(root.join("tenants")) {
        files.extend(entries.flatten().map(|entry| entry.path()));
    }
    for file in files {
        bytes.extend(std::fs::read(file).unwrap_or_default());
    }
    bytes
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn new_place(name: &str) -> NewPlace {
    NewPlace {
        name: name.into(),
        address: Some("12 Kangaroo Lane".into()),
        lat: 35.0116,
        lng: 135.7681,
        category: "food".into(),
        url: None,
        note: Some(NOTE.into()),
        list_id: None,
        external_id: None,
        status: Some(MarkStatus::Want),
        visited_on: None,
        rating: None,
    }
}

/// A place with a personal mark note and a trip with a booking and a checklist.
async fn seed(stores: &Stores, tenant: &str) {
    let store = stores.get(tenant).await.unwrap();
    let place = store.add_place("ana", new_place(NAME)).await.unwrap();
    store
        .set_mark(
            "ana",
            &place.id,
            MarkInput {
                status: MarkStatus::Been,
                visited_on: None,
                rating: Some(5),
                note: Some(MARK_NOTE.into()),
            },
        )
        .await
        .unwrap();
    let trip = store
        .create_trip(
            "ana",
            NewTrip {
                name: "Kyoto spring".into(),
                destination: Some("Kyoto".into()),
                start_date: Some("2027-04-01".into()),
                end_date: Some("2027-04-02".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .add_reservation(
            "ana",
            &trip.trip.id,
            NewReservation {
                title: "Ryokan stay".into(),
                confirmation: Some(CONFIRMATION.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .add_checklist_item(
            "ana",
            &trip.trip.id,
            NewChecklistItem {
                text: CHECK.into(),
                assignee: None,
            },
        )
        .await
        .unwrap();
}

async fn read_back(stores: &Stores, tenant: &str) {
    let store = stores.get(tenant).await.unwrap();
    let places = store
        .list_places("ana", PlaceFilters::default())
        .await
        .unwrap();
    assert_eq!(places[0].name, NAME);
    assert_eq!(places[0].note.as_deref(), Some(NOTE));
    assert_eq!(places[0].marks[0].note.as_deref(), Some(MARK_NOTE));
    // Search matches the opened text.
    let hits = store
        .list_places(
            "ana",
            serde_json::from_value(json!({ "q": "kangaroo" })).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let trips = store.trips().await.unwrap();
    let detail = store.trip(&trips[0].trip.id).await.unwrap();
    assert_eq!(
        detail.reservations[0].confirmation.as_deref(),
        Some(CONFIRMATION)
    );
    assert_eq!(detail.checklist[0].text, CHECK);
    let activity = store.activity(None).await.unwrap();
    assert!(activity
        .iter()
        .any(|entry| entry.place_name.as_deref() == Some(NAME)));
}

fn assert_no_plaintext(root: &Path) {
    let bytes = all_db_bytes(root);
    for needle in [
        NAME,
        NOTE,
        MARK_NOTE,
        CONFIRMATION,
        CHECK,
        "Kangaroo",
        "Ryokan",
    ] {
        assert!(!contains(&bytes, needle), "{needle} on disk");
    }
}

#[tokio::test]
async fn every_map_is_sealed_on_disk_and_reads_back() {
    let root = root();
    let stores = Stores::on_disk_with_keys(root.clone(), keys(1));
    seed(&stores, "local").await;
    seed(&stores, "org:acme").await;
    read_back(&stores, "local").await;
    read_back(&stores, "org:acme").await;
    drop(stores);
    assert_no_plaintext(&root);
    let conn = rusqlite::Connection::open(root.join("places.db")).unwrap();
    let cipher = keys(1).content_cipher(STORE).unwrap();
    ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap()).unwrap();
    let note: String = conn
        .query_row("SELECT note FROM marks", [], |row| row.get(0))
        .unwrap();
    assert!(ContentCipher::is_sealed_text(&note));
    // Coordinates stay readable (documented).
    let lat: f64 = conn
        .query_row("SELECT lat FROM places", [], |row| row.get(0))
        .unwrap();
    assert!((lat - 35.0116).abs() < 1e-9);
}

#[tokio::test]
async fn a_value_copied_into_another_tenants_map_does_not_open() {
    let root = root();
    let stores = Stores::on_disk_with_keys(root.clone(), keys(2));
    seed(&stores, "local").await;
    seed(&stores, "org:acme").await;
    drop(stores);
    let local = rusqlite::Connection::open(root.join("places.db")).unwrap();
    let (id, name, sealed): (String, String, String) = local
        .query_row("SELECT id, name, note FROM places", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    let tenant_db = std::fs::read_dir(root.join("tenants"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "db"))
        .unwrap();
    let tenant = rusqlite::Connection::open(&tenant_db).unwrap();
    // Same row id, columns and ciphertext, other map: the store key differs.
    tenant
        .execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM marks;")
        .unwrap();
    tenant
        .execute(
            "UPDATE places SET id = ?1, name = ?2, address = NULL, note = ?3",
            rusqlite::params![id, name, sealed],
        )
        .unwrap();
    drop(tenant);
    let stores = Stores::on_disk_with_keys(root.clone(), keys(2));
    let acme = stores.get("org:acme").await.unwrap();
    assert!(acme.get_place(&id).await.is_err());
    assert!(acme
        .list_places("ana", PlaceFilters::default())
        .await
        .unwrap()
        .is_empty());
    assert!(acme.seal_health()["unreadable_rows"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn legacy_maps_are_sealed_once_and_still_read() {
    let root = root();
    {
        let stores = Stores::on_disk_with_keys(root.clone(), PluginKeys::compat());
        seed(&stores, "local").await;
        seed(&stores, "org:acme").await;
        assert_eq!(
            stores.get("local").await.unwrap().seal_health()["state"],
            "compat"
        );
    }
    assert!(contains(&all_db_bytes(&root), MARK_NOTE));
    let stores = Stores::on_disk_with_keys(root.clone(), keys(3));
    read_back(&stores, "local").await;
    read_back(&stores, "org:acme").await;
    drop(stores);
    assert_no_plaintext(&root);
    let raw = || -> Vec<String> {
        let conn = rusqlite::Connection::open(root.join("places.db")).unwrap();
        let mut statement = conn
            .prepare("SELECT name FROM places UNION ALL SELECT note FROM marks")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows
    };
    let first = raw();
    drop(
        Stores::on_disk_with_keys(root.clone(), keys(3))
            .get("local")
            .await
            .unwrap(),
    );
    assert_eq!(raw(), first);
}

#[tokio::test]
async fn another_key_or_no_key_refuses_writes_and_keeps_data() {
    let root = root();
    seed(&Stores::on_disk_with_keys(root.clone(), keys(4)), "local").await;
    for other in [keys(5), PluginKeys::compat()] {
        let stores = Stores::on_disk_with_keys(root.clone(), other);
        let store = stores.get("local").await.unwrap();
        assert_eq!(store.seal_health()["state"], "unhealthy");
        assert!(store.add_place("ana", new_place("Refused")).await.is_err());
    }
    read_back(&Stores::on_disk_with_keys(root, keys(4)), "local").await;
}

#[tokio::test]
async fn the_mcp_process_reaches_a_sealed_map_through_the_sidecar_handoff() {
    use ryu_places::{
        geocode::Geocoder,
        mcp,
        sidecar_link::{self, SidecarClient},
        state::{AppState, Config},
    };
    let root = root();
    let stores = Stores::on_disk_with_keys(root.clone(), keys(6));
    seed(&stores, "local").await;
    stores.set_agent_access("local", "ana", true).await.unwrap();
    let state = AppState::new(stores, Geocoder::new(None), Config::for_tests("core-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handoff = sidecar_link::publish(&root, listener.local_addr().unwrap().port()).unwrap();
    let app = axum::Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .merge(mcp::handoff_router(state, handoff.token.clone()));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = SidecarClient::connect(&root)
        .await
        .expect("sidecar answers");
    let result = client
        .call(
            "POST",
            mcp::HANDOFF_MCP_ROUTE,
            Some(&json!({ "name": "list", "arguments": {} })),
        )
        .await
        .unwrap();
    assert_eq!(result["isError"], false, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(NAME));
    // Personal mark notes never reach the agent lane.
    assert!(!text.contains(MARK_NOTE));
    let wrong = SidecarClient::connect(&root).await.unwrap();
    drop(wrong);
    sidecar_link::withdraw(&root, &handoff);
    // Without a sidecar, a keyless process refuses the sealed map.
    let keyless = Stores::on_disk_with_keys(root, PluginKeys::compat());
    assert_eq!(
        keyless.get("local").await.unwrap().seal_health()["state"],
        "unhealthy"
    );
}

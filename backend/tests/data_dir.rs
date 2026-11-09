//! Places persists only under its own plugin data dir, never at the data-dir root.

use std::path::PathBuf;

use ryu_places::{paths, tenancy::Stores, tenancy::LOCAL_TENANT};

fn scratch(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ryu-places-{label}-{}", uuid::Uuid::new_v4()))
}

// One test owns the process env so the two data-dir resolutions cannot race.
#[tokio::test]
async fn databases_live_under_the_plugin_data_dir() {
    // Standalone run (and the MCP child): RYU_DIR only, so the layout fallback
    // resolves `<RYU_DIR>/plugin-data/@ryu+places`.
    let ryu_dir = scratch("root");
    std::env::remove_var("RYU_PLUGIN_DATA_DIR");
    std::env::set_var("RYU_DIR", &ryu_dir);
    let fallback = paths::data_dir();
    assert!(
        fallback.starts_with(ryu_dir.join("plugin-data")),
        "{} is outside plugin-data/",
        fallback.display()
    );
    assert_ne!(fallback, ryu_dir);

    // Core-spawned: the injected RYU_PLUGIN_DATA_DIR wins.
    let injected = scratch("plugin");
    std::env::set_var("RYU_PLUGIN_DATA_DIR", &injected);
    assert_eq!(paths::data_dir(), injected);

    let stores = Stores::on_disk(paths::data_dir());
    stores.get(LOCAL_TENANT).await.expect("open local map");
    stores.get("org_acme").await.expect("open org map");

    assert!(injected.join("places.db").is_file());
    assert!(injected
        .join("tenants")
        .join(format!("{}.db", hex("org_acme")))
        .is_file());
    // Nothing leaks to the data-dir root.
    assert!(!ryu_dir.join("places.db").exists());
    assert!(!ryu_dir.join("places").exists());

    let _ = std::fs::remove_dir_all(&injected);
    let _ = std::fs::remove_dir_all(&ryu_dir);
}

fn hex(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

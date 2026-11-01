//! The MCP lane serves exactly one tenant that opted in, never as a person,
//! and an organization-bound node never serves a personal map to its agents.

use axum::Router;
use serde_json::{json, Value};

use ryu_places::{
    api::{self, CALLER_ORG_ID_HEADER, CALLER_USER_ID_HEADER},
    geocode::Geocoder,
    mcp,
    models::{MarkInput, MarkStatus, NewPlace},
    state::{AppState, Config},
    store::AGENT_MEMBER_ID,
    tenancy::{AgentLaneError, AgentNotConnected, Stores, LOCAL_TENANT},
};

fn state(stores: Stores, managed_node: bool) -> AppState {
    let mut config = Config::for_tests("secret");
    config.managed_node = managed_node;
    AppState::new(stores, Geocoder::new(None), config).with_model(None)
}

fn place(name: &str) -> NewPlace {
    NewPlace {
        name: name.to_owned(),
        address: None,
        lat: 35.0,
        lng: 139.0,
        category: "food".to_owned(),
        url: None,
        note: None,
        list_id: None,
        external_id: None,
        status: None,
        visited_on: None,
        rating: None,
    }
}

fn lane_error(error: &anyhow::Error) -> Option<AgentLaneError> {
    error.downcast_ref::<AgentLaneError>().copied()
}

#[tokio::test]
async fn the_agent_lane_is_off_until_a_map_opts_in() {
    let stores = Stores::memory();
    let state = state(stores.clone(), false);
    let error = mcp::call_tool(&state, "list", &json!({}))
        .await
        .expect_err("no tenant opted in");
    assert!(error.downcast_ref::<AgentNotConnected>().is_some());

    let access = stores.agent_access(LOCAL_TENANT).await.unwrap();
    assert!(!access.enabled);
    assert!(access.available);
}

#[tokio::test]
async fn the_agent_acts_as_itself_and_never_as_a_member() {
    let stores = Stores::memory();
    let state = state(stores.clone(), false);
    let local = stores.get(LOCAL_TENANT).await.unwrap();
    let home = local.add_place("local", place("Home ramen")).await.unwrap();
    local
        .set_mark(
            "local",
            &home.id,
            MarkInput {
                status: MarkStatus::Been,
                visited_on: Some("2026-05-01".to_owned()),
                rating: Some(5),
                note: Some("went with my sister".to_owned()),
            },
        )
        .await
        .unwrap();
    stores
        .set_agent_access(LOCAL_TENANT, "local", true)
        .await
        .unwrap();

    // Reads: marks are visible without their personal notes.
    let listed = mcp::call_tool(&state, "list", &json!({})).await.unwrap();
    let mark = &listed["places"][0]["marks"][0];
    assert_eq!(mark["status"], "been");
    assert_eq!(mark["rating"], 5);
    assert_eq!(mark["note"], Value::Null);

    // Writes are attributed to the agent and never set a mark.
    let saved = mcp::call_tool(
        &state,
        "save",
        &json!({ "name": "Agent cafe", "lat": 35.1, "lng": 139.1, "status": "been", "rating": 4 }),
    )
    .await
    .unwrap();
    assert_eq!(saved["addedBy"], AGENT_MEMBER_ID);
    assert_eq!(saved["marks"], json!([]));
    let members = local.members().await.unwrap();
    let agent = members
        .iter()
        .find(|member| member.id == AGENT_MEMBER_ID)
        .expect("agent member");
    assert_eq!(agent.name, "Places Guide");

    for tool in ["mark", "unmark"] {
        assert!(
            mcp::call_tool(&state, tool, &json!({ "id": home.id, "status": "want" }))
                .await
                .is_err(),
            "{tool} must not exist on the agent lane"
        );
    }

    // The agent can delete only what it saved itself.
    assert!(mcp::call_tool(&state, "delete", &json!({ "id": home.id }))
        .await
        .is_err());
    let agent_place = saved["id"].as_str().unwrap();
    mcp::call_tool(&state, "delete", &json!({ "id": agent_place }))
        .await
        .unwrap();
    assert_eq!(local.counts().await.unwrap(), 1);

    // The agent never pays for or shares in a cost: paidBy must name a person.
    let trip = mcp::call_tool(
        &state,
        "create_trip",
        &json!({ "name": "Tokyo", "dayCount": 2 }),
    )
    .await
    .unwrap();
    let trip_id = trip["trip"]["id"].as_str().unwrap();
    assert!(mcp::call_tool(
        &state,
        "add_expense",
        &json!({ "tripId": trip_id, "title": "Dinner", "amountMinor": 4000 }),
    )
    .await
    .is_err());
    let with_payer = mcp::call_tool(
        &state,
        "add_expense",
        &json!({ "tripId": trip_id, "title": "Dinner", "amountMinor": 4000, "paidBy": "local" }),
    )
    .await
    .unwrap();
    let expense = &with_payer["expenses"][0];
    assert_eq!(expense["paidBy"], "local");
    assert_eq!(expense["splitBetween"], json!(["local"]));
}

#[tokio::test]
async fn one_map_holds_the_lane_and_an_org_drops_a_personal_one() {
    let stores = Stores::memory();
    let state = state(stores.clone(), false);
    let personal = stores.get(LOCAL_TENANT).await.unwrap();
    personal
        .add_place("local", place("Owner's secret spot"))
        .await
        .unwrap();
    stores
        .set_agent_access(LOCAL_TENANT, "local", true)
        .await
        .unwrap();

    // Another map cannot take the lane from the holder.
    let error = stores
        .set_agent_access("user:bob", "bob", true)
        .await
        .unwrap_err();
    assert_eq!(lane_error(&error), Some(AgentLaneError::HeldElsewhere));
    assert!(!stores.agent_access("user:bob").await.unwrap().available);

    // The node starts serving an organization: personal access is dropped.
    stores.note_tenant("org:acme").await.unwrap();
    assert_eq!(stores.agent_tenant().await.unwrap(), None);
    assert!(mcp::call_tool(&state, "list", &json!({})).await.is_err());

    // A personal map cannot take it back on an organization-bound node.
    let error = stores
        .set_agent_access(LOCAL_TENANT, "local", true)
        .await
        .unwrap_err();
    assert_eq!(lane_error(&error), Some(AgentLaneError::OrgBound));
    assert!(!stores.agent_access(LOCAL_TENANT).await.unwrap().available);

    // The organization opts in and its agent sees only the org map.
    stores
        .set_agent_access("org:acme", "ana", true)
        .await
        .unwrap();
    let org = stores.get("org:acme").await.unwrap();
    org.add_place("ana", place("Team lunch")).await.unwrap();
    let listed = mcp::call_tool(&state, "list", &json!({})).await.unwrap();
    let names = listed["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| place["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Team lunch"]);

    // Turning it off disconnects the agent at once.
    stores
        .set_agent_access("org:acme", "ana", false)
        .await
        .unwrap();
    assert!(mcp::call_tool(&state, "list", &json!({})).await.is_err());
}

#[tokio::test]
async fn the_lane_record_is_shared_between_the_sidecar_and_the_mcp_child() {
    let root = std::env::temp_dir().join(format!("ryu-places-lane-{}", uuid::Uuid::new_v4()));
    let sidecar = Stores::on_disk(root.clone());
    let mcp_child = Stores::on_disk(root.clone());
    sidecar
        .set_agent_access("org:acme", "ana", true)
        .await
        .unwrap();
    assert_eq!(
        mcp_child.agent_tenant().await.unwrap().as_deref(),
        Some("org:acme")
    );
    assert!(ryu_places::paths::agent_db_path(&root).is_file());
    let _ = std::fs::remove_dir_all(&root);
}

async fn serve(state: AppState) -> String {
    let app = Router::new().nest("/api/places", api::routes(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/api/places")
}

#[tokio::test]
async fn a_managed_node_rejects_unstamped_requests() {
    let base = serve(state(Stores::memory(), true)).await;
    let client = reqwest::Client::new();
    let unstamped = client.get(format!("{base}/places")).send().await.unwrap();
    assert_eq!(unstamped.status(), reqwest::StatusCode::FORBIDDEN);

    let stamped = client
        .get(format!("{base}/places"))
        .header(CALLER_USER_ID_HEADER, "user-1")
        .header(CALLER_ORG_ID_HEADER, "acme")
        .send()
        .await
        .unwrap();
    assert_eq!(stamped.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn members_toggle_agent_access_over_http() {
    let stores = Stores::memory();
    let base = serve(state(stores.clone(), false)).await;
    let client = reqwest::Client::new();
    let put = |user: &'static str, org: Option<&'static str>, enabled: bool| {
        let mut request = client
            .put(format!("{base}/agent"))
            .header(CALLER_USER_ID_HEADER, user)
            .json(&json!({ "enabled": enabled }));
        if let Some(org) = org {
            request = request.header(CALLER_ORG_ID_HEADER, org);
        }
        request.send()
    };

    let on = put("ana", Some("acme"), true).await.unwrap();
    assert_eq!(on.status(), reqwest::StatusCode::OK);
    assert_eq!(
        on.json::<Value>().await.unwrap(),
        json!({ "enabled": true, "available": true })
    );
    assert_eq!(
        stores.agent_tenant().await.unwrap().as_deref(),
        Some("org:acme")
    );

    // A personal map on the same (now organization-bound) node is refused.
    let refused = put("bob", None, true).await.unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);

    let read = client
        .get(format!("{base}/agent"))
        .header(CALLER_USER_ID_HEADER, "bob")
        .send()
        .await
        .unwrap();
    assert_eq!(
        read.json::<Value>().await.unwrap(),
        json!({ "enabled": false, "available": false })
    );
}

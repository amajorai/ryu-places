use axum::http::{HeaderMap, HeaderValue};
use serde_json::json;

use ryu_places::{
    api::{CALLER_ORG_ID_HEADER, CALLER_USER_ID_HEADER, SERVED_ROUTES},
    geocode::{category_for, parse_photon},
    mcp,
    store::{AGENT_MEMBER_ID, LOCAL_MEMBER_ID},
    tenancy::{caller_from_headers, TenantRequired, LOCAL_TENANT},
};

#[test]
fn served_routes_match_the_manifest_surface() {
    assert_eq!(
        SERVED_ROUTES,
        [
            "/me",
            "/members",
            "/agent",
            "/places",
            "/places/:id",
            "/places/:id/mark",
            "/lists",
            "/lists/:id",
            "/activity",
            "/summary",
            "/search",
            "/trips",
            "/trips/:id",
            "/trips/:id/stops",
            "/trips/:id/stops/:item",
            "/trips/:id/days/:day/optimize",
            "/trips/:id/reservations",
            "/trips/:id/reservations/:item",
            "/trips/:id/expenses",
            "/trips/:id/expenses/:item",
            "/trips/:id/checklist",
            "/trips/:id/checklist/:item",
            "/trips/:id/stops/batch",
            "/trips/:id/suggest",
            "/import/link",
            "/import/file",
            "/import/save",
        ]
    );
}

#[test]
fn mcp_tools_are_bare_and_descriptive() {
    let table = mcp::tools();
    let tools = table.as_array().expect("MCP tool list");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "search",
            "list",
            "get",
            "save",
            "update",
            "delete",
            "lists",
            "create_list",
            "rename_list",
            "overview",
            "import_link",
            "suggest_itinerary",
            "add_stops",
            "trips",
            "trip",
            "create_trip",
            "update_trip",
            "add_stop",
            "move_stop",
            "remove_stop",
            "optimize_day",
            "add_booking",
            "add_expense",
            "add_todo",
            "check_todo",
        ]
    );
    for tool in tools {
        assert!(tool["description"].as_str().unwrap_or_default().len() > 40);
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(!tool["name"].as_str().unwrap().contains('.'));
    }
}

#[test]
fn org_membership_selects_the_shared_map() {
    let mut headers = HeaderMap::new();
    let local = caller_from_headers(&headers, false).unwrap();
    assert_eq!(
        (local.tenant.as_str(), local.member.as_str()),
        (LOCAL_TENANT, LOCAL_MEMBER_ID)
    );

    headers.insert(CALLER_USER_ID_HEADER, HeaderValue::from_static("  "));
    assert_eq!(
        caller_from_headers(&headers, false).unwrap().tenant,
        LOCAL_TENANT
    );

    headers.insert(CALLER_USER_ID_HEADER, HeaderValue::from_static("user-42"));
    let personal = caller_from_headers(&headers, false).unwrap();
    assert_eq!(personal.tenant, "user:user-42");
    assert_eq!(personal.member, "user-42");

    headers.insert(CALLER_ORG_ID_HEADER, HeaderValue::from_static("org-7"));
    let shared = caller_from_headers(&headers, false).unwrap();
    assert_eq!(shared.tenant, "org:org-7");
    assert_eq!(shared.member, "user-42");
}

#[test]
fn a_managed_node_never_falls_back_to_the_local_map() {
    let mut headers = HeaderMap::new();
    assert_eq!(caller_from_headers(&headers, true), Err(TenantRequired));
    headers.insert(CALLER_USER_ID_HEADER, HeaderValue::from_static(" "));
    assert_eq!(caller_from_headers(&headers, true), Err(TenantRequired));

    headers.insert(CALLER_USER_ID_HEADER, HeaderValue::from_static("user-42"));
    assert_eq!(
        caller_from_headers(&headers, true).unwrap().tenant,
        "user:user-42"
    );
}

#[test]
fn malformed_or_reserved_stamps_fail_closed() {
    // An org stamp without a user is never treated as a member of that org.
    let mut headers = HeaderMap::new();
    headers.insert(CALLER_ORG_ID_HEADER, HeaderValue::from_static("org-7"));
    assert_eq!(caller_from_headers(&headers, false), Err(TenantRequired));

    // An oversized stamp is rejected, not dropped back to the local map.
    let mut headers = HeaderMap::new();
    let long = "u".repeat(129);
    headers.insert(CALLER_USER_ID_HEADER, HeaderValue::from_str(&long).unwrap());
    assert_eq!(caller_from_headers(&headers, false), Err(TenantRequired));

    // A stamped user can never act as the node owner or as the agent.
    for reserved in [LOCAL_MEMBER_ID, AGENT_MEMBER_ID] {
        let mut headers = HeaderMap::new();
        headers.insert(
            CALLER_USER_ID_HEADER,
            HeaderValue::from_str(reserved).unwrap(),
        );
        assert_eq!(caller_from_headers(&headers, false), Err(TenantRequired));
    }
}

#[test]
fn photon_features_become_categorized_results() {
    let body = json!({
        "type": "FeatureCollection",
        "features": [
            {
                "geometry": { "type": "Point", "coordinates": [139.7016, 35.6580] },
                "properties": {
                    "osm_id": 123, "osm_type": "N", "osm_key": "amenity", "osm_value": "restaurant",
                    "name": "Ichiran Shibuya", "street": "Udagawacho", "housenumber": "13-7",
                    "city": "Tokyo", "country": "Japan"
                }
            },
            { "geometry": { "type": "Point", "coordinates": [2.3376, 48.8606] },
              "properties": { "osm_id": 7, "osm_type": "W", "osm_key": "tourism", "osm_value": "museum", "name": "Louvre", "city": "Paris", "country": "France" } },
            { "geometry": { "type": "LineString", "coordinates": [[0, 0], [1, 1]] }, "properties": { "name": "Skipped" } },
            { "geometry": { "type": "Point", "coordinates": [0, 0] }, "properties": {} }
        ]
    });
    let results = parse_photon(&body);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name, "Ichiran Shibuya");
    assert_eq!(
        results[0].address.as_deref(),
        Some("13-7 Udagawacho, Tokyo, Japan")
    );
    assert_eq!(results[0].category, "food");
    assert_eq!(results[0].external_id.as_deref(), Some("osm:n123"));
    assert!((results[0].lat - 35.6580).abs() < 1e-9);
    assert_eq!(results[1].category, "sight");
    assert_eq!(results[1].external_id.as_deref(), Some("osm:w7"));
    assert_eq!(category_for("shop", "bakery"), "cafe");
    assert_eq!(category_for("highway", "bus_stop"), "other");
}

#[test]
fn mcp_tool_failures_are_result_content() {
    let response = mcp::tool_content(&json!({ "error": "place not found" }), true);
    assert_eq!(response["isError"], true);
    assert!(response["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("place not found"));
}

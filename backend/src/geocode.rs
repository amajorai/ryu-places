//! Place search over a Photon geocoder (OpenStreetMap data).
//!
//! The default is the public Photon instance (`photon.komoot.io`), which needs no
//! key. What leaves the node is each lookup's query text and optional
//! map-center bias: search text, plus place names and addresses being imported
//! (Google Maps export rows without coordinates, names and cities extracted from
//! links and captions) and AI-drafted stop names with the trip destination (see
//! `ai::resolve_rows`, `ai::resolve_mentions`, and the trip-draft resolver).
//! Marks, notes, and member names never do. `RYU_PLACES_GEOCODER_URL`
//! points at a self-hosted Photon, and `off` disables remote search entirely
//! (places can still be saved by coordinates).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::models::SearchResult;

pub const DEFAULT_GEOCODER_URL: &str = "https://photon.komoot.io/api/";
const MAX_QUERY_LEN: usize = 200;
const RESULT_LIMIT: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("place search is turned off on this node")]
    Disabled,
    #[error("q is required")]
    EmptyQuery,
    #[error("q is too long")]
    QueryTooLong,
}

#[derive(Clone)]
pub struct Geocoder {
    endpoint: Option<String>,
    client: reqwest::Client,
}

impl Geocoder {
    pub fn from_env() -> Self {
        let configured = std::env::var("RYU_PLACES_GEOCODER_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let endpoint = match configured.as_deref() {
            Some("off" | "none" | "disabled") => None,
            Some(url) => Some(url.to_owned()),
            None => Some(DEFAULT_GEOCODER_URL.to_owned()),
        };
        Self::new(endpoint)
    }

    pub fn new(endpoint: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(concat!("ryu-places/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { endpoint, client }
    }

    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    pub async fn search(&self, query: &str, near: Option<(f64, f64)>) -> Result<Vec<SearchResult>> {
        let endpoint = self.endpoint.as_deref().ok_or(SearchError::Disabled)?;
        let query = query.trim();
        if query.is_empty() {
            return Err(SearchError::EmptyQuery.into());
        }
        if query.chars().count() > MAX_QUERY_LEN {
            return Err(SearchError::QueryTooLong.into());
        }
        let limit = RESULT_LIMIT.to_string();
        let mut params: Vec<(&str, String)> = vec![("q", query.to_owned()), ("limit", limit)];
        if let Some((lat, lng)) = near.filter(|(lat, lng)| lat.is_finite() && lng.is_finite()) {
            params.push(("lat", format!("{lat:.4}")));
            params.push(("lon", format!("{lng:.4}")));
        }
        let response = self
            .client
            .get(endpoint)
            .query(&params)
            .send()
            .await
            .context("place search is unreachable")?
            .error_for_status()
            .context("place search failed")?;
        let body: Value = response
            .json()
            .await
            .context("place search returned invalid JSON")?;
        Ok(parse_photon(&body))
    }
}

/// Parse a Photon GeoJSON `FeatureCollection` into search results. Features
/// without a point geometry or a usable label are skipped, not errors.
pub fn parse_photon(body: &Value) -> Vec<SearchResult> {
    let Some(features) = body.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    features
        .iter()
        .filter_map(|feature| {
            let coordinates = feature.pointer("/geometry/coordinates")?.as_array()?;
            let lng = coordinates.first()?.as_f64()?;
            let lat = coordinates.get(1)?.as_f64()?;
            let props = feature.get("properties")?;
            let text = |key: &str| {
                props
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            let street = match (text("street"), text("housenumber")) {
                (Some(street), Some(number)) => Some(format!("{number} {street}")),
                (Some(street), None) => Some(street.to_owned()),
                _ => None,
            };
            let address_parts = [
                street.as_deref(),
                text("district"),
                text("city"),
                text("state"),
                text("country"),
            ];
            let mut address = Vec::new();
            for part in address_parts.into_iter().flatten() {
                if !address.contains(&part) {
                    address.push(part);
                }
            }
            let name = text("name")
                .map(str::to_owned)
                .or_else(|| street.clone())
                .or_else(|| text("city").map(str::to_owned))?;
            let address = address
                .into_iter()
                .filter(|part| *part != name)
                .collect::<Vec<_>>()
                .join(", ");
            let external_id = match (
                text("osm_type"),
                props.get("osm_id").and_then(Value::as_i64),
            ) {
                (Some(kind), Some(id)) => Some(format!("osm:{}{id}", kind.to_ascii_lowercase())),
                _ => None,
            };
            Some(SearchResult {
                name,
                address: (!address.is_empty()).then_some(address),
                lat,
                lng,
                category: category_for(
                    text("osm_key").unwrap_or(""),
                    text("osm_value").unwrap_or(""),
                )
                .to_owned(),
                external_id,
            })
        })
        .collect()
}

/// Map an OSM `key=value` tag onto the app's fixed category vocabulary.
pub fn category_for(key: &str, value: &str) -> &'static str {
    match (key, value) {
        ("amenity", "cafe" | "ice_cream") | ("shop", "coffee" | "bakery" | "pastry") => "cafe",
        ("amenity", "restaurant" | "fast_food" | "food_court" | "bbq") => "food",
        ("amenity", "bar" | "pub" | "biergarten" | "nightclub") => "bar",
        (
            "tourism",
            "hotel" | "hostel" | "motel" | "guest_house" | "apartment" | "camp_site" | "chalet",
        ) => "stay",
        ("tourism", "museum" | "gallery" | "attraction" | "viewpoint" | "artwork")
        | ("historic", _)
        | ("amenity", "place_of_worship" | "theatre" | "arts_centre") => "sight",
        ("leisure", "park" | "garden" | "nature_reserve" | "beach_resort")
        | ("natural", _)
        | ("boundary", "national_park") => "nature",
        ("tourism", "zoo" | "aquarium" | "theme_park")
        | (
            "leisure",
            "playground" | "sports_centre" | "water_park" | "stadium" | "swimming_pool",
        )
        | ("amenity", "cinema") => "activity",
        ("shop", _) | ("amenity", "marketplace") => "shop",
        _ => "other",
    }
}

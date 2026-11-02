//! Parse place exports from other apps into import candidates.
//!
//! - Google Takeout "Saved Places.json" (GeoJSON features with a name, address,
//!   and Google Maps URL),
//! - KML from Google My Maps and most map apps (`<Placemark>` with `<name>` and
//!   `<coordinates>lng,lat`),
//! - Google Takeout saved-list CSV (`Title,Note,URL`, no coordinates; the URL
//!   sometimes carries them, otherwise the title is geocoded).
//!
//! Parsing is deterministic and needs no model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ROWS: usize = 500;

/// One place found in an export, before geocoding or saving.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawPlace {
    pub name: String,
    pub address: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub note: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Geojson,
    Kml,
    Csv,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImportError {
    #[error("the file is not valid {0}")]
    Invalid(&'static str),
    #[error("no places were found in the file")]
    Empty,
}

/// Guess a format from the content when the client does not say.
pub fn sniff(content: &str) -> ExportFormat {
    let start = content.trim_start();
    if start.starts_with('{') || start.starts_with('[') {
        ExportFormat::Geojson
    } else if start.starts_with('<') {
        ExportFormat::Kml
    } else {
        ExportFormat::Csv
    }
}

pub fn parse(format: ExportFormat, content: &str) -> Result<Vec<RawPlace>, ImportError> {
    let mut places = match format {
        ExportFormat::Geojson => parse_geojson(content)?,
        ExportFormat::Kml => parse_kml(content),
        ExportFormat::Csv => parse_csv(content),
    };
    places.retain(|place| !place.name.trim().is_empty());
    places.truncate(MAX_ROWS);
    if places.is_empty() {
        return Err(ImportError::Empty);
    }
    Ok(places)
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

pub fn parse_geojson(content: &str) -> Result<Vec<RawPlace>, ImportError> {
    let value: Value = serde_json::from_str(content).map_err(|_| ImportError::Invalid("JSON"))?;
    let features = value
        .get("features")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or(ImportError::Invalid("GeoJSON"))?;
    Ok(features
        .iter()
        .filter_map(|feature| {
            let props = feature.get("properties").unwrap_or(&Value::Null);
            let location = props.get("location").unwrap_or(&Value::Null);
            let coords = feature
                .pointer("/geometry/coordinates")
                .and_then(Value::as_array);
            let (mut lng, mut lat) = match coords {
                Some(c) => (
                    c.first().and_then(Value::as_f64),
                    c.get(1).and_then(Value::as_f64),
                ),
                None => (None, None),
            };
            // Takeout writes [0, 0] for places it could not resolve.
            if lat == Some(0.0) && lng == Some(0.0) {
                lat = None;
                lng = None;
            }
            let url = text(props.get("google_maps_url")).or_else(|| text(props.get("url")));
            if lat.is_none() {
                if let Some((la, ln)) = url.as_deref().and_then(coords_from_url) {
                    lat = Some(la);
                    lng = Some(ln);
                }
            }
            let name = text(location.get("name"))
                .or_else(|| text(props.get("name")))
                .or_else(|| text(props.get("Title")))
                .or_else(|| text(props.get("title")))?;
            Some(RawPlace {
                name,
                address: text(location.get("address")).or_else(|| text(props.get("address"))),
                lat,
                lng,
                note: text(props.get("Comment")).or_else(|| text(props.get("description"))),
                url,
            })
        })
        .collect())
}

pub fn parse_kml(content: &str) -> Vec<RawPlace> {
    content
        .split("<Placemark")
        .skip(1)
        .filter_map(|chunk| {
            let chunk = chunk.split("</Placemark>").next().unwrap_or(chunk);
            let name = xml_text(chunk, "name")?;
            let (lat, lng) = xml_text(chunk, "coordinates")
                .and_then(|coords| {
                    let first = coords.split_whitespace().next()?.to_owned();
                    let mut parts = first.split(',');
                    let lng = parts.next()?.trim().parse::<f64>().ok()?;
                    let lat = parts.next()?.trim().parse::<f64>().ok()?;
                    Some((Some(lat), Some(lng)))
                })
                .unwrap_or((None, None));
            Some(RawPlace {
                name,
                address: xml_text(chunk, "address"),
                lat,
                lng,
                note: xml_text(chunk, "description").map(|d| strip_tags(&d)),
                url: None,
            })
        })
        .collect()
}

fn xml_text(chunk: &str, tag: &str) -> Option<String> {
    let open = chunk.find(&format!("<{tag}"))?;
    let start = open + chunk[open..].find('>')? + 1;
    let end = start + chunk[start..].find(&format!("</{tag}>"))?;
    let raw = chunk[start..end].trim();
    let raw = raw
        .strip_prefix("<![CDATA[")
        .and_then(|inner| inner.strip_suffix("]]>"))
        .unwrap_or(raw)
        .trim();
    (!raw.is_empty()).then(|| {
        raw.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
    })
}

fn strip_tags(value: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for ch in value.chars() {
        match ch {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn parse_csv(content: &str) -> Vec<RawPlace> {
    let rows = csv_rows(content);
    let Some((header, body)) = rows.split_first() else {
        return Vec::new();
    };
    let column = |names: &[&str]| {
        header.iter().position(|cell| {
            names
                .iter()
                .any(|name| cell.trim().eq_ignore_ascii_case(name))
        })
    };
    let name_at = column(&["title", "name", "place"]).unwrap_or(0);
    let note_at = column(&["note", "notes", "comment", "description"]);
    let url_at = column(&["url", "link", "google maps url"]);
    let address_at = column(&["address"]);
    let lat_at = column(&["lat", "latitude"]);
    let lng_at = column(&["lng", "lon", "long", "longitude"]);
    body.iter()
        .filter_map(|row| {
            let cell = |index: Option<usize>| {
                index
                    .and_then(|i| row.get(i))
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            };
            let name = cell(Some(name_at))?;
            let url = cell(url_at);
            let (mut lat, mut lng) = (
                cell(lat_at).and_then(|v| v.parse().ok()),
                cell(lng_at).and_then(|v| v.parse().ok()),
            );
            if lat.is_none() {
                if let Some((la, ln)) = url.as_deref().and_then(coords_from_url) {
                    lat = Some(la);
                    lng = Some(ln);
                }
            }
            Some(RawPlace {
                name,
                address: cell(address_at),
                lat,
                lng,
                note: cell(note_at),
                url,
            })
        })
        .collect()
}

/// RFC 4180-ish: quoted cells, doubled quotes, and newlines inside quotes.
fn csv_rows(content: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = content.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(ch) = chars.next() {
        match (ch, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => row.push(std::mem::take(&mut cell)),
            ('\n', false) => {
                row.push(std::mem::take(&mut cell));
                if row.iter().any(|c| !c.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            ('\r', false) => {}
            _ => cell.push(ch),
        }
    }
    row.push(cell);
    if row.iter().any(|c| !c.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

/// Pull coordinates out of a Google Maps URL: `@lat,lng`, `!3dlat!4dlng`, or
/// `q=lat,lng` / `query=lat,lng`.
pub fn coords_from_url(url: &str) -> Option<(f64, f64)> {
    let pair = |text: &str| -> Option<(f64, f64)> {
        let mut parts = text.split(',');
        let lat = parts.next()?.trim().parse::<f64>().ok()?;
        let lng = parts
            .next()?
            .trim()
            .split(['&', '/', '?', 'z'])
            .next()?
            .parse::<f64>()
            .ok()?;
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lng)).then_some((lat, lng))
    };
    if let Some(at) = url.find("!3d") {
        let rest = &url[at + 3..];
        let lat = rest.split('!').next()?.parse::<f64>().ok()?;
        let lng_at = rest.find("!4d")?;
        let lng = rest[lng_at + 3..]
            .split(['!', '?', '&'])
            .next()?
            .parse::<f64>()
            .ok()?;
        return Some((lat, lng));
    }
    if let Some(at) = url.find('@') {
        if let Some(found) = pair(&url[at + 1..]) {
            return Some(found);
        }
    }
    for key in ["query=", "q=", "ll="] {
        if let Some(at) = url.find(key) {
            let value = url[at + key.len()..].replace("%2C", ",");
            if let Some(found) = pair(&value) {
                return Some(found);
            }
        }
    }
    None
}

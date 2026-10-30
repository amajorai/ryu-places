use serde::{Deserialize, Deserializer, Serialize};

/// The fixed category vocabulary. A free-form category would fragment the map
/// legend across members ("Cafe", "coffee", "Coffee shop"), so the store
/// accepts only these and the geocoder maps OSM tags onto them.
pub const CATEGORIES: &[&str] = &[
    "food", "cafe", "bar", "sight", "nature", "shop", "stay", "activity", "other",
];

/// One member's relationship to a place. Every member keeps their own
/// mark, so "Mum has been, the kids want to go" is representable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MarkStatus {
    Want,
    Been,
}

impl MarkStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Want => "want",
            Self::Been => "been",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "want" => Some(Self::Want),
            "been" => Some(Self::Been),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: String,
    pub name: String,
    pub color: String,
    pub joined_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberPatch {
    pub name: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaceList {
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub created_by: String,
    pub place_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewList {
    pub name: String,
    #[serde(default)]
    pub emoji: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListPatch {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub emoji: Option<Option<String>>,
}

/// A member's mark joined with the member's display fields so a client can
/// draw "who has been" without a second lookup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub member_id: String,
    pub member_name: String,
    pub member_color: String,
    pub status: MarkStatus,
    pub visited_on: Option<String>,
    pub rating: Option<i64>,
    pub note: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    pub id: String,
    pub name: String,
    pub address: Option<String>,
    pub lat: f64,
    pub lng: f64,
    pub category: String,
    pub url: Option<String>,
    pub note: Option<String>,
    pub list_id: Option<String>,
    pub external_id: Option<String>,
    pub added_by: String,
    pub marks: Vec<Mark>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NewPlace {
    pub name: String,
    #[serde(default)]
    pub address: Option<String>,
    pub lat: f64,
    pub lng: f64,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub list_id: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    /// Optional first mark for the member who saves the place. Omitted means the
    /// place is shared on the map without anyone claiming it yet.
    #[serde(default)]
    pub status: Option<MarkStatus>,
    #[serde(default)]
    pub visited_on: Option<String>,
    #[serde(default)]
    pub rating: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlacePatch {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub address: Option<Option<String>>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub category: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub url: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub note: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub list_id: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MarkInput {
    pub status: MarkStatus,
    #[serde(default)]
    pub visited_on: Option<String>,
    #[serde(default)]
    pub rating: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Which places a list call returns. `status` and `member` combine: `status=been`
/// with `member=<id>` is "everywhere this person has been"; `status=been` alone
/// is "anywhere any member has been".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaceFilters {
    pub status: Option<String>,
    pub member: Option<String>,
    pub list: Option<String>,
    pub category: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub id: i64,
    pub member_id: String,
    pub member_name: String,
    pub kind: String,
    pub place_id: Option<String>,
    pub place_name: Option<String>,
    pub detail: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberStats {
    pub member_id: String,
    pub member_name: String,
    pub member_color: String,
    pub want: i64,
    pub been: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub places: i64,
    pub want: i64,
    pub been: i64,
    pub lists: i64,
    pub members: Vec<MemberStats>,
}

/// One geocoder hit. Not stored until a member saves it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub name: String,
    pub address: Option<String>,
    pub lat: f64,
    pub lng: f64,
    pub category: String,
    pub external_id: Option<String>,
}

fn default_category() -> String {
    "other".to_owned()
}

fn deserialize_optional_field<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

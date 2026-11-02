use chrono::NaiveDate;
use thiserror::Error;

use crate::models::{MarkInput, MarkStatus, NewPlace, PlacePatch, CATEGORIES};

const MAX_NAME_LEN: usize = 160;
const MAX_ADDRESS_LEN: usize = 300;
const MAX_URL_LEN: usize = 2_000;
const MAX_NOTE_LEN: usize = 2_000;
const MAX_MEMBER_NAME_LEN: usize = 60;
const MAX_LIST_NAME_LEN: usize = 80;
const MAX_EMOJI_LEN: usize = 8;
const MAX_EXTERNAL_ID_LEN: usize = 80;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("{field} is required")]
    Required { field: &'static str },
    #[error("{field} is too long")]
    TooLong { field: &'static str },
    #[error("lat must be between -90 and 90 and lng between -180 and 180")]
    InvalidCoordinates,
    #[error("category must be one of: {}", CATEGORIES.join(", "))]
    InvalidCategory,
    #[error("visitedOn must be an exact YYYY-MM-DD date")]
    InvalidDate,
    #[error("rating must be a whole number from 1 to 5")]
    InvalidRating,
    #[error("visitedOn and rating only apply to a 'been' mark")]
    WantHasVisit,
    #[error("url must start with http:// or https://")]
    InvalidUrl,
    #[error("color must be a #RRGGBB hex value")]
    InvalidColor,
    #[error("status must be 'want' or 'been'")]
    InvalidStatus,
    #[error("at least one field is required")]
    EmptyPatch,
}

pub fn parse_date(value: &str) -> Result<NaiveDate, ValidationError> {
    let date =
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| ValidationError::InvalidDate)?;
    if date.format("%Y-%m-%d").to_string() == value {
        Ok(date)
    } else {
        Err(ValidationError::InvalidDate)
    }
}

pub fn validate_coordinates(lat: f64, lng: f64) -> Result<(), ValidationError> {
    if lat.is_finite()
        && lng.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lng)
    {
        Ok(())
    } else {
        Err(ValidationError::InvalidCoordinates)
    }
}

pub fn normalize_category(value: &str) -> Result<String, ValidationError> {
    let normalized = value.trim().to_ascii_lowercase();
    if CATEGORIES.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(ValidationError::InvalidCategory)
    }
}

pub fn normalize_color(value: &str) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    let hex = trimmed
        .strip_prefix('#')
        .ok_or(ValidationError::InvalidColor)?;
    if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(format!("#{}", hex.to_ascii_lowercase()))
    } else {
        Err(ValidationError::InvalidColor)
    }
}

pub fn parse_status(value: &str) -> Result<MarkStatus, ValidationError> {
    MarkStatus::parse(value.trim()).ok_or(ValidationError::InvalidStatus)
}

pub fn normalized_new(input: NewPlace) -> Result<NewPlace, ValidationError> {
    let name = required_text("name", &input.name, MAX_NAME_LEN)?;
    validate_coordinates(input.lat, input.lng)?;
    let category = normalize_category(&input.category)?;
    let url = optional_url(input.url)?;
    if let Some(status) = input.status {
        validate_mark_fields(status, input.visited_on.as_deref(), input.rating)?;
    } else if input.visited_on.is_some() || input.rating.is_some() {
        return Err(ValidationError::WantHasVisit);
    }
    Ok(NewPlace {
        name,
        address: optional_text("address", input.address, MAX_ADDRESS_LEN)?,
        lat: input.lat,
        lng: input.lng,
        category,
        url,
        note: optional_text("note", input.note, MAX_NOTE_LEN)?,
        list_id: normalize_optional(input.list_id),
        external_id: optional_text("externalId", input.external_id, MAX_EXTERNAL_ID_LEN)?,
        status: input.status,
        visited_on: normalize_optional(input.visited_on),
        rating: input.rating,
    })
}

pub fn validate_patch(patch: &PlacePatch) -> Result<(), ValidationError> {
    if patch.name.is_none()
        && patch.address.is_none()
        && patch.lat.is_none()
        && patch.lng.is_none()
        && patch.category.is_none()
        && patch.url.is_none()
        && patch.note.is_none()
        && patch.list_id.is_none()
    {
        return Err(ValidationError::EmptyPatch);
    }
    if let Some(name) = patch.name.as_deref() {
        required_text("name", name, MAX_NAME_LEN)?;
    }
    if let Some(category) = patch.category.as_deref() {
        normalize_category(category)?;
    }
    Ok(())
}

pub fn normalized_mark(input: MarkInput) -> Result<MarkInput, ValidationError> {
    validate_mark_fields(input.status, input.visited_on.as_deref(), input.rating)?;
    Ok(MarkInput {
        status: input.status,
        visited_on: normalize_optional(input.visited_on),
        rating: input.rating,
        note: optional_text("note", input.note, MAX_NOTE_LEN)?,
    })
}

fn validate_mark_fields(
    status: MarkStatus,
    visited_on: Option<&str>,
    rating: Option<i64>,
) -> Result<(), ValidationError> {
    if status == MarkStatus::Want && (visited_on.is_some() || rating.is_some()) {
        return Err(ValidationError::WantHasVisit);
    }
    if let Some(date) = visited_on {
        parse_date(date.trim())?;
    }
    if rating.is_some_and(|value| !(1..=5).contains(&value)) {
        return Err(ValidationError::InvalidRating);
    }
    Ok(())
}

pub fn member_name(value: &str) -> Result<String, ValidationError> {
    required_text("name", value, MAX_MEMBER_NAME_LEN)
}

pub fn list_name(value: &str) -> Result<String, ValidationError> {
    required_text("name", value, MAX_LIST_NAME_LEN)
}

pub fn list_emoji(value: Option<String>) -> Result<Option<String>, ValidationError> {
    optional_text("emoji", value, MAX_EMOJI_LEN)
}

pub fn optional_url(value: Option<String>) -> Result<Option<String>, ValidationError> {
    let Some(url) = optional_text("url", value, MAX_URL_LEN)? else {
        return Ok(None);
    };
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        Ok(Some(url))
    } else {
        Err(ValidationError::InvalidUrl)
    }
}

pub fn optional_text(
    field: &'static str,
    value: Option<String>,
    max_len: usize,
) -> Result<Option<String>, ValidationError> {
    let Some(text) = normalize_optional(value) else {
        return Ok(None);
    };
    if text.chars().count() > max_len {
        return Err(ValidationError::TooLong { field });
    }
    Ok(Some(text))
}

pub fn patch_text(
    field: &'static str,
    value: Option<Option<String>>,
    current: Option<String>,
) -> Result<Option<String>, ValidationError> {
    let max_len = match field {
        "address" => MAX_ADDRESS_LEN,
        "url" => MAX_URL_LEN,
        _ => MAX_NOTE_LEN,
    };
    match value {
        None => Ok(current),
        Some(next) if field == "url" => optional_url(next),
        Some(next) => optional_text(field, next, max_len),
    }
}

fn required_text(
    field: &'static str,
    value: &str,
    max_len: usize,
) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::Required { field });
    }
    if trimmed.chars().count() > max_len {
        return Err(ValidationError::TooLong { field });
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

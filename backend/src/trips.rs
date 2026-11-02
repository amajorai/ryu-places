//! Trip planning: dated trips with a day-by-day itinerary, an unscheduled
//! "ideas" bucket, bookings, shared expenses with split balances, and a
//! checklist. Every mutation returns the whole [`TripDetail`] so a client never
//! has to reconcile partial updates from several members.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::models::{now_ms, Member, CATEGORIES};
use crate::seal_sql::{collect_readable, opened, opened_opt};
use crate::store::{
    ensure_member_locked, get_place_for_link, log_locked, NotFound, PlaceStore, AGENT_MEMBER_ID,
    MEMBER_COLUMNS,
};
use crate::validation::{
    normalize_category, optional_text, parse_date, validate_coordinates, ValidationError,
};

pub const MAX_TRIP_DAYS: i64 = 60;
pub const DEFAULT_TRIP_DAYS: i64 = 3;
pub const MAX_BATCH_STOPS: usize = 60;
/// Largest amount, booking cost or budget accepted, in minor units (10^12, so
/// 10 billion in a two-decimal currency). Thousands of maximal rows still sum
/// far inside `i64`, so totals and balances cannot overflow on read.
pub const MAX_AMOUNT_MINOR: i64 = 1_000_000_000_000;
pub const RESERVATION_KINDS: &[&str] = &[
    "flight",
    "stay",
    "train",
    "bus",
    "car",
    "ferry",
    "restaurant",
    "activity",
    "other",
];
const MAX_NAME_LEN: usize = 120;
const MAX_TEXT_LEN: usize = 2_000;
const MAX_SHORT_LEN: usize = 200;

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trips (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           destination TEXT,
           start_date TEXT,
           end_date TEXT,
           day_count INTEGER NOT NULL,
           emoji TEXT,
           note TEXT,
           budget_minor INTEGER,
           currency TEXT NOT NULL,
           created_by TEXT NOT NULL,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS trip_stops (
           id TEXT PRIMARY KEY,
           trip_id TEXT NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
           day INTEGER,
           position INTEGER NOT NULL,
           place_id TEXT,
           name TEXT NOT NULL,
           lat REAL NOT NULL,
           lng REAL NOT NULL,
           category TEXT NOT NULL,
           start_time TEXT,
           note TEXT,
           created_by TEXT NOT NULL,
           created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_trip_stops ON trip_stops(trip_id, day, position);
         CREATE TABLE IF NOT EXISTS trip_reservations (
           id TEXT PRIMARY KEY,
           trip_id TEXT NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
           kind TEXT NOT NULL,
           title TEXT NOT NULL,
           starts_at TEXT,
           ends_at TEXT,
           location TEXT,
           confirmation TEXT,
           note TEXT,
           cost_minor INTEGER,
           currency TEXT NOT NULL,
           created_by TEXT NOT NULL,
           created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS trip_expenses (
           id TEXT PRIMARY KEY,
           trip_id TEXT NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
           title TEXT NOT NULL,
           amount_minor INTEGER NOT NULL,
           currency TEXT NOT NULL,
           paid_by TEXT NOT NULL,
           split_between TEXT NOT NULL,
           spent_on TEXT,
           category TEXT NOT NULL,
           created_by TEXT NOT NULL,
           created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS trip_checklist (
           id TEXT PRIMARY KEY,
           trip_id TEXT NOT NULL REFERENCES trips(id) ON DELETE CASCADE,
           text TEXT NOT NULL,
           assignee TEXT,
           done INTEGER NOT NULL DEFAULT 0,
           position INTEGER NOT NULL,
           created_by TEXT NOT NULL,
           updated_at INTEGER NOT NULL
         );",
    )
    .context("creating trips schema")
}

// ── Models ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Trip {
    pub id: String,
    pub name: String,
    pub destination: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub day_count: i64,
    pub emoji: Option<String>,
    pub note: Option<String>,
    pub budget_minor: Option<i64>,
    pub currency: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TripSummary {
    #[serde(flatten)]
    pub trip: Trip,
    pub stop_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewTrip {
    pub name: String,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    /// Only used when the trip has no dates yet.
    #[serde(default)]
    pub day_count: Option<i64>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub budget_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TripPatch {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    pub destination: Option<Option<String>>,
    #[serde(default, deserialize_with = "optional_field")]
    pub start_date: Option<Option<String>>,
    #[serde(default, deserialize_with = "optional_field")]
    pub end_date: Option<Option<String>>,
    pub day_count: Option<i64>,
    #[serde(default, deserialize_with = "optional_field")]
    pub emoji: Option<Option<String>>,
    #[serde(default, deserialize_with = "optional_field")]
    pub note: Option<Option<String>>,
    #[serde(default, deserialize_with = "optional_field")]
    pub budget_minor: Option<Option<i64>>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stop {
    pub id: String,
    pub trip_id: String,
    /// 1-based day of the trip; `None` is the unscheduled ideas bucket.
    pub day: Option<i64>,
    pub position: i64,
    pub place_id: Option<String>,
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    pub category: String,
    pub start_time: Option<String>,
    pub note: Option<String>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NewStop {
    /// Link a saved place; its name, coordinates and category are copied so the
    /// itinerary survives the place being deleted from the map.
    #[serde(default)]
    pub place_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub day: Option<i64>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopPatch {
    /// Move to another day (`null` = back to ideas). Combine with `position`.
    #[serde(default, deserialize_with = "optional_field")]
    pub day: Option<Option<i64>>,
    /// 0-based index within the (new) day.
    pub position: Option<i64>,
    pub name: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    pub start_time: Option<Option<String>>,
    #[serde(default, deserialize_with = "optional_field")]
    pub note: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Reservation {
    pub id: String,
    pub trip_id: String,
    pub kind: String,
    pub title: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub confirmation: Option<String>,
    pub note: Option<String>,
    pub cost_minor: Option<i64>,
    pub currency: String,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewReservation {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub ends_at: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub confirmation: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub cost_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Expense {
    pub id: String,
    pub trip_id: String,
    pub title: String,
    pub amount_minor: i64,
    pub currency: String,
    pub paid_by: String,
    pub split_between: Vec<String>,
    pub spent_on: Option<String>,
    pub category: String,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewExpense {
    #[serde(default)]
    pub title: String,
    pub amount_minor: i64,
    #[serde(default)]
    pub currency: Option<String>,
    /// Defaults to the caller.
    #[serde(default)]
    pub paid_by: Option<String>,
    /// Defaults to every current member.
    #[serde(default)]
    pub split_between: Vec<String>,
    #[serde(default)]
    pub spent_on: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub id: String,
    pub trip_id: String,
    pub text: String,
    pub assignee: Option<String>,
    pub done: bool,
    pub position: i64,
    pub created_by: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NewChecklistItem {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistPatch {
    pub text: Option<String>,
    pub done: Option<bool>,
    #[serde(default, deserialize_with = "optional_field")]
    pub assignee: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TripDay {
    pub day: i64,
    pub date: Option<String>,
    pub stops: Vec<Stop>,
    /// Straight-line distance through the day's stops in order.
    pub distance_km: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CurrencyTotal {
    pub currency: String,
    pub spent_minor: i64,
    pub booked_minor: i64,
    pub budget_minor: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub currency: String,
    pub member_id: String,
    pub member_name: String,
    /// Positive: is owed money. Negative: owes money.
    pub net_minor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settlement {
    pub currency: String,
    pub from: String,
    pub from_name: String,
    pub to: String,
    pub to_name: String,
    pub amount_minor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub totals: Vec<CurrencyTotal>,
    pub balances: Vec<Balance>,
    pub settlements: Vec<Settlement>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TripDetail {
    pub trip: Trip,
    pub members: Vec<Member>,
    pub days: Vec<TripDay>,
    pub ideas: Vec<Stop>,
    pub reservations: Vec<Reservation>,
    pub expenses: Vec<Expense>,
    pub budget: Budget,
    pub checklist: Vec<ChecklistItem>,
}

fn optional_field<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

// ── Validation ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TripError {
    #[error("endDate must not be before startDate")]
    DateOrder,
    #[error("a trip can span at most {MAX_TRIP_DAYS} days")]
    TooLong,
    #[error("startDate and endDate must be set together")]
    HalfDates,
    #[error("day must be between 1 and the trip's day count")]
    DayOutOfRange,
    #[error("startTime must be HH:MM")]
    InvalidTime,
    #[error("startsAt and endsAt must be YYYY-MM-DD or YYYY-MM-DDTHH:MM")]
    InvalidDateTime,
    #[error("kind must be one of: {}", RESERVATION_KINDS.join(", "))]
    InvalidKind,
    #[error("currency must be three letters")]
    InvalidCurrency,
    #[error("amounts must be positive whole minor units no larger than {MAX_AMOUNT_MINOR}")]
    InvalidAmount,
    #[error("a stop needs a placeId, or a name with lat and lng")]
    StopNeedsLocation,
    #[error("paidBy and splitBetween must name people who are members of this map")]
    UnknownMember,
    #[error("add between 1 and {MAX_BATCH_STOPS} stops at a time")]
    BatchSize,
    #[error("day {0} has fewer than three stops; nothing to reorder")]
    NothingToOptimize(i64),
}

fn required(field: &'static str, value: &str, max: usize) -> Result<String, ValidationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::Required { field });
    }
    if trimmed.chars().count() > max {
        return Err(ValidationError::TooLong { field });
    }
    Ok(trimmed.to_owned())
}

/// Budgets and booking costs may be zero; expenses must be positive. Both are
/// capped so a mis-scaled amount cannot overflow a trip's totals.
fn amount_in_range(amount: i64, minimum: i64) -> Result<(), TripError> {
    if (minimum..=MAX_AMOUNT_MINOR).contains(&amount) {
        Ok(())
    } else {
        Err(TripError::InvalidAmount)
    }
}

fn currency(value: Option<&str>) -> Result<String, TripError> {
    let code = value.unwrap_or("USD").trim().to_ascii_uppercase();
    if code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_uppercase()) {
        Ok(code)
    } else {
        Err(TripError::InvalidCurrency)
    }
}

fn clock(value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    match chrono::NaiveTime::parse_from_str(&value, "%H:%M") {
        Ok(time) if time.format("%H:%M").to_string() == value => Ok(Some(value)),
        _ => Err(TripError::InvalidTime.into()),
    }
}

fn date_time(value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let valid = parse_date(&value).is_ok()
        || chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%dT%H:%M")
            .is_ok_and(|parsed| parsed.format("%Y-%m-%dT%H:%M").to_string() == value);
    if valid {
        Ok(Some(value))
    } else {
        Err(TripError::InvalidDateTime.into())
    }
}

/// Resolve dates into a day count. Both dates or neither.
fn day_span(start: Option<&str>, end: Option<&str>, fallback: Option<i64>) -> Result<i64> {
    match (start, end) {
        (Some(start), Some(end)) => {
            let start = parse_date(start)?;
            let end = parse_date(end)?;
            if end < start {
                return Err(TripError::DateOrder.into());
            }
            let days = (end - start).num_days() + 1;
            if days > MAX_TRIP_DAYS {
                return Err(TripError::TooLong.into());
            }
            Ok(days)
        }
        (None, None) => {
            let days = fallback.unwrap_or(DEFAULT_TRIP_DAYS);
            if !(1..=MAX_TRIP_DAYS).contains(&days) {
                return Err(TripError::TooLong.into());
            }
            Ok(days)
        }
        _ => Err(TripError::HalfDates.into()),
    }
}

fn clean(value: Option<String>, field: &'static str, max: usize) -> Result<Option<String>> {
    optional_text(field, value, max).map_err(Into::into)
}

// ── Geometry ──────────────────────────────────────────────────────────────

pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lng1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lng2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lng2 - lng1) / 2.0).sin().powi(2);
    2.0 * 6_371.0 * h.sqrt().asin()
}

pub fn path_km(points: &[(f64, f64)]) -> f64 {
    points
        .windows(2)
        .map(|pair| haversine_km(pair[0], pair[1]))
        .sum()
}

/// Shortest open path that starts at the first point: nearest neighbour, then
/// 2-opt. Returns the visiting order as indices into `points`. The first stop
/// stays first because it is usually where the day starts (the hotel).
pub fn optimize_order(points: &[(f64, f64)]) -> Vec<usize> {
    if points.len() < 3 {
        return (0..points.len()).collect();
    }
    let mut order = vec![0];
    let mut left: Vec<usize> = (1..points.len()).collect();
    while !left.is_empty() {
        let last = points[*order.last().unwrap_or(&0)];
        let (best, _) = left
            .iter()
            .enumerate()
            .map(|(slot, &index)| (slot, haversine_km(last, points[index])))
            .fold(
                (0, f64::INFINITY),
                |acc, item| if item.1 < acc.1 { item } else { acc },
            );
        order.push(left.remove(best));
    }
    let length = |order: &[usize]| path_km(&order.iter().map(|&i| points[i]).collect::<Vec<_>>());
    let mut best = length(&order);
    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 50 {
        improved = false;
        rounds += 1;
        for i in 1..order.len() - 1 {
            for j in i + 1..order.len() {
                let mut candidate = order.clone();
                candidate[i..=j].reverse();
                let next = length(&candidate);
                if next + 1e-9 < best {
                    order = candidate;
                    best = next;
                    improved = true;
                }
            }
        }
    }
    order
}

// ── Money ─────────────────────────────────────────────────────────────────

/// Net position per member per currency. Each share is the integer split of
/// the amount; the remainder cents go to the first members in the split list so
/// shares always add back up to the amount exactly.
pub fn balances(expenses: &[Expense]) -> BTreeMap<(String, String), i64> {
    let mut net: BTreeMap<(String, String), i64> = BTreeMap::new();
    for expense in expenses {
        let paid = net
            .entry((expense.currency.clone(), expense.paid_by.clone()))
            .or_default();
        *paid = paid.saturating_add(expense.amount_minor);
        let count = i64::try_from(expense.split_between.len())
            .unwrap_or(1)
            .max(1);
        let share = expense.amount_minor / count;
        let remainder = expense.amount_minor % count;
        for (index, member) in expense.split_between.iter().enumerate() {
            let extra = i64::from(i64::try_from(index).unwrap_or(i64::MAX) < remainder);
            let owed = net
                .entry((expense.currency.clone(), member.clone()))
                .or_default();
            *owed = owed.saturating_sub(share.saturating_add(extra));
        }
    }
    net
}

/// Fewest-ish transfers that settle every balance: repeatedly pay the largest
/// creditor from the largest debtor, per currency.
pub fn settle(net: &BTreeMap<(String, String), i64>) -> Vec<(String, String, String, i64)> {
    let mut by_currency: BTreeMap<&str, Vec<(String, i64)>> = BTreeMap::new();
    for ((currency, member), amount) in net {
        if *amount != 0 {
            by_currency
                .entry(currency)
                .or_default()
                .push((member.clone(), *amount));
        }
    }
    let mut transfers = Vec::new();
    for (currency, mut people) in by_currency {
        loop {
            people.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            let (Some(debtor), Some(creditor)) = (people.first().cloned(), people.last().cloned())
            else {
                break;
            };
            if debtor.1 >= 0 || creditor.1 <= 0 {
                break;
            }
            let amount = debtor.1.saturating_neg().min(creditor.1);
            transfers.push((
                currency.to_owned(),
                debtor.0.clone(),
                creditor.0.clone(),
                amount,
            ));
            let last = people.len() - 1;
            people[0].1 += amount;
            people[last].1 -= amount;
        }
    }
    transfers
}

// ── Store ─────────────────────────────────────────────────────────────────

const TRIP_COLUMNS: &str = "id, ryu_open('trips.name', id, name) AS name, ryu_open('trips.destination', id, destination) AS destination, start_date, end_date, day_count, emoji, ryu_open('trips.note', id, note) AS note, budget_minor, currency, created_by, created_at, updated_at";
const STOP_COLUMNS: &str = "id, trip_id, day, position, place_id, ryu_open('trip_stops.name', id, name) AS name, lat, lng, category, start_time, ryu_open('trip_stops.note', id, note) AS note, created_by, created_at";
/// Raw column lists, for writes.
const TRIP_INSERT_COLUMNS: &str = "id, name, destination, start_date, end_date, day_count, emoji, note, budget_minor, currency, created_by, created_at, updated_at";
const STOP_INSERT_COLUMNS: &str = "id, trip_id, day, position, place_id, name, lat, lng, category, start_time, note, created_by, created_at";

fn row_to_trip(row: &Row<'_>) -> rusqlite::Result<Trip> {
    Ok(Trip {
        id: row.get(0)?,
        name: opened(row, 1)?,
        destination: opened_opt(row, 2)?,
        start_date: row.get(3)?,
        end_date: row.get(4)?,
        day_count: row.get(5)?,
        emoji: row.get(6)?,
        note: opened_opt(row, 7)?,
        budget_minor: row.get(8)?,
        currency: row.get(9)?,
        created_by: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn row_to_stop(row: &Row<'_>) -> rusqlite::Result<Stop> {
    Ok(Stop {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        day: row.get(2)?,
        position: row.get(3)?,
        place_id: row.get(4)?,
        name: opened(row, 5)?,
        lat: row.get(6)?,
        lng: row.get(7)?,
        category: row.get(8)?,
        start_time: row.get(9)?,
        note: opened_opt(row, 10)?,
        created_by: row.get(11)?,
        created_at: row.get(12)?,
    })
}

fn get_trip(conn: &Connection, id: &str) -> Result<Trip> {
    conn.query_row(
        &format!("SELECT {TRIP_COLUMNS} FROM trips WHERE id = ?"),
        params![id],
        row_to_trip,
    )
    .optional()?
    .ok_or_else(|| NotFound::Trip.into())
}

fn touch(conn: &Connection, trip_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE trips SET updated_at = ? WHERE id = ?",
        params![now_ms(), trip_id],
    )?;
    Ok(())
}

fn stops_in(conn: &Connection, trip_id: &str, day: Option<i64>) -> Result<Vec<Stop>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {STOP_COLUMNS} FROM trip_stops WHERE trip_id = ? AND day IS ? ORDER BY position, created_at"
    ))?;
    let rows = statement.query_map(params![trip_id, day], row_to_stop)?;
    collect_readable(rows).map_err(Into::into)
}

/// Write `ids` as the bucket's order, 0..n.
fn renumber(conn: &Connection, ids: &[String], day: Option<i64>) -> Result<()> {
    for (position, id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE trip_stops SET day = ?, position = ? WHERE id = ?",
            params![day, i64::try_from(position).unwrap_or(i64::MAX), id],
        )?;
    }
    Ok(())
}

fn members(conn: &Connection) -> Result<Vec<Member>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {MEMBER_COLUMNS} FROM members ORDER BY joined_at, id"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok(Member {
            id: row.get(0)?,
            name: opened(row, 1)?,
            color: row.get(2)?,
            joined_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    collect_readable(rows).map_err(Into::into)
}

fn detail(conn: &Connection, id: &str) -> Result<TripDetail> {
    let trip = get_trip(conn, id)?;
    let start = trip
        .start_date
        .as_deref()
        .and_then(|date| parse_date(date).ok());
    let mut days = Vec::new();
    for day in 1..=trip.day_count {
        let stops = stops_in(conn, id, Some(day))?;
        let distance_km = path_km(
            &stops
                .iter()
                .map(|stop| (stop.lat, stop.lng))
                .collect::<Vec<_>>(),
        );
        days.push(TripDay {
            day,
            date: start.map(|start: NaiveDate| {
                (start + chrono::Days::new(u64::try_from(day - 1).unwrap_or(0)))
                    .format("%Y-%m-%d")
                    .to_string()
            }),
            stops,
            distance_km: (distance_km * 10.0).round() / 10.0,
        });
    }
    let ideas = stops_in(conn, id, None)?;

    let mut statement = conn.prepare(
        "SELECT id, trip_id, kind, ryu_open('trip_reservations.title', id, title) AS title, starts_at, ends_at, ryu_open('trip_reservations.location', id, location) AS location, ryu_open('trip_reservations.confirmation', id, confirmation) AS confirmation, ryu_open('trip_reservations.note', id, note) AS note, cost_minor, currency, created_by, created_at
         FROM trip_reservations WHERE trip_id = ? ORDER BY starts_at IS NULL, starts_at, created_at",
    )?;
    let reservations = collect_readable(statement.query_map(params![id], |row| {
        Ok(Reservation {
            id: row.get(0)?,
            trip_id: row.get(1)?,
            kind: row.get(2)?,
            title: opened(row, 3)?,
            starts_at: row.get(4)?,
            ends_at: row.get(5)?,
            location: opened_opt(row, 6)?,
            confirmation: opened_opt(row, 7)?,
            note: opened_opt(row, 8)?,
            cost_minor: row.get(9)?,
            currency: row.get(10)?,
            created_by: row.get(11)?,
            created_at: row.get(12)?,
        })
    })?)?;

    let mut statement = conn.prepare(
        "SELECT id, trip_id, ryu_open('trip_expenses.title', id, title) AS title, amount_minor, currency, paid_by, split_between, spent_on, category, created_by, created_at
         FROM trip_expenses WHERE trip_id = ? ORDER BY spent_on IS NULL, spent_on DESC, created_at DESC",
    )?;
    let expenses = collect_readable(statement.query_map(params![id], |row| {
        let split: String = row.get(6)?;
        Ok(Expense {
            id: row.get(0)?,
            trip_id: row.get(1)?,
            title: opened(row, 2)?,
            amount_minor: row.get(3)?,
            currency: row.get(4)?,
            paid_by: row.get(5)?,
            split_between: serde_json::from_str(&split).unwrap_or_default(),
            spent_on: row.get(7)?,
            category: row.get(8)?,
            created_by: row.get(9)?,
            created_at: row.get(10)?,
        })
    })?)?;

    let mut statement = conn.prepare(
        "SELECT id, trip_id, ryu_open('trip_checklist.text', id, text) AS text, assignee, done, position, created_by, updated_at
         FROM trip_checklist WHERE trip_id = ? ORDER BY done, position",
    )?;
    let checklist = collect_readable(statement.query_map(params![id], |row| {
        Ok(ChecklistItem {
            id: row.get(0)?,
            trip_id: row.get(1)?,
            text: opened(row, 2)?,
            assignee: row.get(3)?,
            done: row.get::<_, i64>(4)? != 0,
            position: row.get(5)?,
            created_by: row.get(6)?,
            updated_at: row.get(7)?,
        })
    })?)?;

    let members = members(conn)?;
    let names: HashMap<&str, &str> = members
        .iter()
        .map(|m| (m.id.as_str(), m.name.as_str()))
        .collect();
    let name_of = |id: &str| {
        names
            .get(id)
            .map_or_else(|| id.to_owned(), |name| (*name).to_owned())
    };

    let mut totals: BTreeMap<String, CurrencyTotal> = BTreeMap::new();
    for expense in &expenses {
        let total = totals
            .entry(expense.currency.clone())
            .or_insert_with(|| CurrencyTotal {
                currency: expense.currency.clone(),
                spent_minor: 0,
                booked_minor: 0,
                budget_minor: None,
            });
        total.spent_minor = total.spent_minor.saturating_add(expense.amount_minor);
    }
    for reservation in &reservations {
        if let Some(cost) = reservation.cost_minor {
            let total = totals
                .entry(reservation.currency.clone())
                .or_insert_with(|| CurrencyTotal {
                    currency: reservation.currency.clone(),
                    spent_minor: 0,
                    booked_minor: 0,
                    budget_minor: None,
                });
            total.booked_minor = total.booked_minor.saturating_add(cost);
        }
    }
    if let Some(budget) = trip.budget_minor {
        totals
            .entry(trip.currency.clone())
            .or_insert_with(|| CurrencyTotal {
                currency: trip.currency.clone(),
                spent_minor: 0,
                booked_minor: 0,
                budget_minor: None,
            })
            .budget_minor = Some(budget);
    }
    let net = balances(&expenses);
    let budget = Budget {
        totals: totals.into_values().collect(),
        balances: net
            .iter()
            .filter(|(_, amount)| **amount != 0)
            .map(|((currency, member), amount)| Balance {
                currency: currency.clone(),
                member_id: member.clone(),
                member_name: name_of(member),
                net_minor: *amount,
            })
            .collect(),
        settlements: settle(&net)
            .into_iter()
            .map(|(currency, from, to, amount_minor)| Settlement {
                currency,
                from_name: name_of(&from),
                to_name: name_of(&to),
                from,
                to,
                amount_minor,
            })
            .collect(),
    };

    Ok(TripDetail {
        trip,
        // The people who can pay for or share a cost; the agent is not one.
        members: members
            .into_iter()
            .filter(|member| member.id != AGENT_MEMBER_ID)
            .collect(),
        days,
        ideas,
        reservations,
        expenses,
        budget,
        checklist,
    })
}

fn insert_stop(conn: &Connection, actor: &str, trip: &Trip, input: NewStop) -> Result<String> {
    if input
        .day
        .is_some_and(|day| !(1..=trip.day_count).contains(&day))
    {
        return Err(TripError::DayOutOfRange.into());
    }
    let linked = input
        .place_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let (place_id, name, lat, lng, category) = if let Some(place_id) = linked {
        let place = get_place_for_link(conn, place_id)?.ok_or(NotFound::Place)?;
        let name = match input.name.as_deref() {
            Some(name) if !name.trim().is_empty() => required("name", name, 160)?,
            _ => place.name,
        };
        (Some(place.id), name, place.lat, place.lng, place.category)
    } else {
        let (Some(name), Some(lat), Some(lng)) = (input.name.as_deref(), input.lat, input.lng)
        else {
            return Err(TripError::StopNeedsLocation.into());
        };
        validate_coordinates(lat, lng)?;
        let category = normalize_category(input.category.as_deref().unwrap_or("other"))?;
        (None, required("name", name, 160)?, lat, lng, category)
    };
    let position = stops_in(conn, &trip.id, input.day)?.len();
    conn.execute(
        &format!("INSERT INTO trip_stops ({STOP_INSERT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ryu_seal('trip_stops.name', ?1, ?6), ?7, ?8, ?9, ?10, ryu_seal('trip_stops.note', ?1, ?11), ?12, ?13)"),
        params![
            new_id(),
            trip.id,
            input.day,
            i64::try_from(position).unwrap_or(i64::MAX),
            place_id,
            name,
            lat,
            lng,
            category,
            clock(input.start_time)?,
            clean(input.note, "note", MAX_TEXT_LEN)?,
            actor,
            now_ms(),
        ],
    )?;
    Ok(name)
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

impl PlaceStore {
    pub async fn trips(&self) -> Result<Vec<TripSummary>> {
        let conn = self.conn.lock().await;
        let mut statement = conn.prepare(&format!(
            "SELECT {TRIP_COLUMNS}, (SELECT COUNT(*) FROM trip_stops s WHERE s.trip_id = trips.id)
             FROM trips ORDER BY start_date IS NULL, start_date DESC, updated_at DESC"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok(TripSummary {
                trip: row_to_trip(row)?,
                stop_count: row.get(13)?,
            })
        })?;
        collect_readable(rows).map_err(Into::into)
    }

    pub async fn trip(&self, id: &str) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        detail(&conn, id)
    }

    pub async fn create_trip(&self, actor: &str, input: NewTrip) -> Result<TripDetail> {
        let name = required("name", &input.name, MAX_NAME_LEN)?;
        let start = clean(input.start_date, "startDate", 10)?;
        let end = clean(input.end_date, "endDate", 10)?;
        let day_count = day_span(start.as_deref(), end.as_deref(), input.day_count)?;
        let currency = currency(input.currency.as_deref())?;
        if let Some(budget) = input.budget_minor {
            amount_in_range(budget, 0)?;
        }
        let id = new_id();
        let now = now_ms();
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        conn.execute(
            &format!(
                "INSERT INTO trips ({TRIP_INSERT_COLUMNS}) VALUES (?1, ryu_seal('trips.name', ?1, ?2), ryu_seal('trips.destination', ?1, ?3), ?4, ?5, ?6, ?7, ryu_seal('trips.note', ?1, ?8), ?9, ?10, ?11, ?12, ?13)"
            ),
            params![
                id,
                name,
                clean(input.destination, "destination", MAX_SHORT_LEN)?,
                start,
                end,
                day_count,
                clean(input.emoji, "emoji", 8)?,
                clean(input.note, "note", MAX_TEXT_LEN)?,
                input.budget_minor,
                currency,
                actor,
                now,
                now,
            ],
        )?;
        log_locked(&conn, actor, "trip_created", None, None, Some(&name))?;
        detail(&conn, &id)
    }

    pub async fn update_trip(&self, actor: &str, id: &str, patch: TripPatch) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, id)?;
        let name = match patch.name.as_deref() {
            Some(name) => required("name", name, MAX_NAME_LEN)?,
            None => trip.name,
        };
        let start = match patch.start_date {
            Some(value) => clean(value, "startDate", 10)?,
            None => trip.start_date,
        };
        let end = match patch.end_date {
            Some(value) => clean(value, "endDate", 10)?,
            None => trip.end_date,
        };
        let day_count = day_span(
            start.as_deref(),
            end.as_deref(),
            Some(patch.day_count.unwrap_or(trip.day_count)),
        )?;
        let currency = match patch.currency.as_deref() {
            Some(code) => currency(Some(code))?,
            None => trip.currency,
        };
        let budget = patch.budget_minor.unwrap_or(trip.budget_minor);
        if let Some(value) = budget {
            amount_in_range(value, 0)?;
        }
        conn.execute(
            "UPDATE trips SET name = ryu_seal('trips.name', ?11, ?1), destination = ryu_seal('trips.destination', ?11, ?2), start_date = ?3, end_date = ?4, day_count = ?5, emoji = ?6,
             note = ryu_seal('trips.note', ?11, ?7), budget_minor = ?8, currency = ?9, updated_at = ?10 WHERE id = ?11",
            params![
                name,
                match patch.destination { Some(v) => clean(v, "destination", MAX_SHORT_LEN)?, None => trip.destination },
                start,
                end,
                day_count,
                match patch.emoji { Some(v) => clean(v, "emoji", 8)?, None => trip.emoji },
                match patch.note { Some(v) => clean(v, "note", MAX_TEXT_LEN)?, None => trip.note },
                budget,
                currency,
                now_ms(),
                id,
            ],
        )?;
        // Shortening a trip never deletes plans: stops past the end go to ideas.
        conn.execute(
            "UPDATE trip_stops SET day = NULL, position = 100000 + position WHERE trip_id = ? AND day > ?",
            params![id, day_count],
        )?;
        let ideas: Vec<String> = stops_in(&conn, id, None)?
            .into_iter()
            .map(|stop| stop.id)
            .collect();
        renumber(&conn, &ideas, None)?;
        detail(&conn, id)
    }

    pub async fn delete_trip(&self, actor: &str, id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, id)?;
        for table in [
            "trip_stops",
            "trip_reservations",
            "trip_expenses",
            "trip_checklist",
        ] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE trip_id = ?"),
                params![id],
            )?;
        }
        conn.execute("DELETE FROM trips WHERE id = ?", params![id])?;
        log_locked(&conn, actor, "trip_deleted", None, None, Some(&trip.name))?;
        Ok(())
    }

    pub async fn add_stop(&self, actor: &str, trip_id: &str, input: NewStop) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let name = insert_stop(&conn, actor, &trip, input)?;
        touch(&conn, trip_id)?;
        log_locked(
            &conn,
            actor,
            "stop_added",
            None,
            Some(&name),
            Some(&trip.name),
        )?;
        detail(&conn, trip_id)
    }

    /// Add several stops at once (an accepted AI plan or an import). All or
    /// nothing: one invalid stop leaves the itinerary unchanged.
    pub async fn add_stops(
        &self,
        actor: &str,
        trip_id: &str,
        inputs: Vec<NewStop>,
    ) -> Result<TripDetail> {
        if inputs.is_empty() || inputs.len() > MAX_BATCH_STOPS {
            return Err(TripError::BatchSize.into());
        }
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let tx = conn.unchecked_transaction()?;
        let count = inputs.len();
        for input in inputs {
            insert_stop(&tx, actor, &trip, input)?;
        }
        tx.commit()?;
        touch(&conn, trip_id)?;
        log_locked(
            &conn,
            actor,
            "stop_added",
            None,
            Some(&format!("{count} stops")),
            Some(&trip.name),
        )?;
        detail(&conn, trip_id)
    }

    pub async fn update_stop(
        &self,
        actor: &str,
        trip_id: &str,
        stop_id: &str,
        patch: StopPatch,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let stop = conn
            .query_row(
                &format!("SELECT {STOP_COLUMNS} FROM trip_stops WHERE id = ? AND trip_id = ?"),
                params![stop_id, trip_id],
                row_to_stop,
            )
            .optional()?
            .ok_or(NotFound::TripItem)?;
        let name = match patch.name.as_deref() {
            Some(name) => required("name", name, 160)?,
            None => stop.name.clone(),
        };
        let start_time = match patch.start_time {
            Some(value) => clock(value)?,
            None => stop.start_time.clone(),
        };
        let note = match patch.note {
            Some(value) => clean(value, "note", MAX_TEXT_LEN)?,
            None => stop.note.clone(),
        };
        // Validate the whole patch before writing any of it, so a rejected
        // move never leaves a half-applied rename or note behind.
        let moves = patch.day.is_some() || patch.position.is_some();
        let target = patch.day.unwrap_or(stop.day);
        if moves && target.is_some_and(|day| !(1..=trip.day_count).contains(&day)) {
            return Err(TripError::DayOutOfRange.into());
        }
        conn.execute(
            "UPDATE trip_stops SET name = ryu_seal('trip_stops.name', ?4, ?1), start_time = ?2, note = ryu_seal('trip_stops.note', ?4, ?3) WHERE id = ?4",
            params![name, start_time, note, stop_id],
        )?;
        if moves {
            let mut source: Vec<String> = stops_in(&conn, trip_id, stop.day)?
                .into_iter()
                .map(|s| s.id)
                .filter(|id| id != stop_id)
                .collect();
            if target == stop.day {
                let index = usize::try_from(patch.position.unwrap_or(stop.position).max(0))
                    .unwrap_or(0)
                    .min(source.len());
                source.insert(index, stop_id.to_owned());
                renumber(&conn, &source, target)?;
            } else {
                renumber(&conn, &source, stop.day)?;
                let mut destination: Vec<String> = stops_in(&conn, trip_id, target)?
                    .into_iter()
                    .map(|s| s.id)
                    .collect();
                let index = usize::try_from(patch.position.unwrap_or(i64::MAX).max(0))
                    .unwrap_or(usize::MAX)
                    .min(destination.len());
                destination.insert(index, stop_id.to_owned());
                renumber(&conn, &destination, target)?;
            }
        }
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }

    pub async fn remove_stop(
        &self,
        actor: &str,
        trip_id: &str,
        stop_id: &str,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        get_trip(&conn, trip_id)?;
        let day: Option<Option<i64>> = conn
            .query_row(
                "SELECT day FROM trip_stops WHERE id = ? AND trip_id = ?",
                params![stop_id, trip_id],
                |row| row.get(0),
            )
            .optional()?;
        let day = day.ok_or(NotFound::TripItem)?;
        conn.execute("DELETE FROM trip_stops WHERE id = ?", params![stop_id])?;
        let rest: Vec<String> = stops_in(&conn, trip_id, day)?
            .into_iter()
            .map(|s| s.id)
            .collect();
        renumber(&conn, &rest, day)?;
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }

    /// Reorder one day's stops for the shortest walk, keeping the first stop.
    pub async fn optimize_day(&self, actor: &str, trip_id: &str, day: i64) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        if !(1..=trip.day_count).contains(&day) {
            return Err(TripError::DayOutOfRange.into());
        }
        let stops = stops_in(&conn, trip_id, Some(day))?;
        if stops.len() < 3 {
            return Err(TripError::NothingToOptimize(day).into());
        }
        let points: Vec<(f64, f64)> = stops.iter().map(|stop| (stop.lat, stop.lng)).collect();
        let order: Vec<String> = optimize_order(&points)
            .into_iter()
            .map(|index| stops[index].id.clone())
            .collect();
        renumber(&conn, &order, Some(day))?;
        touch(&conn, trip_id)?;
        log_locked(
            &conn,
            actor,
            "route_optimized",
            None,
            Some(&format!("day {day}")),
            Some(&trip.name),
        )?;
        detail(&conn, trip_id)
    }

    pub async fn add_reservation(
        &self,
        actor: &str,
        trip_id: &str,
        input: NewReservation,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let reservation = normalized_reservation(input, &trip.currency)?;
        conn.execute(
            "INSERT INTO trip_reservations (id, trip_id, kind, title, starts_at, ends_at, location, confirmation, note, cost_minor, currency, created_by, created_at)
             VALUES (?1, ?2, ?3, ryu_seal('trip_reservations.title', ?1, ?4), ?5, ?6, ryu_seal('trip_reservations.location', ?1, ?7), ryu_seal('trip_reservations.confirmation', ?1, ?8), ryu_seal('trip_reservations.note', ?1, ?9), ?10, ?11, ?12, ?13)",
            params![
                new_id(), trip_id, reservation.kind, reservation.title, reservation.starts_at, reservation.ends_at,
                reservation.location, reservation.confirmation, reservation.note, reservation.cost_minor,
                reservation.currency, actor, now_ms(),
            ],
        )?;
        touch(&conn, trip_id)?;
        log_locked(
            &conn,
            actor,
            "booking_added",
            None,
            Some(&reservation.title),
            Some(&trip.name),
        )?;
        detail(&conn, trip_id)
    }

    pub async fn update_reservation(
        &self,
        actor: &str,
        trip_id: &str,
        item: &str,
        input: NewReservation,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let reservation = normalized_reservation(input, &trip.currency)?;
        let changed = conn.execute(
            "UPDATE trip_reservations SET kind = ?1, title = ryu_seal('trip_reservations.title', ?10, ?2), starts_at = ?3, ends_at = ?4, location = ryu_seal('trip_reservations.location', ?10, ?5), confirmation = ryu_seal('trip_reservations.confirmation', ?10, ?6),
             note = ryu_seal('trip_reservations.note', ?10, ?7), cost_minor = ?8, currency = ?9 WHERE id = ?10 AND trip_id = ?11",
            params![
                reservation.kind, reservation.title, reservation.starts_at, reservation.ends_at, reservation.location,
                reservation.confirmation, reservation.note, reservation.cost_minor, reservation.currency, item, trip_id,
            ],
        )?;
        if changed == 0 {
            return Err(NotFound::TripItem.into());
        }
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }

    pub async fn add_expense(
        &self,
        actor: &str,
        trip_id: &str,
        input: NewExpense,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        let trip = get_trip(&conn, trip_id)?;
        let title = required("title", &input.title, MAX_SHORT_LEN)?;
        amount_in_range(input.amount_minor, 1)?;
        let currency = currency(Some(input.currency.as_deref().unwrap_or(&trip.currency)))?;
        // The agent member is not a person: it never pays or shares a cost, so
        // an agent-written expense must name who paid.
        let known: Vec<String> = members(&conn)?
            .into_iter()
            .map(|member| member.id)
            .filter(|id| id != AGENT_MEMBER_ID)
            .collect();
        let paid_by = input
            .paid_by
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| actor.to_owned());
        let mut split: Vec<String> = input
            .split_between
            .into_iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        // Charge each member once, keeping first-mention order.
        let mut seen = HashSet::new();
        split.retain(|id| seen.insert(id.clone()));
        if split.is_empty() {
            split.clone_from(&known);
        }
        if !known.contains(&paid_by) || split.iter().any(|id| !known.contains(id)) {
            return Err(TripError::UnknownMember.into());
        }
        let spent_on = match clean(input.spent_on, "spentOn", 10)? {
            Some(date) => Some(parse_date(&date).map(|_| date)?),
            None => None,
        };
        let category = input
            .category
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "other".to_owned());
        let category = if CATEGORIES.contains(&category.as_str())
            || ["transport", "tickets", "groceries"].contains(&category.as_str())
        {
            category
        } else {
            "other".to_owned()
        };
        conn.execute(
            "INSERT INTO trip_expenses (id, trip_id, title, amount_minor, currency, paid_by, split_between, spent_on, category, created_by, created_at)
             VALUES (?1, ?2, ryu_seal('trip_expenses.title', ?1, ?3), ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new_id(), trip_id, title, input.amount_minor, currency, paid_by,
                serde_json::to_string(&split)?, spent_on, category, actor, now_ms(),
            ],
        )?;
        touch(&conn, trip_id)?;
        log_locked(
            &conn,
            actor,
            "expense_added",
            None,
            Some(&title),
            Some(&trip.name),
        )?;
        detail(&conn, trip_id)
    }

    pub async fn add_checklist_item(
        &self,
        actor: &str,
        trip_id: &str,
        input: NewChecklistItem,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        get_trip(&conn, trip_id)?;
        let text = required("text", &input.text, MAX_SHORT_LEN)?;
        let position: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM trip_checklist WHERE trip_id = ?",
            params![trip_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO trip_checklist (id, trip_id, text, assignee, done, position, created_by, updated_at)
             VALUES (?1, ?2, ryu_seal('trip_checklist.text', ?1, ?3), ?4, 0, ?5, ?6, ?7)",
            params![new_id(), trip_id, text, clean(input.assignee, "assignee", 128)?, position, actor, now_ms()],
        )?;
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }

    pub async fn update_checklist_item(
        &self,
        actor: &str,
        trip_id: &str,
        item: &str,
        patch: ChecklistPatch,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        get_trip(&conn, trip_id)?;
        let current: Option<(String, Option<String>, i64)> = conn
            .query_row(
                "SELECT ryu_open('trip_checklist.text', id, text), assignee, done FROM trip_checklist WHERE id = ? AND trip_id = ?",
                params![item, trip_id],
                |row| Ok((opened(row, 0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (text, assignee, done) = current.ok_or(NotFound::TripItem)?;
        let text = match patch.text.as_deref() {
            Some(text) => required("text", text, MAX_SHORT_LEN)?,
            None => text,
        };
        let assignee = match patch.assignee {
            Some(value) => clean(value, "assignee", 128)?,
            None => assignee,
        };
        let done = patch.done.map_or(done, i64::from);
        conn.execute(
            "UPDATE trip_checklist SET text = ryu_seal('trip_checklist.text', ?5, ?1), assignee = ?2, done = ?3, updated_at = ?4 WHERE id = ?5",
            params![text, assignee, done, now_ms(), item],
        )?;
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }

    /// Delete a reservation, expense, or checklist item.
    pub async fn remove_trip_item(
        &self,
        actor: &str,
        trip_id: &str,
        kind: TripItemKind,
        item: &str,
    ) -> Result<TripDetail> {
        let conn = self.conn.lock().await;
        ensure_member_locked(&conn, actor)?;
        get_trip(&conn, trip_id)?;
        let table = match kind {
            TripItemKind::Reservation => "trip_reservations",
            TripItemKind::Expense => "trip_expenses",
            TripItemKind::Checklist => "trip_checklist",
        };
        let changed = conn.execute(
            &format!("DELETE FROM {table} WHERE id = ? AND trip_id = ?"),
            params![item, trip_id],
        )?;
        if changed == 0 {
            return Err(NotFound::TripItem.into());
        }
        touch(&conn, trip_id)?;
        detail(&conn, trip_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripItemKind {
    Reservation,
    Expense,
    Checklist,
}

fn normalized_reservation(input: NewReservation, trip_currency: &str) -> Result<NewReservation> {
    let kind = input
        .kind
        .as_deref()
        .unwrap_or("other")
        .trim()
        .to_ascii_lowercase();
    if !RESERVATION_KINDS.contains(&kind.as_str()) {
        return Err(TripError::InvalidKind.into());
    }
    if let Some(cost) = input.cost_minor {
        amount_in_range(cost, 0)?;
    }
    let starts_at = date_time(input.starts_at)?;
    let ends_at = date_time(input.ends_at)?;
    if let (Some(start), Some(end)) = (starts_at.as_deref(), ends_at.as_deref()) {
        if end < start {
            return Err(TripError::DateOrder.into());
        }
    }
    Ok(NewReservation {
        kind: Some(kind),
        title: required("title", &input.title, MAX_SHORT_LEN)?,
        starts_at,
        ends_at,
        location: clean(input.location, "location", MAX_SHORT_LEN)?,
        confirmation: clean(input.confirmation, "confirmation", 80)?,
        note: clean(input.note, "note", MAX_TEXT_LEN)?,
        cost_minor: input.cost_minor,
        currency: Some(currency(Some(
            input.currency.as_deref().unwrap_or(trip_currency),
        ))?),
    })
}

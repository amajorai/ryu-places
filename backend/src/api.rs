use axum::{
    async_trait,
    extract::{FromRequestParts, Path, Query, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    geocode::SearchError,
    models::{ListPatch, MarkInput, MemberPatch, NewList, NewPlace, PlaceFilters, PlacePatch},
    state::AppState,
    store::{NotFound, PlaceStore},
    tenancy::{caller_from_headers, AgentLaneError, Caller, TenantRequired},
    trips::{
        ChecklistPatch, NewChecklistItem, NewExpense, NewReservation, NewStop, NewTrip, StopPatch,
        TripError, TripItemKind, TripPatch,
    },
    validation::ValidationError,
};

pub use crate::tenancy::{CALLER_ORG_ID_HEADER, CALLER_USER_ID_HEADER};

pub const SERVED_ROUTES: &[&str] = &[
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
];

/// Exports can be large (a Takeout file with hundreds of places); the proxy caps
/// bodies at 10 MiB, so allow up to that on the file import route only.
const MAX_IMPORT_BODY: usize = 10 * 1024 * 1024;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn from_error(error: anyhow::Error) -> Self {
        let message = error.to_string();
        let status = if error.downcast_ref::<ValidationError>().is_some()
            || error.downcast_ref::<TripError>().is_some()
            || error.downcast_ref::<crate::link::LinkError>().is_some()
            || error
                .downcast_ref::<crate::importers::ImportError>()
                .is_some()
        {
            StatusCode::UNPROCESSABLE_ENTITY
        } else if error.downcast_ref::<TenantRequired>().is_some() {
            StatusCode::FORBIDDEN
        } else if error.downcast_ref::<AgentLaneError>().is_some() {
            StatusCode::CONFLICT
        } else if error.downcast_ref::<crate::host::NoHost>().is_some() {
            StatusCode::SERVICE_UNAVAILABLE
        } else if error.downcast_ref::<NotFound>().is_some() {
            StatusCode::NOT_FOUND
        } else if let Some(search) = error.downcast_ref::<SearchError>() {
            match search {
                SearchError::Disabled => StatusCode::SERVICE_UNAVAILABLE,
                SearchError::EmptyQuery | SearchError::QueryTooLong => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
            }
        } else if message.starts_with("place search") {
            tracing::warn!(error = %format!("{error:#}"), "place search failed");
            StatusCode::BAD_GATEWAY
        } else {
            tracing::error!(error = %format!("{error:#}"), "places request failed");
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self { status, message }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::from_error(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn to_json<T: Serialize>(value: T) -> ApiResult {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|error| ApiError::from_error(error.into()))
}

/// The caller and the store for the map they share.
pub struct Ctx {
    pub caller: Caller,
    pub store: PlaceStore,
}

#[async_trait]
impl FromRequestParts<AppState> for Ctx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let caller = caller_from_headers(&parts.headers, state.config.managed_node)
            .map_err(|error| ApiError::from_error(error.into()))?;
        state.stores.note_tenant(&caller.tenant).await?;
        let store = state.stores.get(&caller.tenant).await?;
        Ok(Self { caller, store })
    }
}

pub fn routes(state: AppState) -> Router<()> {
    Router::new()
        .route("/me", get(get_me).patch(update_me))
        .route("/members", get(list_members))
        .route("/agent", get(get_agent_access).put(set_agent_access))
        .route("/places", get(list_places).post(create_place))
        .route(
            "/places/:id",
            get(get_place).patch(update_place).delete(delete_place),
        )
        .route("/places/:id/mark", put(set_mark).delete(clear_mark))
        .route("/lists", get(list_lists).post(create_list))
        .route("/lists/:id", patch(update_list).delete(delete_list))
        .route("/activity", get(get_activity))
        .route("/summary", get(get_summary))
        .route("/search", get(search))
        .route("/trips", get(list_trips).post(create_trip))
        .route(
            "/trips/:id",
            get(get_trip).patch(update_trip).delete(delete_trip),
        )
        .route("/trips/:id/stops", post(add_stop))
        .route("/trips/:id/stops/batch", post(add_stops))
        .route("/trips/:id/suggest", post(suggest_trip))
        .route("/import/link", post(import_link))
        .route(
            "/import/file",
            post(import_file).layer(axum::extract::DefaultBodyLimit::max(MAX_IMPORT_BODY)),
        )
        .route("/import/save", post(import_save))
        .route(
            "/trips/:id/stops/:item",
            patch(update_stop).delete(remove_stop),
        )
        .route("/trips/:id/days/:day/optimize", post(optimize_day))
        .route("/trips/:id/reservations", post(add_reservation))
        .route(
            "/trips/:id/reservations/:item",
            patch(update_reservation).delete(remove_reservation),
        )
        .route("/trips/:id/expenses", post(add_expense))
        .route(
            "/trips/:id/expenses/:item",
            axum::routing::delete(remove_expense),
        )
        .route("/trips/:id/checklist", post(add_checklist_item))
        .route(
            "/trips/:id/checklist/:item",
            patch(update_checklist_item).delete(remove_checklist_item),
        )
        .with_state(state)
}

// ── Members ──────────────────────────────────────────────────────────────

async fn get_me(State(state): State<AppState>, ctx: Ctx) -> ApiResult {
    let member = ctx.store.ensure_member(&ctx.caller.member).await?;
    let members = ctx.store.members().await?;
    Ok(Json(json!({
        "member": member,
        "members": members,
        "shared": ctx.caller.tenant.starts_with("org:"),
        "searchEnabled": state.geocoder.enabled(),
    })))
}

async fn update_me(ctx: Ctx, Json(patch): Json<MemberPatch>) -> ApiResult {
    let member = ctx.store.update_member(&ctx.caller.member, patch).await?;
    Ok(Json(json!({ "member": member })))
}

async fn list_members(ctx: Ctx) -> ApiResult {
    Ok(Json(json!({ "members": ctx.store.members().await? })))
}

// ── Agent access ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessPatch {
    pub enabled: bool,
}

async fn get_agent_access(State(state): State<AppState>, ctx: Ctx) -> ApiResult {
    to_json(state.stores.agent_access(&ctx.caller.tenant).await?)
}

async fn set_agent_access(
    State(state): State<AppState>,
    ctx: Ctx,
    Json(patch): Json<AgentAccessPatch>,
) -> ApiResult {
    to_json(
        state
            .stores
            .set_agent_access(&ctx.caller.tenant, &ctx.caller.member, patch.enabled)
            .await?,
    )
}

// ── Places ───────────────────────────────────────────────────────────────

async fn list_places(ctx: Ctx, Query(filters): Query<PlaceFilters>) -> ApiResult {
    let places = ctx.store.list_places(&ctx.caller.member, filters).await?;
    Ok(Json(json!({ "places": places })))
}

async fn create_place(ctx: Ctx, Json(input): Json<NewPlace>) -> ApiResult {
    to_json(ctx.store.add_place(&ctx.caller.member, input).await?)
}

async fn get_place(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    to_json(ctx.store.get_place(&id).await?)
}

async fn update_place(
    ctx: Ctx,
    Path(id): Path<String>,
    Json(patch): Json<PlacePatch>,
) -> ApiResult {
    to_json(
        ctx.store
            .update_place(&ctx.caller.member, &id, patch)
            .await?,
    )
}

async fn delete_place(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    ctx.store.delete_place(&ctx.caller.member, &id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn set_mark(ctx: Ctx, Path(id): Path<String>, Json(input): Json<MarkInput>) -> ApiResult {
    to_json(ctx.store.set_mark(&ctx.caller.member, &id, input).await?)
}

async fn clear_mark(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    to_json(ctx.store.clear_mark(&ctx.caller.member, &id).await?)
}

async fn list_lists(ctx: Ctx) -> ApiResult {
    Ok(Json(json!({ "lists": ctx.store.lists().await? })))
}

async fn create_list(ctx: Ctx, Json(input): Json<NewList>) -> ApiResult {
    to_json(ctx.store.create_list(&ctx.caller.member, input).await?)
}

async fn update_list(ctx: Ctx, Path(id): Path<String>, Json(patch): Json<ListPatch>) -> ApiResult {
    to_json(
        ctx.store
            .update_list(&ctx.caller.member, &id, patch)
            .await?,
    )
}

async fn delete_list(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    ctx.store.delete_list(&ctx.caller.member, &id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct ActivityQuery {
    limit: Option<i64>,
}

async fn get_activity(ctx: Ctx, Query(query): Query<ActivityQuery>) -> ApiResult {
    Ok(Json(
        json!({ "activity": ctx.store.activity(query.limit).await? }),
    ))
}

async fn get_summary(ctx: Ctx) -> ApiResult {
    Ok(Json(json!({ "summary": ctx.store.summary().await? })))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    lat: Option<f64>,
    lng: Option<f64>,
}

async fn search(State(state): State<AppState>, Query(query): Query<SearchQuery>) -> ApiResult {
    let near = query.lat.zip(query.lng);
    let results = state
        .geocoder
        .search(query.q.as_deref().unwrap_or_default(), near)
        .await?;
    Ok(Json(json!({ "results": results })))
}

// ── Trips ────────────────────────────────────────────────────────────────

async fn list_trips(ctx: Ctx) -> ApiResult {
    Ok(Json(json!({ "trips": ctx.store.trips().await? })))
}

async fn create_trip(ctx: Ctx, Json(input): Json<NewTrip>) -> ApiResult {
    to_json(ctx.store.create_trip(&ctx.caller.member, input).await?)
}

async fn get_trip(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    to_json(ctx.store.trip(&id).await?)
}

async fn update_trip(ctx: Ctx, Path(id): Path<String>, Json(patch): Json<TripPatch>) -> ApiResult {
    to_json(
        ctx.store
            .update_trip(&ctx.caller.member, &id, patch)
            .await?,
    )
}

async fn delete_trip(ctx: Ctx, Path(id): Path<String>) -> ApiResult {
    ctx.store.delete_trip(&ctx.caller.member, &id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn add_stop(ctx: Ctx, Path(id): Path<String>, Json(input): Json<NewStop>) -> ApiResult {
    to_json(ctx.store.add_stop(&ctx.caller.member, &id, input).await?)
}

async fn update_stop(
    ctx: Ctx,
    Path((id, item)): Path<(String, String)>,
    Json(patch): Json<StopPatch>,
) -> ApiResult {
    to_json(
        ctx.store
            .update_stop(&ctx.caller.member, &id, &item, patch)
            .await?,
    )
}

async fn remove_stop(ctx: Ctx, Path((id, item)): Path<(String, String)>) -> ApiResult {
    to_json(
        ctx.store
            .remove_stop(&ctx.caller.member, &id, &item)
            .await?,
    )
}

async fn optimize_day(ctx: Ctx, Path((id, day)): Path<(String, i64)>) -> ApiResult {
    to_json(ctx.store.optimize_day(&ctx.caller.member, &id, day).await?)
}

async fn add_reservation(
    ctx: Ctx,
    Path(id): Path<String>,
    Json(input): Json<NewReservation>,
) -> ApiResult {
    to_json(
        ctx.store
            .add_reservation(&ctx.caller.member, &id, input)
            .await?,
    )
}

async fn update_reservation(
    ctx: Ctx,
    Path((id, item)): Path<(String, String)>,
    Json(input): Json<NewReservation>,
) -> ApiResult {
    to_json(
        ctx.store
            .update_reservation(&ctx.caller.member, &id, &item, input)
            .await?,
    )
}

async fn remove_reservation(ctx: Ctx, Path((id, item)): Path<(String, String)>) -> ApiResult {
    to_json(
        ctx.store
            .remove_trip_item(&ctx.caller.member, &id, TripItemKind::Reservation, &item)
            .await?,
    )
}

async fn add_expense(ctx: Ctx, Path(id): Path<String>, Json(input): Json<NewExpense>) -> ApiResult {
    to_json(
        ctx.store
            .add_expense(&ctx.caller.member, &id, input)
            .await?,
    )
}

async fn remove_expense(ctx: Ctx, Path((id, item)): Path<(String, String)>) -> ApiResult {
    to_json(
        ctx.store
            .remove_trip_item(&ctx.caller.member, &id, TripItemKind::Expense, &item)
            .await?,
    )
}

async fn add_checklist_item(
    ctx: Ctx,
    Path(id): Path<String>,
    Json(input): Json<NewChecklistItem>,
) -> ApiResult {
    to_json(
        ctx.store
            .add_checklist_item(&ctx.caller.member, &id, input)
            .await?,
    )
}

async fn update_checklist_item(
    ctx: Ctx,
    Path((id, item)): Path<(String, String)>,
    Json(patch): Json<ChecklistPatch>,
) -> ApiResult {
    to_json(
        ctx.store
            .update_checklist_item(&ctx.caller.member, &id, &item, patch)
            .await?,
    )
}

async fn remove_checklist_item(ctx: Ctx, Path((id, item)): Path<(String, String)>) -> ApiResult {
    to_json(
        ctx.store
            .remove_trip_item(&ctx.caller.member, &id, TripItemKind::Checklist, &item)
            .await?,
    )
}

// ── AI and import ────────────────────────────────────────────────────────

fn model(state: &AppState) -> Result<&dyn crate::host::Model, ApiError> {
    state
        .model
        .as_deref()
        .ok_or_else(|| ApiError::from_error(crate::host::NoHost.into()))
}

fn require_search(state: &AppState) -> Result<(), ApiError> {
    if state.geocoder.enabled() {
        Ok(())
    } else {
        Err(ApiError::from_error(SearchError::Disabled.into()))
    }
}

async fn every_place(ctx: &Ctx) -> Result<Vec<crate::models::Place>, ApiError> {
    Ok(ctx
        .store
        .list_places(
            &ctx.caller.member,
            PlaceFilters {
                limit: Some(crate::store::MAX_LIMIT),
                ..Default::default()
            },
        )
        .await?)
}

#[derive(Debug, Deserialize)]
pub struct Near {
    pub lat: f64,
    pub lng: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkImport {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub near: Option<Near>,
}

/// Read a link (and/or pasted text), have the model name the places in it, and
/// geocode them. Returns candidates; nothing is saved.
pub async fn read_and_extract(
    state: &AppState,
    saved: &[crate::models::Place],
    input: LinkImport,
) -> anyhow::Result<Value> {
    let model = state.model.as_deref().ok_or(crate::host::NoHost)?;
    if !state.geocoder.enabled() {
        return Err(SearchError::Disabled.into());
    }
    let url = input
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let text = input
        .text
        .as_deref()
        .map(|t| t.trim().chars().take(8_000).collect::<String>())
        .filter(|t| !t.is_empty());
    let source = match (url, &text) {
        (Some(url), _) => match state.links.read(url).await {
            Ok(content) => content,
            // A login-walled post can still be imported from its pasted caption.
            Err(_) if text.is_some() => crate::link::LinkContent {
                url: url.to_owned(),
                provider: "web".to_owned(),
                ..Default::default()
            },
            Err(error) => return Err(error),
        },
        (None, Some(_)) => crate::link::LinkContent {
            provider: "text".to_owned(),
            ..Default::default()
        },
        (None, None) => {
            return Err(ValidationError::Required {
                field: "url or text",
            }
            .into())
        }
    };
    let mentions = crate::ai::extract_mentions(model, &source, text.as_deref()).await?;
    let source_url = (!source.url.is_empty()).then_some(source.url.as_str());
    let near = input.near.map(|n| (n.lat, n.lng));
    let resolved =
        crate::ai::resolve_mentions(&state.geocoder, mentions, saved, source_url, near).await;
    Ok(json!({
        "source": {
            "url": source_url,
            "provider": source.provider,
            "title": source.title,
            "author": source.author,
        },
        "candidates": resolved.candidates,
        "unresolved": resolved.unresolved,
    }))
}

async fn import_link(
    State(state): State<AppState>,
    ctx: Ctx,
    Json(input): Json<LinkImport>,
) -> ApiResult {
    let saved = every_place(&ctx).await?;
    Ok(Json(read_and_extract(&state, &saved, input).await?))
}

#[derive(Debug, Deserialize)]
struct FileImport {
    content: String,
    #[serde(default)]
    format: Option<crate::importers::ExportFormat>,
}

async fn import_file(
    State(state): State<AppState>,
    ctx: Ctx,
    Json(input): Json<FileImport>,
) -> ApiResult {
    let format = input
        .format
        .unwrap_or_else(|| crate::importers::sniff(&input.content));
    let rows = crate::importers::parse(format, &input.content)
        .map_err(|e| ApiError::from_error(e.into()))?;
    let total = rows.len();
    let saved = every_place(&ctx).await?;
    let resolved = crate::ai::resolve_rows(Some(&state.geocoder), rows, &saved).await;
    Ok(Json(json!({
        "total": total,
        "candidates": resolved.candidates,
        "unresolved": resolved.unresolved,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveImport {
    pub places: Vec<crate::ai::Candidate>,
    #[serde(default)]
    pub status: Option<crate::models::MarkStatus>,
    #[serde(default)]
    pub list_id: Option<String>,
}

/// Save confirmed candidates. Already-saved places only get the caller's mark.
pub async fn save_candidates(
    store: &PlaceStore,
    actor: &str,
    input: SaveImport,
) -> anyhow::Result<Value> {
    let mut saved = Vec::new();
    let mut failed = Vec::new();
    for candidate in input.places.into_iter().take(crate::importers::MAX_ROWS) {
        let name = candidate.name.clone();
        let result = match candidate.saved_place_id.as_deref() {
            Some(id) => match input.status {
                Some(status) => {
                    store
                        .set_mark(
                            actor,
                            id,
                            MarkInput {
                                status,
                                visited_on: None,
                                rating: None,
                                note: None,
                            },
                        )
                        .await
                }
                None => store.get_place(id).await,
            },
            None => {
                store
                    .add_place(
                        actor,
                        NewPlace {
                            name: candidate.name,
                            address: candidate.address,
                            lat: candidate.lat,
                            lng: candidate.lng,
                            category: candidate.category,
                            url: candidate.url,
                            note: candidate.note,
                            list_id: input.list_id.clone(),
                            external_id: candidate.external_id,
                            status: input.status,
                            visited_on: None,
                            rating: None,
                        },
                    )
                    .await
            }
        };
        match result {
            Ok(place) => saved.push(place),
            Err(error) => failed.push(json!({ "name": name, "error": error.to_string() })),
        }
    }
    Ok(json!({ "places": saved, "failed": failed }))
}

async fn import_save(ctx: Ctx, Json(input): Json<SaveImport>) -> ApiResult {
    Ok(Json(
        save_candidates(&ctx.store, &ctx.caller.member, input).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct StopBatch {
    stops: Vec<NewStop>,
}

async fn add_stops(ctx: Ctx, Path(id): Path<String>, Json(input): Json<StopBatch>) -> ApiResult {
    to_json(
        ctx.store
            .add_stops(&ctx.caller.member, &id, input.stops)
            .await?,
    )
}

#[derive(Debug, Deserialize)]
pub struct SuggestInput {
    pub prompt: String,
    /// Plan only this day; omit for every day.
    #[serde(default)]
    pub day: Option<i64>,
}

pub async fn suggest_for_trip(
    state: &AppState,
    store: &PlaceStore,
    actor: &str,
    trip_id: &str,
    input: SuggestInput,
) -> anyhow::Result<crate::ai::Suggestion> {
    let model = state.model.as_deref().ok_or(crate::host::NoHost)?;
    if !state.geocoder.enabled() {
        return Err(SearchError::Disabled.into());
    }
    let prompt: String = input.prompt.trim().chars().take(1_000).collect();
    if prompt.is_empty() {
        return Err(ValidationError::Required { field: "prompt" }.into());
    }
    let detail = store.trip(trip_id).await?;
    let saved = store
        .list_places(
            actor,
            PlaceFilters {
                limit: Some(200),
                ..Default::default()
            },
        )
        .await?;
    let days = detail
        .days
        .iter()
        .filter(|day| input.day.is_none_or(|only| only == day.day))
        .map(|day| (day.day, day.date.clone()))
        .collect::<Vec<_>>();
    if days.is_empty() {
        return Err(crate::trips::TripError::DayOutOfRange.into());
    }
    let planned = detail
        .days
        .iter()
        .flat_map(|day| {
            day.stops
                .iter()
                .map(move |stop| (day.day, stop.name.clone()))
        })
        .collect::<Vec<_>>();
    let points: Vec<(f64, f64)> = detail
        .days
        .iter()
        .flat_map(|day| day.stops.iter().map(|stop| (stop.lat, stop.lng)))
        .collect();
    let near = (!points.is_empty()).then(|| {
        let count = points.len() as f64;
        (
            points.iter().map(|p| p.0).sum::<f64>() / count,
            points.iter().map(|p| p.1).sum::<f64>() / count,
        )
    });
    let brief = crate::ai::TripBrief {
        name: &detail.trip.name,
        destination: detail.trip.destination.as_deref(),
        days,
        planned,
        saved: &saved,
    };
    crate::ai::suggest_itinerary(model, &state.geocoder, &brief, &prompt, near).await
}

async fn suggest_trip(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    Json(input): Json<SuggestInput>,
) -> ApiResult {
    model(&state)?;
    require_search(&state)?;
    to_json(suggest_for_trip(&state, &ctx.store, &ctx.caller.member, &id, input).await?)
}

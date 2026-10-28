// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/events — the caller asks for something
//! to happen in the persona's day; the engine decides whether and what. The
//! persona tells the user in a message, except for an offline kind she does
//! not announce, which is only her silence (spec
//! docs/superpowers/specs/2026-10-16-life-events-offline-and-scares-design.md).

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::events::{self, DrawInput, EventKind};
use eros_engine_core::scope::MemoryScope;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::events::EventRepo;
use eros_engine_store::life_choices::LifeChoiceRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, Occasion};
use crate::prompt::NowContext;
use crate::routes::companion::{
    image_request_payload, validate_llm_audit, validate_prompt_traits, ImageRequestPayloadResponse,
    LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_image_params, validate_tier, validated_outfit, AffinityScopeDto,
    ImageReplyParams, StreamPreErrorBody,
};
use crate::routes::session_open::GreetingDto;
use crate::state::AppState;
use eros_engine_llm::model_config::StyleKey;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const MAX_EVENT_KEY_CHARS: usize = 128;

/// A kind of life event. `taking_space` and `fell_out` are placed by the
/// engine, never drawn: they appear in responses only, and a `kinds` filter
/// that lists one is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKindDto {
    PoliceEncounter,
    AnimalEncounter,
    HitSomeone,
    SuddenDesire,
    GroceryShopping,
    Showering,
    Bathing,
    InClass,
    OnTheWay,
    PeriodScare,
    PregnancyScare,
    GotBumped,
    BumpedSomeone,
    Quarrel,
    ScoldedBySuperior,
    ScoldedSuperior,
    SawBag,
    SawClothes,
    AdmiringScenery,
    AdmiringArt,
    TakingSelfie,
    TempWork,
    ExMessage,
    WeatherChange,
    GotHit,
    Breakup,
    WorkingOut,
    Reading,
    Gaming,
    AwayFromPhone,
    Unwell,
    Irritable,
    Busy,
    InBattle,
    Asleep,
    OutShopping,
    BrosGathering,
    SisGathering,
    OnTheRoad,
    WatchingMatch,
    WatchingShow,
    HavingSex,
    Sick,
    Period,
    Hospitalized,
    PostSurgery,
    BusinessTrip,
    ShortTrip,
    LongTrip,
    HometownVisit,
    TakingSpace,
    FellOut,
}

/// Reads the drawable kinds only: a request names kinds to draw, and the
/// engine's own (`EventKind::SPECIAL`) are never drawn.
impl<'de> Deserialize<'de> for EventKindDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let name = String::deserialize(d)?;
        match EventKind::parse(&name) {
            Some(k) if !EventKind::SPECIAL.contains(&k) => Ok(k.into()),
            Some(_) => Err(D::Error::custom(format!(
                "`{name}` is placed by the engine and cannot be listed in kinds"
            ))),
            None => Err(D::Error::custom(format!("unknown kind `{name}`"))),
        }
    }
}

impl From<EventKindDto> for EventKind {
    fn from(k: EventKindDto) -> Self {
        match k {
            EventKindDto::PoliceEncounter => EventKind::PoliceEncounter,
            EventKindDto::AnimalEncounter => EventKind::AnimalEncounter,
            EventKindDto::HitSomeone => EventKind::HitSomeone,
            EventKindDto::SuddenDesire => EventKind::SuddenDesire,
            EventKindDto::GroceryShopping => EventKind::GroceryShopping,
            EventKindDto::Showering => EventKind::Showering,
            EventKindDto::Bathing => EventKind::Bathing,
            EventKindDto::InClass => EventKind::InClass,
            EventKindDto::OnTheWay => EventKind::OnTheWay,
            EventKindDto::PeriodScare => EventKind::PeriodScare,
            EventKindDto::PregnancyScare => EventKind::PregnancyScare,
            EventKindDto::GotBumped => EventKind::GotBumped,
            EventKindDto::BumpedSomeone => EventKind::BumpedSomeone,
            EventKindDto::Quarrel => EventKind::Quarrel,
            EventKindDto::ScoldedBySuperior => EventKind::ScoldedBySuperior,
            EventKindDto::ScoldedSuperior => EventKind::ScoldedSuperior,
            EventKindDto::SawBag => EventKind::SawBag,
            EventKindDto::SawClothes => EventKind::SawClothes,
            EventKindDto::TempWork => EventKind::TempWork,
            EventKindDto::ExMessage => EventKind::ExMessage,
            EventKindDto::WeatherChange => EventKind::WeatherChange,
            EventKindDto::GotHit => EventKind::GotHit,
            EventKindDto::Breakup => EventKind::Breakup,
            EventKindDto::WorkingOut => EventKind::WorkingOut,
            EventKindDto::Reading => EventKind::Reading,
            EventKindDto::Gaming => EventKind::Gaming,
            EventKindDto::AwayFromPhone => EventKind::AwayFromPhone,
            EventKindDto::Unwell => EventKind::Unwell,
            EventKindDto::Irritable => EventKind::Irritable,
            EventKindDto::Busy => EventKind::Busy,
            EventKindDto::InBattle => EventKind::InBattle,
            EventKindDto::Asleep => EventKind::Asleep,
            EventKindDto::OutShopping => EventKind::OutShopping,
            EventKindDto::BrosGathering => EventKind::BrosGathering,
            EventKindDto::SisGathering => EventKind::SisGathering,
            EventKindDto::AdmiringScenery => EventKind::AdmiringScenery,
            EventKindDto::AdmiringArt => EventKind::AdmiringArt,
            EventKindDto::TakingSelfie => EventKind::TakingSelfie,
            EventKindDto::OnTheRoad => EventKind::OnTheRoad,
            EventKindDto::WatchingMatch => EventKind::WatchingMatch,
            EventKindDto::WatchingShow => EventKind::WatchingShow,
            EventKindDto::HavingSex => EventKind::HavingSex,
            EventKindDto::Sick => EventKind::Sick,
            EventKindDto::Period => EventKind::Period,
            EventKindDto::Hospitalized => EventKind::Hospitalized,
            EventKindDto::PostSurgery => EventKind::PostSurgery,
            EventKindDto::BusinessTrip => EventKind::BusinessTrip,
            EventKindDto::ShortTrip => EventKind::ShortTrip,
            EventKindDto::LongTrip => EventKind::LongTrip,
            EventKindDto::HometownVisit => EventKind::HometownVisit,
            EventKindDto::TakingSpace => EventKind::TakingSpace,
            EventKindDto::FellOut => EventKind::FellOut,
        }
    }
}

impl From<EventKind> for EventKindDto {
    fn from(k: EventKind) -> Self {
        match k {
            EventKind::PoliceEncounter => EventKindDto::PoliceEncounter,
            EventKind::AnimalEncounter => EventKindDto::AnimalEncounter,
            EventKind::HitSomeone => EventKindDto::HitSomeone,
            EventKind::SuddenDesire => EventKindDto::SuddenDesire,
            EventKind::GroceryShopping => EventKindDto::GroceryShopping,
            EventKind::Showering => EventKindDto::Showering,
            EventKind::Bathing => EventKindDto::Bathing,
            EventKind::InClass => EventKindDto::InClass,
            EventKind::OnTheWay => EventKindDto::OnTheWay,
            EventKind::PeriodScare => EventKindDto::PeriodScare,
            EventKind::PregnancyScare => EventKindDto::PregnancyScare,
            EventKind::GotBumped => EventKindDto::GotBumped,
            EventKind::BumpedSomeone => EventKindDto::BumpedSomeone,
            EventKind::Quarrel => EventKindDto::Quarrel,
            EventKind::ScoldedBySuperior => EventKindDto::ScoldedBySuperior,
            EventKind::ScoldedSuperior => EventKindDto::ScoldedSuperior,
            EventKind::SawBag => EventKindDto::SawBag,
            EventKind::SawClothes => EventKindDto::SawClothes,
            EventKind::TempWork => EventKindDto::TempWork,
            EventKind::ExMessage => EventKindDto::ExMessage,
            EventKind::WeatherChange => EventKindDto::WeatherChange,
            EventKind::GotHit => EventKindDto::GotHit,
            EventKind::Breakup => EventKindDto::Breakup,
            EventKind::WorkingOut => EventKindDto::WorkingOut,
            EventKind::Reading => EventKindDto::Reading,
            EventKind::Gaming => EventKindDto::Gaming,
            EventKind::AwayFromPhone => EventKindDto::AwayFromPhone,
            EventKind::Unwell => EventKindDto::Unwell,
            EventKind::Irritable => EventKindDto::Irritable,
            EventKind::Busy => EventKindDto::Busy,
            EventKind::InBattle => EventKindDto::InBattle,
            EventKind::Asleep => EventKindDto::Asleep,
            EventKind::OutShopping => EventKindDto::OutShopping,
            EventKind::BrosGathering => EventKindDto::BrosGathering,
            EventKind::SisGathering => EventKindDto::SisGathering,
            EventKind::AdmiringScenery => EventKindDto::AdmiringScenery,
            EventKind::AdmiringArt => EventKindDto::AdmiringArt,
            EventKind::TakingSelfie => EventKindDto::TakingSelfie,
            EventKind::OnTheRoad => EventKindDto::OnTheRoad,
            EventKind::WatchingMatch => EventKindDto::WatchingMatch,
            EventKind::WatchingShow => EventKindDto::WatchingShow,
            EventKind::HavingSex => EventKindDto::HavingSex,
            EventKind::Sick => EventKindDto::Sick,
            EventKind::Period => EventKindDto::Period,
            EventKind::Hospitalized => EventKindDto::Hospitalized,
            EventKind::PostSurgery => EventKindDto::PostSurgery,
            EventKind::BusinessTrip => EventKindDto::BusinessTrip,
            EventKind::ShortTrip => EventKindDto::ShortTrip,
            EventKind::LongTrip => EventKindDto::LongTrip,
            EventKind::HometownVisit => EventKindDto::HometownVisit,
            EventKind::TakingSpace => EventKindDto::TakingSpace,
            EventKind::FellOut => EventKindDto::FellOut,
        }
    }
}

/// A life event's category: `short` and `mid` are incidents, `body` and
/// `trip` backgrounds that last days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventCategoryDto {
    Short,
    Mid,
    Body,
    Trip,
}

impl From<eros_engine_core::events::EventCategory> for EventCategoryDto {
    fn from(c: eros_engine_core::events::EventCategory) -> Self {
        use eros_engine_core::events::EventCategory;
        match c {
            EventCategory::Short => EventCategoryDto::Short,
            EventCategory::Mid => EventCategoryDto::Mid,
            EventCategory::Body => EventCategoryDto::Body,
            EventCategory::Trip => EventCategoryDto::Trip,
        }
    }
}

/// A life event: what happened, when it started and ends, whether it keeps
/// the persona offline while it lasts, and the message that told it (`null`
/// for an event that is not told).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EventView {
    pub kind: EventKindDto,
    pub category: EventCategoryDto,
    /// While it lasts, her 1:1 text turns in any of her sessions ghost,
    /// except turns carrying a user `action` or `image.force`, and except on
    /// a deployment with ghosting off; `/open` returns no greeting.
    pub offline: bool,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub message_id: Option<Uuid>,
}

impl EventView {
    /// The event as `traits`' persona lives it: whether it keeps her offline
    /// depends on the persona for `out_shopping`.
    pub(crate) fn new(
        e: &eros_engine_store::events::EventRecord,
        traits: &eros_engine_core::events::PersonaTraits,
    ) -> Self {
        Self {
            kind: e.kind.into(),
            category: e.kind.category().into(),
            offline: e.kind.is_offline(traits),
            started_at: e.started_at,
            ends_at: e.ends_at,
            message_id: e.message_id,
        }
    }
}

/// The kinds a draw may pick: a list of the allowed kinds, or an object of
/// rules (spec 2026-10-17-life-events-run-ins-and-kind-filters-design.md §3).
#[derive(Debug, Clone, PartialEq, utoipa::ToSchema)]
#[serde(untagged)]
pub enum KindsDto {
    /// Only these kinds; `[]` allows none.
    List(Vec<EventKindDto>),
    Rules(KindRulesDto),
}

/// An array reads as the list and an object as the rules; nothing else is
/// accepted. A derived untagged enum would also read an array into the rules
/// struct by position, so `[null]` would allow every kind.
impl<'de> Deserialize<'de> for KindsDto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Kinds;
        impl<'de> serde::de::Visitor<'de> for Kinds {
            type Value = KindsDto;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of kinds or an object of allow and disallow")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<KindsDto, A::Error> {
                Vec::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(KindsDto::List)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<KindsDto, A::Error> {
                KindRulesDto::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(KindsDto::Rules)
            }
        }
        d.deserialize_any(Kinds)
    }
}

/// `allow` is what may be drawn, `disallow` what may not; a kind in both is
/// not drawn. `{}` allows every kind.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct KindRulesDto {
    /// Absent: every kind. `[]`: none.
    #[serde(default)]
    pub allow: Option<Vec<EventKindDto>>,
    /// Absent or `[]`: nothing removed.
    #[serde(default)]
    pub disallow: Option<Vec<EventKindDto>>,
}

/// The kinds a request allows, in draw order.
fn allowed_kinds(kinds: Option<&KindsDto>) -> Vec<EventKind> {
    let listed =
        |ks: &[EventKindDto]| -> Vec<EventKind> { ks.iter().map(|k| (*k).into()).collect() };
    match kinds {
        None => EventKind::ALL.to_vec(),
        Some(KindsDto::List(ks)) => listed(ks),
        Some(KindsDto::Rules(r)) => {
            let allow = r.allow.as_deref().map(listed);
            let disallow = listed(r.disallow.as_deref().unwrap_or_default());
            EventKind::ALL
                .into_iter()
                .filter(|k| allow.as_ref().is_none_or(|a| a.contains(k)))
                .filter(|k| !disallow.contains(k))
                .collect()
        }
    }
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct DrawEventRequest {
    /// The caller's idempotency key, scoped to the session: the same key
    /// returns the same event. 1–128 chars of `[A-Za-z0-9_.:-]`.
    pub event_key: String,
    /// The kinds that may be drawn. Absent: all of them. An array allows
    /// the listed kinds (`[]`: none); `{"allow": […], "disallow": […]}`
    /// allows what `allow` lists (absent: every kind) minus what `disallow`
    /// lists.
    #[serde(default)]
    pub kinds: Option<KindsDto>,
    /// As on `/open`: the user's IANA timezone, for `[now]`.
    #[serde(default)]
    pub user_timezone: Option<String>,
    #[serde(default)]
    pub user_country: Option<String>,
    #[serde(default)]
    pub user_region: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub prompt_traits: Option<Vec<PromptTraitDto>>,
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub memory_scope: Option<MemoryScope>,
    #[serde(default)]
    pub affinity_scope: Option<AffinityScopeDto>,
    #[serde(default)]
    pub audit: Option<LlmAuditDto>,
    /// The caller handles images: a kind about a photo (`taking_selfie`)
    /// then carries her picture, when the deployment configures
    /// `[tasks.chat_image_prompt_compose]`. Absent: words only.
    #[serde(default)]
    pub image: Option<EventImageParams>,
    /// The outfit the persona wears, as on a chat turn: typically one of her
    /// wardrobe entries, not checked against it. 1–64 chars after trim; blank
    /// reads as absent. Her words follow it, and so does her picture.
    #[serde(default)]
    #[schema(max_length = 64)]
    pub outfit: Option<String>,
}

/// How her picture is composed; each field as on a chat turn's `image`.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct EventImageParams {
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub style: Option<StyleKey>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Which `[tasks.chat_image_prompt_compose].filter_prompt` variant to use:
    /// an index (`"0"`, `"1"`) for the array shape, or a key (`"a"`, `"b"`)
    /// for the table shape. An unknown one falls back to the built-in
    /// composer prompt.
    #[serde(default)]
    pub prompt_variant: Option<String>,
}

impl From<EventImageParams> for ImageReplyParams {
    fn from(p: EventImageParams) -> Self {
        Self {
            force: false,
            style: p.style,
            aspect_ratio: p.aspect_ratio,
            prompt_variant: p.prompt_variant,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DrawEventResponse {
    /// The event drawn (or the key's existing one); `null` when nothing
    /// happened.
    pub event: Option<EventView>,
    /// The persona's message telling it; `null` with `event`.
    pub message: Option<GreetingDto>,
    /// Her picture on the message, the restore route's payload. Absent when
    /// the message carries none, or its composed prompt was never recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_request: Option<ImageRequestPayloadResponse>,
}

impl DrawEventResponse {
    fn none() -> Self {
        Self {
            event: None,
            message: None,
            image_request: None,
        }
    }
}

/// The event written under `event_key` in this session, with the message
/// that told it; nulls when there is none.
async fn respond(
    state: &AppState,
    session_id: Uuid,
    event_key: &str,
    traits: &events::PersonaTraits,
) -> Result<DrawEventResponse, AppError> {
    let Some(event) = EventRepo { pool: &state.pool }
        .by_key(session_id, event_key)
        .await?
    else {
        return Ok(DrawEventResponse::none());
    };
    let message = match event.message_id {
        Some(id) => {
            ChatRepo { pool: &state.pool }
                .message_by_id_in_session(session_id, id)
                .await?
        }
        None => None,
    };
    let image_request = match &message {
        Some(m) => match image_request_payload(state, session_id, m).await {
            Ok(p) => Some(p),
            Err(AppError::NotFound(_)) => None,
            Err(e) => return Err(e),
        },
        None => None,
    };
    Ok(DrawEventResponse {
        event: Some(EventView::new(&event, traits)),
        message: message.map(Into::into),
        image_request,
    })
}

/// Ask for something to happen in the persona's day.
///
/// The engine draws one kind of event, or none (a short or mid-length
/// incident is under way, every allowed kind is cooling down, or she has asked
/// the user for space and waits for the answer, in any session of the persona,
/// until it is answered or lapses). During a background event (a trip or a
/// body state) only incidents are drawn. The
/// persona then tells the user in one proactive message, persisted like any
/// reply and returned with the event. The offline kinds (`away_from_phone`,
/// `busy`, `in_battle`, `asleep`, `bros_gathering`, `sis_gathering`,
/// `having_sex`, and `out_shopping` for a female persona) make her
/// unreachable while they last.
/// `busy`, `in_battle` and a female persona's `out_shopping` are announced in
/// a message at intimacy rung 2 and up; below that, and always for
/// `away_from_phone`, `asleep`, `having_sex` and the gatherings, the event comes back with
/// `message: null`. For any other persona `out_shopping` is not offline and
/// is announced at every rung. The same `event_key` returns the same event.
///
/// With `image` in the request and `[tasks.chat_image_prompt_compose]`
/// configured, a `taking_selfie` message also carries her picture: her words
/// are written first, the composer draws from them, and the response's
/// `image_request` is the `image-request` route's payload (absent without a
/// picture). `outfit` dresses her as on a chat turn, in her words and in the
/// picture.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/events",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = DrawEventRequest,
    responses(
        (status = 200, body = DrawEventResponse),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
        (status = 422, description = "unknown kind, `taking_space` or `fell_out` in `kinds`, \
            an `outfit` over 64 chars, an unsupported `image.aspect_ratio`, \
            or another malformed body field"),
        (status = 429, description = "per-user in-flight cap reached"),
        (status = "5XX", description = "companion chain exhausted; the provider's own status \
            passes through. Nothing is persisted.")
    ),
    security(("bearer" = []))
)]
async fn draw_event(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<DrawEventRequest>,
) -> Result<Json<DrawEventResponse>, AppError> {
    // The generator is not `Send`: it is dropped before the first await.
    let rolls = {
        let mut rng = rand::thread_rng();
        events::Rolls {
            category: rng.gen(),
            kind: rng.gen(),
            duration: rng.gen(),
        }
    };
    draw_event_at(&state, session_id, user_id, req, Utc::now(), rolls)
        .await
        .map(Json)
}

fn invalid_payload(message: String) -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_payload",
        message,
        user_message: "请求无效".into(),
        original_user_message_id: None,
    })
}

/// The handler with its clock and rolls injected, for tests.
pub(crate) async fn draw_event_at(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    req: DrawEventRequest,
    now: DateTime<Utc>,
    rolls: events::Rolls,
) -> Result<DrawEventResponse, AppError> {
    validate_tier(req.tier.as_deref())?;
    let prompt_traits = validate_prompt_traits(req.prompt_traits.as_deref().unwrap_or(&[]))
        .map_err(|e| invalid_payload(e.to_string()))?;
    let audit =
        validate_llm_audit(req.audit.clone()).map_err(|e| invalid_payload(e.to_string()))?;
    let outfit = validated_outfit(req.outfit.as_deref()).map_err(|message| {
        AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message,
            user_message: "穿着无效".into(),
            original_user_message_id: None,
        })
    })?;
    validate_image_params(req.image.clone().map(ImageReplyParams::from).as_ref())?;
    let key = req.event_key.clone();
    let well_formed = (1..=MAX_EVENT_KEY_CHARS).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    if !well_formed {
        return Err(invalid_payload(format!(
            "event_key must be 1..={MAX_EVENT_KEY_CHARS} chars of [A-Za-z0-9_.:-]"
        )));
    }
    let (_session, persona, instance_id) =
        resolve_text_turn(state, session_id, user_id, "life events").await?;

    let chat_repo = ChatRepo { pool: &state.pool };
    let event_repo = EventRepo { pool: &state.pool };
    let traits = crate::prompt::persona_traits(&persona);
    if event_repo.by_key(session_id, &key).await?.is_some() {
        return respond(state, session_id, &key, &traits).await;
    }
    // She has asked for space and waits for an answer: the ask is the
    // foreground drama, and it keeps an offline stretch from starting
    // mid-question (spec 2026-10-19 §3.1). Any session of the instance.
    let waiting = LifeChoiceRepo { pool: &state.pool }
        .pending_for_instance(instance_id, now)
        .await?;
    if waiting {
        return Ok(DrawEventResponse::none());
    }

    let needs = if state.model_config.pde_enabled() {
        crate::pipeline::handlers::load_needs(
            &state.pool,
            instance_id,
            None,
            now,
            crate::prompt::persona_habits(&persona),
        )
        .await?
    } else {
        None
    };
    let mut affinity = AffinityRepo { pool: &state.pool }
        .load_or_create(session_id, user_id, instance_id)
        .await?;
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let history: Vec<events::PastEvent> = event_repo
        .recent(instance_id, now - events::lookback())
        .await?
        .iter()
        .map(|e| e.past())
        .collect();
    let mut allowed = allowed_kinds(req.kinds.as_ref());
    // An offline kind is silence: it follows the ghost protection (the
    // deployment's ghosting switch and the session's message floor).
    let signals =
        crate::pipeline::compute_signals_for_session(&state.pool, session_id, &affinity).await?;
    if !state.model_config.pde_ghosting_enabled()
        || signals.message_count < eros_engine_core::ghost::GHOST_MIN_MESSAGES
    {
        allowed.retain(|k| !k.is_offline(&traits));
    }
    let rung = affinity.intimacy_rung();
    let ever_had_sex = event_repo
        .ever(instance_id, events::EventKind::HavingSex)
        .await?;
    let Some(drawn) = events::draw(
        &DrawInput {
            now,
            needs,
            intimacy_rung: rung,
            persona: &traits,
            history: &history,
            allowed: &allowed,
            ever_had_sex,
        },
        rolls,
    ) else {
        return Ok(DrawEventResponse::none());
    };

    // A kind she does not tell is written without a message (spec
    // 2026-10-16-life-events-offline-and-scares-design.md §4.1).
    if !events::is_told(drawn.kind, rung, &traits) {
        event_repo
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: &key,
                    kind: drawn.kind,
                    started_at: now,
                    ends_at: now + drawn.lasts,
                },
                None,
            )
            .await?;
        return respond(state, session_id, &key, &traits).await;
    }

    let background = history
        .iter()
        .find(|e| e.kind.is_background() && e.ends_at > now)
        .map(|e| e.kind);
    // Her picture: a kind about a photo, a caller that handles images, and a
    // configured composer (spec 2026-10-26-event-message-image-design.md §3).
    let image = req
        .image
        .filter(|_| {
            drawn.kind.is_photo()
                && crate::pipeline::stream::image_capability_available(true, &state.model_config)
        })
        .map(ImageReplyParams::from);

    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;
    let locale = UserLocale::resolve(
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let mut metadata = serde_json::Map::new();
    holiday::record_raw(
        &mut metadata,
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let recall_query = chat_repo
        .latest_user_message(session_id)
        .await?
        .as_ref()
        .map(recall_query_text)
        .unwrap_or_default();
    let now_ctx = NowContext::for_user(&locale, now);
    greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona: &persona,
            now: &now_ctx,
            occasion: Occasion::Event {
                event_key: key.clone(),
                kind: drawn.kind,
                lasts: drawn.lasts,
                background,
                image,
                outfit,
            },
            recall_query,
            since: now,
            tier: req.tier,
            prompt_traits,
            memory_scope: req.memory_scope.unwrap_or_default(),
            affinity_scope: req
                .affinity_scope
                .as_ref()
                .map(AffinityScopeDto::resolve)
                .unwrap_or_default(),
            audit,
            metadata,
        },
    )
    .await?;
    respond(state, session_id, &key, &traits).await
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(draw_event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        seed_byoc_session, seed_genome, seed_instance, seed_session, session_footprint,
    };

    fn with_companion(mut state: AppState, mock_uri: &str) -> AppState {
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"companion\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    async fn mount_companion(mock: &MockServer, reply: &str) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-event",
                "model": "served/companion",
                "choices": [{"message": {"content": reply}}],
            })))
            .mount(mock)
            .await;
    }

    /// The companion plus the image composer (its prompt is "COMPOSE").
    fn with_companion_and_composer(mut state: AppState, mock_uri: &str) -> AppState {
        state = with_companion(state, mock_uri);
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"companion\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\nfilter_prompt = \"COMPOSE\"\n",
            )
            .unwrap(),
        );
        state
    }

    async fn mount_composer(mock: &MockServer, prompt: &str, caption: &str) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("COMPOSE"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-compose",
                "model": "served/composer",
                "choices": [{"message": {"content":
                    json!({"prompt": prompt, "caption": caption}).to_string()}}],
            })))
            .mount(mock)
            .await;
    }

    fn selfie_req(key: &str, image: bool) -> DrawEventRequest {
        DrawEventRequest {
            image: image.then(|| EventImageParams {
                aspect_ratio: Some("3:4".into()),
                ..Default::default()
            }),
            ..req(key, Some(vec![EventKindDto::TakingSelfie]))
        }
    }

    /// The message row's metadata.
    async fn row_meta(pool: &PgPool, message_id: Uuid) -> serde_json::Value {
        sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
            .bind(message_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The compose audit row a marker names: (source, status, inputs).
    async fn compose_row(
        pool: &PgPool,
        marker: &serde_json::Value,
    ) -> (String, String, serde_json::Value) {
        let id: Uuid = marker["compose_event_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        sqlx::query_as("SELECT source, status, inputs FROM engine.chat_images_events WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn last_content(call: &serde_json::Value) -> String {
        call["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A persona (`seed_genome`'s prompt is "you are a companion") with one
    /// text session; returns (instance, session).
    async fn setup(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid) {
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = seed_session(pool, user_id, instance_id).await;
        (instance_id, session_id)
    }

    fn req(key: &str, kinds: Option<Vec<EventKindDto>>) -> DrawEventRequest {
        DrawEventRequest {
            event_key: key.into(),
            kinds: kinds.map(KindsDto::List),
            user_timezone: Some("Asia/Taipei".into()),
            ..Default::default()
        }
    }

    async fn event_rows(pool: &PgPool, session_id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND metadata->>'proactive' = 'event'",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_draw_tells_the_event_and_returns_it(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚被老板叫去加班，晚点聊").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let now = Utc::now() - chrono::Duration::minutes(1);

        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::TempWork])),
            now,
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("an event");
        let m = out.message.expect("a message");
        assert_eq!(e.kind, EventKindDto::TempWork);
        assert_eq!(e.category, EventCategoryDto::Mid);
        assert_eq!(e.message_id, Some(m.message_id));
        assert_eq!(m.content, "刚被老板叫去加班，晚点聊");
        assert_eq!(
            m.occasion,
            crate::routes::session_open::GreetingOccasion::Event
        );
        assert_eq!((e.ends_at - e.started_at).num_hours(), 4);
        assert!(e.started_at > now, "the event's time is when it is written");

        let meta: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(m.message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(meta["proactive"], json!("event"));
        assert!(meta.get("event").is_none() && meta.get("event_key").is_none());
        let (key, kind): (String, String) = sqlx::query_as(
            "SELECT event_key, kind FROM engine.persona_events WHERE message_id = $1",
        )
        .bind(m.message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((key.as_str(), kind.as_str()), ("k1", "temp_work"));
        assert_eq!(meta["user_timezone"], json!("Asia/Taipei"));

        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        let msgs = c[0]["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(
            last["content"],
            json!("（你刚临时接到一份工作，得马上去处理。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_same_key_returns_the_same_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "外面突然下大雨了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let kinds = Some(vec![EventKindDto::WeatherChange]);
        let first = draw_event_at(
            &state,
            session_id,
            user_id,
            req("same", kinds.clone()),
            Utc::now(),
            rolls(0.1),
        )
        .await
        .unwrap();
        let again = draw_event_at(
            &state,
            session_id,
            user_id,
            req("same", kinds),
            Utc::now(),
            rolls(0.9),
        )
        .await
        .unwrap();
        assert_eq!(
            first.message.as_ref().map(|m| m.message_id),
            again.message.as_ref().map(|m| m.message_id)
        );
        assert!(again.event.is_some());
        assert_eq!(calls(&mock).await.len(), 1, "no second generation");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn nothing_is_drawn_while_an_event_is_under_way(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "路上遇到一只小猫").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let first = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::AnimalEncounter])),
            Utc::now(),
            rolls(0.1),
        )
        .await
        .unwrap();
        assert!(first.event.is_some());
        let second = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k2", None),
            Utc::now(),
            rolls(0.1),
        )
        .await
        .unwrap();
        assert!(second.event.is_none() && second.message.is_none());
        assert_eq!(calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_event_in_another_session_blocks_the_draw(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let other = seed_session(&pool, user_id, instance_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "被警察拦下来问话了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        assert!(draw_event_at(
            &state,
            other,
            user_id,
            req("k1", Some(vec![EventKindDto::PoliceEncounter])),
            Utc::now(),
            rolls(0.1)
        )
        .await
        .unwrap()
        .event
        .is_some());
        let here = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k2", None),
            Utc::now(),
            rolls(0.1),
        )
        .await
        .unwrap();
        assert!(
            here.event.is_none(),
            "the persona is already in the middle of something"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_kind_in_cooldown_is_not_drawn(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        // A weather change that started 5 hours ago and ended an hour ago:
        // over, but still inside its 8-hour cooldown.
        let started = Utc::now() - chrono::Duration::hours(5);
        eros_engine_store::events::EventRepo { pool: &pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: "old",
                    kind: EventKind::WeatherChange,
                    started_at: started,
                    ends_at: started + chrono::Duration::hours(4),
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "又下雨了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::WeatherChange])),
            Utc::now(),
            rolls(0.1),
        )
        .await
        .unwrap();
        assert!(out.event.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn ex_message_needs_closeness(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "前任给我发消息了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        // A fresh session sits on rung 1; the ex only writes from rung 2.
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::ExMessage])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert!(calls(&mock).await.is_empty());
        assert_eq!(event_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_empty_allowlist_draws_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_disallowed_kind_is_not_drawn(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚跟楼下邻居吵了一架").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let request = DrawEventRequest {
            kinds: Some(KindsDto::Rules(KindRulesDto {
                allow: Some(vec![EventKindDto::Quarrel, EventKindDto::Reading]),
                disallow: Some(vec![EventKindDto::Reading]),
            })),
            ..req("k1", None)
        };
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            request,
            Utc::now(),
            rolls(0.99),
        )
        .await
        .unwrap();
        let e = out.event.expect("an event");
        assert_eq!(e.kind, EventKindDto::Quarrel);
        assert_eq!(e.category, EventCategoryDto::Short);
        assert_eq!((e.ends_at - e.started_at).num_minutes(), 60);
        let c = calls(&mock).await;
        let last = c[0]["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["content"], json!("（你刚跟人吵了一架。）"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_reply_leaves_no_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "   ").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::TempWork])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert_eq!(event_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_malformed_key_is_rejected(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        for key in ["", "has space", &"x".repeat(129)] {
            let err = draw_event_at(
                &state,
                session_id,
                user_id,
                req(key, None),
                Utc::now(),
                rolls(0.5),
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{key:?} accepted"));
            assert!(
                matches!(
                    err,
                    AppError::StreamPre(StreamPreError {
                        status: StatusCode::BAD_REQUEST,
                        ..
                    })
                ),
                "{key:?}"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_session_gate_applies(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        let status = |e: AppError| match e {
            AppError::StreamPre(p) => p.status,
            other => panic!("{other:?}"),
        };
        let stranger = draw_event_at(
            &state,
            session_id,
            Uuid::new_v4(),
            req("k", None),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(stranger), StatusCode::FORBIDDEN);
        let missing = draw_event_at(
            &state,
            Uuid::new_v4(),
            user_id,
            req("k", None),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(missing), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn events_refuse_a_byoc_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = seed_byoc_session(&pool, user_id).await;
        let before = session_footprint(&pool, session_id).await;
        let err = draw_event_at(
            &test_state(pool.clone()),
            session_id,
            user_id,
            req("k", None),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .err()
        .unwrap();
        let AppError::StreamPre(e) = err else {
            panic!("expected a coded error")
        };
        assert_eq!(
            (e.status, e.code),
            (StatusCode::BAD_REQUEST, "byoc_unsupported")
        );
        assert_eq!(session_footprint(&pool, session_id).await, before);
    }

    fn rolls(r: f64) -> events::Rolls {
        events::Rolls {
            category: r,
            kind: r,
            duration: r,
        }
    }

    async fn set_art(pool: &PgPool, instance_id: Uuid, art: serde_json::Value) {
        sqlx::query(
            "UPDATE engine.persona_genomes SET art_metadata = $2 \
             WHERE id = (SELECT genome_id FROM engine.persona_instances WHERE id = $1)",
        )
        .bind(instance_id)
        .bind(art)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_drawn_length_sets_when_the_event_ends(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "我出发去旅行啦").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::LongTrip])),
            Utc::now(),
            events::Rolls {
                category: 0.5,
                kind: 0.5,
                duration: 0.0,
            },
        )
        .await
        .unwrap();
        let e = out.event.expect("a trip");
        assert_eq!(e.kind, EventKindDto::LongTrip);
        assert_eq!(e.category, EventCategoryDto::Trip);
        assert_eq!((e.ends_at - e.started_at).num_hours(), 72);
        let c = calls(&mock).await;
        let last = c[0]["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            last["content"],
            json!("（你刚出发去长途旅行，要去 3 天。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_incident_during_a_trip_is_told_from_the_trip(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let started = Utc::now() - chrono::Duration::hours(2);
        eros_engine_store::events::EventRepo { pool: &pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: "trip",
                    kind: EventKind::BusinessTrip,
                    started_at: started,
                    ends_at: started + chrono::Duration::hours(24),
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "出差路上遇到一只小猫").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::AnimalEncounter])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_some());
        let c = calls(&mock).await;
        let msgs = c[0]["messages"].as_array().unwrap();
        assert_eq!(
            msgs.last().unwrap()["content"],
            json!("（你正在外地出差。你刚在路上遇到了一只小动物。）")
        );
        assert!(
            c[0].to_string()
                .contains("- 正在经历：出差（约 22 小时后结束）"),
            "{}",
            c[0]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn period_is_drawn_only_for_a_female_persona(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "那个来了，肚子好痛").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let only = || req("k", Some(vec![EventKindDto::Period]));
        let out = draw_event_at(&state, session_id, user_id, only(), Utc::now(), rolls(0.5))
            .await
            .unwrap();
        assert!(out.event.is_none());
        assert!(calls(&mock).await.is_empty());
        set_art(&pool, instance_id, json!({"gender": "female"})).await;
        let out = draw_event_at(&state, session_id, user_id, only(), Utc::now(), rolls(0.5))
            .await
            .unwrap();
        assert_eq!(out.event.map(|e| e.kind), Some(EventKindDto::Period));
    }

    /// `n` user rows in the session: what the ghost protection counts.
    async fn seed_user_messages(pool: &PgPool, session_id: Uuid, n: usize) {
        for _ in 0..n {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                 VALUES ($1, 'user', 'hi')",
            )
            .bind(session_id)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn event_count(pool: &PgPool, session_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.persona_events WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn away_from_phone_is_not_drawn_before_the_ghost_threshold(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        let only = |key: &str| req(key, Some(vec![EventKindDto::AwayFromPhone]));

        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            only("a1"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none(), "a fresh session has no messages");
        assert_eq!(event_count(&pool, session_id).await, 0);

        seed_user_messages(&pool, session_id, 9).await;
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            only("a2"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none(), "nine messages are still below ten");
        assert_eq!(event_count(&pool, session_id).await, 0);

        seed_user_messages(&pool, session_id, 1).await;
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            only("a3"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert_eq!(
            out.event.map(|e| e.kind),
            Some(EventKindDto::AwayFromPhone),
            "ten messages lift the veto"
        );
        assert_eq!(event_count(&pool, session_id).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_ghost_threshold_leaves_other_kinds_alone(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "外面突然下大雨了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::WeatherChange])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert_eq!(
            out.event.map(|e| e.kind),
            Some(EventKindDto::WeatherChange),
            "a fresh session still draws the kinds that tell a message"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn away_from_phone_is_not_drawn_while_ghosting_is_off(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        let only = |key: &str| req(key, Some(vec![EventKindDto::AwayFromPhone]));
        let mut off = test_state(pool.clone());
        off.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.pde_decision]\nmodel = \"judge\"\nghosting = false\n",
            )
            .unwrap(),
        );
        assert!(!off.model_config.pde_ghosting_enabled());

        let out = draw_event_at(
            &off,
            session_id,
            user_id,
            only("a1"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert_eq!(event_count(&pool, session_id).await, 0);

        // The same session draws it once the switch is on.
        let on = test_state(pool.clone());
        assert!(on.model_config.pde_ghosting_enabled());
        let out = draw_event_at(&on, session_id, user_id, only("a2"), Utc::now(), rolls(0.5))
            .await
            .unwrap();
        assert_eq!(out.event.map(|e| e.kind), Some(EventKindDto::AwayFromPhone));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn having_sex_is_stripped_like_any_offline_kind(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        let only = |key: &str| req(key, Some(vec![EventKindDto::HavingSex]));
        let mut off = test_state(pool.clone());
        off.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.pde_decision]\nmodel = \"judge\"\nghosting = false\n",
            )
            .unwrap(),
        );
        let out = draw_event_at(
            &off,
            session_id,
            user_id,
            only("x1"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert_eq!(event_count(&pool, session_id).await, 0);

        // Ghosting on: written without a message at rung 1 …
        let mock = MockServer::start().await;
        let on = with_companion(test_state(pool.clone()), &mock.uri());
        let earlier = Utc::now() - chrono::Duration::hours(61);
        let out = draw_event_at(&on, session_id, user_id, only("x2"), earlier, rolls(0.5))
            .await
            .unwrap();
        let e = out.event.expect("having_sex at rung 1");
        assert!(e.offline && e.message_id.is_none() && out.message.is_none());

        // … and at rung 2 alike: it is never announced.
        let other = seed_session(&pool, user_id, instance_id).await;
        seed_user_messages(&pool, other, 10).await;
        rung_two(&pool, other, user_id, instance_id).await;
        let out = draw_event_at(&on, other, user_id, only("x3"), Utc::now(), rolls(0.5))
            .await
            .unwrap();
        let e = out.event.expect("having_sex at rung 2");
        assert!(e.offline && e.message_id.is_none() && out.message.is_none());
        assert!(calls(&mock).await.is_empty(), "nothing is generated");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn draw_reads_whether_she_ever_had_sex(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        set_art(&pool, instance_id, json!({"is_virgin": true})).await;
        let long_ago = Utc::now() - chrono::Duration::days(200);
        eros_engine_store::events::EventRepo { pool: &pool }
            .insert_preempting(&eros_engine_store::events::NewEvent {
                instance_id,
                session_id,
                event_key: "old",
                kind: EventKind::HavingSex,
                started_at: long_ago,
                ends_at: long_ago + chrono::Duration::hours(1),
            })
            .await
            .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "在看书").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req(
                "x1",
                Some(vec![EventKindDto::HavingSex, EventKindDto::Reading]),
            ),
            Utc::now(),
            rolls(0.8),
        )
        .await
        .unwrap();
        // Draw order is Reading 2 then HavingSex: 1 once she has (0.8 × 3 =
        // 2.4 lands on HavingSex), 0.2 if the read were not wired (0.8 × 2.2
        // = 1.76 would land on Reading).
        assert_eq!(
            out.event.map(|e| e.kind),
            Some(EventKindDto::HavingSex),
            "the ever-had-sex read reaches the draw"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn away_from_phone_is_drawn_without_a_message(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        let mock = MockServer::start().await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("a1", Some(vec![EventKindDto::AwayFromPhone])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("an away stretch");
        assert_eq!(e.kind, EventKindDto::AwayFromPhone);
        assert_eq!(e.message_id, None);
        assert!(out.message.is_none());
        // 1–3 h, duration roll 0.5 → 1 + ⌊0.5 × 3⌋ = 2 h.
        assert_eq!((e.ends_at - e.started_at).num_hours(), 2);
        assert!(calls(&mock).await.is_empty(), "nothing is generated");
        assert_eq!(event_rows(&pool, session_id).await, 0, "no message row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_same_key_returns_the_same_away_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        let state = test_state(pool.clone());
        let only = || req("a1", Some(vec![EventKindDto::AwayFromPhone]));
        let first = draw_event_at(&state, session_id, user_id, only(), Utc::now(), rolls(0.1))
            .await
            .unwrap()
            .event
            .expect("drawn");
        let again = draw_event_at(&state, session_id, user_id, only(), Utc::now(), rolls(0.9))
            .await
            .unwrap()
            .event
            .expect("the same");
        assert_eq!(
            (first.started_at, first.ends_at),
            (again.started_at, again.ends_at)
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.persona_events WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1);
    }

    /// Put the session's relationship on rung 2: bond (0.6 + 0.6) / 2 = 0.6.
    async fn rung_two(pool: &PgPool, session_id: Uuid, user_id: Uuid, instance_id: Uuid) {
        AffinityRepo { pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.companion_affinity SET trust = 0.6, intrigue = 0.6, updated_at = now() \
             WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();
    }

    const OFFLINE: [EventKindDto; 4] = [
        EventKindDto::AwayFromPhone,
        EventKindDto::Busy,
        EventKindDto::InBattle,
        EventKindDto::Asleep,
    ];

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_ghost_protection_keeps_every_offline_kind_out(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("a1", Some(OFFLINE.to_vec())),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none(), "a fresh session is below the floor");

        seed_user_messages(&pool, session_id, 10).await;
        let mut off = test_state(pool.clone());
        off.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.pde_decision]\nmodel = \"judge\"\nghosting = false\n",
            )
            .unwrap(),
        );
        let out = draw_event_at(
            &off,
            session_id,
            user_id,
            req("a2", Some(OFFLINE.to_vec())),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none(), "ghosting is off");
        assert_eq!(event_count(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn busy_is_announced_only_to_someone_close(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "我先去忙了，晚点找你").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        // Rung 1, 13 hours ago: she goes quiet without a word.
        let earlier = Utc::now() - chrono::Duration::hours(13);
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("b1", Some(vec![EventKindDto::Busy])),
            earlier,
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("busy");
        assert!(e.offline);
        assert_eq!((e.message_id, out.message.is_none()), (None, true));
        assert!(calls(&mock).await.is_empty());

        // Rung 2, a fresh session of the same persona: she says so first.
        let other = seed_session(&pool, user_id, instance_id).await;
        seed_user_messages(&pool, other, 10).await;
        rung_two(&pool, other, user_id, instance_id).await;
        let out = draw_event_at(
            &state,
            other,
            user_id,
            req("b2", Some(vec![EventKindDto::Busy])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("busy, announced");
        assert!(e.offline && e.message_id.is_some());
        assert_eq!(
            out.message.expect("the announcement").content,
            "我先去忙了，晚点找你"
        );
        assert!(calls(&mock).await[0]
            .to_string()
            .contains("（你接下来要忙一阵，这段时间顾不上看手机。）"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn asleep_is_never_announced(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        rung_two(&pool, session_id, user_id, instance_id).await;
        let mock = MockServer::start().await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("s1", Some(vec![EventKindDto::Asleep])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("asleep");
        assert!(e.offline && e.message_id.is_none() && out.message.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_announcement_writes_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        rung_two(&pool, session_id, user_id, instance_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "   ").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("b1", Some(vec![EventKindDto::Busy])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert_eq!(event_count(&pool, session_id).await, 0, "no silence either");
    }

    #[test]
    fn each_form_of_kinds_resolves_to_its_set() {
        use EventKind::{Quarrel, Reading, SuddenDesire};
        let parse = |kinds: Option<serde_json::Value>| -> Vec<EventKind> {
            let mut body = json!({"event_key": "k"});
            if let Some(v) = kinds {
                body["kinds"] = v;
            }
            let r: DrawEventRequest = serde_json::from_value(body).unwrap();
            allowed_kinds(r.kinds.as_ref())
        };
        let all = EventKind::ALL.to_vec();
        let cases: [(Option<serde_json::Value>, Vec<EventKind>); 9] = [
            (None, all.clone()),
            (Some(json!(null)), all.clone()),
            (Some(json!([])), vec![]),
            (Some(json!(["reading", "quarrel"])), vec![Reading, Quarrel]),
            (Some(json!({})), all.clone()),
            (Some(json!({"allow": null, "disallow": []})), all.clone()),
            (Some(json!({"allow": []})), vec![]),
            (
                Some(json!({"allow": ["reading", "quarrel"]})),
                vec![Quarrel, Reading],
            ),
            (
                Some(json!({"allow": ["reading", "quarrel"], "disallow": ["quarrel"]})),
                vec![Reading],
            ),
        ];
        for (kinds, want) in cases {
            assert_eq!(parse(kinds.clone()), want, "{kinds:?}");
        }
        let without = parse(Some(json!({"disallow": ["sudden_desire"]})));
        assert_eq!(without.len(), EventKind::ALL.len() - 1);
        assert!(!without.contains(&SuddenDesire));
    }

    #[test]
    fn a_kinds_object_rejects_unknown_keys_and_kinds() {
        for kinds in [
            json!({"deny": ["sudden_desire"]}),
            json!({"disallow": ["dragon"]}),
            json!({"allow": ["dragon"]}),
            json!({"allow": "reading"}),
            json!("reading"),
            json!(5),
            json!([null]),
            json!([["reading"]]),
            json!([{"allow": ["reading"]}]),
        ] {
            assert!(
                serde_json::from_value::<DrawEventRequest>(
                    json!({"event_key": "k", "kinds": kinds})
                )
                .is_err(),
                "{kinds}"
            );
        }
    }

    #[test]
    fn an_unknown_kind_fails_deserialization() {
        assert!(serde_json::from_value::<DrawEventRequest>(
            json!({"event_key": "k", "kinds": ["dragon"]})
        )
        .is_err());
        assert!(
            serde_json::from_value::<DrawEventRequest>(json!({"kinds": []})).is_err(),
            "event_key is required"
        );
    }

    /// `taking_space` and `fell_out` are placed by the writers, never drawn:
    /// the DTO carries them for responses only.
    #[test]
    fn special_kinds_are_rejected_in_the_kinds_filter() {
        for kinds in [
            json!(["taking_space"]),
            json!(["reading", "fell_out"]),
            json!({"allow": ["taking_space"]}),
            json!({"allow": ["reading", "fell_out"]}),
            json!({"disallow": ["taking_space"]}),
            json!({"disallow": ["fell_out"]}),
        ] {
            assert!(
                serde_json::from_value::<DrawEventRequest>(
                    json!({"event_key": "k", "kinds": kinds})
                )
                .is_err(),
                "{kinds}"
            );
        }
        // Both still serialize in a response.
        assert_eq!(
            serde_json::to_value(EventKindDto::TakingSpace).unwrap(),
            json!("taking_space")
        );
        assert_eq!(
            serde_json::to_value(EventKindDto::FellOut).unwrap(),
            json!("fell_out")
        );
    }

    /// Draw `kind` for a fresh persona (`art_metadata` `art`, `tip_personality`
    /// `tip`) whose session is on rung 2 when `close`, else rung 1, with the
    /// ghost floor met. Returns the event, its message and the cue the model
    /// read (`None` when no message was generated).
    async fn draw_told(
        pool: &PgPool,
        kind: EventKindDto,
        art: serde_json::Value,
        tip: Option<&str>,
        close: bool,
    ) -> (
        EventView,
        Option<crate::routes::session_open::GreetingDto>,
        Option<String>,
    ) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(pool, user_id).await;
        set_art(pool, instance_id, art).await;
        sqlx::query(
            "UPDATE engine.persona_genomes SET tip_personality = $2 \
             WHERE id = (SELECT genome_id FROM engine.persona_instances WHERE id = $1)",
        )
        .bind(instance_id)
        .bind(tip)
        .execute(pool)
        .await
        .unwrap();
        seed_user_messages(pool, session_id, 10).await;
        if close {
            rung_two(pool, session_id, user_id, instance_id).await;
        }
        let mock = MockServer::start().await;
        mount_companion(&mock, "好").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k", Some(vec![kind])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let cue = calls(&mock).await.first().map(|c| {
            let msgs = c["messages"].as_array().unwrap();
            msgs.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_string()
        });
        (out.event.expect("drawn"), out.message, cue)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_found_bag_is_told_by_rung_and_tip_personality(pool: PgPool) {
        let female = || json!({"gender": "female"});
        for (kind, rung_one, wants, tipped) in [
            (
                EventKindDto::SawBag,
                "（你在逛街，随手看到一个挺好看的包。）",
                "（你在逛街，看到一个特别喜欢的包，很想要。）",
                "（你在逛街，看到一个特别喜欢的包，很想要。代入你「傲娇」的打赏人格，把这份心动告诉对方。）",
            ),
            (
                EventKindDto::SawClothes,
                "（你在逛街，随手看到一件挺好看的衣服。）",
                "（你在逛街，看到一件特别喜欢的衣服，很想要。）",
                "（你在逛街，看到一件特别喜欢的衣服，很想要。代入你「傲娇」的打赏人格，把这份心动告诉对方。）",
            ),
        ] {
            for (close, tip, want) in [
                (false, Some("傲娇"), rung_one),
                (true, None, wants),
                (true, Some("傲娇"), tipped),
            ] {
                let (e, message, cue) = draw_told(&pool, kind, female(), tip, close).await;
                assert!(!e.offline && message.is_some(), "{kind:?}");
                assert_eq!(cue.as_deref(), Some(want), "{kind:?} close={close} {tip:?}");
            }
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn out_shopping_is_offline_and_told_by_rung_only_for_a_female_persona(pool: PgPool) {
        let kind = EventKindDto::OutShopping;
        // She says she is going, from rung 2, and goes dark.
        let (e, message, cue) =
            draw_told(&pool, kind, json!({"gender": "female"}), None, true).await;
        assert!(e.offline && message.is_some());
        assert_eq!(
            cue.as_deref(),
            Some("（你接下来要出门逛街，这段时间顾不上看手机。）")
        );
        // Below rung 2 she goes dark without a word.
        let (e, message, cue) =
            draw_told(&pool, kind, json!({"gender": "female"}), None, false).await;
        assert!(e.offline && e.message_id.is_none() && message.is_none() && cue.is_none());
        // Anyone else keeps replying: she is shopping, not away, at any rung.
        for close in [false, true] {
            let (e, message, cue) =
                draw_told(&pool, kind, json!({"gender": "male"}), None, close).await;
            assert!(!e.offline && message.is_some(), "close={close}");
            assert_eq!(
                cue.as_deref(),
                Some("（你正在外面逛街。）"),
                "close={close}"
            );
        }
    }

    /// She has asked for space and waits for an answer: the ask is the
    /// foreground drama, so nothing is drawn for the instance, from any of
    /// its sessions, until it is answered or lapses.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_draw_while_a_choice_is_pending(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let sibling = seed_session(&pool, user_id, instance_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚被老板叫去加班，晚点聊").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let choice: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
             VALUES ($1, 'ask_for_space', 3, now() - interval '1 hour') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let temp_work = |key: &str| req(key, Some(vec![EventKindDto::TempWork]));

        // Pending: the asking session and its sibling both draw nothing, and
        // nothing is generated or written.
        for session in [session_id, sibling] {
            let out = draw_event_at(
                &state,
                session,
                user_id,
                temp_work("k1"),
                Utc::now(),
                rolls(0.5),
            )
            .await
            .unwrap();
            assert!(out.event.is_none() && out.message.is_none());
            assert_eq!(
                serde_json::to_value(&out).unwrap(),
                json!({"event": null, "message": null})
            );
        }
        assert!(calls(&mock).await.is_empty(), "no generation");
        assert_eq!(event_count(&pool, session_id).await, 0);
        assert_eq!(event_count(&pool, sibling).await, 0);
        assert_eq!(event_rows(&pool, session_id).await, 0);

        // Another persona's choice has no say over this one.
        let (_, elsewhere) = setup(&pool, user_id).await;
        assert!(draw_event_at(
            &state,
            elsewhere,
            user_id,
            temp_work("k1"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap()
        .event
        .is_some());

        // Lapsed (still `pending` in the table): the draw works again.
        sqlx::query(
            "UPDATE engine.life_choices SET asked_at = asked_at - interval '24 hours' \
              WHERE id = $1",
        )
        .bind(choice)
        .execute(&pool)
        .await
        .unwrap();
        let drawn = draw_event_at(
            &state,
            session_id,
            user_id,
            temp_work("k2"),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert_eq!(drawn.event.map(|e| e.kind), Some(EventKindDto::TempWork));
        assert!(drawn.message.is_some());
    }

    /// A key that already drew its event returns it even when a choice is
    /// pending: the replay reads, it draws nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_replayed_key_still_returns_its_event_while_a_choice_is_pending(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚被老板叫去加班，晚点聊").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let first = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::TempWork])),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
             VALUES ($1, 'ask_for_space', 3, now() - interval '1 minute')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        let again = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", None),
            Utc::now(),
            rolls(0.9),
        )
        .await
        .unwrap();
        assert_eq!(
            again.message.map(|m| m.message_id),
            first.message.map(|m| m.message_id)
        );
        assert!(again.event.is_some());
        assert_eq!(calls(&mock).await.len(), 1, "no second generation");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_with_image_carries_its_picture(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的，好看吗").await;
        mount_composer(&mock, "在海边对着镜头笑", "海边自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("an event");
        let m = out.message.expect("a message");
        assert_eq!(e.message_id, Some(m.message_id));

        let c = calls(&mock).await;
        assert_eq!(c.len(), 2, "the companion, then the composer");
        assert_eq!(last_content(&c[0]), "（你刚拍了一张自拍，发给了对方。）");
        let payload = last_content(&c[1]);
        assert!(payload.contains("[最近场景]\n"), "{payload}");
        assert!(
            payload.contains("AI: 刚拍的，好看吗\n\n[对方最新消息]\n（无）"),
            "{payload}"
        );
        assert!(payload.contains("[画幅]\n3:4"), "{payload}");

        let img = out.image_request.expect("an image request");
        assert_eq!(img.message_id, m.message_id);
        assert_eq!(img.image_ref.as_deref(), Some("face"));
        assert_eq!(img.aspect_ratio.as_deref(), Some("3:4"));
        use base64::Engine as _;
        let prompt = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(&img.composed_prompt)
                .unwrap(),
        )
        .unwrap();
        assert!(prompt.contains("在海边对着镜头笑"), "{prompt}");

        let marker = row_meta(&pool, m.message_id).await["image"].clone();
        assert_eq!(marker["caption"], json!("海边自拍"));
        assert_eq!(marker["image_ref"], json!("face"));
        let (source, status, _) = compose_row(&pool, &marker).await;
        assert_eq!((source.as_str(), status.as_str()), ("event", "ok"));
    }

    /// A lingering accepted give reaches her picture's `[身体状态]` and the
    /// audit `inputs.body_state`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_picture_hears_her_body_state(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'user', '', $2, now() - make_interval(mins => 1))",
        )
        .bind(session_id)
        .bind(json!({"action": {"type": "give", "item": "cigarette", "quantity": 1, "response": "accept"}}))
        .execute(&pool)
        .await
        .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的").await;
        mount_composer(&mock, "叼着烟自拍", "叼烟自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        let payload = last_content(&c[1]);
        assert!(
            payload.contains("[身体状态]\n1 支烟，烟劲很足，正在劲头上"),
            "{payload}"
        );
        let m = out.message.expect("a message");
        let marker = row_meta(&pool, m.message_id).await["image"].clone();
        let (_, _, inputs) = compose_row(&pool, &marker).await;
        assert_eq!(inputs["body_state"], json!("1 支烟，烟劲很足，正在劲头上"));
    }

    /// A selfie drawn during a trip is told from inside it, and still says
    /// she sent the picture.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_during_a_trip_is_sent_from_the_trip(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let started = Utc::now() - chrono::Duration::hours(2);
        EventRepo { pool: &pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: "trip",
                    kind: EventKind::ShortTrip,
                    started_at: started,
                    ends_at: started + chrono::Duration::hours(48),
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "在海边拍的").await;
        mount_composer(&mock, "海边自拍", "海边自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.image_request.is_some());
        let c = calls(&mock).await;
        assert_eq!(
            last_content(&c[0]),
            "（你正在短途旅行。你刚拍了一张自拍，发给了对方。）"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_without_image_is_words_only(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍了张自拍").await;
        mount_composer(&mock, "p", "c").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", false),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        assert_eq!(
            last_content(&c[0]),
            "（你刚拍了一张自拍，想发给对方看看。）"
        );
        assert!(out.image_request.is_none());
        let m = out.message.expect("a message");
        assert!(row_meta(&pool, m.message_id).await.get("image").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_without_a_composer_is_words_only(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍了张自拍").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        assert_eq!(
            last_content(&c[0]),
            "（你刚拍了一张自拍，想发给对方看看。）"
        );
        assert!(out.message.is_some() && out.image_request.is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn another_kind_with_image_gets_no_picture(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "要去加班了").await;
        mount_composer(&mock, "p", "c").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                image: Some(EventImageParams::default()),
                ..req("k1", Some(vec![EventKindDto::TempWork]))
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        assert_eq!(
            last_content(&c[0]),
            "（你刚临时接到一份工作，得马上去处理。）"
        );
        assert!(out.message.is_some() && out.image_request.is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_selfie_calls_no_composer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "").await;
        mount_composer(&mock, "p", "c").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none() && out.image_request.is_none());
        assert_eq!(calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_replayed_selfie_key_returns_its_picture(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的").await;
        mount_composer(&mock, "在窗边", "窗边自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let draw = || {
            draw_event_at(
                &state,
                session_id,
                user_id,
                selfie_req("k1", true),
                Utc::now(),
                rolls(0.5),
            )
        };
        let first = draw().await.unwrap();
        let again = draw().await.unwrap();
        assert!(first.image_request.is_some());
        assert_eq!(
            serde_json::to_value(&again.image_request).unwrap(),
            serde_json::to_value(&first.image_request).unwrap()
        );
        assert_eq!(calls(&mock).await.len(), 2, "no second generation");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_whose_composer_fails_still_carries_a_portrait(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的").await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("COMPOSE"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            selfie_req("k1", true),
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        assert!(out.image_request.is_some(), "the portrait fallback");
        let m = out.message.expect("a message");
        let marker = row_meta(&pool, m.message_id).await["image"].clone();
        assert!(marker.get("caption").is_none() && marker.get("compose_model").is_none());
        let (source, status, _) = compose_row(&pool, &marker).await;
        assert_eq!((source.as_str(), status.as_str()), ("event", "exhausted"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_selfie_without_a_recorded_compose_returns_no_image_request(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的").await;
        mount_composer(&mock, "在窗边", "窗边自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let draw = || {
            draw_event_at(
                &state,
                session_id,
                user_id,
                selfie_req("k1", true),
                Utc::now(),
                rolls(0.5),
            )
        };
        let first = draw().await.unwrap();
        let m = first.message.expect("a message");
        // As when the fail-open audit write did not land.
        sqlx::query(
            "UPDATE engine.chat_messages SET metadata = metadata #- '{image,compose_event_id}' \
             WHERE id = $1",
        )
        .bind(m.message_id)
        .execute(&pool)
        .await
        .unwrap();
        let again = draw().await.unwrap();
        assert_eq!(again.message.map(|m| m.message_id), Some(m.message_id));
        assert!(again.image_request.is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_untold_kind_with_image_composes_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        seed_user_messages(&pool, session_id, 10).await;
        rung_two(&pool, session_id, user_id, instance_id).await;
        let mock = MockServer::start().await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                image: Some(EventImageParams::default()),
                ..req("s1", Some(vec![EventKindDto::Asleep]))
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let e = out.event.expect("asleep");
        assert!(e.message_id.is_none() && out.message.is_none() && out.image_request.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_outfit_dresses_the_event(pool: PgPool) {
        // Told without a picture: her words follow it.
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "要去加班了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                outfit: Some("红色旗袍".into()),
                ..req("k1", Some(vec![EventKindDto::TempWork]))
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert!(
            c[0].to_string().contains("- 此刻的穿着：红色旗袍"),
            "{}",
            c[0]
        );
        let m = out.message.expect("a message");
        assert_eq!(
            row_meta(&pool, m.message_id).await["outfit"],
            json!("红色旗袍")
        );

        // With a picture: the composer dresses her in it too.
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚拍的").await;
        mount_composer(&mock, "穿着旗袍自拍", "旗袍自拍").await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                outfit: Some("红色旗袍".into()),
                ..selfie_req("k1", true)
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert!(
            last_content(&c[1]).contains("[此刻穿着]\n红色旗袍"),
            "{}",
            last_content(&c[1])
        );
        let m = out.message.expect("a message");
        let marker = row_meta(&pool, m.message_id).await["image"].clone();
        let (_, _, inputs) = compose_row(&pool, &marker).await;
        assert_eq!(inputs["clothing"], json!("红色旗袍"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_outfit_is_absent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "要去加班了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                outfit: Some("   ".into()),
                ..req("k1", Some(vec![EventKindDto::TempWork]))
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .unwrap();
        let c = calls(&mock).await;
        assert!(!c[0].to_string().contains("此刻的穿着"), "{}", c[0]);
        let m = out.message.expect("a message");
        assert!(row_meta(&pool, m.message_id).await.get("outfit").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unsupported_aspect_ratio_is_422(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        let state = with_companion_and_composer(test_state(pool.clone()), &mock.uri());
        let err = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                image: Some(EventImageParams {
                    aspect_ratio: Some("2:1".into()),
                    ..Default::default()
                }),
                ..req("k1", Some(vec![EventKindDto::TakingSelfie]))
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .expect_err("rejected");
        assert!(
            matches!(
                err,
                AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_long_outfit_is_422(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let err = draw_event_at(
            &state,
            session_id,
            user_id,
            DrawEventRequest {
                outfit: Some("字".repeat(65)),
                ..req("k1", None)
            },
            Utc::now(),
            rolls(0.5),
        )
        .await
        .expect_err("rejected");
        assert!(
            matches!(
                err,
                AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(calls(&mock).await.is_empty());
    }
}

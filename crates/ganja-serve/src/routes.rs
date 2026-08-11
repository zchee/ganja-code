//! The REST surface: every route, the request guard in front of them, and
//! the session-routing policy the session routes share.
//!
//! Spec: upstream `packages/opencode/src/server/routes/instance/httpapi/groups/*.ts`
//! (the paths) and `handlers/*.ts` (the behavior), on the legacy `/session/…`
//! spellings (`groups/session.ts:78-105`).
//!
//! # Session routing
//!
//! Upstream's server loads a session per request; this engine holds **one**
//! current session, the one every event names. A session route naming the
//! current session acts on it directly. Naming any other stored session
//! resumes it first — the same install `Engine::resume` gives a frontend —
//! and then acts: a `404` when nothing stored answers to the id, a `409`
//! when a turn is streaming, because the turn in flight is writing into the
//! session it started on and cannot be switched out from under
//! (deviation: one-session-at-a-time-routing).
//!
//! # The switch routes
//!
//! Upstream re-sends the agent and model with every prompt; ganja's engine
//! holds them as session state. Both spellings are served: optional `agent` /
//! `model` fields on the prompt body, and explicit `POST
//! /session/{id}/agent` / `…/model` routes (deviation:
//! switches-are-state-not-prompt-fields).

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use ganja_core::{EngineError, config::AgentMode};
use ganja_protocol::{Command, Mention, PermissionId, PermissionReply, SessionId};
use serde::Deserialize;

use crate::{
    auth::{self, WWW_AUTHENTICATE},
    error::ApiError,
    sse,
    state::AppState,
};

/// The header a client names its directory in — upstream's
/// `x-opencode-directory` (`middleware/workspace-routing.ts:87`), spelled in
/// this build's name the way every `GANJA_` variable is.
pub const DIRECTORY_HEADER: &str = "x-ganja-directory";

pub(crate) fn router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/global/health", get(health))
        .route("/config", get(config))
        .route("/path", get(path))
        .route("/agent", get(agents))
        .route("/command", get(commands))
        .route("/event", get(sse::events))
        .route("/session", get(list_sessions).post(create_session))
        .route("/session/{id}", get(get_session))
        // Upstream's synchronous prompt route answers with the finished
        // message; here both prompt spellings are the fire-and-forget `204`
        // and the reply arrives on `/event`, which is the one place a
        // transcript is truth anyway (deviation: prompts-are-asynchronous).
        .route("/session/{id}/message", get(messages).post(prompt))
        .route("/session/{id}/prompt_async", post(prompt))
        .route("/session/{id}/abort", post(abort))
        .route("/session/{id}/summarize", post(summarize))
        .route("/session/{id}/command", post(run_command))
        .route("/session/{id}/shell", post(run_shell))
        // Upstream's revert names a message and deletes; ganja's undo is
        // anchor-based and defers deletion until the next prompt makes it
        // permanent, so these translate to `Undo`/`Redo` rather than port a
        // payload the engine has no use for (deviation:
        // revert-is-anchor-based-undo).
        .route("/session/{id}/revert", post(revert))
        .route("/session/{id}/unrevert", post(unrevert))
        .route("/session/{id}/agent", post(switch_agent))
        .route("/session/{id}/model", post(switch_model))
        .route("/permission", get(list_permissions))
        .route("/permission/{id}/reply", post(reply_permission))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Every request passes here first: one log line that is **method and path
/// only** — never the query, which may carry `auth_token` — then the
/// credential when one is configured, then the directory.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    tracing::debug!(
        method = %request.method(),
        path = request.uri().path(),
        "serving"
    );

    if let Some(credentials) = &state.credentials
        && !auth::authorized(request.headers(), request.uri().query(), credentials)
    {
        // Upstream's challenge, empty-bodied
        // (`middleware/authorization.ts:90-98`).
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, WWW_AUTHENTICATE)
            .body(axum::body::Body::empty())
            .expect("a static header set always builds");
    }

    if let Some(refusal) = wrong_directory(&state, request.uri(), request.headers()) {
        return refusal.into_response();
    }

    next.run(request).await
}

/// The launch directory is the only directory served: a request naming
/// another — by `?directory=` or by [`DIRECTORY_HEADER`] — is refused rather
/// than silently answered about the wrong worktree. Upstream would load an
/// instance for it instead (`middleware/workspace-routing.ts:86-88`); this
/// engine was built in one directory and cannot (deviation:
/// other-directories-are-refused).
fn wrong_directory(state: &AppState, uri: &Uri, headers: &HeaderMap) -> Option<ApiError> {
    let asked = uri
        .query()
        .and_then(|query| auth::query_param(query, "directory"))
        .or_else(|| {
            headers
                .get(DIRECTORY_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })?;

    if state.directory.matches(&asked) {
        return None;
    }

    Some(ApiError::Invalid(format!(
        "this server serves {} and no other directory; {asked} is not it",
        state.directory.given().display()
    )))
}

/// `400` for a body that is not the JSON the route takes — the one refusal
/// the engine never sees.
fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::Invalid(format!("the payload does not parse: {error}")))
}

/// Installs `id` as the engine's current session when it is not already —
/// the module-level routing policy.
async fn ensure_session(state: &AppState, id: &str) -> Result<SessionId, ApiError> {
    let wanted = SessionId::from(id.to_owned());
    if state.engine.session_id() == wanted {
        return Ok(wanted);
    }

    match state.engine.resume(&wanted).await {
        Ok(_) => Ok(wanted),
        // An engine without storage holds nothing under any other name, and
        // "no stored session" is a 404 whichever refusal spelled it.
        Err(EngineError::Ephemeral) => {
            Err(ApiError::NotFound(format!("no stored session named {id}")))
        }
        Err(error) => Err(error.into()),
    }
}

/// `GET /global/health` (`groups/global.ts:76`).
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "healthy": true,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// `GET /config`: a named projection of the configuration the engine was
/// assembled from. Upstream serves its whole resolved config; ganja's
/// `Config` is deliberately deserialize-only — refusing unknown keys is its
/// contract — so the route serves the fields a remote client can act on
/// rather than invent a second serializer for the type (deviation:
/// config-route-is-a-projection).
async fn config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(config) = &state.config else {
        return Json(serde_json::json!({}));
    };

    Json(serde_json::json!({
        "model": config.model,
        "small_model": config.small_model,
        "default_agent": config.default_agent,
        "theme": config.theme,
        "instructions": config.instructions,
        "shell": config.shell,
        "snapshot": config.snapshot,
        "agent": config.agent.keys().collect::<Vec<_>>(),
        "command": config.command.keys().collect::<Vec<_>>(),
        "mcp": config.mcp.keys().collect::<Vec<_>>(),
    }))
}

/// `GET /path`: where this server is working — the served directory, the
/// project root it resolves into, and the data directory when one exists.
async fn path(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "directory": state.directory.given().display().to_string(),
        "root": state.root.display().to_string(),
        "data": state.data.as_ref().map(|data| data.display().to_string()),
    }))
}

/// `GET /agent`: the roster, in definition order, without the prompts — a
/// remote picker needs names and modes, and a prompt can run to pages.
async fn agents(State(state): State<AppState>) -> Json<serde_json::Value> {
    let listed: Vec<serde_json::Value> = state
        .engine
        .agents()
        .map(|registry| {
            registry
                .agents()
                .iter()
                .map(|agent| {
                    serde_json::json!({
                        "name": agent.name,
                        "description": agent.description,
                        "mode": match agent.mode {
                            AgentMode::Primary => "primary",
                            AgentMode::Subagent => "subagent",
                            AgentMode::All => "all",
                        },
                        "hidden": agent.hidden,
                        "model": agent.model,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(serde_json::Value::Array(listed))
}

/// `GET /command`: every command a session can run, sorted by name.
async fn commands(State(state): State<AppState>) -> Json<serde_json::Value> {
    let listed: Vec<serde_json::Value> = state
        .engine
        .commands()
        .commands()
        .iter()
        .map(|command| {
            serde_json::json!({
                "name": command.name,
                "description": command.description,
                "template": command.template,
                "agent": command.agent,
                "model": command.model,
            })
        })
        .collect();

    Json(serde_json::Value::Array(listed))
}

/// `GET /session` (`groups/session.ts:111`): every stored session, newest
/// first. An engine without storage stores nothing, so it lists nothing.
async fn list_sessions(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    match state.engine.sessions().await {
        Ok(sessions) => Ok(Json(
            serde_json::to_value(sessions).unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
        )),
        Err(EngineError::Ephemeral) => Ok(Json(serde_json::Value::Array(Vec::new()))),
        Err(error) => Err(error.into()),
    }
}

/// `POST /session` (`groups/session.ts:87`): points the engine at a fresh
/// session and answers its id. The stored row is minted by the first prompt,
/// which is when ganja sessions have always been born (deviation:
/// sessions-are-created-lazily).
async fn create_session(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.engine.send(Command::NewSession).await?;

    Ok(Json(serde_json::json!({
        "id": state.engine.session_id().as_str(),
    })))
}

/// `GET /session/{id}` (`groups/session.ts:81`): the stored row, read-only —
/// this route installs nothing.
async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(storage) = state.storage.clone() else {
        return Err(ApiError::NotFound(format!("no stored session named {id}")));
    };

    let wanted = SessionId::from(id.clone());
    let info = tokio::task::spawn_blocking(move || storage.load_info(&wanted))
        .await
        .expect("the session load neither panics nor is aborted")
        .map_err(|error| ApiError::Internal(error.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no stored session named {id}")))?;

    Ok(Json(serde_json::to_value(info).unwrap_or_default()))
}

/// `GET /session/{id}/message` (`groups/session.ts:85`): the stored
/// transcript, oldest first, read-only.
async fn messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(storage) = state.storage.clone() else {
        return Err(ApiError::NotFound(format!("no stored session named {id}")));
    };

    let wanted = SessionId::from(id.clone());
    let transcript = tokio::task::spawn_blocking(move || {
        if storage.load_info(&wanted)?.is_none() {
            return Ok(None);
        }
        storage.load_transcript(&wanted).map(Some)
    })
    .await
    .expect("the transcript load neither panics nor is aborted")
    .map_err(|error: ganja_core::StorageError| ApiError::Internal(error.to_string()))?
    .ok_or_else(|| ApiError::NotFound(format!("no stored session named {id}")))?;

    Ok(Json(serde_json::to_value(transcript).unwrap_or_default()))
}

/// What both prompt routes take: ganja's `SendPrompt` shape, with the
/// optional switches upstream carries per prompt (see the module docs).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptBody {
    text: String,
    #[serde(default)]
    mentions: Vec<Mention>,
    agent: Option<String>,
    model: Option<String>,
}

/// `POST /session/{id}/message` and `POST /session/{id}/prompt_async`
/// (`groups/session.ts:95-96`): accept, `204`, and the turn reports itself
/// on `/event`.
async fn prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: PromptBody = parse(&body)?;
    ensure_session(&state, &id).await?;

    if let Some(agent) = body.agent {
        state
            .engine
            .send(Command::SwitchAgent { name: agent })
            .await?;
    }
    if let Some(model) = body.model {
        state.engine.send(Command::SwitchModel { model }).await?;
    }
    state
        .engine
        .send(Command::SendPrompt {
            text: body.text,
            mentions: body.mentions,
        })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /session/{id}/abort` (`groups/session.ts:91`): stops the streaming
/// turn; a no-op `true` when the engine is idle, as upstream answers.
async fn abort(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<bool>, ApiError> {
    // Only the streaming session can be aborted: a mismatched id would need a
    // resume, and a resume is refused mid-turn — which is this policy saying
    // an abort names the turn it stops. `ensure_session` starts with exactly
    // that same-session check, so the busy case falls through it untouched.
    ensure_session(&state, SessionId::from(id).as_str()).await?;
    state.engine.send(Command::CancelTurn).await?;

    Ok(Json(true))
}

/// `POST /session/{id}/summarize` (`groups/session.ts:94`): ganja's
/// `Compact` — the summary streams as a turn on `/event`.
async fn summarize(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<bool>, ApiError> {
    ensure_session(&state, &id).await?;
    state.engine.send(Command::Compact).await?;

    Ok(Json(true))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    name: String,
    #[serde(default)]
    args: String,
}

/// `POST /session/{id}/command` (`groups/session.ts:97`).
async fn run_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: CommandBody = parse(&body)?;
    ensure_session(&state, &id).await?;
    state
        .engine
        .send(Command::RunCommand {
            name: body.name,
            args: body.args,
        })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellBody {
    command: String,
}

/// `POST /session/{id}/shell` (`groups/session.ts:98`): the `!` passthrough,
/// ungated for the same reason it is in a terminal — this is the person, not
/// the model, and the person authenticated at the door.
async fn run_shell(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: ShellBody = parse(&body)?;
    ensure_session(&state, &id).await?;
    state
        .engine
        .send(Command::RunShell {
            command: body.command,
        })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /session/{id}/revert` (`groups/session.ts:99`) as ganja's `Undo` —
/// see the router's note on the divergence.
async fn revert(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<bool>, ApiError> {
    ensure_session(&state, &id).await?;
    state.engine.send(Command::Undo).await?;

    Ok(Json(true))
}

/// `POST /session/{id}/unrevert` (`groups/session.ts:100`) as ganja's
/// `Redo`.
async fn unrevert(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<bool>, ApiError> {
    ensure_session(&state, &id).await?;
    state.engine.send(Command::Redo).await?;

    Ok(Json(true))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchAgentBody {
    name: String,
}

/// `POST /session/{id}/agent`: the explicit spelling of the prompt body's
/// `agent` field.
async fn switch_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: SwitchAgentBody = parse(&body)?;
    ensure_session(&state, &id).await?;
    state
        .engine
        .send(Command::SwitchAgent { name: body.name })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchModelBody {
    model: String,
}

/// `POST /session/{id}/model`: the explicit spelling of the prompt body's
/// `model` field.
async fn switch_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: SwitchModelBody = parse(&body)?;
    ensure_session(&state, &id).await?;
    state
        .engine
        .send(Command::SwitchModel { model: body.model })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /permission`: every request the engine is waiting on, oldest first —
/// what a client that just connected reads before it can answer anything.
async fn list_permissions(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(state.pending.list())
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
    )
}

/// The reply body, upstream's `{"response": …}`
/// (`groups/session.ts:74-76`), carrying ganja's reply spellings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyBody {
    response: PermissionReply,
}

/// `POST /permission/{id}/reply`: answers a dialog. A reply nothing is
/// waiting for is defined to be ignored — which is what a reply racing a
/// cancel becomes — so this cannot 404 on a race.
async fn reply_permission(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let body: ReplyBody = parse(&body)?;
    state
        .engine
        .send(Command::ReplyPermission {
            id: PermissionId::from(id),
            reply: body.response,
        })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

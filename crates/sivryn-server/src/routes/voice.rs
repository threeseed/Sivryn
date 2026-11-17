//! Voice channels map 1:1 to LiveKit rooms named after the channel ID.

use std::sync::OnceLock;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use livekit_api::access_token::{AccessToken, VideoGrants};
use serde::Deserialize;
use sivryn_protocol::api::{VoiceChannelState, VoiceJoinResponse, VoiceParticipant};
use sivryn_protocol::call::{ModerateAction, ModerateRequest, MuteAllResponse};
use sivryn_protocol::gateway::{Audience, Event};
use sivryn_protocol::permissions::Permissions;
use sivryn_protocol::{ChannelId, GuildId, UserId};

use crate::AppState;
use crate::auth::AuthUser;
use crate::db;
use crate::error::{ApiError, ApiResult};

/// Participant attribute a client sets to "1" while its sound is off.
pub const DEAFENED_ATTRIBUTE: &str = "deafened";
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
const LIVEKIT_API_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn join(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(channel_id): Path<ChannelId>,
) -> ApiResult<Json<VoiceJoinResponse>> {
    let channel = db::require_channel(&state.db, channel_id, auth.id, Permissions::CONNECT).await?;
    if !channel.kind().has_calls() {
        return Err(ApiError::BadRequest("this channel has no calls".into()));
    }

    // DMs and groups allow everything a call needs; guilds use the member's
    // permissions in this channel.
    let perms = match channel.guild() {
        Some(_) => db::channel_permissions_of(&state.db, &channel, auth.id).await?.ok_or(ApiError::NotFound)?,
        None => Permissions::SPEAK | Permissions::VIDEO | Permissions::SCREEN_SHARE,
    };
    let user = db::user(&state.db, auth.id).await?;

    // Guild permissions decide which track sources LiveKit lets this user publish.
    let mut sources = Vec::new();
    if perms.allows(Permissions::SPEAK) {
        sources.push("microphone".to_owned());
    }
    if perms.allows(Permissions::VIDEO) {
        sources.push("camera".to_owned());
    }
    if perms.allows(Permissions::SCREEN_SHARE) {
        sources.extend(["screen_share".to_owned(), "screen_share_audio".to_owned()]);
    }

    let grants = VideoGrants {
        room_join: true,
        room: channel_id.to_string(),
        can_publish: Some(!sources.is_empty()),
        can_publish_sources: sources,
        can_subscribe: Some(true),
        can_publish_data: Some(true),
        // Clients announce whether they turned their sound off as an attribute.
        can_update_own_metadata: Some(true),
        ..Default::default()
    };

    let token = AccessToken::with_api_key(&state.config.livekit.api_key, &state.config.livekit.api_secret)
        .with_identity(&auth.id.to_string())
        .with_name(user.display_name.as_deref().unwrap_or(&user.username))
        .with_ttl(TOKEN_TTL)
        .with_grants(grants)
        .to_jwt()
        .map_err(|e| anyhow::anyhow!("signing LiveKit token: {e}"))?;

    // Guilds choose their voice bitrate; DMs and groups use the default.
    let audio_bitrate = match channel.guild() {
        Some(guild) => db::guild(&state.db, guild).await?.audio_bitrate,
        None => sivryn_protocol::model::default_audio_bitrate(),
    };

    // Ring the other members when a DM or group call starts.
    if channel.guild().is_none() {
        let others: Vec<UserId> =
            db::channel_member_ids(&state.db, channel_id).await?.into_iter().filter(|u| *u != auth.id).collect();
        let already_in_call = list_participants(&state, channel_id).await.map(|p| !p.is_empty()).unwrap_or(false);
        if !already_in_call && !others.is_empty() {
            state.events.publish(Audience::Users(others), Event::CallRing { channel_id, user_id: auth.id });
        }
    }

    Ok(Json(VoiceJoinResponse {
        channel_id,
        livekit_url: state.config.livekit.public_url.clone(),
        token,
        screen_share: state.config.screen_share.clone(),
        camera: state.config.camera.clone(),
        e2ee: channel.e2ee,
        audio_bitrate,
    }))
}

/// Lists who is connected to each voice channel of a guild, straight from
/// LiveKit (rooms are named after channel IDs). A LiveKit outage shows up as
/// empty channels rather than an error, so the rest of the UI keeps working.
pub async fn guild_states(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(guild_id): Path<GuildId>,
) -> ApiResult<Json<Vec<VoiceChannelState>>> {
    let member = db::membership(&state.db, guild_id, auth.id).await?.ok_or(ApiError::NotFound)?;
    let overwrites = db::guild_overwrites(&state.db, guild_id).await?;
    let channels: Vec<db::ChannelRow> =
        sqlx::query_as("SELECT * FROM channels WHERE guild_id = $1 AND kind = 'voice' ORDER BY position, id")
            .bind(guild_id.0)
            .fetch_all(&state.db)
            .await?;
    // Only channels this member can see.
    let visible: Vec<uuid::Uuid> = channels
        .iter()
        .filter(|c| member.in_channel(auth.id, &overwrites, c).allows(Permissions::VIEW_CHANNEL))
        .map(|c| c.id)
        .collect();

    let lookups = visible.into_iter().map(|id| {
        let state = state.clone();
        async move {
            let channel_id = ChannelId(id);
            let participants = match list_participants(&state, channel_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, %channel_id, "could not list LiveKit participants");
                    Vec::new()
                }
            };
            VoiceChannelState { channel_id, participants }
        }
    });
    Ok(Json(futures::future::join_all(lookups).await))
}

#[derive(Deserialize)]
struct ListParticipantsResponse {
    #[serde(default)]
    participants: Vec<ParticipantInfo>,
}

#[derive(Deserialize)]
struct ParticipantInfo {
    identity: String,
    #[serde(default)]
    tracks: Vec<TrackInfo>,
    #[serde(default)]
    attributes: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct TrackInfo {
    #[serde(default)]
    sid: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    muted: bool,
}

/// Calls a LiveKit Twirp `RoomService` method with JSON encoding as room
/// admin. `None` when the room (or participant) doesn't exist.
async fn room_service(
    state: &AppState,
    channel: ChannelId,
    method: &str,
    body: serde_json::Value,
) -> anyhow::Result<Option<reqwest::Response>> {
    let lk = &state.config.livekit;
    let room = channel.to_string();
    let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
        .with_ttl(Duration::from_secs(60))
        .with_grants(VideoGrants { room_admin: true, room, ..Default::default() })
        .to_jwt()?;
    let resp = http()
        .post(format!("{}/twirp/livekit.RoomService/{method}", lk.api_url))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?))
}

/// The room's participants as LiveKit reports them (empty if no room).
async fn raw_participants(state: &AppState, channel: ChannelId) -> anyhow::Result<Vec<ParticipantInfo>> {
    let room = channel.to_string();
    let Some(resp) = room_service(state, channel, "ListParticipants", serde_json::json!({ "room": room })).await?
    else {
        // A room that doesn't exist yet simply has nobody in it.
        return Ok(Vec::new());
    };
    let body: ListParticipantsResponse = resp.json().await?;
    Ok(body.participants)
}

/// Who is in a channel's call, from LiveKit.
async fn list_participants(state: &AppState, channel: ChannelId) -> anyhow::Result<Vec<VoiceParticipant>> {
    Ok(raw_participants(state, channel)
        .await?
        .into_iter()
        .filter_map(|p| {
            let user_id = UserId(p.identity.parse().ok()?);
            let has = |source: &str| p.tracks.iter().any(|t| t.source.as_deref() == Some(source) && !t.muted);
            Some(VoiceParticipant {
                user_id,
                muted: !has("MICROPHONE"),
                deafened: p.attributes.get(DEAFENED_ATTRIBUTE).is_some_and(|v| v == "1"),
                camera: has("CAMERA"),
                screen_sharing: has("SCREEN_SHARE"),
            })
        })
        .collect())
}

/// Who is in one channel's call (DMs, groups or a single voice channel).
pub async fn channel_state(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(channel_id): Path<ChannelId>,
) -> ApiResult<Json<VoiceChannelState>> {
    let channel = db::require_channel(&state.db, channel_id, auth.id, Permissions::VIEW_CHANNEL).await?;
    if !channel.kind().has_calls() {
        return Err(ApiError::BadRequest("this channel has no calls".into()));
    }
    let participants = list_participants(&state, channel_id).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, %channel_id, "could not list LiveKit participants");
        Vec::new()
    });
    Ok(Json(VoiceChannelState { channel_id, participants }))
}

/// Disconnects someone from a guild voice channel (`MOVE_MEMBERS`).
pub async fn disconnect(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((channel_id, user_id)): Path<(ChannelId, UserId)>,
) -> ApiResult<StatusCode> {
    let channel = db::channel(&state.db, channel_id).await?;
    let guild = channel.guild().ok_or(ApiError::Forbidden)?;
    db::require(&state.db, guild, auth.id, Permissions::MOVE_MEMBERS).await?;
    remove_participant(&state, channel_id, user_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// LiveKit track sources a moderator can mute, per action.
fn sources_for(action: ModerateAction) -> &'static [&'static str] {
    match action {
        ModerateAction::Mute => &["MICROPHONE"],
        ModerateAction::StopCamera => &["CAMERA"],
        ModerateAction::StopShare => &["SCREEN_SHARE", "SCREEN_SHARE_AUDIO"],
        ModerateAction::AllowSpeak | ModerateAction::RevokeSpeak => &[],
    }
}

/// Checks that `user` may moderate the call in a guild channel (`MUTE_MEMBERS`
/// there). DM and group calls have no moderators.
async fn require_moderator(state: &AppState, channel_id: ChannelId, user: UserId) -> ApiResult<()> {
    let channel = db::channel(&state.db, channel_id).await?;
    if channel.guild().is_none() || !channel.kind().has_calls() {
        return Err(ApiError::Forbidden);
    }
    let perms = db::channel_permissions_of(&state.db, &channel, user).await?.ok_or(ApiError::NotFound)?;
    if !perms.allows(Permissions::MUTE_MEMBERS) {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Mutes the given sources of one participant.
async fn mute_sources(
    state: &AppState,
    channel: ChannelId,
    p: &ParticipantInfo,
    sources: &[&str],
) -> anyhow::Result<u32> {
    let mut muted = 0;
    for track in p.tracks.iter().filter(|t| !t.muted && t.source.as_deref().is_some_and(|s| sources.contains(&s))) {
        let body = serde_json::json!({
            "room": channel.to_string(),
            "identity": p.identity,
            "track_sid": track.sid,
            "muted": true,
        });
        if room_service(state, channel, "MutePublishedTrack", body).await?.is_some() {
            muted += 1;
        }
    }
    Ok(muted)
}

/// What a participant may publish when a moderator allows or revokes speaking.
fn speak_permission(allowed: bool) -> serde_json::Value {
    let sources: Vec<&str> =
        if allowed { vec!["MICROPHONE", "CAMERA", "SCREEN_SHARE", "SCREEN_SHARE_AUDIO"] } else { Vec::new() };
    serde_json::json!({
        "can_subscribe": true,
        "can_publish": allowed,
        "can_publish_data": true,
        "can_update_metadata": true,
        "can_publish_sources": sources,
    })
}

/// A moderator mutes, stops the camera or share of, or lets speak / silences
/// someone in a guild call (`MUTE_MEMBERS`).
pub async fn moderate(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((channel_id, user_id)): Path<(ChannelId, UserId)>,
    Json(req): Json<ModerateRequest>,
) -> ApiResult<StatusCode> {
    require_moderator(&state, channel_id, auth.id).await?;
    let participants = raw_participants(&state, channel_id).await.map_err(livekit_error)?;
    let target = participants.iter().find(|p| p.identity == user_id.to_string()).ok_or(ApiError::NotFound)?;
    match req.action {
        ModerateAction::AllowSpeak | ModerateAction::RevokeSpeak => {
            let body = serde_json::json!({
                "room": channel_id.to_string(),
                "identity": target.identity,
                "permission": speak_permission(req.action == ModerateAction::AllowSpeak),
            });
            room_service(&state, channel_id, "UpdateParticipant", body).await.map_err(livekit_error)?;
        }
        action => {
            mute_sources(&state, channel_id, target, sources_for(action)).await.map_err(livekit_error)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Mutes every microphone in a guild call except the caller's (`MUTE_MEMBERS`).
pub async fn mute_all(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(channel_id): Path<ChannelId>,
) -> ApiResult<Json<MuteAllResponse>> {
    require_moderator(&state, channel_id, auth.id).await?;
    let me = auth.id.to_string();
    let mut muted = 0;
    for p in raw_participants(&state, channel_id).await.map_err(livekit_error)?.iter().filter(|p| p.identity != me) {
        muted += mute_sources(&state, channel_id, p, sources_for(ModerateAction::Mute)).await.map_err(livekit_error)?;
    }
    Ok(Json(MuteAllResponse { muted }))
}

fn livekit_error(e: anyhow::Error) -> ApiError {
    tracing::warn!(error = %e, "LiveKit room service call failed");
    ApiError::from(e)
}

/// Removes a user from a LiveKit room (best effort; absent users are fine).
pub async fn remove_participant(state: &AppState, channel: ChannelId, user: UserId) {
    let result: anyhow::Result<()> = async {
        let lk = &state.config.livekit;
        let room = channel.to_string();
        let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
            .with_ttl(Duration::from_secs(60))
            .with_grants(VideoGrants { room_admin: true, room: room.clone(), ..Default::default() })
            .to_jwt()?;
        let resp = http()
            .post(format!("{}/twirp/livekit.RoomService/RemoveParticipant", lk.api_url))
            .bearer_auth(token)
            .json(&serde_json::json!({ "room": room, "identity": user.to_string() }))
            .send()
            .await?;
        if resp.status() != reqwest::StatusCode::NOT_FOUND {
            resp.error_for_status()?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::debug!(error = %e, %channel, %user, "could not remove LiveKit participant");
    }
}

/// Ends a voice channel's LiveKit room (the channel was deleted).
pub async fn close_room(state: &AppState, channel: ChannelId) {
    let result: anyhow::Result<()> = async {
        let lk = &state.config.livekit;
        let room = channel.to_string();
        let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
            .with_ttl(Duration::from_secs(60))
            .with_grants(VideoGrants { room_create: true, room: room.clone(), ..Default::default() })
            .to_jwt()?;
        let resp = http()
            .post(format!("{}/twirp/livekit.RoomService/DeleteRoom", lk.api_url))
            .bearer_auth(token)
            .json(&serde_json::json!({ "room": room }))
            .send()
            .await?;
        if resp.status() != reqwest::StatusCode::NOT_FOUND {
            resp.error_for_status()?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::debug!(error = %e, %channel, "could not close LiveKit room");
    }
}

fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| reqwest::Client::builder().timeout(LIVEKIT_API_TIMEOUT).build().expect("HTTP client builds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moderation_sources() {
        assert_eq!(sources_for(ModerateAction::Mute), ["MICROPHONE"]);
        assert_eq!(sources_for(ModerateAction::StopShare), ["SCREEN_SHARE", "SCREEN_SHARE_AUDIO"]);
        assert!(sources_for(ModerateAction::AllowSpeak).is_empty());
    }

    #[test]
    fn speak_permissions() {
        let allowed = speak_permission(true);
        assert_eq!(allowed["can_publish"], true);
        assert_eq!(allowed["can_publish_sources"].as_array().unwrap().len(), 4);
        let revoked = speak_permission(false);
        assert_eq!(revoked["can_publish"], false);
        assert!(revoked["can_publish_sources"].as_array().unwrap().is_empty());
        // Clients must still be able to send hand raises and their deafen state.
        assert_eq!(revoked["can_publish_data"], true);
        assert_eq!(revoked["can_update_metadata"], true);
    }
}

//! Calls: voice channels and DM calls, the participant grid, controls,
//! incoming calls, the AFK move and the screen picker.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus_native::prelude::dioxus_core::spawn_forever;
use dioxus_native::prelude::*;
use dioxus_native::use_wgpu;
use sivryn_client_core::protocol::ChannelId;
use sivryn_client_core::protocol::call::{CallSignal, ModerateAction};
use sivryn_client_core::protocol::media::{ContentHint, VideoCodec};
use sivryn_client_core::protocol::permissions::Permissions;
use sivryn_media::{
    CallEvent, CameraPreview, CameraShare, ConnectionQuality, LoopbackTarget, ScreenAudio, ScreenShare, ScreenSource,
    Sound, SourceKind, SourcePreviews, TrackSource, VideoFeed, VideoQuality, VoiceCall, list_sources,
    screen_volume_key, window_process,
};

use super::icons::{Icon, Svg};
use super::media::{FrameCanvas, Frames};
use super::widgets::{Avatar, IconButton, Toggle};
use super::{AppState, CallReaction, CallState, Focus, Modal, Participant, Services};
use crate::chat;
use crate::i18n::t;
use crate::settings::{CallLayout, SCREEN_FPS, ScreenResolution};
use crate::video_surface::{SharedView, VideoSurface, View};

/// Joins a voice channel and shows it, after asking when that means leaving
/// another one (unless the user turned the question off).
pub fn join_channel(mut state: AppState, channel: ChannelId) {
    let elsewhere = state.call.peek().as_ref().is_some_and(|c| c.channel != channel);
    if elsewhere && state.settings.peek().confirm_voice_switch {
        state.modal.set(Some(Modal::SwitchVoice(channel)));
        return;
    }
    show_and_join(state, channel);
}

fn show_and_join(mut state: AppState, channel: ChannelId) {
    state.home.set(false);
    if let Some(g) = state.guild_of(channel) {
        state.guild.set(Some(g));
    }
    state.channel.set(Some(channel));
    if !state.call.peek().as_ref().is_some_and(|c| c.channel == channel) {
        start(state, channel, false);
    }
}

/// "Join this voice channel?", for a channel mentioned in the chat.
#[component]
pub fn JoinVoiceDialog(channel: ChannelId) -> Element {
    let mut state = use_context::<AppState>();
    let name = state.channel_name(channel);
    let join = move |_| {
        state.modal.set(None);
        show_and_join(state, channel);
    };
    rsx! {
        div { class: "overlay", onclick: move |_| state.modal.set(None),
            div { class: "dialog", onclick: move |e| e.stop_propagation(),
                div { class: "dialog-header",
                    h2 { {t("call.join_title")} }
                    IconButton { icon: Icon::X, size: 18, onclick: move |_| state.modal.set(None) }
                }
                p { class: "muted", "„{name}“ {t(\"call.join_text\")}" }
                div { class: "dialog-actions",
                    button { class: "button ghost", onclick: move |_| state.modal.set(None), {t("common.cancel")} }
                    button { class: "button primary", autofocus: "true", onclick: join, {t("call.join")} }
                }
            }
        }
    }
}

/// "You're in another voice channel. Switch?"
#[component]
pub fn SwitchVoiceDialog(channel: ChannelId) -> Element {
    let mut state = use_context::<AppState>();
    let mut dont_ask = use_signal(|| false);
    let name = state.channel_name(channel);
    let current = state.call.read().as_ref().map(|c| state.channel_name(c.channel)).unwrap_or_default();
    let switch = move |_| {
        if *dont_ask.peek() {
            state.update_settings(|s| s.confirm_voice_switch = false);
        }
        state.modal.set(None);
        show_and_join(state, channel);
    };
    rsx! {
        div { class: "overlay", onclick: move |_| state.modal.set(None),
            div { class: "dialog", onclick: move |e| e.stop_propagation(),
                div { class: "dialog-header",
                    h2 { {t("call.switch_title")} }
                    IconButton { icon: Icon::X, size: 18, onclick: move |_| state.modal.set(None) }
                }
                p { class: "muted", "{t(\"call.switch_text_1\")} „{current}“{t(\"call.switch_text_2\")} „{name}“ {t(\"call.switch_text_3\")}" }
                div { class: "check-row", onclick: move |_| dont_ask.toggle(),
                    div { class: if *dont_ask.read() { "checkbox on" } else { "checkbox" },
                        if *dont_ask.read() { Svg { icon: Icon::Check, size: 13 } }
                    }
                    span { {t("call.dont_ask_again")} }
                }
                div { class: "dialog-actions",
                    button { class: "button ghost", onclick: move |_| state.modal.set(None), {t("common.cancel")} }
                    button { class: "button primary", autofocus: "true", onclick: switch, {t("call.switch_action")} }
                }
            }
        }
    }
}

/// A voice channel: its call (or who is there while not connected), with
/// its chat at the side when opened.
#[component]
pub fn VoiceArea(channel: ChannelId) -> Element {
    let state = use_context::<AppState>();
    let in_call = state.call.read().as_ref().is_some_and(|c| c.channel == channel);
    let chat = *state.voice_chat.read();
    rsx! {
        div { class: "voice-area",
            if in_call {
                CallView {}
            } else {
                VoiceIdle { channel }
            }
            if chat {
                div { class: "voice-chat-panel", style: "width: {super::resize::width(&state, super::resize::Pane::VoiceChat)}px;",
                    for c in [channel] {
                        super::chat_view::ChatView { key: "{c}", channel: c, side: true }
                    }
                    // Last: Blitz paints and hit-tests later siblings over it otherwise.
                    super::resize::ResizeHandle { pane: super::resize::Pane::VoiceChat }
                }
            }
        }
    }
}

/// A voice channel you aren't connected to (connecting, or only reading its chat).
#[component]
fn VoiceIdle(channel: ChannelId) -> Element {
    let state = use_context::<AppState>();
    let name = state.channel_name(channel);
    let present = state.voice.read().get(&channel).cloned().unwrap_or_default();
    let connecting = *state.connecting.read() == Some(channel);
    rsx! {
        div { class: "join",
            div { class: "join-card",
                div { class: "join-icon", Svg { icon: Icon::Volume, size: 34 } }
                h2 { "{name}" }
                if present.is_empty() {
                    p { class: "muted", {t("call.nobody_here")} }
                } else {
                    div { class: "join-present",
                        for p in present.iter() {
                            Avatar { key: "{p.user_id}", name: state.username(p.user_id), id: Some(p.user_id), size: 32 }
                        }
                    }
                    p { class: "muted", "{present.len()} {t(\"call.people_here\")}" }
                }
                button {
                    class: if connecting { "button primary large busy" } else { "button primary large" },
                    onclick: move |_| join_channel(state, channel),
                    if connecting { {t("call.connecting")} } else { {t("call.join")} }
                }
                if !connecting {
                    button {
                        class: "button ghost",
                        onclick: move |_| {
                            let mut state = state;
                            state.modal.set(Some(Modal::PreJoin(channel)));
                        },
                        Svg { icon: Icon::Video, size: 16 }
                        {t("call.preview")}
                    }
                }
            }
        }
    }
}

async fn connect(mut state: AppState, channel: ChannelId, engine: sivryn_media::AudioEngine) -> Result<(), String> {
    state.connecting.set(Some(channel));
    let result = connect_inner(state, channel, engine).await;
    state.connecting.set(None);
    result
}

async fn connect_inner(
    mut state: AppState,
    channel: ChannelId,
    engine: sivryn_media::AudioEngine,
) -> Result<(), String> {
    let Some(s) = state.session.cloned() else { return Ok(()) };
    if let Some(old) = state.call.take() {
        leave(old).await;
    }
    let join = s.api.join_voice(channel).await.map_err(|e| e.to_string())?;
    let media_key = if join.e2ee {
        if !chat::ensure_group(&s, channel).await? {
            return Err(t("call.waiting_for_key").into());
        }
        Some(s.mls.borrow().media_key(channel).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let voices = engine.clone();
    let (mut call, mut events) = VoiceCall::join(&join, media_key, engine).await.map_err(|e| e.to_string())?;
    if let Err(e) = call.enable_microphone().await {
        state.report(Err(format!("{}: {e}", t("call.mic_unavailable"))));
    }
    // The AFK channel keeps the microphone off.
    let silenced = *state.muted.peek() || *state.deafened.peek() || state.is_afk_channel(channel);
    call.set_muted(silenced);
    let deafened = *state.deafened.peek();
    if deafened {
        call.set_deafened(true);
    }

    let identity = call.identity();
    let me = Participant {
        identity: identity.clone(),
        name: s.user.username.clone(),
        muted: silenced,
        deafened,
        sharing: false,
    };
    state.call.set(Some(CallState {
        channel,
        join,
        call: Rc::new(RefCell::new(Some(call))),
        share: Rc::new(RefCell::new(None)),
        share_audio: Rc::new(RefCell::new(None)),
        camera: Rc::new(RefCell::new(None)),
        identity,
        participants: vec![me],
        feeds: Vec::new(),
        speakers: Vec::new(),
        sharing: false,
        camera_on: false,
        focus: None,
        auto_focus: true,
        strip_hidden: false,
        hands: Vec::new(),
        reactions: Vec::new(),
        quality: Default::default(),
        reconnecting: false,
        can_publish: true,
        stats_open: false,
        stats: None,
        last_speaker: None,
        page: 0,
        wanted: Default::default(),
    }));
    spawn_forever(state.refresh_voice());

    // Who is speaking, from the audio itself: your own voice detection and
    // the voices being played. The server's report lags by half a second.
    let me = state.call.peek().as_ref().map(|c| c.identity.clone()).unwrap_or_default();
    spawn_forever(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let mut now = voices.speaking();
            if voices.transmitting() {
                now.push(me.clone());
            }
            now.sort();
            let changed = match state.call.peek().as_ref() {
                Some(c) if c.channel == channel => c.speakers != now,
                _ => break,
            };
            if changed && let Some(c) = state.call.write().as_mut() {
                // The speaker layout keeps the last person who spoke large
                // (someone else than you, unless you are alone).
                if let Some(other) = now.iter().find(|p| **p != c.identity) {
                    c.last_speaker = Some(other.clone());
                }
                c.speakers = now;
            }
        }
    });

    spawn_forever(async move {
        while let Some(event) = events.recv().await {
            // Look up names before taking the write lock on the call.
            let joined_name = match &event {
                CallEvent::ParticipantJoined { identity, name } => {
                    Some(identity.parse().ok().map(|id| state.username(id)).unwrap_or_else(|| name.clone()))
                }
                _ => None,
            };
            let event = match on_call_event(state, channel, event) {
                Some(e) => e,
                None => continue,
            };
            let mut call = state.call.write();
            let Some(c) = call.as_mut().filter(|c| c.channel == channel) else { break };
            match event {
                CallEvent::VideoAdded(feed) => c.feeds.push(feed),
                CallEvent::VideoRemoved { track_sid } => {
                    c.feeds.retain(|f| f.track_sid != track_sid);
                    if c.focus == Some(Focus::Screen(track_sid.clone())) {
                        c.focus = None;
                    }
                }
                CallEvent::ParticipantJoined { identity, .. } => {
                    if !c.participants.iter().any(|p| p.identity == identity) {
                        let name = joined_name.unwrap_or_default();
                        c.participants.push(Participant {
                            identity,
                            name,
                            muted: false,
                            deafened: false,
                            sharing: false,
                        });
                    }
                }
                CallEvent::ParticipantLeft { identity } => {
                    c.hands.retain(|h| *h != identity);
                    c.quality.remove(&identity);
                    c.participants.retain(|p| p.identity != identity);
                }
                CallEvent::Quality { identity, quality } => {
                    c.quality.insert(identity, quality);
                }
                CallEvent::Reconnecting => c.reconnecting = true,
                CallEvent::Reconnected => c.reconnecting = false,
                // Handled in `on_call_event`.
                CallEvent::Signal { .. } | CallEvent::LocalMuted { .. } | CallEvent::CanPublish(_) => {}
                CallEvent::MuteChanged { identity, muted } => {
                    if let Some(p) = c.participants.iter_mut().find(|p| p.identity == identity) {
                        p.muted = muted;
                    }
                }
                CallEvent::DeafenChanged { identity, deafened } => {
                    if let Some(p) = c.participants.iter_mut().find(|p| p.identity == identity) {
                        p.deafened = deafened;
                    }
                }
                CallEvent::ShareChanged { identity, sharing } => {
                    if let Some(p) = c.participants.iter_mut().find(|p| p.identity == identity) {
                        p.sharing = sharing;
                    }
                }
                CallEvent::Disconnected => {
                    *call = None;
                    drop(call);
                    state.toast(t("call.disconnected"), true);
                    spawn_forever(state.refresh_voice());
                    break;
                }
            }
        }
    });
    Ok(())
}

pub async fn leave(call: CallState) {
    let camera = call.camera.borrow_mut().take();
    if let Some(camera) = camera {
        camera.stop().await;
    }
    let audio = call.share_audio.borrow_mut().take();
    if let Some(audio) = audio {
        audio.stop().await;
    }
    let share = call.share.borrow_mut().take();
    if let Some(share) = share {
        share.stop().await;
    }
    let vc = call.call.borrow_mut().take();
    if let Some(vc) = vc {
        vc.leave().await;
    }
}

pub fn hang_up(mut state: AppState) {
    if let Some(call) = state.call.take() {
        // The call view unmounts right away; leaving must still finish.
        spawn_forever(async move {
            leave(call).await;
            state.refresh_voice().await;
        });
    }
}

pub fn stop_share(mut state: AppState) {
    let parts = state.call.write().as_mut().map(|c| {
        c.sharing = false;
        if let Some(share) = c.share.borrow().as_ref() {
            let sid = share.preview.track_sid.clone();
            c.feeds.retain(|f| f.track_sid != sid);
        }
        let me = c.identity.clone();
        if let Some(p) = c.participants.iter_mut().find(|p| p.identity == me) {
            p.sharing = false;
        }
        (c.share.borrow_mut().take(), c.share_audio.borrow_mut().take())
    });
    if let Some((share, audio)) = parts {
        if share.is_some() {
            super::use_services_engine().play(Sound::ShareStop);
        }
        spawn_forever(async move {
            if let Some(audio) = audio {
                audio.stop().await;
            }
            if let Some(share) = share {
                share.stop().await;
            }
        });
    }
}

pub fn toggle_camera(mut state: AppState) {
    let Some(c) = state.call.cloned() else { return };
    if c.camera_on {
        let camera = c.camera.borrow_mut().take();
        if let Some(call) = state.call.write().as_mut() {
            call.camera_on = false;
            if let Some(cam) = &camera {
                let sid = cam.preview.track_sid.clone();
                call.feeds.retain(|f| f.track_sid != sid);
            }
        }
        if let Some(camera) = camera {
            spawn_forever(camera.stop());
        }
        return;
    }
    let Some(room) = c.call.borrow().as_ref().map(|vc| vc.room.clone()) else { return };
    let video = state.settings.peek().video.clone();
    let limits = c.join.camera.clone();
    if let Some(call) = state.call.write().as_mut() {
        call.camera_on = true;
    }
    spawn_forever(async move {
        match CameraShare::start(room, video.camera.as_deref(), video.camera_quality, &limits, video.low_light).await {
            Ok(camera) => {
                let preview = camera.preview.clone();
                *c.camera.borrow_mut() = Some(camera);
                if let Some(call) = state.call.write().as_mut() {
                    call.feeds.push(preview);
                }
            }
            Err(e) => {
                if let Some(call) = state.call.write().as_mut() {
                    call.camera_on = false;
                }
                let reason = match e {
                    sivryn_media::MediaError::NoCamera => t("video.no_camera").to_string(),
                    sivryn_media::MediaError::NoMediaFoundation => t("video.no_mf").to_string(),
                    other => other.to_string(),
                };
                state.report(Err(format!("{}: {reason}", t("call.camera_failed"))));
            }
        }
    });
}

/// Handles call events that need more than updating the call state (toasts,
/// stopping your own camera). Returns the event when the caller should still
/// apply it to the call state.
fn on_call_event(mut state: AppState, channel: ChannelId, event: CallEvent) -> Option<CallEvent> {
    let me = state.call.peek().as_ref().filter(|c| c.channel == channel).map(|c| c.identity.clone())?;
    let names = state;
    let name_of = move |identity: &str| -> String {
        let guild = names.guild_of(channel);
        identity.parse().ok().map(|id| names.display_name_in(guild, id)).unwrap_or_else(|| identity.to_owned())
    };
    match event {
        CallEvent::Signal { identity, signal } => {
            match signal {
                CallSignal::Hand { raised } => {
                    if let Some(c) = state.call.write().as_mut() {
                        c.hands.retain(|h| *h != identity);
                        if raised {
                            c.hands.push(identity.clone());
                        }
                    }
                    if raised {
                        state.toast(format!("✋ {} {}", name_of(&identity), t("call.hand_raised")), false);
                    }
                }
                CallSignal::Reaction { emoji } => show_reaction(state, identity, emoji),
                CallSignal::AskUnmute { target } if target == me => {
                    // Only someone who may moderate this channel can ask.
                    state.toast(format!("{} {}", name_of(&identity), t("call.asked_unmute")), false);
                }
                CallSignal::AskUnmute { .. } => {}
            }
            None
        }
        CallEvent::MuteChanged { ref identity, muted: true }
            if *identity == me && !*state.muted.peek() && !*state.deafened.peek() && !state.is_afk_channel(channel) =>
        {
            // Muted by someone else: a moderator.
            state.muted.set(true);
            state.apply_mute();
            state.toast(t("call.muted_by_mod"), false);
            Some(event)
        }
        CallEvent::LocalMuted { source } => {
            let camera_on = state.call.peek().as_ref().is_some_and(|c| c.camera_on);
            match source {
                TrackSource::Camera if camera_on => {
                    toggle_camera(state);
                    state.toast(t("call.camera_by_mod"), false);
                }
                TrackSource::Screenshare | TrackSource::ScreenshareAudio => {
                    if state.call.peek().as_ref().is_some_and(|c| c.sharing) {
                        stop_share(state);
                        state.toast(t("call.share_by_mod"), false);
                    }
                }
                _ => {}
            }
            None
        }
        CallEvent::CanPublish(allowed) => {
            let was = state.call.peek().as_ref().is_some_and(|c| c.can_publish);
            if let Some(c) = state.call.write().as_mut() {
                c.can_publish = allowed;
            }
            if allowed == was {
                return None;
            }
            if allowed {
                state.toast(t("call.speak_allowed"), false);
                // The server removed the microphone; publish it again, muted
                // until the user unmutes.
                state.muted.set(true);
                let call = state.call.peek().as_ref().map(|c| c.call.clone());
                if let Some(call) = call {
                    spawn_forever(async move {
                        let vc = call.borrow_mut().take();
                        if let Some(mut vc) = vc {
                            if let Err(e) = vc.restore_microphone().await {
                                tracing::warn!(error = %e, "could not publish the microphone again");
                            }
                            *call.borrow_mut() = Some(vc);
                        }
                        state.apply_mute();
                    });
                }
            } else {
                state.toast(t("call.speak_revoked"), false);
                state.muted.set(true);
                state.apply_mute();
                if state.call.peek().as_ref().is_some_and(|c| c.camera_on) {
                    toggle_camera(state);
                }
                if state.call.peek().as_ref().is_some_and(|c| c.sharing) {
                    stop_share(state);
                }
            }
            None
        }
        CallEvent::Reconnected => {
            state.toast(t("call.reconnected"), false);
            Some(event)
        }
        other => Some(other),
    }
}

/// How long a reaction floats over a tile.
const REACTION_TIME: std::time::Duration = std::time::Duration::from_millis(3500);
/// Reactions offered in the call controls.
pub const REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "🎉", "😮", "👏"];

fn show_reaction(mut state: AppState, identity: String, emoji: String) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(c) = state.call.write().as_mut() {
        // At most three at once per person, so spamming stays readable.
        let theirs = c.reactions.iter().filter(|r| r.identity == identity).count();
        if theirs >= 3 {
            return;
        }
        c.reactions.push(CallReaction { id, identity, emoji });
    }
    spawn_forever(async move {
        tokio::time::sleep(REACTION_TIME).await;
        if let Some(c) = state.call.write().as_mut() {
            c.reactions.retain(|r| r.id != id);
        }
    });
}

fn send_signal(state: AppState, signal: CallSignal) {
    let call = state.call.peek().as_ref().map(|c| c.call.clone());
    if let Some(call) = call
        && let Some(vc) = call.borrow().as_ref()
    {
        vc.send_signal(signal);
    }
}

/// Raises or lowers your hand.
pub fn toggle_hand(mut state: AppState) {
    let Some(raised) = state.call.write().as_mut().map(|c| {
        let raised = !c.hands.contains(&c.identity);
        let me = c.identity.clone();
        c.hands.retain(|h| *h != me);
        if raised {
            c.hands.push(me);
        }
        raised
    }) else {
        return;
    };
    send_signal(state, CallSignal::Hand { raised });
}

/// Sends an emoji reaction (and shows it on your own tile).
pub fn react(state: AppState, emoji: &str) {
    let Some(me) = state.call.peek().as_ref().map(|c| c.identity.clone()) else { return };
    show_reaction(state, me, emoji.to_owned());
    send_signal(state, CallSignal::Reaction { emoji: emoji.to_owned() });
}

/// Asks someone to unmute (a moderator request; never forced).
pub fn ask_unmute(state: AppState, identity: String) {
    send_signal(state, CallSignal::AskUnmute { target: identity });
}

/// Whether you may moderate the call you are in.
pub fn can_moderate(state: &AppState) -> bool {
    let Some(channel) = state.call.read().as_ref().map(|c| c.channel) else { return false };
    state.guild_of(channel).is_some() && state.channel_permissions(channel).allows(Permissions::MUTE_MEMBERS)
}

/// A moderator action on someone in your call.
pub fn moderate(
    state: AppState,
    channel: ChannelId,
    user: sivryn_client_core::protocol::UserId,
    action: ModerateAction,
) {
    let Some(s) = state.session.cloned() else { return };
    spawn_forever(async move {
        let result = s.api.moderate_voice(channel, user, action).await.map_err(|e| e.to_string());
        state.report(result);
    });
}

/// Mutes everyone else in the call.
pub fn mute_all(state: AppState) {
    let (Some(s), Some(channel)) = (state.session.cloned(), state.call.peek().as_ref().map(|c| c.channel)) else {
        return;
    };
    spawn_forever(async move {
        match s.api.mute_all_voice(channel).await {
            Ok(n) => state.toast(format!("{} {n}", t("call.muted_all")), false),
            Err(e) => state.report(Err(e.to_string())),
        }
    });
}

/// Turns the low-light boost on or off (also on a running camera).
pub fn set_low_light(state: AppState, on: bool) {
    state.update_settings(|s| s.video.low_light = on);
    if let Some(c) = state.call.peek().as_ref()
        && let Some(camera) = c.camera.borrow().as_ref()
    {
        camera.set_low_light(on);
    }
}

/// Opens or closes the connection panel; while open, it samples the stats
/// every two seconds.
pub fn toggle_stats(mut state: AppState) {
    let Some(open) = state.call.write().as_mut().map(|c| {
        c.stats_open = !c.stats_open;
        if !c.stats_open {
            c.stats = None;
        }
        c.stats_open
    }) else {
        return;
    };
    if !open {
        return;
    }
    spawn_forever(async move {
        let mut prev = None;
        loop {
            let Some((call, true)) = state.call.peek().as_ref().map(|c| (c.call.clone(), c.stats_open)) else { break };
            // The room is shared; stats don't need the call borrowed across the await.
            let room = call.borrow().as_ref().map(|vc| vc.room.clone());
            let Some(room) = room else { break };
            let sample = sivryn_media::stats::sample(&room, prev).await;
            if let Some((stats, totals)) = sample {
                prev = Some((totals, std::time::Instant::now()));
                if let Some(c) = state.call.write().as_mut()
                    && c.stats_open
                {
                    c.stats = Some(stats);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

/// Which video layer each remote track should be received at: paused when
/// its tile isn't shown (strip hidden, another page, data saver), small in
/// the strip or a big grid, full size when shown large.
pub fn wanted_video(
    feeds: &[VideoFeed],
    me: &str,
    focus: Option<&Focus>,
    shown: &[String],
    strip_hidden: bool,
    data_saver: bool,
    grid_size: usize,
) -> Vec<(String, Option<VideoQuality>)> {
    feeds
        .iter()
        .filter(|f| f.participant != me)
        .map(|f| {
            let big = match focus {
                Some(Focus::Screen(sid)) => *sid == f.track_sid,
                Some(Focus::User(id)) => *id == f.participant && f.source == TrackSource::Camera,
                None => false,
            };
            let visible = shown.contains(&f.participant) || shown.contains(&f.track_sid);
            let quality = if f.source == TrackSource::Screenshare {
                // Screens are what people came for: never paused, only
                // lowered in data saver mode or when small.
                Some(if data_saver {
                    VideoQuality::Low
                } else if big || focus.is_none() {
                    VideoQuality::High
                } else {
                    VideoQuality::Low
                })
            } else if data_saver || !visible || (focus.is_some() && !big && strip_hidden) {
                None
            } else if big {
                Some(VideoQuality::High)
            } else if focus.is_some() || grid_size > 6 {
                Some(VideoQuality::Low)
            } else if grid_size > 2 {
                Some(VideoQuality::Medium)
            } else {
                Some(VideoQuality::High)
            };
            (f.track_sid.clone(), quality)
        })
        .collect()
}

/// Tells the server about changed wishes from [`wanted_video`].
fn apply_wanted(c: &CallState, wanted: Vec<(String, Option<VideoQuality>)>) {
    let mut applied = c.wanted.borrow_mut();
    let call = c.call.borrow();
    let Some(vc) = call.as_ref() else { return };
    for (sid, quality) in wanted {
        if applied.get(&sid) != Some(&quality) {
            vc.set_video_wanted(&sid, quality);
            applied.insert(sid, quality);
        }
    }
    applied.retain(|sid, _| c.feeds.iter().any(|f| &f.track_sid == sid));
}

/// Most tiles on one page of the grid.
pub const PAGE_SIZE: usize = 16;

/// The people on one page of the grid (screens are always shown).
pub fn page_of<T: Clone>(people: &[T], page: usize) -> (Vec<T>, usize) {
    let pages = people.len().div_ceil(PAGE_SIZE).max(1);
    let page = page.min(pages - 1);
    (people.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE).cloned().collect(), pages)
}

/// The call grid with controls. `compact` is the version above a DM's chat.
#[component]
pub fn CallView(#[props(default)] compact: bool) -> Element {
    let mut state = use_context::<AppState>();
    let mut reactions_open = use_signal(|| false);
    let video_settings = state.settings.read().video.clone();
    let moderator = can_moderate(&state);
    let call = state.call.read();
    let Some(c) = call.as_ref() else { return rsx! {} };
    let name = state.channel_name(c.channel);
    let channel = c.channel;
    let me = c.identity.clone();

    // One screen share or participant can be shown large; the rest go into
    // a strip below (which can be hidden).
    let screens: Vec<VideoFeed> = c.feeds.iter().filter(|f| f.source == TrackSource::Screenshare).cloned().collect();
    // Your own tile can be left out (when you aren't alone).
    let participants: Vec<Participant> = c
        .participants
        .iter()
        .filter(|p| !(video_settings.hide_self && p.identity == me && c.participants.len() > 1))
        .cloned()
        .collect();
    let focus = c
        .focus
        .clone()
        .filter(|f| match f {
            Focus::Screen(sid) => screens.iter().any(|s| &s.track_sid == sid),
            Focus::User(id) => participants.iter().any(|p| &p.identity == id),
        })
        // Others' screens come up large by themselves, your own preview doesn't.
        .or_else(|| {
            screens
                .iter()
                .find(|s| s.participant != c.identity)
                .filter(|_| c.auto_focus)
                .map(|s| Focus::Screen(s.track_sid.clone()))
        })
        // The speaker layout shows whoever spoke last large.
        .or_else(|| {
            if video_settings.call_layout != CallLayout::Speaker || participants.len() < 2 || !c.auto_focus {
                return None;
            }
            c.last_speaker
                .clone()
                .filter(|s| participants.iter().any(|p| &p.identity == s))
                .or_else(|| participants.iter().find(|p| p.identity != me).map(|p| p.identity.clone()))
                .map(Focus::User)
        });
    let speakers = c.speakers.clone();
    let cameras: Vec<VideoFeed> = c.feeds.iter().filter(|f| f.source == TrackSource::Camera).cloned().collect();
    let strip_hidden = c.strip_hidden;
    let (sharing, camera_on) = (c.sharing, c.camera_on);
    let hands = c.hands.clone();
    let hand_up = hands.contains(&me);
    let reactions = c.reactions.clone();
    let quality = c.quality.clone();
    let reconnecting = c.reconnecting;
    let can_publish = c.can_publish;
    let (stats_open, stats) = (c.stats_open, c.stats.clone());
    let my_quality = quality.get(&me).copied();

    // Without a large tile, big calls are split into pages.
    let (people, pages, page) = if focus.is_none() {
        let page = c.page.min(participants.len().div_ceil(PAGE_SIZE).max(1) - 1);
        let (people, pages) = page_of(&participants, page);
        (people, pages, page)
    } else {
        (participants.clone(), 1, 0)
    };
    // Every tile in one keyed list with the focused one first: a tile that
    // moves between the big spot and the strip is moved, not rebuilt
    // (rebuilding video surfaces made quick focus changes get lost).
    let mut tiles: Vec<StageTile> = screens.iter().cloned().map(StageTile::Screen).collect();
    tiles.extend(people.iter().cloned().map(StageTile::Person));
    if let Some(i) = tiles.iter().position(|t| Some(t.focus()) == focus) {
        let big = tiles.remove(i);
        tiles.insert(0, big);
    }

    // Receive only the video that is shown, at the size it is shown.
    let shown: Vec<String> = tiles
        .iter()
        .enumerate()
        .filter(|(i, _)| focus.is_none() || *i == 0 || !strip_hidden)
        .map(|(_, t)| match t {
            StageTile::Screen(f) => f.track_sid.clone(),
            StageTile::Person(p) => p.identity.clone(),
        })
        .collect();
    let wanted =
        wanted_video(&c.feeds, &me, focus.as_ref(), &shown, strip_hidden, video_settings.data_saver, tiles.len());
    apply_wanted(c, wanted);
    drop(call);
    let muted = *state.muted.read() || *state.deafened.read() || state.in_afk_channel();
    let deafened = *state.deafened.read();
    let count =
        participants.len() + usize::from(video_settings.hide_self && participants.iter().all(|p| p.identity != me));

    rsx! {
        div { class: if compact { "call compact" } else { "call" },
            if !compact {
            header { class: "topbar",
                span { class: "topbar-icon", Svg { icon: Icon::Volume, size: 20 } }
                span { class: "topbar-title", "{name}" }
                span { class: "e2ee-badge", Svg { icon: Icon::ShieldCheck, size: 14 } {t("call.e2ee")} }
                if video_settings.data_saver {
                    span { class: "e2ee-badge", {t("call.data_saver_on")} }
                }
                span { class: "topbar-spacer" }
                if !hands.is_empty() {
                    span { class: "hand-count", title: t("call.hands"), Svg { icon: Icon::Hand, size: 14 } "{hands.len()}" }
                }
                span { class: "muted small", "{count} {t(\"call.in_call\")}" }
                if moderator {
                    IconButton { icon: Icon::MicOff, size: 18, title: t("call.mute_all"), onclick: move |_| mute_all(state) }
                }
                IconButton {
                    icon: Icon::LayoutGrid,
                    size: 18,
                    title: t("call.layout_toggle"),
                    onclick: move |_| {
                        state.update_settings(|s| s.video.call_layout = if s.video.call_layout == CallLayout::Speaker { CallLayout::Grid } else { CallLayout::Speaker });
                        if let Some(c) = state.call.write().as_mut() {
                            c.focus = None;
                            c.auto_focus = true;
                        }
                    },
                }
                IconButton { icon: Icon::Activity, size: 18, title: t("call.stats"), onclick: move |_| toggle_stats(state) }
                // Voice channels have a chat of their own.
                if state.channel_kind(channel) == Some(sivryn_client_core::protocol::model::ChannelKind::Voice) {
                    IconButton { icon: Icon::Message, size: 18, title: t("channels.open_chat"), onclick: move |_| state.voice_chat.toggle() }
                }
            }
            }
            if reconnecting {
                div { class: "callout warning call-banner", div { class: "spinner small" } span { {t("call.reconnecting")} } }
            } else if matches!(my_quality, Some(ConnectionQuality::Poor | ConnectionQuality::Lost)) && !video_settings.data_saver {
                div { class: "callout warning call-banner",
                    Svg { icon: Icon::Info, size: 16 }
                    span { {t("call.your_connection_poor")} }
                    button { class: "button ghost small", onclick: move |_| state.update_settings(|s| s.video.data_saver = true), {t("video.data_saver")} }
                }
            }
            if let Some(stats) = stats.filter(|_| stats_open) {
                StatsPanel { stats }
            }
            div { class: if focus.is_some() { "stage focus" } else { "stage" },
                div {
                    class: match (focus.is_some(), strip_hidden) {
                        (true, true) => "stage-tiles focus strip-hidden",
                        (true, false) => "stage-tiles focus",
                        _ => "stage-tiles",
                    },
                    // First in the flow, so it sits above the (centred) strip.
                    if focus.is_some() {
                        div { class: "strip-toggle-row",
                            div { class: "strip-toggle",
                                onclick: move |_| {
                                    if let Some(c) = state.call.write().as_mut() {
                                        c.strip_hidden = !c.strip_hidden;
                                    }
                                },
                                if strip_hidden { Svg { icon: Icon::ChevronDown, size: 14 } } else { Svg { icon: Icon::ChevronRight, size: 14 } }
                                if strip_hidden { {t("call.show_members")} } else { {t("call.hide_members")} }
                            }
                        }
                    }
                    for (i, tile) in tiles.into_iter().enumerate() {
                        {
                            let big = focus.is_some() && i == 0;
                            let in_strip = focus.is_some() && i > 0;
                            match tile {
                                StageTile::Screen(feed) => rsx! {
                                    if !(in_strip && strip_hidden) {
                                        VideoTile { key: "{feed.track_sid}", feed: feed.clone(), large: big }
                                    }
                                },
                                StageTile::Person(p) => {
                                    let own = p.identity == me;
                                    rsx! {
                                        if !(in_strip && strip_hidden) {
                                            ParticipantTile {
                                                key: "{p.identity}",
                                                // Data saver: no cameras (your own self view stays).
                                                camera: cameras.iter().find(|f| f.participant == p.identity).filter(|_| own || !video_settings.data_saver).cloned(),
                                                speaking: speakers.contains(&p.identity),
                                                hand: hands.contains(&p.identity),
                                                quality: quality.get(&p.identity).copied(),
                                                reactions: reactions.iter().filter(|r| r.identity == p.identity).map(|r| (r.id, r.emoji.clone())).collect::<Vec<_>>(),
                                                mirror: own && video_settings.mirror_self,
                                                moderator,
                                                participant: p,
                                                large: big,
                                                strip: in_strip,
                                            }
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
            if pages > 1 {
                div { class: "pager",
                    IconButton { icon: Icon::ArrowUp, size: 16, title: t("call.page"), onclick: move |_| {
                        if let Some(c) = state.call.write().as_mut() { c.page = page.saturating_sub(1); }
                    } }
                    span { class: "muted small", "{t(\"call.page\")} {page + 1} / {pages}" }
                    IconButton { icon: Icon::ArrowDown, size: 16, title: t("call.page"), onclick: move |_| {
                        if let Some(c) = state.call.write().as_mut() { c.page = (page + 1).min(pages - 1); }
                    } }
                }
            }
            div { class: "controls",
                if muted {
                    IconButton { icon: Icon::MicOff, class: "control alert", title: t("call.unmute"), onclick: move |_| {
                        if can_publish { state.set_muted(false) } else { state.toast(t("call.speak_revoked"), false) }
                    } }
                } else {
                    IconButton { icon: Icon::Mic, class: "control", title: t("call.mute"), onclick: move |_| state.set_muted(true) }
                }
                if deafened {
                    IconButton { icon: Icon::HeadphonesOff, class: "control alert", title: t("call.undeafen"), onclick: move |_| state.set_deafened(false) }
                } else {
                    IconButton { icon: Icon::Headphones, class: "control", title: t("call.deafen"), onclick: move |_| state.set_deafened(true) }
                }
                if camera_on {
                    IconButton { icon: Icon::Video, class: "control on", title: t("call.camera_off"), onclick: move |_| toggle_camera(state) }
                } else {
                    IconButton { icon: Icon::VideoOff, class: "control", title: t("call.camera_on"), onclick: move |_| {
                        if can_publish { toggle_camera(state) } else { state.toast(t("call.speak_revoked"), false) }
                    } }
                }
                if sharing {
                    IconButton { icon: Icon::MonitorOff, class: "control on", title: t("call.stop_share"), onclick: move |_| stop_share(state) }
                } else {
                    IconButton { icon: Icon::MonitorUp, class: "control", title: t("call.share"), onclick: move |_| {
                        if can_publish { state.modal.set(Some(Modal::PickScreen)) } else { state.toast(t("call.speak_revoked"), false) }
                    } }
                }
                IconButton {
                    icon: Icon::Hand,
                    class: if hand_up { "control on" } else { "control" },
                    title: if hand_up { t("call.lower_hand") } else { t("call.raise_hand") },
                    onclick: move |_| toggle_hand(state),
                }
                div { class: "reaction-control",
                    IconButton { icon: Icon::Smile, class: "control", title: t("call.react"), onclick: move |_| reactions_open.toggle() }
                    if *reactions_open.read() {
                        div { class: "reaction-pop",
                            for emoji in REACTIONS {
                                div {
                                    class: "reaction-choice",
                                    onclick: move |_| {
                                        react(state, emoji);
                                        reactions_open.set(false);
                                    },
                                    "{emoji}"
                                }
                            }
                        }
                    }
                }
                IconButton { icon: Icon::PhoneOff, class: "control danger", title: t("call.leave"), onclick: move |_| hang_up(state) }
            }
        }
    }
}

/// The connection panel: ping, loss, bitrates and codecs.
#[component]
fn StatsPanel(stats: sivryn_media::CallStats) -> Element {
    let rows: Vec<(&'static str, String)> = stats_rows(&stats);
    rsx! {
        div { class: "stats-panel",
            for (label, value) in rows {
                div { class: "stats-row",
                    span { class: "muted", {t(label)} }
                    span { "{value}" }
                }
            }
        }
    }
}

/// The connection panel's rows (label key, value), in the units people know:
/// milliseconds, percent and kbit/s throughout.
pub fn stats_rows(s: &sivryn_media::CallStats) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("call.stats_rtt", s.rtt_ms.map(|v| format!("{v:.0} ms")).unwrap_or_else(|| "–".into())),
        ("call.stats_loss", format!("{:.1} %", s.loss_percent)),
        ("call.stats_jitter", format!("{:.0} ms", s.jitter_ms)),
        ("call.stats_send", format!("{:.0} kbit/s", s.send_kbps)),
        ("call.stats_receive", format!("{:.0} kbit/s", s.receive_kbps)),
    ];
    if let Some(v) = s.available_send_kbps {
        rows.push(("call.stats_available", format!("{v:.0} kbit/s")));
    }
    if let Some((w, h, fps)) = s.video_in {
        rows.push(("call.stats_video", format!("{w}×{h} · {fps:.0} fps")));
    }
    if !s.codecs.is_empty() {
        rows.push(("call.stats_codecs", s.codecs.join(", ")));
    }
    rows
}

/// A tile on the call stage.
#[derive(Clone, PartialEq)]
enum StageTile {
    Screen(VideoFeed),
    Person(Participant),
}

impl StageTile {
    fn focus(&self) -> Focus {
        match self {
            StageTile::Screen(feed) => Focus::Screen(feed.track_sid.clone()),
            StageTile::Person(p) => Focus::User(p.identity.clone()),
        }
    }
}

/// A participant: their camera if it's on, otherwise their avatar.
#[component]
fn ParticipantTile(
    participant: Participant,
    camera: Option<VideoFeed>,
    speaking: bool,
    large: bool,
    #[props(default)] strip: bool,
    #[props(default)] hand: bool,
    #[props(default)] quality: Option<ConnectionQuality>,
    /// Reactions floating over the tile (id, emoji).
    #[props(default)]
    reactions: Vec<(u64, String)>,
    /// Show the camera flipped (your own self view).
    #[props(default)]
    mirror: bool,
    /// You may moderate this call (adds moderation to the menu).
    #[props(default)]
    moderator: bool,
) -> Element {
    let state = use_context::<AppState>();
    let call_channel = state.call.read().as_ref().map(|c| c.channel);
    let id: Option<sivryn_client_core::protocol::UserId> = participant.identity.parse().ok();
    let class = match (large, camera.is_some()) {
        (true, true) => "tile participant large has-video",
        (true, false) => "tile participant large",
        (false, true) => "tile participant has-video",
        (false, false) => "tile participant",
    };
    // A camera tile takes the video's shape, so no grey bars remain beside it.
    let aspect = use_video_aspect(camera.clone());
    let style = match (aspect, large) {
        (Some(a), false) => format!("width: auto; aspect-ratio: {a:.4};"),
        _ => String::new(),
    };
    let (identity, sharing, muted) = (participant.identity.clone(), participant.sharing, participant.muted);
    let has_camera = camera.is_some();
    // The name on this server (nickname or display name).
    let name = id
        .map(|id| state.display_name_in(call_channel.and_then(|c| state.guild_of(c)), id))
        .unwrap_or(participant.name.clone());
    let open_menu = move |e: MouseEvent| {
        let Some(user) = id else { return };
        let p = e.client_coordinates();
        let guild = call_channel.and_then(|c| state.guild_of(c));
        let mut items = super::menu::user_items(state, user, guild, call_channel);
        if sharing && Some(user) != state.me() {
            items.insert(
                items.iter().position(|i| matches!(i, super::MenuItem::Check { .. })).unwrap_or(items.len()),
                super::MenuItem::Volume { label: t("call.stream_volume").into(), key: screen_volume_key(&identity) },
            );
        }
        if let Some(channel) = call_channel.filter(|_| moderator && Some(user) != state.me()) {
            let act = |label: &'static str, icon: Icon, action: ModerateAction| {
                super::MenuItem::action(t(label), icon, super::menu::MenuAction::ModerateVoice(channel, user, action))
            };
            let mut moderation = vec![super::MenuItem::Separator];
            if muted {
                moderation.push(super::MenuItem::action(
                    t("call.mod_ask_unmute"),
                    Icon::Mic,
                    super::menu::MenuAction::AskUnmute(identity.clone()),
                ));
            } else {
                moderation.push(act("call.mod_mute", Icon::MicOff, ModerateAction::Mute));
            }
            if has_camera {
                moderation.push(act("call.mod_stop_camera", Icon::VideoOff, ModerateAction::StopCamera));
            }
            if sharing {
                moderation.push(act("call.mod_stop_share", Icon::MonitorOff, ModerateAction::StopShare));
            }
            if hand {
                moderation.push(act("call.mod_allow_speak", Icon::Hand, ModerateAction::AllowSpeak));
            }
            moderation.push(act("call.mod_revoke_speak", Icon::Ban, ModerateAction::RevokeSpeak));
            items.extend(moderation);
        }
        super::menu::open(state, p.x, p.y, items);
    };

    let focus_key = Focus::User(participant.identity.clone());
    let (hovered, mark) = super::widgets::use_hover();
    let camera_sid = camera.as_ref().map(|f| f.track_sid.clone());
    rsx! {
        div { class: "{class}", style: "{style}",
            onmousemove: mark,
            // Left click shows this person large (again: back to the grid).
            onclick: move |_| toggle_focus(state, focus_key.clone()),
            onmousedown: move |e| {
                if super::menu::right_click(&e).is_some() {
                    open_menu(e);
                }
            },
            match camera {
                Some(feed) => rsx! { VideoCanvas { key: "{feed.track_sid}-{mirror}", feed, mirror } },
                None => rsx! {
                    div { class: "tile-avatar",
                        Avatar { name: name.clone(), id, size: if large { 160 } else if strip { 64 } else { 96 }, speaking }
                    }
                },
            }
            div { class: "name-tag",
                if participant.deafened {
                    span { class: "tag-muted", Svg { icon: Icon::HeadphonesOff, size: 13 } }
                } else if participant.muted {
                    span { class: "tag-muted", Svg { icon: Icon::MicOff, size: 13 } }
                }
                span { "{name}" }
                if participant.sharing {
                    span { class: "live-badge", "LIVE" }
                }
                if let Some(q) = quality {
                    QualityBars { quality: q }
                }
            }
            if hand {
                div { class: "hand-badge", title: t("call.hand_raised"), Svg { icon: Icon::Hand, size: 16 } }
            }
            if !reactions.is_empty() {
                div { class: "reactions",
                    for (id, emoji) in reactions {
                        span { key: "{id}", class: "reaction-float", "{emoji}" }
                    }
                }
            }
            if let (true, Some(sid)) = (hovered, camera_sid) {
                div { class: "tile-controls", FullscreenButton { sid } }
            }
            // A ring over the picture; a background or inset shadow was
            // hidden behind the video or showed as a green fill.
            if speaking {
                div { class: "speaking-ring" }
            }
        }
    }
}

/// The width-to-height ratio of a camera's picture, once frames arrive.
fn use_video_aspect(camera: Option<VideoFeed>) -> Option<f32> {
    let mut current = use_signal(|| camera.clone());
    if *current.peek() != camera {
        current.set(camera);
    }
    let mut aspect = use_signal(|| None::<f32>);
    use_future(move || async move {
        loop {
            let now = current.peek().as_ref().and_then(|feed| {
                feed.frames.borrow().as_ref().filter(|f| f.height > 0).map(|f| f.width as f32 / f.height as f32)
            });
            let changed = match (*aspect.peek(), now) {
                (Some(old), Some(new)) => (old - new).abs() > 0.01,
                (old, new) => old.is_some() != new.is_some(),
            };
            if changed {
                aspect.set(now);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
    *aspect.read()
}

/// The volume of someone's stream sound (their screen's audio), 0–200 %.
/// The speaker mutes it; hovering it shows a slider above it, or `inline`
/// next to it (the fullscreen bar has the room).
#[component]
pub(super) fn StreamVolume(identity: String, #[props(default)] inline: bool) -> Element {
    let state = use_context::<AppState>();
    // A drag keeps the pop-up open when the pointer leaves it.
    let mut dragging = use_signal(|| false);
    let key = screen_volume_key(&identity);
    let (value, muted) = {
        let s = state.settings.read();
        (s.volumes.get(&key).copied().unwrap_or(1.0), s.muted_users.contains(&key))
    };
    let shown = if muted { 0.0 } else { value };
    let toggle = {
        let key = key.clone();
        move |e: MouseEvent| {
            e.stop_propagation();
            let key = key.clone();
            state.update_settings(|s| {
                if !s.muted_users.remove(&key) {
                    s.muted_users.insert(key);
                }
            });
        }
    };
    // Moving the slider also unmutes.
    let set = move |v: f32| {
        dragging.set(true);
        let key = key.clone();
        state.update_settings(|s| {
            s.muted_users.remove(&key);
            s.volumes.insert(key, v);
        });
    };
    let slider = rsx! {
        super::widgets::Slider {
            value: shown,
            min: 0.0,
            max: 2.0,
            steps: 40,
            vertical: !inline,
            onchange: set,
            onrelease: move |_| dragging.set(false),
        }
    };
    let speaker = rsx! {
        div { class: "icon-button", title: t("call.stream_volume"), onclick: toggle,
            if shown <= 0.0 { Svg { icon: Icon::VolumeX, size: 16 } } else { Svg { icon: Icon::Volume, size: 16 } }
        }
    };
    let percent = (shown * 100.0).round();
    if inline {
        return rsx! {
            div { class: "stream-volume inline", onclick: move |e| e.stop_propagation(),
                {speaker}
                div { class: "stream-volume-slider", {slider} }
                span { class: "stream-volume-value", "{percent} %" }
            }
        };
    }
    rsx! {
        div {
            class: if *dragging.read() { "volume-hover open" } else { "volume-hover" },
            onclick: move |e| e.stop_propagation(),
            div { class: "volume-pop", {slider} }
            {speaker}
        }
    }
}

/// Shows a tile's video over the whole screen (on hover, bottom right).
#[component]
fn FullscreenButton(sid: String) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        div {
            class: "tile-fullscreen",
            title: t("media.fullscreen"),
            onclick: move |e| {
                e.stop_propagation();
                super::fullscreen::open(state, super::fullscreen::FullView::Feed(sid.clone()));
            },
            Svg { icon: Icon::Maximize, size: 16 }
        }
    }
}

/// Shows a tile large, or goes back to the grid when it already is.
fn toggle_focus(mut state: AppState, key: Focus) {
    if let Some(c) = state.call.write().as_mut() {
        let current = c.focus.clone().or_else(|| {
            c.feeds
                .iter()
                .find(|f| f.source == TrackSource::Screenshare)
                .filter(|_| c.auto_focus)
                .map(|f| Focus::Screen(f.track_sid.clone()))
        });
        if current.as_ref() == Some(&key) {
            c.focus = None;
            c.auto_focus = false;
        } else {
            c.focus = Some(key);
        }
    }
}

/// Zoom steps for a screen share (mouse wheel over the large tile).
const ZOOM_STEPS: [f32; 6] = [1.0, 1.5, 2.0, 3.0, 4.0, 6.0];
/// Width of the overview in the corner while zoomed (CSS pixels).
const MINIMAP_WIDTH: f32 = 200.0;

static NEXT_ZOOM_TARGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// The zoomable tile under the mouse, which takes wheel notches.
static ZOOM_TARGET: std::sync::Mutex<Option<(u64, tokio::sync::mpsc::UnboundedSender<f64>)>> =
    std::sync::Mutex::new(None);

/// The window's wheel handler: zooms the screen share under the mouse.
pub fn wheel_zoom(notches: f64) -> bool {
    let target = ZOOM_TARGET.lock().unwrap_or_else(|e| e.into_inner());
    target.as_ref().is_some_and(|(_, tx)| tx.send(notches).is_ok())
}

fn set_zoom_target(id: u64, target: Option<&tokio::sync::mpsc::UnboundedSender<f64>>) {
    let mut current = ZOOM_TARGET.lock().unwrap_or_else(|e| e.into_inner());
    match target {
        Some(tx) if current.as_ref().is_none_or(|(i, _)| *i != id) => *current = Some((id, tx.clone())),
        None if current.as_ref().is_some_and(|(i, _)| *i == id) => *current = None,
        _ => {}
    }
}

/// The next zoom step up (notches > 0) or down.
fn step_zoom(zoom: f32, notches: f64) -> f32 {
    let at = ZOOM_STEPS.iter().rposition(|z| *z <= zoom + 1e-3).unwrap_or(0);
    let next = if notches > 0.0 { (at + 1).min(ZOOM_STEPS.len() - 1) } else { at.saturating_sub(1) };
    ZOOM_STEPS[next]
}

/// Dragging the frame in the overview.
#[derive(Clone, Copy)]
struct MinimapDrag {
    view: Signal<View>,
    /// Where the drag started (client coordinates) and the centre then.
    start: (f64, f64),
    centre: (f32, f32),
    /// The overview's size in CSS pixels.
    size: (f32, f32),
}

thread_local! {
    static MINIMAP_DRAG: std::cell::Cell<Option<MinimapDrag>> = const { std::cell::Cell::new(None) };
}

/// Called by the window root on every mouse move: moves a dragged frame.
pub fn minimap_pointer_move(e: &MouseEvent) {
    MINIMAP_DRAG.with(|d| {
        let Some(MinimapDrag { mut view, start, centre, size }) = d.get() else { return };
        // The button was let go somewhere else.
        if e.held_buttons().is_empty() {
            d.set(None);
            return;
        }
        let p = e.client_coordinates();
        let dx = (p.x - start.0) as f32 / size.0;
        let dy = (p.y - start.1) as f32 / size.1;
        let mut next = *view.peek();
        next.centre = (centre.0 + dx, centre.1 + dy);
        // Keep the centre where the frame stays inside the picture.
        let ((u0, v0), (u1, v1)) = next.bounds();
        next.centre = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
        if next != *view.peek() {
            view.set(next);
        }
    });
}

/// Called by the window root when the mouse button is released.
pub fn minimap_pointer_up() {
    MINIMAP_DRAG.with(|d| d.set(None));
}

#[component]
fn VideoTile(feed: VideoFeed, large: bool) -> Element {
    let state = use_context::<AppState>();
    let guild = state.call.read().as_ref().and_then(|c| state.guild_of(c.channel));
    let name = feed.participant.parse().map(|id| state.display_name_in(guild, id)).unwrap_or(feed.participant.clone());
    let sid = feed.track_sid.clone();
    let full_sid = feed.track_sid.clone();
    let feed_owner = feed.participant.clone();
    let own_identity = state.me().map(|id| id.to_string());
    let (hovered, mark) = super::widgets::use_hover();

    // Zoom (wheel over the large tile) and the shown part, shared with the
    // video surface.
    let mut view = use_signal(View::default);
    let shared = use_hook(SharedView::default);
    let zoom_id = use_hook(|| NEXT_ZOOM_TARGET.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let zoom_tx = use_hook(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<f64>();
        spawn(async move {
            while let Some(notches) = rx.recv().await {
                let mut next = *view.peek();
                next.zoom = step_zoom(next.zoom, notches);
                if next != *view.peek() {
                    view.set(next);
                }
            }
        });
        tx
    });
    if !large && *view.peek() != View::default() {
        view.set(View::default());
    }
    set_zoom_target(zoom_id, (hovered && large).then_some(&zoom_tx));
    use_drop(move || set_zoom_target(zoom_id, None));
    let current = *view.read();
    if shared.set(current) {
        blitz_shell::canvas_frames::request_frame();
    }
    let aspect = use_video_aspect(Some(feed.clone())).unwrap_or(16.0 / 9.0);
    let (map_w, map_h) = (MINIMAP_WIDTH, MINIMAP_WIDTH / aspect);
    let ((u0, v0), (u1, v1)) = current.bounds();
    let frame_style = format!(
        "left: {:.1}px; top: {:.1}px; width: {:.1}px; height: {:.1}px;",
        u0 * map_w,
        v0 * map_h,
        (u1 - u0) * map_w,
        (v1 - v0) * map_h
    );
    let minimap_feed = feed.clone();

    rsx! {
        div {
            class: if large { "tile screen large" } else { "tile screen" },
            onmousemove: mark,
            onclick: move |_| toggle_focus(state, Focus::Screen(sid.clone())),
            VideoCanvas { feed, view: Some(shared.clone()) }
            div { class: "name-tag",
                Svg { icon: Icon::Monitor, size: 13 }
                span { "{name}" }
            }
            if hovered {
                div { class: "tile-controls",
                    // The sound of someone else's stream (one's own isn't played).
                    if Some(feed_owner.as_str()) != own_identity.as_deref() {
                        StreamVolume { identity: feed_owner.clone() }
                    }
                    FullscreenButton { sid: full_sid }
                }
            }
            // While zoomed: the whole picture with a frame for the shown part,
            // which can be dragged around.
            if current.zoom > 1.0 {
                div {
                    class: "minimap",
                    style: "width: {map_w:.0}px; height: {map_h:.0}px;",
                    onclick: move |e| e.stop_propagation(),
                    onmousedown: move |e| {
                        e.stop_propagation();
                        let p = e.client_coordinates();
                        let centre = view.peek().centre;
                        let drag = MinimapDrag { view, start: (p.x, p.y), centre, size: (map_w, map_h) };
                        MINIMAP_DRAG.with(|d| d.set(Some(drag)));
                    },
                    VideoCanvas { feed: minimap_feed }
                    div { class: "minimap-frame", style: "{frame_style}" }
                    div { class: "minimap-zoom", "{current.zoom}×" }
                }
            }
        }
    }
}

#[component]
fn VideoCanvas(feed: VideoFeed, #[props(default)] view: Option<SharedView>, #[props(default)] mirror: bool) -> Element {
    let frames = feed.frames.clone();
    let paint_source = use_wgpu(move || match view {
        Some(view) => VideoSurface::new(frames).with_view(view).mirrored(mirror),
        None => VideoSurface::new(frames).mirrored(mirror),
    });
    rsx! {
        canvas { class: "video", "src": "{paint_source}" }
    }
}

/// Signal-strength bars for someone's connection.
#[component]
fn QualityBars(quality: ConnectionQuality) -> Element {
    let (level, label, class) = match quality {
        ConnectionQuality::Excellent => (3, "call.quality_excellent", "quality good"),
        ConnectionQuality::Good => (2, "call.quality_good", "quality good"),
        ConnectionQuality::Poor => (1, "call.quality_poor", "quality poor"),
        ConnectionQuality::Lost => (0, "call.quality_lost", "quality lost"),
    };
    rsx! {
        span { class: "{class}", title: t(label),
            for i in 1..=3 {
                span { key: "{i}", class: if i <= level { "bar on bar-{i}" } else { "bar bar-{i}" } }
            }
        }
    }
}

/// Camera and microphone check before joining a call: see yourself, watch
/// the microphone level, pick camera on or off and the low-light boost.
#[component]
pub fn PreJoinDialog(channel: ChannelId) -> Element {
    let mut state = use_context::<AppState>();
    let engine = super::use_services_engine();
    let video = state.settings.read().video.clone();
    let mut camera_wanted = use_signal(|| true);
    let mut join_muted = use_signal(|| *state.muted.peek());
    let preview = use_signal(|| None::<Rc<CameraPreview>>);
    let mut camera_error = use_signal(|| None::<String>);
    let mut level = use_signal(|| -90.0f32);

    // The camera runs while the dialog is open and the camera is wanted.
    use_effect(move || {
        let wanted = *camera_wanted.read();
        let mut preview = preview;
        if !wanted {
            preview.set(None);
            return;
        }
        if preview.peek().is_some() {
            return;
        }
        let video = state.settings.peek().video.clone();
        spawn(async move {
            match CameraPreview::start(video.camera.as_deref(), video.camera_quality, video.low_light).await {
                Ok(p) => {
                    camera_error.set(None);
                    if *camera_wanted.peek() {
                        preview.set(Some(Rc::new(p)));
                    }
                }
                Err(e) => {
                    camera_error.set(Some(match e {
                        sivryn_media::MediaError::NoCamera => t("video.no_camera").to_string(),
                        sivryn_media::MediaError::NoMediaFoundation => t("video.no_mf").to_string(),
                        other => other.to_string(),
                    }));
                    camera_wanted.set(false);
                }
            }
        });
    });
    // The microphone level (captured for the meter only, not played back).
    let meter_engine = engine.clone();
    use_hook(move || meter_engine.set_mic_meter(true));
    let drop_engine = engine.clone();
    use_drop(move || drop_engine.set_mic_meter(false));
    let level_engine = engine.clone();
    use_future(move || {
        let engine = level_engine.clone();
        async move {
            loop {
                let db = engine.level_db();
                if (db - *level.peek()).abs() > 1.0 {
                    level.set(db);
                }
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            }
        }
    });
    let percent = ((*level.read() + 60.0) / 60.0 * 100.0).clamp(0.0, 100.0);
    let name = state.channel_name(channel);
    let feed = preview.read().as_ref().map(|p| p.preview.clone());
    let low_light = video.low_light;

    let join = move |_| {
        let camera = *camera_wanted.peek();
        let muted = *join_muted.peek();
        state.modal.set(None);
        if muted != *state.muted.peek() {
            state.set_muted(muted);
        }
        let mut s = state;
        s.home.set(false);
        if let Some(g) = s.guild_of(channel) {
            s.guild.set(Some(g));
        }
        s.channel.set(Some(channel));
        start(state, channel, camera);
    };
    rsx! {
        div { class: "overlay", onclick: move |_| state.modal.set(None),
            div { class: "dialog prejoin", onclick: move |e| e.stop_propagation(),
                div { class: "dialog-header",
                    h2 { {t("call.preview_title")} }
                    IconButton { icon: Icon::X, size: 18, onclick: move |_| state.modal.set(None) }
                }
                p { class: "muted", "„{name}“" }
                div { class: "prejoin-video",
                    match feed {
                        Some(feed) => rsx! { VideoCanvas { key: "prejoin-{video.mirror_self}", feed, mirror: video.mirror_self } },
                        None => rsx! {
                            div { class: "prejoin-off",
                                Svg { icon: Icon::VideoOff, size: 32 }
                                span { class: "muted", {camera_error.read().clone().unwrap_or_else(|| t("call.preview_no_camera").to_string())} }
                            }
                        },
                    }
                }
                div { class: "prejoin-meter", title: t("call.preview_mic"),
                    Svg { icon: if *join_muted.read() { Icon::MicOff } else { Icon::Mic }, size: 16 }
                    div { class: "meter", div { class: "meter-fill", style: "width: {percent:.0}%;" } }
                }
                div { class: "prejoin-toggles",
                    Toggle { on: *camera_wanted.read(), onchange: move |v: bool| camera_wanted.set(v) }
                    span { {t("call.preview_camera")} }
                    Toggle { on: !*join_muted.read(), onchange: move |v: bool| join_muted.set(!v) }
                    span { {t("call.preview_mic")} }
                    Toggle { on: low_light, onchange: move |v: bool| {
                        set_low_light(state, v);
                        if let Some(p) = preview.peek().as_ref() { p.set_low_light(v); }
                    } }
                    span { {t("video.low_light")} }
                }
                div { class: "dialog-actions",
                    button { class: "button ghost", onclick: move |_| state.modal.set(None), {t("common.cancel")} }
                    button { class: "button primary", autofocus: "true", onclick: join, {t("call.preview_join")} }
                }
            }
        }
    }
}

#[component]
pub fn ScreenPicker() -> Element {
    let mut state = use_context::<AppState>();
    let services = use_context::<Services>();
    let sources = use_hook(list_sources);
    // Live thumbnails while the picker is open (dropped with it).
    let previews = use_hook({
        let sources = sources.clone();
        move || Rc::new(SourcePreviews::start(&sources))
    });
    let video = state.settings.read().video.clone();
    let mut starting = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // The picker stays open until the share has started; closing it earlier
    // would cancel the task spawned here.
    let pick = use_callback(move |source: ScreenSource| {
        if *starting.read() {
            return;
        }
        let Some(c) = state.call.cloned() else { return };
        let Some(room) = c.call.borrow().as_ref().map(|vc| vc.room.clone()) else { return };
        let video = state.settings.peek().video.clone();
        let limits = video.screen_limits(&c.join.screen_share);
        let engine = services.engine.clone();
        starting.set(true);
        error.set(None);
        spawn(async move {
            // TODO: collect decoder support from participants instead of assuming it.
            let viewers = [VideoCodec::Av1, VideoCodec::H264];
            let result = ScreenShare::start(room.clone(), &source, &limits, video.screen_hint, &viewers).await;
            match result {
                Ok(share) => {
                    tracing::info!(codec = ?share.codec, "screen share started");
                    let preview = share.preview.clone();
                    *c.share.borrow_mut() = Some(share);
                    if let Some(call) = state.call.write().as_mut() {
                        call.feeds.push(preview);
                    }
                    if video.screen_audio {
                        let target = match source.kind {
                            SourceKind::Window => window_process(source.id).map(LoopbackTarget::Process),
                            SourceKind::Screen => Some(LoopbackTarget::AllExcept(std::process::id())),
                        };
                        if let Some(target) = target {
                            match ScreenAudio::start(room, target).await {
                                Ok(audio) => *c.share_audio.borrow_mut() = Some(audio),
                                Err(e) => state.toast(format!("{}: {e}", t("share.audio_failed")), true),
                            }
                        }
                    }
                    if let Some(call) = state.call.write().as_mut() {
                        call.sharing = true;
                        let me = call.identity.clone();
                        if let Some(p) = call.participants.iter_mut().find(|p| p.identity == me) {
                            p.sharing = true;
                        }
                    }
                    engine.play(Sound::ShareStart);
                    starting.set(false);
                    state.modal.set(None);
                }
                Err(e) => {
                    starting.set(false);
                    error.set(Some(e.to_string()));
                }
            }
        });
    });

    let windows_tab = video.share_windows_tab;
    let kind = if windows_tab { SourceKind::Window } else { SourceKind::Screen };
    let shown: Vec<ScreenSource> = sources.iter().filter(|s| s.kind == kind).cloned().collect();
    let quality_label = video.screen_label();

    rsx! {
        div { class: "overlay", onclick: move |_| state.modal.set(None),
            div { class: "dialog share-dialog", onclick: move |e| e.stop_propagation(),
                div { class: "dialog-header",
                    h2 { {t("share.title")} }
                    IconButton { icon: Icon::X, size: 18, onclick: move |_| state.modal.set(None) }
                }
                div { class: "share-tabs",
                    div { class: if !windows_tab { "tab active" } else { "tab" },
                        onclick: move |_| state.update_settings(|s| s.video.share_windows_tab = false),
                        Svg { icon: Icon::Monitor, size: 16 }
                        {t("share.screens")}
                    }
                    div { class: if windows_tab { "tab active" } else { "tab" },
                        onclick: move |_| state.update_settings(|s| s.video.share_windows_tab = true),
                        Svg { icon: Icon::Maximize, size: 16 }
                        {t("share.windows")}
                    }
                }
                // The empty case outside the grid: Blitz counted the empty
                // placeholder as a grid item, which shifted the first row.
                if shown.is_empty() {
                    div { class: "share-grid muted", {t("share.none")} }
                }
                div { class: "share-grid",
                    for source in shown {
                        {
                            let thumb = previews.frames.get(&source.id).cloned();
                            let s2 = source.clone();
                            rsx! {
                                div {
                                    key: "{source.id}",
                                    class: "share-card",
                                    onclick: move |_| pick.call(s2.clone()),
                                    div { class: "share-thumb",
                                        if let Some(frames) = thumb {
                                            FrameCanvas { frames: Frames(frames) }
                                        }
                                    }
                                    div { class: "share-card-title",
                                        if source.kind == SourceKind::Screen {
                                            Svg { icon: Icon::Monitor, size: 14 }
                                        } else {
                                            Svg { icon: Icon::Maximize, size: 14 }
                                        }
                                        span { class: "source-title", "{source.title}" }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "share-options",
                    div { class: "share-option",
                        span { class: "field-label", {t("share.resolution")} }
                        div { class: "segmented",
                            for r in ScreenResolution::ALL {
                                div {
                                    key: "{r:?}",
                                    class: if video.screen_resolution == r { "seg active" } else { "seg" },
                                    onclick: move |_| state.update_settings(|s| s.video.screen_resolution = r),
                                    "{r.label()}"
                                }
                            }
                        }
                    }
                    div { class: "share-option",
                        span { class: "field-label", {t("share.fps")} }
                        div { class: "segmented",
                            for fps in SCREEN_FPS {
                                div {
                                    key: "{fps}",
                                    class: if video.screen_fps == fps { "seg active" } else { "seg" },
                                    onclick: move |_| state.update_settings(|s| s.video.screen_fps = fps),
                                    "{fps} FPS"
                                }
                            }
                        }
                    }
                    div { class: "share-option",
                        span { class: "field-label", {t("share.optimize")} }
                        div { class: "segmented",
                            div {
                                class: if video.screen_hint == ContentHint::Detail { "seg active" } else { "seg" },
                                onclick: move |_| state.update_settings(|s| s.video.screen_hint = ContentHint::Detail),
                                {t("share.detail")}
                            }
                            div {
                                class: if video.screen_hint == ContentHint::Motion { "seg active" } else { "seg" },
                                onclick: move |_| state.update_settings(|s| s.video.screen_hint = ContentHint::Motion),
                                {t("share.motion")}
                            }
                        }
                    }
                    div { class: "share-option inline",
                        Toggle { on: video.screen_audio, onchange: move |v| state.update_settings(|s| s.video.screen_audio = v) }
                        span { {t("share.audio")} }
                    }
                }
                if *starting.read() {
                    div { class: "share-status", div { class: "spinner small" } span { "{t(\"share.starting\")} ({quality_label})" } }
                }
                if let Some(err) = error.read().as_ref() {
                    div { class: "form-error", "{err}" }
                }
            }
        }
    }
}

/// Starts or joins the call in a channel (voice channel or DM), optionally
/// with the camera on.
pub fn start(state: AppState, channel: ChannelId, camera: bool) {
    if state.call.peek().as_ref().is_some_and(|c| c.channel == channel) {
        if camera && !state.call.peek().as_ref().is_some_and(|c| c.camera_on) {
            toggle_camera(state);
        }
        return;
    }
    let engine = super::use_services_engine();
    spawn_forever(async move {
        match connect(state, channel, engine.clone()).await {
            Ok(()) => {
                if camera {
                    toggle_camera(state);
                }
            }
            Err(e) => state.report(Err(e)),
        }
    });
}

/// The call area above a DM's messages: the call itself while connected,
/// otherwise a banner when others are in it.
#[component]
pub fn DmCall(channel: ChannelId) -> Element {
    let state = use_context::<AppState>();
    let in_call = state.call.read().as_ref().is_some_and(|c| c.channel == channel);
    let mut others = use_signal(Vec::<sivryn_client_core::protocol::UserId>::new);
    use_future(move || async move {
        loop {
            if let Some(s) = state.session.cloned()
                && !state.call.peek().as_ref().is_some_and(|c| c.channel == channel)
            {
                let now: Vec<_> = match s.api.channel_voice_state(channel).await {
                    Ok(v) => v.participants.into_iter().map(|p| p.user_id).collect(),
                    Err(_) => Vec::new(),
                };
                if *others.peek() != now {
                    others.set(now);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
    if in_call {
        return rsx! { div { class: "dm-call", CallView { compact: true } } };
    }
    let present = others.read().clone();
    if present.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "dm-call-banner",
            div { class: "join-present",
                for u in present.iter() {
                    Avatar { key: "{u}", name: state.username(*u), id: Some(*u), size: 28 }
                }
            }
            span { {t("call.ongoing")} }
            span { class: "topbar-spacer" }
            button { class: "button primary small", onclick: move |_| start(state, channel, false),
                Svg { icon: Icon::Phone, size: 15 }
                {t("call.join")}
            }
        }
    }
}

/// Rings until the call is answered, declined or 30 seconds have passed.
pub fn ring(mut state: AppState) {
    static RINGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RINGING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let engine = super::use_services_engine();
    spawn_forever(async move {
        let started = std::time::Instant::now();
        let first = *state.ringing.peek();
        while state.ringing.peek().is_some() && *state.ringing.peek() == first {
            if started.elapsed() > std::time::Duration::from_secs(30) {
                state.ringing.set(None);
                break;
            }
            engine.play(Sound::Ring);
            tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        }
        RINGING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

#[component]
pub fn IncomingCall() -> Element {
    let mut state = use_context::<AppState>();
    let Some((channel, caller)) = *state.ringing.read() else { return rsx! {} };
    let name = state.username(caller);
    let group = state
        .dm(channel)
        .filter(|d| d.channel.kind == sivryn_client_core::protocol::model::ChannelKind::Group)
        .map(|d| state.dm_title(&d));
    let subtitle = match group {
        Some(group) => format!("{} {group}", t("call.incoming_group")),
        None => t("call.incoming").to_string(),
    };

    rsx! {
        div { class: "incoming-call",
            Avatar { name: name.clone(), id: Some(caller), size: 72 }
            div { class: "incoming-name", "{name}" }
            div { class: "muted small", "{subtitle}" }
            div { class: "incoming-actions",
                IconButton { icon: Icon::PhoneOff, class: "control danger", title: t("call.decline"), onclick: move |_| state.ringing.set(None) }
                IconButton {
                    icon: Icon::Video,
                    class: "control accept",
                    title: t("call.accept_video"),
                    onclick: move |_| {
                        state.ringing.set(None);
                        state.open_channel(channel);
                        start(state, channel, true);
                    },
                }
                IconButton {
                    icon: Icon::Phone,
                    class: "control accept",
                    title: t("call.accept"),
                    onclick: move |_| {
                        state.ringing.set(None);
                        state.open_channel(channel);
                        start(state, channel, false);
                    },
                }
            }
        }
    }
}

/// Milliseconds since the last keyboard or mouse input anywhere.
#[cfg(windows)]
fn idle_ms() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: plain query into a correctly sized struct.
    if unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        // SAFETY: no arguments.
        u64::from(unsafe { GetTickCount() }.wrapping_sub(info.dwTime))
    } else {
        0
    }
}

#[cfg(not(windows))]
fn idle_ms() -> u64 {
    0
}

/// Moves you to the guild's AFK channel after its timeout without input and
/// without speaking.
pub async fn afk_watch(state: AppState) {
    let engine = super::use_services_engine();
    let mut last_spoke = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if engine.transmitting() {
            last_spoke = std::time::Instant::now();
        }
        let Some(channel) = state.call.peek().as_ref().map(|c| c.channel) else { continue };
        let Some(guild) = state.guild_of(channel).and_then(|g| state.guild(g)) else { continue };
        let Some(afk) = guild.afk_channel_id.filter(|afk| *afk != channel) else { continue };
        let timeout = u64::from(guild.afk_timeout_minutes.max(1)) * 60_000;
        if idle_ms() >= timeout && last_spoke.elapsed().as_millis() as u64 >= timeout {
            tracing::info!("idle, moving to the AFK channel");
            match connect(state, afk, engine.clone()).await {
                Ok(()) => state.toast(t("call.moved_afk"), false),
                Err(e) => state.report(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_goes_in_steps_and_stays_in_the_picture() {
        assert_eq!(step_zoom(1.0, 1.0), 1.5);
        assert_eq!(step_zoom(1.5, 3.0), 2.0);
        assert_eq!(step_zoom(6.0, 1.0), 6.0);
        assert_eq!(step_zoom(2.0, -1.0), 1.5);
        assert_eq!(step_zoom(1.0, -1.0), 1.0);
        // At 2× the shown half can't move past the edges.
        let view = View { zoom: 2.0, centre: (0.95, 0.1) };
        assert_eq!(view.bounds(), ((0.5, 0.0), (1.0, 0.5)));
    }
}

#[cfg(test)]
mod video_tests {
    use super::*;

    fn feed(participant: &str, source: TrackSource, sid: &str) -> VideoFeed {
        let (_tx, frames) = tokio::sync::watch::channel(None);
        VideoFeed { participant: participant.into(), source, track_sid: sid.into(), frames }
    }

    fn quality_of(wanted: &[(String, Option<VideoQuality>)], sid: &str) -> Option<VideoQuality> {
        wanted.iter().find(|(s, _)| s == sid).and_then(|(_, q)| *q)
    }

    #[test]
    fn grid_quality_follows_size() {
        let feeds = vec![
            feed("me", TrackSource::Camera, "mine"),
            feed("a", TrackSource::Camera, "cam-a"),
            feed("b", TrackSource::Camera, "cam-b"),
        ];
        let shown = vec!["me".to_string(), "a".to_string(), "b".to_string()];
        let w = wanted_video(&feeds, "me", None, &shown, false, false, 2);
        // Your own camera is never subscribed.
        assert!(w.iter().all(|(s, _)| s != "mine"));
        assert_eq!(quality_of(&w, "cam-a"), Some(VideoQuality::High));
        let w = wanted_video(&feeds, "me", None, &shown, false, false, 5);
        assert_eq!(quality_of(&w, "cam-a"), Some(VideoQuality::Medium));
        let w = wanted_video(&feeds, "me", None, &shown, false, false, 12);
        assert_eq!(quality_of(&w, "cam-b"), Some(VideoQuality::Low));
    }

    #[test]
    fn hidden_tiles_and_data_saver_pause_cameras() {
        let feeds = vec![
            feed("a", TrackSource::Camera, "cam-a"),
            feed("b", TrackSource::Camera, "cam-b"),
            feed("c", TrackSource::Screenshare, "screen-c"),
        ];
        // b is on another page.
        let w = wanted_video(&feeds, "me", None, &["a".into(), "screen-c".into()], false, false, 2);
        assert_eq!(quality_of(&w, "cam-b"), None);
        assert_eq!(quality_of(&w, "screen-c"), Some(VideoQuality::High));

        // Focus on the screen with the strip hidden: cameras pause.
        let focus = Focus::Screen("screen-c".into());
        let shown = vec!["screen-c".to_string()];
        let w = wanted_video(&feeds, "me", Some(&focus), &shown, true, false, 3);
        assert_eq!(quality_of(&w, "cam-a"), None);
        assert_eq!(quality_of(&w, "screen-c"), Some(VideoQuality::High));

        // Strip shown: cameras small.
        let shown = vec!["screen-c".to_string(), "a".into(), "b".into()];
        let w = wanted_video(&feeds, "me", Some(&focus), &shown, false, false, 3);
        assert_eq!(quality_of(&w, "cam-a"), Some(VideoQuality::Low));

        // Data saver: no cameras, screens low.
        let w = wanted_video(&feeds, "me", None, &shown, false, true, 3);
        assert_eq!(quality_of(&w, "cam-a"), None);
        assert_eq!(quality_of(&w, "screen-c"), Some(VideoQuality::Low));
    }

    #[test]
    fn focused_camera_is_full_size() {
        let feeds = vec![feed("a", TrackSource::Camera, "cam-a"), feed("b", TrackSource::Camera, "cam-b")];
        let focus = Focus::User("a".into());
        let shown = vec!["a".to_string(), "b".to_string()];
        let w = wanted_video(&feeds, "me", Some(&focus), &shown, false, false, 2);
        assert_eq!(quality_of(&w, "cam-a"), Some(VideoQuality::High));
        assert_eq!(quality_of(&w, "cam-b"), Some(VideoQuality::Low));
    }

    #[test]
    fn pages() {
        let people: Vec<u32> = (0..40).collect();
        let (first, pages) = page_of(&people, 0);
        assert_eq!((first.len(), pages), (PAGE_SIZE, 3));
        let (last, _) = page_of(&people, 2);
        assert_eq!(last, (32..40).collect::<Vec<_>>());
        // A page past the end shows the last one.
        assert_eq!(page_of(&people, 9).0, last);
        assert_eq!(page_of::<u32>(&[], 0), (Vec::new(), 1));
    }

    #[test]
    fn stats_rows_use_consistent_units() {
        let stats = sivryn_media::CallStats {
            rtt_ms: Some(41.6),
            loss_percent: 2.25,
            send_kbps: 1200.4,
            receive_kbps: 3400.0,
            available_send_kbps: Some(5000.0),
            codecs: vec!["audio/opus".into(), "video/AV1".into()],
            video_in: Some((1920, 1080, 30.0)),
            jitter_ms: 3.0,
        };
        let rows = stats_rows(&stats);
        let get = |k: &str| rows.iter().find(|(l, _)| *l == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("call.stats_rtt"), "42 ms");
        assert_eq!(get("call.stats_loss"), "2.2 %");
        assert_eq!(get("call.stats_send"), "1200 kbit/s");
        assert_eq!(get("call.stats_receive"), "3400 kbit/s");
        assert_eq!(get("call.stats_available"), "5000 kbit/s");
        assert_eq!(get("call.stats_video"), "1920×1080 · 30 fps");
        assert_eq!(get("call.stats_codecs"), "audio/opus, video/AV1");
        // Every label has a translation.
        for (label, _) in rows {
            assert_ne!(crate::i18n::t(label), label);
        }
    }
}

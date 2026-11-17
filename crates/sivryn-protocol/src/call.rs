//! In-call signals between clients (raise hand, reactions, moderation
//! requests) and the moderation API.
//!
//! Signals travel over LiveKit's reliable data channel with the topic
//! [`SIGNAL_TOPIC`]. They carry no secrets: the SFU (run by the instance
//! admin) can see them, like it sees who is muted.

use serde::{Deserialize, Serialize};

/// LiveKit data topic for [`CallSignal`]s.
pub const SIGNAL_TOPIC: &str = "sivryn.call";

/// Longest reaction accepted (one emoji, possibly with modifiers).
pub const MAX_REACTION_LEN: usize = 32;

/// A message one participant sends to the others during a call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallSignal {
    /// Raised or lowered a hand (also "request to speak" where speaking
    /// needs a moderator's go-ahead).
    Hand { raised: bool },
    /// A short-lived emoji reaction over the sender's tile.
    Reaction { emoji: String },
    /// A moderator asks `target` to unmute (it is never forced).
    AskUnmute { target: String },
}

impl CallSignal {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("call signals serialize")
    }

    /// Decodes a signal; unknown or oversized ones are ignored (`None`).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > 1024 {
            return None;
        }
        let signal: Self = serde_json::from_slice(bytes).ok()?;
        match &signal {
            CallSignal::Reaction { emoji } if emoji.is_empty() || emoji.len() > MAX_REACTION_LEN => None,
            _ => Some(signal),
        }
    }
}

/// What a moderator does to someone in a call
/// (`POST /api/v1/channels/{id}/voice/{user}/moderate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerateAction {
    /// Mutes their microphone (they can unmute again if allowed to speak).
    Mute,
    /// Turns their camera off.
    StopCamera,
    /// Stops their screen share.
    StopShare,
    /// Lets them speak, show video and share for the rest of this call
    /// (a raised hand that was accepted).
    AllowSpeak,
    /// Takes speaking, video and sharing away for the rest of this call.
    RevokeSpeak,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModerateRequest {
    pub action: ModerateAction,
}

/// `POST /api/v1/channels/{id}/voice/mute-all`: mutes everyone but the caller.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MuteAllResponse {
    pub muted: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_round_trip() {
        for s in [
            CallSignal::Hand { raised: true },
            CallSignal::Reaction { emoji: "🎉".into() },
            CallSignal::AskUnmute { target: "abc".into() },
        ] {
            assert_eq!(CallSignal::decode(&s.encode()), Some(s));
        }
    }

    #[test]
    fn bad_signals_are_ignored() {
        assert_eq!(CallSignal::decode(b"not json"), None);
        assert_eq!(CallSignal::decode(br#"{"type":"unknown"}"#), None);
        assert_eq!(CallSignal::decode(br#"{"type":"reaction","emoji":""}"#), None);
        let long = format!(r#"{{"type":"reaction","emoji":"{}"}}"#, "x".repeat(MAX_REACTION_LEN + 1));
        assert_eq!(CallSignal::decode(long.as_bytes()), None);
        assert_eq!(CallSignal::decode(&vec![b' '; 2000]), None);
    }

    #[test]
    fn wire_format() {
        assert_eq!(
            String::from_utf8(CallSignal::Hand { raised: true }.encode()).unwrap(),
            r#"{"type":"hand","raised":true}"#
        );
        let req: ModerateRequest = serde_json::from_str(r#"{"action":"allow_speak"}"#).unwrap();
        assert_eq!(req.action, ModerateAction::AllowSpeak);
    }
}

//! Who actually spoke a turn, when that is not the same as whose turn it is.
//!
//! An execution session is defined by its **initiator**: the account or agent
//! whose task it is. The intervention tier (plan D1/D2) lets a human append a
//! turn to a session they did not start — most importantly to the
//! agent-to-agent session whose only other writer is an agent that has already
//! been told the run failed. Those two facts must not be flattened into one:
//! "whose session is this" and "who typed this line" are different, and a
//! transcript that cannot tell them apart lets a human's words pass as the
//! agent's own.
//!
//! So the row carries who spoke, as data, at the moment it is written — never
//! inferred later from the session's initiator, from the logged-in account, or
//! from which surface happens to be rendering. The shape follows
//! [`crate::agent::intent_turn::IntentDisplay`]: one metadata key, one struct,
//! one reader, all three owned by the producer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Metadata key under which a `user` row records its speaker.
///
/// A member of its own (`speaker`) rather than a field of the `intent` display:
/// the row's metadata is a shared bag (`round_id`, `internal`, `intent`, tool
/// fields) and folding provenance into any of those would change what they mean.
pub const META_KEY_TURN_SPEAKER: &str = "speaker";

/// The two kinds of party that can append a turn to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerOrigin {
    /// A human account. [`TurnSpeaker::author`] names it.
    Human,
    /// An agent instance — a turn that arrived through an Intent envelope, so
    /// nobody typed it.
    Agent,
}

impl SpeakerOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

/// Who spoke one turn, and on whose behalf.
///
/// Written once, onto the `user` row it describes, by the only party that knows
/// the answer (the write entry point, which has the verified caller uid). Every
/// surface reads it back through [`TurnSpeaker::from_metadata`].
///
/// Emptiness is judged once too: a turn with nothing to say about its speaker
/// (no identity on the transport, the ordinary human-typing-into-their-own-chat
/// case included) writes no key at all, and reads back as `None`. Surfaces then
/// render exactly what they rendered before this type existed — the fallback,
/// not a blank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpeaker {
    pub origin: SpeakerOrigin,
    /// The human account that wrote the row (`origin = Human`). For an agent
    /// turn this is the originating agent instance, when the transport carried
    /// one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// The party this row is voiced **as**, when that is not the author: the
    /// session's initiator, whose task the human is standing in for.
    ///
    /// `Some` is the whole point of the type — it is what makes "agent A said
    /// this" and "a human said this as A" distinguishable on the row itself. It
    /// is `None` when the author speaks for nobody but themselves, which
    /// includes a human who has joined the session in their own right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

impl TurnSpeaker {
    /// Record a turn appended by `caller` to a session whose initiator is
    /// `initiator`, bound to `chat` (the Chat the caller may or may not be a
    /// participant of).
    ///
    /// The voice rule (plan D3), in one place:
    ///
    /// * no caller identity → `None`. The MQTT path carries no identity, and a
    ///   fabricated author is worse than an absent one.
    /// * the caller is the initiator → their own voice. This is the ordinary
    ///   single-author session and must stay invisible.
    /// * the caller is a participant of the session's bound Chat → their own
    ///   voice (mode 3, "the human joined"). A person who is present does not
    ///   need to be spoken for; voicing them as someone else would erase their
    ///   presence from the record.
    /// * otherwise → the initiator's voice (`on_behalf_of`), which is mode 1:
    ///   an agent-to-agent session where nobody could answer as themselves.
    ///
    /// Voice follows **presence**, not permission: an intervener of a
    /// chat-bound session they participate in speaks as themselves even though
    /// their write access came from the roster.
    pub fn for_write(
        initiator: Option<&str>,
        session_chat: Option<&str>,
        caller: &str,
        caller_chats: &[String],
    ) -> Option<Self> {
        let caller = caller.trim();
        if caller.is_empty() {
            return None;
        }
        let initiator = initiator.map(str::trim).filter(|s| !s.is_empty());
        if initiator == Some(caller) {
            return None;
        }
        let participates = session_chat
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .is_some_and(|c| caller_chats.iter().any(|x| x == c));
        if participates {
            return Some(Self {
                origin: SpeakerOrigin::Human,
                author: caller.to_string(),
                on_behalf_of: None,
            });
        }
        Some(Self {
            origin: SpeakerOrigin::Human,
            author: caller.to_string(),
            on_behalf_of: initiator.map(str::to_string),
        })
    }

    /// Is this row voiced as somebody else?
    pub fn is_substitution(&self) -> bool {
        self.on_behalf_of.is_some()
    }

    /// The `metadata` member this speaker contributes.
    ///
    /// A map rather than a bare value so the write point can merge it into the
    /// row's existing metadata (`round_id`, an Intent display) without having to
    /// know this type's key — the shape the runtime's
    /// `agent::intent_turn::turn_row_meta` already uses.
    pub fn to_meta(&self) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        if let Ok(value) = serde_json::to_value(self) {
            map.insert(META_KEY_TURN_SPEAKER.to_string(), value);
        }
        map
    }

    /// The speaker a row's `metadata` records, when it records a usable one.
    ///
    /// The **only** reader of [`META_KEY_TURN_SPEAKER`] and the only place that
    /// decides what "usable" means, for the same reason
    /// [`crate::agent::intent_turn::IntentDisplay::from_metadata`] is: a second
    /// reader would let the key or the rule drift per surface.
    pub fn from_metadata(metadata: &Value) -> Option<Self> {
        let raw = metadata.get(META_KEY_TURN_SPEAKER)?;
        let speaker: Self = serde_json::from_value(raw.clone()).ok()?;
        let says_something = !speaker.author.trim().is_empty() || speaker.on_behalf_of.is_some();
        says_something.then_some(speaker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ordinary_author_stays_invisible() {
        // A human typing into the session they started: nothing to announce.
        assert_eq!(TurnSpeaker::for_write(Some("u-alice"), None, "u-alice", &[]), None);
        // No identity on the transport (MQTT): never invent an author.
        assert_eq!(TurnSpeaker::for_write(Some("u-alice"), None, "  ", &[]), None);
    }

    #[test]
    fn an_intervener_speaks_as_the_initiator() {
        // The reported case: an agent started this session, a human keeps it
        // alive. The row must say so, and must name the agent it speaks for.
        let s = TurnSpeaker::for_write(Some("agent-a"), None, "nicholas", &[]).expect("speaker");
        assert_eq!(s.origin, SpeakerOrigin::Human);
        assert_eq!(s.author, "nicholas");
        assert_eq!(s.on_behalf_of.as_deref(), Some("agent-a"));
        assert!(s.is_substitution());
    }

    #[test]
    fn a_participant_speaks_as_themselves() {
        // Mode 3: the human joined the Chat, so they are present — voicing them
        // as the initiator would erase exactly the fact the mode exists to add.
        let chats = vec!["chat-1".to_string()];
        let s = TurnSpeaker::for_write(Some("agent-a"), Some("chat-1"), "nicholas", &chats)
            .expect("speaker");
        assert_eq!(s.author, "nicholas");
        assert_eq!(s.on_behalf_of, None);
        assert!(!s.is_substitution());
        // A chat the caller is not in changes nothing about the voice rule for a
        // different chat.
        let other = TurnSpeaker::for_write(Some("agent-a"), Some("chat-2"), "nicholas", &chats)
            .expect("speaker");
        assert_eq!(other.on_behalf_of.as_deref(), Some("agent-a"));
    }

    #[test]
    fn the_row_is_the_only_source_and_it_round_trips() {
        let s = TurnSpeaker::for_write(Some("agent-a"), None, "nicholas", &[]).unwrap();
        let meta = Value::Object(s.to_meta());
        assert_eq!(TurnSpeaker::from_metadata(&meta), Some(s));
        // Absent, null and junk metadata all read as "no speaker recorded" —
        // one answer, so no surface has to distinguish them.
        assert_eq!(TurnSpeaker::from_metadata(&serde_json::json!({})), None);
        assert_eq!(
            TurnSpeaker::from_metadata(&serde_json::json!({ META_KEY_TURN_SPEAKER: null })),
            None
        );
        assert_eq!(
            TurnSpeaker::from_metadata(&serde_json::json!({ META_KEY_TURN_SPEAKER: "junk" })),
            None
        );
        // A partial write (author empty, nobody stood in for) says nothing.
        assert_eq!(
            TurnSpeaker::from_metadata(&serde_json::json!({
                META_KEY_TURN_SPEAKER: { "origin": "human", "author": " " }
            })),
            None
        );
    }

    #[test]
    fn the_wire_names_match_the_variants() {
        assert_eq!(
            serde_json::to_value(SpeakerOrigin::Human).unwrap(),
            serde_json::json!("human")
        );
        assert_eq!(
            serde_json::to_value(SpeakerOrigin::Agent).unwrap(),
            serde_json::json!("agent")
        );
        // The row is read by surfaces that only know these exact strings.
        let s = TurnSpeaker::for_write(None, None, "nicholas", &[]).unwrap();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["origin"], "human");
        assert_eq!(v["author"], "nicholas");
        // An absent `on_behalf_of` is omitted, not `null`: a reader that checks
        // the field's presence and one that checks its value agree.
        assert!(v.get("on_behalf_of").is_none(), "{v}");
    }
}

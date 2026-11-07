//! Presence: the live heartbeat, and how a manual choice reads (design `docs/design/hub.md`,
//! section 5).
//!
//! - **Live heartbeat**: while a person holds a socket or a stream, it refreshes a Valkey key
//!   `presence:<account>` with a short lifetime. A present key means reachable now.
//! - **Manual choice** (`active`, `away`, `dnd`, `invisible`): how a reachable person reads to
//!   others. It moves from Propolis's accounts to the hub with the rest of presence.
//!
//! Who sees whose presence (whoever shares an organisation), the manual choice's storage, counting
//! connections across processes and announcing changes come with the replica of the organisations'
//! members.

use fred::interfaces::KeysInterface;
use fred::prelude::Pool;
use fred::types::Expiration;
use uuid::Uuid;

use super::ws::HEARTBEAT_SECS;

const PREFIX: &str = "presence:";

/// How long a heartbeat holds: two of them, so one late ping does not read as gone.
pub const TTL_SECS: i64 = 2 * HEARTBEAT_SECS as i64;

/// Refresh an account's heartbeat.
pub async fn heartbeat(valkey: &Pool, account: Uuid) {
    if let Err(error) = valkey
        .set::<(), _, _>(
            format!("{PREFIX}{account}"),
            "1",
            Some(Expiration::EX(TTL_SECS)),
            None,
            false,
        )
        .await
    {
        tracing::warn!(%error, "cannot write a presence heartbeat");
    }
}

/// Clear an account's heartbeat (its last connection here went).
pub async fn clear(valkey: &Pool, account: Uuid) {
    let _ = valkey.del::<u64, _>(format!("{PREFIX}{account}")).await;
}

/// Whether an account has a live heartbeat.
pub async fn is_online(valkey: &Pool, account: Uuid) -> bool {
    matches!(valkey.exists::<u64, _>(format!("{PREFIX}{account}")).await, Ok(n) if n > 0)
}

/// The presence a *third party* sees. `invisible` and an absent heartbeat both read as `offline`.
///
/// An override says how to read someone who is reachable; it does not make them reachable. Without
/// a heartbeat the answer is `offline` whatever they chose, because the alternative is a dot that
/// claims a colleague is there when nothing of theirs is connected. `active` is the case that made
/// this visible: it was returned even with the heartbeat long lapsed, so anyone who had once
/// picked "online" stayed green to the whole space for good.
pub fn effective_for_others(manual: Option<&str>, online: bool) -> &'static str {
    if !online {
        return "offline";
    }
    match manual {
        Some("invisible") => "offline",
        Some("dnd") => "dnd",
        Some("away") => "away",
        _ => "active",
    }
}

/// The presence a user sees for *themselves* (keeps `invisible` visible in their own UI).
///
/// Their own choice is shown back to them even while nothing is connected, so the menu reflects
/// what they picked rather than silently disagreeing with it. What everyone else sees is still
/// [`effective_for_others`], which needs a heartbeat.
pub fn effective_for_self(manual: Option<&str>, online: bool) -> &'static str {
    match manual {
        Some("invisible") => "invisible",
        Some("dnd") => "dnd",
        Some("away") => "away",
        Some("active") => "active",
        _ => {
            if online {
                "active"
            } else {
                "offline"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_for_others, effective_for_self};

    #[test]
    fn auto_presence_follows_the_heartbeat() {
        assert_eq!(effective_for_others(None, true), "active");
        assert_eq!(effective_for_others(None, false), "offline");
    }

    #[test]
    fn invisible_reads_as_offline_to_others_but_visible_to_self() {
        assert_eq!(effective_for_others(Some("invisible"), true), "offline");
        assert_eq!(effective_for_self(Some("invisible"), true), "invisible");
    }

    #[test]
    fn an_override_colours_a_live_connection() {
        assert_eq!(effective_for_others(Some("dnd"), true), "dnd");
        assert_eq!(effective_for_others(Some("away"), true), "away");
        assert_eq!(effective_for_others(Some("active"), true), "active");
    }

    #[test]
    fn no_override_makes_an_absent_user_look_present() {
        // The rule that matters: a choice says how to read someone who is reachable, it does not
        // make them reachable. Anything else leaves a colleague green with nothing connected.
        for manual in [
            None,
            Some("active"),
            Some("away"),
            Some("dnd"),
            Some("invisible"),
        ] {
            assert_eq!(effective_for_others(manual, false), "offline");
        }
    }

    #[test]
    fn a_user_still_sees_their_own_choice_while_disconnected() {
        assert_eq!(effective_for_self(Some("away"), false), "away");
        assert_eq!(effective_for_self(None, false), "offline");
    }
}

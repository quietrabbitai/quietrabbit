// src-tauri/src/conductor/plan_memory_tests.rs
//
// Tests of conductor::plan_memory (items.id=901 stage 3): the bounds, the expiry, the
// scoping, the single-use token, the marker, the clearing, and the privacy scans.

use super::*;
use crate::conductor::run_plan::{OfferStrategy, Pin};

const USER: &str = "user-1";
const PERSONA: &str = "persona-1";
const FOCUS: &str = "writing-assistant";

fn scope(chat: &str) -> ChatScope {
    ChatScope {
        user_id: USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        context_key: chat.to_owned(),
        focus_id: FOCUS.to_owned(),
    }
}

fn owner() -> OfferOwner {
    OfferOwner {
        user_id: USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        focus_id: FOCUS.to_owned(),
    }
}

fn approval(tag: &str) -> Approval {
    Approval {
        digest: format!("{tag:0>64}"),
        ceiling_micros: 1_000,
        pins: vec![Pin {
            action_id: "draft".to_owned(),
            strategy: OfferStrategy::PaidApi,
            provider_id: "host-a".to_owned(),
            model_id: "SECRET-MODEL".to_owned(),
        }],
    }
}

// -- approvals --------------------------------------------------------------------

#[test]
fn approvals_are_bounded_and_the_oldest_is_evicted() {
    let mem = PlanMemory::default();
    for i in 0..MAX_APPROVALS {
        let s = scope(&format!("chat-{i}"));
        mem.put_chat_approval(&s, approval("a"));
        // Older chats get older ages, so the order is exact.
        mem.age_approval(&s, Duration::from_secs((MAX_APPROVALS - i) as u64));
    }
    assert_eq!(mem.approval_count(), MAX_APPROVALS);
    mem.put_chat_approval(&scope("newest"), approval("b"));
    assert_eq!(
        mem.approval_count(),
        MAX_APPROVALS,
        "never more than the bound"
    );
    assert!(
        mem.get_chat_approval(&scope("chat-0")).is_none(),
        "the oldest went"
    );
    assert!(mem.get_chat_approval(&scope("chat-1")).is_some());
    assert!(mem.get_chat_approval(&scope("newest")).is_some());
    // Replacing one of them does not evict anything.
    mem.put_chat_approval(&scope("chat-1"), approval("c"));
    assert_eq!(mem.approval_count(), MAX_APPROVALS);
}

#[test]
fn an_approval_expires_two_hours_after_it_was_made_and_reading_does_not_extend_it() {
    assert_eq!(APPROVAL_TTL, Duration::from_secs(7200));
    assert_eq!(MAX_APPROVALS, 64);
    let mem = PlanMemory::default();
    let s = scope("chat-1");
    mem.put_chat_approval(&s, approval("a"));
    mem.age_approval(&s, APPROVAL_TTL - Duration::from_secs(5));
    assert!(mem.get_chat_approval(&s).is_some());
    // Reading did not slide the window: five more seconds and it is gone.
    mem.age_approval(&s, Duration::from_secs(10));
    assert!(mem.get_chat_approval(&s).is_none());
    assert_eq!(mem.approval_count(), 0, "an expired approval is dropped");
}

#[test]
fn an_approval_is_only_visible_to_its_own_user_persona_chat_and_focus() {
    let mem = PlanMemory::default();
    let s = scope("chat-1");
    mem.put_chat_approval(&s, approval("a"));
    for other in [
        ChatScope {
            user_id: "user-2".into(),
            ..s.clone()
        },
        ChatScope {
            persona_id: "persona-2".into(),
            ..s.clone()
        },
        ChatScope {
            context_key: "chat-2".into(),
            ..s.clone()
        },
        ChatScope {
            focus_id: "tech-support".into(),
            ..s.clone()
        },
    ] {
        assert!(mem.get_chat_approval(&other).is_none());
        assert!(!mem.remove_chat_approval(&other));
    }
    assert!(mem.get_chat_approval(&s).is_some());
    // Revocation reaches the memory-only approval.
    assert!(mem.remove_chat_approval(&s));
    assert!(mem.get_chat_approval(&s).is_none());
}

// -- offers of a run with no chat ---------------------------------------------------

#[test]
fn a_chatless_offer_is_identified_by_an_opaque_token_approved_once_and_consumed_with_the_run() {
    let mem = PlanMemory::default();
    let digest = "d".repeat(64);
    let token = mem.register_offer(&owner(), &digest).unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(token, mem.register_offer(&owner(), &digest).unwrap());

    // Not approved yet: a run cannot take it, and it stays.
    assert!(mem.take_offer_approval(&token, &owner()).is_none());
    assert_eq!(mem.offer_count(), 2);

    // Another user, Persona or Focus cannot approve or take it.
    for other in [
        OfferOwner {
            user_id: "user-2".into(),
            ..owner()
        },
        OfferOwner {
            persona_id: "persona-2".into(),
            ..owner()
        },
        OfferOwner {
            focus_id: "tech-support".into(),
            ..owner()
        },
    ] {
        assert_eq!(
            mem.approve_offer(&token, &other, &digest, approval("a")),
            Err(OfferError::NotFound)
        );
    }
    // A changed plan approves nothing.
    assert_eq!(
        mem.approve_offer(&token, &owner(), &"e".repeat(64), approval("a")),
        Err(OfferError::PlanChanged)
    );
    assert!(mem.take_offer_approval(&token, &owner()).is_none());

    mem.approve_offer(&token, &owner(), &digest, approval("a"))
        .unwrap();
    let other = OfferOwner {
        user_id: "user-2".into(),
        ..owner()
    };
    assert!(
        mem.take_offer_approval(&token, &other).is_none(),
        "not another owner's"
    );
    assert!(mem
        .take_offer_approval("0".repeat(32).as_str(), &owner())
        .is_none());

    // The run takes it, and it is gone: a later run cannot reuse it.
    assert_eq!(
        mem.take_offer_approval(&token, &owner()),
        Some(approval("a"))
    );
    assert!(mem.take_offer_approval(&token, &owner()).is_none());
    assert_eq!(
        mem.offer_count(),
        1,
        "only the other, unapproved offer is left"
    );
}

#[test]
fn offers_are_bounded_expire_in_ten_minutes_and_can_be_revoked() {
    assert_eq!((MAX_OFFERS, OFFER_TTL), (16, Duration::from_secs(600)));
    let mem = PlanMemory::default();
    let digest = "d".repeat(64);
    let mut tokens = Vec::new();
    for i in 0..MAX_OFFERS {
        let t = mem.register_offer(&owner(), &digest).unwrap();
        mem.age_offer(&t, Duration::from_secs((MAX_OFFERS - i) as u64));
        tokens.push(t);
    }
    let newest = mem.register_offer(&owner(), &digest).unwrap();
    assert_eq!(mem.offer_count(), MAX_OFFERS);
    assert_eq!(
        mem.approve_offer(&tokens[0], &owner(), &digest, approval("a")),
        Err(OfferError::NotFound),
        "the oldest was evicted"
    );
    assert!(mem
        .approve_offer(&newest, &owner(), &digest, approval("a"))
        .is_ok());

    // Expiry: ten minutes, taken or not.
    mem.age_offer(&newest, OFFER_TTL + Duration::from_secs(1));
    assert!(mem.take_offer_approval(&newest, &owner()).is_none());
    assert!(mem.offer_count() < MAX_OFFERS, "expired offers are dropped");

    // Revocation removes an approved offer, only for its owner.
    let t = mem.register_offer(&owner(), &digest).unwrap();
    mem.approve_offer(&t, &owner(), &digest, approval("a"))
        .unwrap();
    let other = OfferOwner {
        persona_id: "persona-2".into(),
        ..owner()
    };
    assert!(!mem.revoke_offer(&t, &other));
    assert!(mem.revoke_offer(&t, &owner()));
    assert!(mem.take_offer_approval(&t, &owner()).is_none());
}

// -- the marker -----------------------------------------------------------------------

#[test]
fn the_marker_holds_a_hash_one_per_chat_and_is_always_removed_when_taken() {
    assert_eq!((MARKER_TTL, MAX_MARKERS), (Duration::from_secs(120), 64));
    let mem = PlanMemory::default();
    mem.set_marker(USER, PERSONA, "chat-1", "m-1", "the turn text");
    let m = mem.take_marker(USER, PERSONA, "chat-1").unwrap();
    assert_eq!(m.message_id, "m-1");
    assert!(m.matches("the turn text"));
    assert!(!m.matches("the turn text "));
    assert!(
        mem.take_marker(USER, PERSONA, "chat-1").is_none(),
        "taking consumes it"
    );

    // A mismatch is the caller's to decide, and the marker is gone either way.
    mem.set_marker(USER, PERSONA, "chat-1", "m-2", "one");
    let m = mem.take_marker(USER, PERSONA, "chat-1").unwrap();
    assert!(!m.matches("two"));
    assert!(mem.take_marker(USER, PERSONA, "chat-1").is_none());

    // One per chat: a newer one replaces it. Another chat is separate.
    mem.set_marker(USER, PERSONA, "chat-1", "m-3", "first");
    mem.set_marker(USER, PERSONA, "chat-1", "m-4", "second");
    mem.set_marker(USER, PERSONA, "chat-2", "m-5", "other chat");
    assert_eq!(mem.marker_count(), 2);
    assert_eq!(
        mem.take_marker(USER, PERSONA, "chat-1").unwrap().message_id,
        "m-4"
    );
    assert!(mem.take_marker("user-2", PERSONA, "chat-2").is_none());
    assert!(mem.take_marker(USER, "persona-2", "chat-2").is_none());
    assert!(mem.take_marker(USER, PERSONA, "chat-2").is_some());
}

#[test]
fn the_marker_expires_in_two_minutes_and_is_bounded() {
    let mem = PlanMemory::default();
    mem.set_marker(USER, PERSONA, "chat-1", "m-1", "text");
    mem.age_marker(USER, PERSONA, "chat-1", MARKER_TTL - Duration::from_secs(5));
    assert!(mem.take_marker(USER, PERSONA, "chat-1").is_some());
    mem.set_marker(USER, PERSONA, "chat-1", "m-1", "text");
    mem.age_marker(USER, PERSONA, "chat-1", MARKER_TTL + Duration::from_secs(1));
    assert!(mem.take_marker(USER, PERSONA, "chat-1").is_none());

    for i in 0..MAX_MARKERS {
        let chat = format!("chat-{i}");
        mem.set_marker(USER, PERSONA, &chat, "m", "t");
        mem.age_marker(
            USER,
            PERSONA,
            &chat,
            Duration::from_secs((MAX_MARKERS - i) as u64),
        );
    }
    mem.set_marker(USER, PERSONA, "newest", "m", "t");
    assert_eq!(mem.marker_count(), MAX_MARKERS);
    assert!(
        mem.take_marker(USER, PERSONA, "chat-0").is_none(),
        "the oldest went"
    );
}

// -- clearing ---------------------------------------------------------------------------

fn fill(mem: &PlanMemory) {
    for user in [USER, "user-2"] {
        let mut s = scope("chat-1");
        s.user_id = user.to_owned();
        mem.put_chat_approval(&s, approval("a"));
        let mut other_focus = s.clone();
        other_focus.focus_id = "tech-support".into();
        mem.put_chat_approval(&other_focus, approval("b"));
        let mut other_chat = s.clone();
        other_chat.context_key = "chat-2".into();
        mem.put_chat_approval(&other_chat, approval("c"));
        mem.set_marker(user, PERSONA, "chat-1", "m", "t");
        mem.set_marker(user, PERSONA, "chat-2", "m", "t");
        let o = OfferOwner {
            user_id: user.to_owned(),
            ..owner()
        };
        mem.register_offer(&o, &"d".repeat(64)).unwrap();
    }
}

#[test]
fn logout_clears_one_users_memory_and_nothing_of_anyone_else() {
    let mem = PlanMemory::default();
    fill(&mem);
    mem.clear_user(USER);
    let mut mine = scope("chat-1");
    assert!(mem.get_chat_approval(&mine).is_none());
    assert!(mem.take_marker(USER, PERSONA, "chat-1").is_none());
    assert!(mem.take_marker(USER, PERSONA, "chat-2").is_none());
    mine.user_id = "user-2".into();
    assert!(
        mem.get_chat_approval(&mine).is_some(),
        "another account is untouched"
    );
    assert_eq!(mem.approval_count(), 3);
    assert_eq!(mem.offer_count(), 1);
    assert_eq!(mem.marker_count(), 2);
}

#[test]
fn clearing_a_chat_removes_every_focus_of_it_and_its_marker_only() {
    let mem = PlanMemory::default();
    fill(&mem);
    mem.clear_chat(USER, PERSONA, "chat-1");
    assert!(mem.get_chat_approval(&scope("chat-1")).is_none());
    let mut other_focus = scope("chat-1");
    other_focus.focus_id = "tech-support".into();
    assert!(
        mem.get_chat_approval(&other_focus).is_none(),
        "every Focus of the chat"
    );
    assert!(
        mem.get_chat_approval(&scope("chat-2")).is_some(),
        "another chat stays"
    );
    assert!(mem.take_marker(USER, PERSONA, "chat-1").is_none());
    assert!(mem.take_marker(USER, PERSONA, "chat-2").is_some());
    assert_eq!(
        mem.approval_count(),
        4,
        "user-2's three and this user's chat-2"
    );
}

#[test]
fn clear_all_empties_everything() {
    let mem = PlanMemory::default();
    fill(&mem);
    mem.clear_all();
    assert_eq!(
        (mem.approval_count(), mem.offer_count(), mem.marker_count()),
        (0, 0, 0)
    );
}

// -- privacy ----------------------------------------------------------------------------

#[test]
fn debug_output_shows_counts_and_nothing_else() {
    let mem = PlanMemory::default();
    let s = scope("SECRET-CHAT");
    let a = approval("abc");
    mem.put_chat_approval(&s, a.clone());
    mem.set_marker(
        USER,
        PERSONA,
        "SECRET-CHAT",
        "SECRET-MESSAGE-ID",
        "SECRET TEXT",
    );
    let token = mem.register_offer(&owner(), &"d".repeat(64)).unwrap();
    let marker = mem.take_marker(USER, PERSONA, "SECRET-CHAT").unwrap();
    let shown = format!(
        "{mem:?} {a:?} {s:?} {:?} {marker:?} {:?}",
        owner(),
        OfferError::NotFound
    );
    for secret in [
        "SECRET-CHAT",
        "SECRET-MESSAGE-ID",
        "SECRET TEXT",
        "SECRET-MODEL",
        "draft",
        USER,
        PERSONA,
        FOCUS,
        &token,
        &a.digest,
        "1000",
    ] {
        assert!(!shown.contains(secret), "Debug leaked {secret}: {shown}");
    }
    assert!(shown.contains("approvals: 1"));
}

// -- the park of a run whose row could not be written (items.id=901 stage 7) ---------------

fn park(tag: &str) -> crate::conductor::run_park::ParkRecord {
    crate::conductor::run_park::ParkRecord {
        code: crate::spend::codes::Code::ResolverTooWeak,
        action_id: "draft".to_owned(),
        params_json: format!("{{\"tag\":\"{tag}\"}}"),
    }
}

#[test]
fn a_held_park_is_scoped_to_user_persona_and_run_and_replaced_by_a_newer_one() {
    let m = PlanMemory::default();
    m.put_run_park(USER, PERSONA, "run-1", park("a"));
    assert_eq!(m.get_run_park(USER, PERSONA, "run-1"), Some(park("a")));
    assert!(m.get_run_park("user-2", PERSONA, "run-1").is_none());
    assert!(m.get_run_park(USER, "persona-2", "run-1").is_none());
    assert!(m.get_run_park(USER, PERSONA, "run-2").is_none());
    m.put_run_park(USER, PERSONA, "run-1", park("b"));
    assert_eq!(m.park_count(), 1);
    assert_eq!(m.get_run_park(USER, PERSONA, "run-1"), Some(park("b")));
    assert!(m.remove_run_park(USER, PERSONA, "run-1"));
    assert!(!m.remove_run_park(USER, PERSONA, "run-1"));
    assert_eq!(m.park_count(), 0);
}

#[test]
fn held_parks_are_bounded_expire_and_go_with_the_user() {
    let m = PlanMemory::default();
    for i in 0..(MAX_PARKS + 10) {
        m.put_run_park(USER, PERSONA, &format!("run-{i}"), park("x"));
    }
    assert_eq!(m.park_count(), MAX_PARKS, "the oldest are evicted");
    assert!(m.get_run_park(USER, PERSONA, "run-0").is_none());
    assert!(m
        .get_run_park(USER, PERSONA, &format!("run-{}", MAX_PARKS + 9))
        .is_some());

    m.put_run_park("user-2", PERSONA, "other", park("y"));
    m.clear_user(USER);
    assert!(m.get_run_park("user-2", PERSONA, "other").is_some());
    assert_eq!(m.park_count(), 1);

    m.age_park("user-2", PERSONA, "other", PARK_TTL);
    assert!(
        m.get_run_park("user-2", PERSONA, "other").is_none(),
        "expired"
    );

    m.put_run_park(USER, PERSONA, "again", park("z"));
    m.clear_all();
    assert_eq!(m.park_count(), 0);
}

fn code_without_comments() -> String {
    include_str!("plan_memory.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The memory home is never serialized, logged, written to a database or a file, put in a
/// checkpoint, or shown in Debug (the derive list is pinned: one derive of Debug).
#[test]
fn the_memory_home_never_reaches_a_store_a_log_or_a_serializer() {
    let code = code_without_comments();
    for banned in [
        "serde",
        "Serialize",
        "Deserialize",
        "log::",
        "println!",
        "eprintln!",
        "dbg!",
        "tracing::",
        "sqlx",
        "persistence",
        "std::fs",
        "std::env",
        "std::net",
        "tauri",
        "checkpoint",
        "format!(\"{:?}\"",
    ] {
        assert!(!code.contains(banned), "'{banned}' in plan_memory.rs");
    }
    // Only the plain error enum derives Debug: every value-holding type has a manual,
    // redacting impl.
    let derives: Vec<&str> = code
        .lines()
        .filter(|l| l.contains("#[derive(") && l.contains("Debug"))
        .collect();
    assert_eq!(derives.len(), 1, "{derives:?}");
    assert!(code.contains("impl fmt::Debug for PlanMemory"));
    assert!(code.contains("impl fmt::Debug for ChatScope"));
    assert!(code.contains("impl fmt::Debug for OfferOwner"));
    assert!(code.contains("impl fmt::Debug for Marker"));
}

/// The clearing is wired where the plan said: the key leaving the registry (logout, a lock,
/// a changed account) and an archived chat (the nearest chat-removal path until items.id=888
/// builds a deletion, which must make the same call).
#[test]
fn the_memory_is_cleared_at_logout_and_when_a_chat_is_archived() {
    let main = include_str!("../main.rs");
    assert!(
        main.contains(".manage(quietrabbit_lib::conductor::plan_memory::PlanMemory::default())")
    );
    let hook = main
        .split(".on_key_leave(")
        .nth(1)
        .expect("main.rs registers a key-leave observer for the plan memory");
    assert!(hook
        .trim_start()
        .starts_with("move |left| memory.clear_user(&left.user_id)"));

    let chats = include_str!("../commands/chats.rs");
    let archive = chats
        .split("pub async fn archive_chat")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(archive.contains("plan_memory.clear_chat(&user_id, &persona_id, &chat_id)"));
}

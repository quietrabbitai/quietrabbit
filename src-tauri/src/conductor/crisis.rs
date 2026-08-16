// src-tauri/src/conductor/crisis.rs
//
// R1 crisis-handling floor (decisions.id=607, items.id=265). Product-wide
// per decisions.id=593 -- not gated to Medical Persona or any Focus.
//
// Four-part spec:
//   1. Detection  -- Tier 1, local-only, always-on, no network call, no
//      exception. Fires on specificity of imminent risk (expressed intent,
//      means, or a plan, in the user's own words) -- not on topic or
//      sentiment alone. Deliberately narrow: ordinary grief, stress, or
//      venting must not fire. This is a curated phrase match, not a
//      sentiment/mood classifier -- decisions.id=607 explicitly rules out
//      building the latter.
//   2. Response   -- non-clinical, additive to QR's normal voice, not a
//      scripted mode-switch. Implemented by the caller (lifecycle.rs
//      output()) appending resource_block() to the model's real response,
//      never replacing it.
//   3. Resource sourcing -- static, local, no network call. Country-specific
//      data for US/UK/CA/AU, generic fallback elsewhere. Numbers verified
//      via live web search on 2026-08-16 (not recalled from memory):
//      988lifeline.org / SAMHSA (US), samaritans.org (UK), 988.ca / CAMH /
//      suicide.ca (CA, including the Quebec text exception -- texting 988
//      from Quebec does not reach Quebec's own service), lifeline.org.au
//      (AU). Tracked for ongoing accuracy by maintenance_items.id=44.
//   4. Placement  -- named floor entry, applied everywhere unconditionally.
//      See lifecycle.rs FocusRun.crisis_floor_triggered / output().
//
// STRUCTURAL LIMITATION (state this plainly, not just here): a curated,
// hardcoded phrase list is inherently narrow by construction. It will miss
// real disclosures phrased in ways the list didn't anticipate -- different
// wording, indirect language, typos. This is not a defect in the specific
// list below; it is the cost decisions.id=607 already accepted in exchange
// for the Tier-1/no-network/no-LLM guarantee that sensitive candor never
// leaves the device to be classified. The tests below demonstrate the list
// behaves as intended on the cases it was built for -- they do not
// demonstrate completeness.
//
// HYPERBOLE COLLISION (found in independent review, items.id=265): a bare
// substring match on phrases like "kill myself" collides hard with common
// everyday hyperbole -- "kill myself laughing", "could kill myself for
// forgetting my keys", "so embarrassing I could just kill myself". These
// phrases are simultaneously the single most likely wording in a genuine
// disclosure AND some of the most common hyperbolic idiom in English, so
// removing them entirely would trade a real false-positive problem for a
// worse false-negative one. Fix: split into HIGH_SPECIFICITY_PHRASES (fire
// unconditionally -- already temporally/intentionally qualified enough that
// hyperbolic collision is rare) and COLLISION_PRONE_PHRASES (fire only when
// no HYPERBOLE_MARKER appears within PROXIMITY_WINDOW chars of the match).
// This is a denylist mitigation, not a general solution -- it narrows but
// does not eliminate the hyperbole false-positive rate (unanticipated
// hyperbole phrasing still slips through), AND it introduces a new, small
// false-negative risk in the opposite direction: a genuine disclosure that
// happens to mention a marker word nearby for an unrelated reason (e.g.
// "I keep forgetting why I should stay alive, I'm going to end my life")
// could be incorrectly suppressed. Both residual risks are real and are
// stated here, not implied away by the passing test suite below.
//
// SECOND-ROUND ADVERSARIAL PASS (independent review, items.id=265): the
// review that caught the "kill myself" collision above never re-examined
// the acute-abuse phrase family ("she's going to kill me" etc.), which
// predated that analysis and sat unconditional in HIGH_SPECIFICITY_PHRASES.
// "[pronoun] is/are going to kill me" is at least as idiom-collision-prone
// as "kill myself" -- it's a completely standard way to describe minor
// trouble with someone ("my mom's going to kill me if I'm late"), not just
// dark humor. Prompted a full deliberate pass over BOTH tiers for the same
// question, not just the reported phrase. Findings and changes:
//   - The whole "[he/she/they] is/are going to kill me" family moved from
//     HIGH_SPECIFICITY_PHRASES to COLLISION_PRONE_PHRASES -- same mitigation
//     as "kill myself" now applies uniformly across both idiom families.
//   - "going to kill me tonight" and "trying to kill me right now" removed
//     as their own phrases: unlike the rest of this family they carry no
//     pronoun subject, so they matched entirely inanimate/abstract subjects
//     too ("this deadline is going to kill me tonight", "this workout is
//     trying to kill me right now, I can barely move") -- a collision the
//     mitigation can't fix by proximity marker alone since no marker is
//     typically nearby. Replaced with pronoun-qualified equivalents
//     ("he's/she's/they're trying to kill me right now") in
//     COLLISION_PRONE_PHRASES, which narrows real coverage of "someone
//     (unnamed) is trying to kill me right now" -- an accepted trade,
//     folded into the structural completeness limitation already stated
//     above, not a new category of gap.
//   - "goodbye note" (bare, and "writing"/"wrote my goodbye note") removed
//     entirely, not just re-tiered: none of the three encode a self-harm
//     verb, intent, or plan at all -- they're topic-adjacent inference only
//     ("I wrote my goodbye note to the team before my last day" is ordinary
//     job-farewell phrasing), which is exactly the "not on topic... alone"
//     firing decisions.id=607 point 1 rules out directly, a stronger
//     objection than hyperbole-collision. This is a real loss of a
//     true-positive signal (an actual goodbye-note disclosure without any
//     other trigger phrase nearby will now be missed) -- accepted because
//     firing on ordinary job/relationship farewells is a worse failure mode
//     for a "deliberately narrow" floor than missing this one phrasing.
//   - "jump off a bridge" / "jump in front of a train(ing)" moved to
//     COLLISION_PRONE_PHRASES: both collide with frustration hyperbole
//     ("this bug makes me want to jump off a bridge"), a real gap this
//     pass found on its own, not reported externally.
//   - Kept unconditional, with reasons: "not going to wake up tomorrow"
//     (not an established idiom the way the above are); "cut/cutting my
//     wrists", "overdose on my pills/medication", "suffocate/asphyxiate
//     myself" (no everyday hyperbolic use found); "have a plan to...",
//     "bought a gun to...", "have the pills to end it" (already
//     means+plan-qualified, near-zero plausible non-crisis reading);
//     "scared he's/she's going to kill me" (people do not typically say
//     "I'm scared" when using the bare idiom hyperbolically -- "scared" is
//     doing real disambiguating work here, unlike a bare "going to").
// This pass is bounded and deliberate, not exhaustive: it does not newly
// solve the already-disclosed structural limitation (unanticipated
// phrasing), the HYPERBOLE_MARKERS list's own completeness limit, or
// third-person/fictional mentions ("the character in the book decides to
// jump off a bridge") -- all three remain accepted, stated residual risk,
// same as before this pass.
//
// No I/O, no async, no network-capable imports anywhere in this file --
// that absence is itself the "no network call, no exception" property,
// checkable by grep, not just by reading this comment.

/// Phrases specific enough (temporal/intent/plan-qualified, or otherwise low
/// collision with everyday idiom) to fire unconditionally.
const HIGH_SPECIFICITY_PHRASES: &[&str] = &[
    // -- Self-harm: intent --
    // NOTE: "i'm/i am going to kill myself" and "...end my life" are
    // deliberately NOT listed here even though they're unambiguous in
    // isolation -- each one contains "kill myself" / "end my life" as a
    // substring, so a bare "going to" prefix does not actually disambiguate
    // from hyperbole ("I'm going to kill myself laughing at this" also has
    // "going to kill myself" in it). They're still fully covered via
    // COLLISION_PRONE_PHRASES below, which correctly requires the absence
    // of a nearby hyperbole marker either way.
    "planning to kill myself",
    "planning to end my life",
    "decided to kill myself",
    "decided to end my life",
    "won't be alive tomorrow",
    "not going to be alive tomorrow",
    "not going to wake up tomorrow",
    // -- Self-harm: means --
    "cut my wrists",
    "cutting my wrists",
    "overdose on my pills",
    "overdose on my medication",
    "suffocate myself",
    "asphyxiate myself",
    "bought a gun to kill myself",
    "have the pills to end it",
    // -- Self-harm: plan --
    "have a plan to kill myself",
    "have a plan to end my life",
    // -- Acute abuse / imminent danger from another person --
    // "scared" is doing real disambiguating work here (see module doc) --
    // the bare "[pronoun] is/are going to kill me" family below does not
    // get the same unconditional treatment.
    "scared he's going to kill me",
    "scared she's going to kill me",
];

/// Phrases that are strongly self-harm/abuse-relevant on their own but
/// collide with common hyperbolic English idiom ("kill myself laughing",
/// "could shoot myself for that mistake", "shoot myself in the foot", "my
/// mom's going to kill me if I'm late", "this bug makes me want to jump off
/// a bridge"). Fire only when no HYPERBOLE_MARKER is within
/// PROXIMITY_WINDOW chars of the match -- see module doc for the residual
/// risk this trades in both directions.
const COLLISION_PRONE_PHRASES: &[&str] = &[
    "kill myself",
    "killing myself",
    "end my life",
    "ending my life",
    "end my own life",
    "take my own life",
    "taking my own life",
    "hang myself",
    "hanging myself",
    "shoot myself",
    "shooting myself",
    "jump off a bridge",
    "jump in front of a train",
    "jumping in front of a train",
    "he is going to kill me",
    "he's going to kill me",
    "she is going to kill me",
    "she's going to kill me",
    "they are going to kill me",
    // Pronoun-qualified, unlike the bare "trying to kill me right now" this
    // replaces -- see module doc for why the unqualified form was dropped.
    "he's trying to kill me right now",
    "he is trying to kill me right now",
    "she's trying to kill me right now",
    "she is trying to kill me right now",
    "they're trying to kill me right now",
];

/// Jocular/idiomatic tells that, near a COLLISION_PRONE_PHRASES match,
/// indicate hyperbole rather than genuine disclosure. Deliberately narrow
/// and high-precision (not general emotion words like "mistake" or
/// "stupid", which can plausibly appear in genuine crisis text too).
const HYPERBOLE_MARKERS: &[&str] = &[
    "laughing",
    "lol",
    "haha",
    "hilarious",
    "embarrass", // covers embarrassing / embarrassed
    "forgetting",
    "forgot",
    "in the foot", // "shoot myself in the foot" idiom
    "joking",
    "kidding",
];

/// How many chars of surrounding context (each side) to scan for a
/// HYPERBOLE_MARKER around a COLLISION_PRONE_PHRASES match. 60, not 40 --
/// widened after the reported "she's going to kill me when she sees what I
/// did to her car lol" case landed the trailing "lol" right at the edge of
/// the original 40-char window.
const PROXIMITY_WINDOW: usize = 60;

/// Local, deterministic, no network call. Case-insensitive substring match.
/// Fires on specificity (intent/means/plan in the user's own words), not on
/// topic or sentiment alone -- see module doc for the deliberate exclusions
/// this implies, and for the hyperbole-collision mitigation and its two
/// residual risks.
pub fn detect(text: &str) -> bool {
    let lower = text.to_lowercase();

    if HIGH_SPECIFICITY_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }

    for phrase in COLLISION_PRONE_PHRASES {
        for (pos, _) in lower.match_indices(phrase) {
            let end = pos + phrase.len();
            if !has_nearby_hyperbole_marker(&lower, pos, end) {
                return true;
            }
        }
    }

    false
}

/// True if any HYPERBOLE_MARKERS phrase appears within PROXIMITY_WINDOW
/// chars of the [phrase_start, phrase_end) match. Window bounds are pulled
/// in to the nearest char boundary rather than sliced raw, since arbitrary
/// byte-offset arithmetic on a to_lowercase()'d string is not guaranteed to
/// land on a UTF-8 boundary for non-ASCII input.
fn has_nearby_hyperbole_marker(lower: &str, phrase_start: usize, phrase_end: usize) -> bool {
    let raw_start = phrase_start.saturating_sub(PROXIMITY_WINDOW);
    let raw_end = (phrase_end + PROXIMITY_WINDOW).min(lower.len());
    let start = (0..=raw_start)
        .rev()
        .find(|&i| lower.is_char_boundary(i))
        .unwrap_or(0);
    let end = (raw_end..=lower.len())
        .find(|&i| lower.is_char_boundary(i))
        .unwrap_or(lower.len());
    if start >= end {
        return false;
    }
    let window = &lower[start..end];
    HYPERBOLE_MARKERS.iter().any(|m| window.contains(m))
}

/// Static, local, no network call. Country-specific data for US/UK/CA/AU
/// plus a generic fallback for every other locale. States "QR is not a
/// clinician" once, plainly. Plain factual tone -- no dramatized language.
/// Appended to (never substituted for) the model's normal response by the
/// caller -- see lifecycle.rs output().
pub fn resource_block() -> String {
    "Quiet Rabbit isn't a clinician, and this isn't a substitute for \
     professional care. If you're in immediate danger, contact your local \
     emergency number.\n\n\
     United States: 988 Suicide & Crisis Lifeline — call or text 988 (free, 24/7).\n\
     United Kingdom: Samaritans — call 116 123 (free, 24/7).\n\
     Canada: 9-8-8 Suicide Crisis Helpline — call or text 988 (24/7). In Quebec: call 1-866-APPELLE (1-866-277-3553) or text 53 53 53.\n\
     Australia: Lifeline — call 13 11 14 or text 0477 13 11 14 (24/7).\n\
     Elsewhere: contact your local emergency number, or search for a crisis line in your country."
        .to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- True positives: intent / means / plan / acute abuse --

    #[test]
    fn detect_fires_on_explicit_intent() {
        assert!(detect(
            "I'm going to kill myself tonight, I can't do this anymore."
        ));
    }

    #[test]
    fn detect_fires_on_means() {
        assert!(detect(
            "I have the pills to end it and I'm going to take them all."
        ));
    }

    #[test]
    fn detect_fires_on_plan() {
        assert!(detect(
            "I have a plan to kill myself and I already wrote my goodbye note."
        ));
    }

    #[test]
    fn detect_fires_on_acute_abuse() {
        assert!(detect(
            "He said he's going to kill me tonight and I don't know what to do."
        ));
    }

    #[test]
    fn detect_is_case_insensitive() {
        assert!(detect("I'M GOING TO KILL MYSELF"));
    }

    // -- False positives: ordinary grief, stress, venting, hyperbole --

    #[test]
    fn detect_does_not_fire_on_ordinary_grief() {
        assert!(!detect(
            "I'm grieving my dad's death and it's been really hard the past few weeks."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_stress_hyperbole() {
        assert!(!detect(
            "This assignment is going to kill me, I'm so stressed about the deadline."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_work_venting() {
        assert!(!detect(
            "Work is killing me lately, I need a real vacation."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_passive_ideation_without_specificity() {
        assert!(!detect(
            "Sometimes I feel hopeless and wonder what the point of any of this is."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_embarrassment_hyperbole() {
        assert!(!detect("I could just die of embarrassment right now."));
    }

    #[test]
    fn detect_does_not_fire_on_ordinary_message() {
        assert!(!detect("Can you help me draft an email to my landlord?"));
    }

    // -- Regression: the actual adversarial phrases from independent review
    // (items.id=265) -- these hit COLLISION_PRONE_PHRASES ("kill myself"),
    // not the "die of embarrassment" substitute the earlier test suite used.

    #[test]
    fn detect_does_not_fire_on_kill_myself_forgetting_hyperbole() {
        assert!(!detect("I could kill myself for forgetting my keys"));
    }

    #[test]
    fn detect_does_not_fire_on_kill_myself_laughing_hyperbole() {
        assert!(!detect("I'm going to kill myself laughing at this"));
    }

    #[test]
    fn detect_does_not_fire_on_kill_myself_embarrassment_hyperbole() {
        assert!(!detect(
            "ugh I could just kill myself right now, so embarrassing"
        ));
    }

    #[test]
    fn detect_does_not_fire_on_shoot_myself_in_the_foot_idiom() {
        assert!(!detect(
            "I always shoot myself in the foot when I procrastinate like this."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_lol_kill_myself_hyperbole() {
        assert!(!detect("lol I could kill myself, that's so bad"));
    }

    // -- Regression: genuine terse disclosures using the same COLLISION_PRONE
    // phrases (no hyperbole marker nearby) must still fire after the fix.

    #[test]
    fn detect_still_fires_on_bare_kill_myself_without_hyperbole_context() {
        assert!(detect(
            "I want to kill myself, I can't take this pain anymore."
        ));
    }

    #[test]
    fn detect_still_fires_on_bare_end_my_life_without_hyperbole_context() {
        assert!(detect(
            "I don't want to be here anymore, I want to end my life."
        ));
    }

    #[test]
    fn detect_still_fires_on_hang_myself_without_hyperbole_context() {
        assert!(detect("I've been thinking about how I would hang myself."));
    }

    // -- Second-round adversarial pass (items.id=265): the acute-abuse
    // family gap plus my own follow-up finds during the deliberate full-list
    // review it prompted. See module doc for what moved/was removed and why.

    #[test]
    fn detect_does_not_fire_on_reported_acute_abuse_hyperbole() {
        // The exact phrase from independent review.
        assert!(!detect(
            "she's going to kill me when she sees what I did to her car lol"
        ));
    }

    #[test]
    fn detect_still_fires_on_acute_abuse_without_hyperbole_context() {
        assert!(detect(
            "He said he's going to kill me tonight and I don't know what to do."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_workout_trying_to_kill_me_no_subject() {
        // "trying to kill me right now" (no pronoun) was removed for exactly
        // this collision -- inanimate/abstract subjects matched it too.
        assert!(!detect(
            "This workout is trying to kill me right now, I can barely move."
        ));
    }

    #[test]
    fn detect_still_fires_on_pronoun_qualified_trying_to_kill_me() {
        assert!(detect(
            "He's trying to kill me right now, please send help."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_deadline_going_to_kill_me_tonight_no_subject() {
        // "going to kill me tonight" (no pronoun) was removed for the same
        // no-subject collision reason.
        assert!(!detect(
            "This deadline is going to kill me tonight, I have so much to finish."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_goodbye_note_job_farewell() {
        // "goodbye note" / "writing"/"wrote my goodbye note" removed
        // entirely -- topic-only signal, not intent/means/plan.
        assert!(!detect(
            "I wrote my goodbye note to the team before my last day at the company."
        ));
    }

    #[test]
    fn detect_does_not_fire_on_jump_off_a_bridge_hyperbole_with_marker() {
        assert!(!detect(
            "lol I want to jump off a bridge, this bug has been driving me crazy for six hours"
        ));
    }

    #[test]
    fn detect_still_fires_on_jump_off_a_bridge_without_hyperbole_context() {
        assert!(detect(
            "I've been standing here for an hour and I think I'm going to jump off a bridge tonight."
        ));
    }

    // -- Resource block content --

    #[test]
    fn resource_block_states_not_a_clinician() {
        assert!(resource_block().contains("isn't a clinician"));
    }

    #[test]
    fn resource_block_contains_us_hotline() {
        assert!(resource_block().contains("988 Suicide & Crisis Lifeline"));
    }

    #[test]
    fn resource_block_contains_uk_hotline() {
        assert!(resource_block().contains("116 123"));
    }

    #[test]
    fn resource_block_contains_canada_hotline_and_quebec_exceptions() {
        let block = resource_block();
        assert!(block.contains("988"));
        assert!(block.contains("1-866-APPELLE"));
        assert!(block.contains("53 53 53"));
    }

    #[test]
    fn resource_block_contains_australia_hotline() {
        let block = resource_block();
        assert!(block.contains("13 11 14"));
        assert!(block.contains("0477 13 11 14"));
    }

    #[test]
    fn resource_block_contains_generic_fallback() {
        assert!(resource_block().contains("Elsewhere"));
    }
}

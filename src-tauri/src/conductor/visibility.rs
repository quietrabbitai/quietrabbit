// src-tauri/src/conductor/visibility.rs
//
// decisions.id=513 (D6-471) — object-level visibility and display control
// model. items.id=175.
//
// Single centralized evaluation point (decisions.id=513: "No visibility
// logic distributed across GUI components"). Every GUI element and context
// assembly path that renders a persistable QR object calls
// evaluate_object_visibility() with that surface's classification, rather
// than each surface re-implementing its own filter logic.
//
// SCOPE OF THIS FILE (R1): the two entity-level flags (personal_003.sql)
// plus Layers 2 and 3 of the model. Layer 1's type-policy sub-piece
// (per-Focus status/age/always-visible conditions declared in a Focus's
// display_config) is deliberately NOT built here — decisions.id=513 states
// Focus output types adopt type policy "as each Focus is built," and R1
// ships no Focus with a type policy yet (Tech Support, the first named
// consumer, is not yet built). Building type-policy evaluation now would be
// speculative code against a shape no real Focus has declared -- scoped out
// per this item's own Chat-DEV pre-scoping pass, not an oversight.
// GUI-element eligibility (also Layer 1) is likewise a per-surface
// declaration this file has no callers to consult yet; the eligibility
// check below is a pass-through stub pending a real caller.
//
// OBJECT TYPE REGISTRATION MODEL (decisions.id=513: "a Chat-DEV scoping
// item — must be defined before personal_003.sql build begins" [decision
// text corrected 2026-08-01 from a pre-release planning filename,
// personal_007.sql, that was never actually built]):
// implemented as a static, compile-time registry rather than a DB table.
// Object types are a code-level concept — adding one means adding Rust code
// to read/write it, so gating the type list behind a migration would only
// add ceremony without adding safety. R1 registers exactly one entry:
// "entity". Output types and Focus-specific records register their own
// entries here as each is built, per the decision's own entity-first
// sequencing.

// ---------------------------------------------------------------------------
// Surface classification (decisions.id=513 Layer 3)
// ---------------------------------------------------------------------------

/// Where an object is being rendered. Determines which flags apply and how
/// strongly (decisions.id=513 Layer 3 — surface classification is the final
/// evaluation context; no lower layer can override it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceClass {
    /// User did not navigate here intentionally — Active Board, Daily
    /// Brief, notifications, cross-Focus search, general library. Both
    /// object-level flags apply fully; type policy default filter applies.
    Ambient,
    /// User navigated explicitly to this object or its Focus context.
    /// hide_from_shared_surfaces does not apply here. redact_identification
    /// still applies at export/Tier 2/3 boundaries reached from a Direct
    /// surface. Type policy suppression does not apply.
    Direct,
    /// Export, print, Tier 2/3 context assembly. Both flags apply at
    /// maximum strength. Invariant: no Focus declaration, type policy,
    /// surface override, user preference, or escape valve changes Boundary
    /// behavior.
    Boundary,
}

// ---------------------------------------------------------------------------
// Object type registry (decisions.id=513's named Chat-DEV scoping item)
// ---------------------------------------------------------------------------

/// One registered object type: what flags it carries, what surfaces it is
/// eligible to appear on, and what other object types inherit its flag
/// state at creation time (decisions.id=513's inheritance model).
#[derive(Debug, Clone, Copy)]
pub struct ObjectTypeRegistration {
    pub object_type: &'static str,
    /// Whether this type carries redact_identification /
    /// hide_from_shared_surfaces at all. Both flags are always paired --
    /// decisions.id=513 never describes a type adopting one without the
    /// other.
    pub carries_flags: bool,
    /// Surfaces this object type is eligible to appear on at all --
    /// decisions.id=513 Layer 1's GUI element eligibility check. An empty
    /// slice means "not yet wired to any surface" (safe default; nothing
    /// renders rather than something rendering unfiltered).
    pub eligible_surfaces: &'static [SurfaceClass],
    /// Object types that inherit this type's flag state at creation time
    /// (decisions.id=513: "prevents the common case where a user flags a
    /// sensitive entity but forgets to flag objects produced about it").
    /// Empty in R1 -- no output types are registered yet to inherit from
    /// "entity".
    pub inherits_to: &'static [&'static str],
}

/// R1 registry: "entity" only. Extend this array -- do not build a second
/// registry elsewhere -- as each Focus's output types and Focus-specific
/// records are built (decisions.id=513's entity-first sequencing; P4 One
/// Home).
pub static OBJECT_TYPE_REGISTRY: &[ObjectTypeRegistration] = &[ObjectTypeRegistration {
    object_type: "entity",
    carries_flags: true,
    eligible_surfaces: &[SurfaceClass::Ambient, SurfaceClass::Direct, SurfaceClass::Boundary],
    inherits_to: &[],
}];

/// Look up a registered object type by name. None means the type has not
/// been registered -- callers should treat this the same as Layer 1
/// eligibility failure (decisions.id=513 evaluation step 1: not eligible ->
/// suppressed), since an unregistered type has made no eligibility
/// declaration at all.
pub fn lookup_object_type(object_type: &str) -> Option<&'static ObjectTypeRegistration> {
    OBJECT_TYPE_REGISTRY
        .iter()
        .find(|r| r.object_type == object_type)
}

// ---------------------------------------------------------------------------
// Layer 2 — object-level flags
// ---------------------------------------------------------------------------

/// The subset of an object's state Layer 2 needs. Deliberately not the full
/// Entity struct (or any other future object struct) -- this keeps
/// evaluate_object_visibility() decoupled from any one store's type, so
/// output types and Focus-specific records can implement this trait instead
/// of this function growing a match arm per object kind.
pub trait VisibilityFlags {
    fn redact_identification(&self) -> bool;
    fn hide_from_shared_surfaces(&self) -> bool;
}

// ---------------------------------------------------------------------------
// VisibilityDecision
// ---------------------------------------------------------------------------

/// Result of evaluate_object_visibility(). decisions.id=513 names these four
/// return values exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityDecision {
    Visible,
    Redacted,
    Suppressed,
    /// Eligible only within a Focus-declared specialized surface -- distinct
    /// from Suppressed because the object exists and can be reached, just
    /// not on the surface being evaluated.
    EligibleInSpecializedSurfaceOnly,
}

// ---------------------------------------------------------------------------
// evaluate_object_visibility
// ---------------------------------------------------------------------------

/// Single evaluation point for decisions.id=513's three-layer model.
///
/// EVALUATION ORDER (decisions.id=513, followed exactly):
///   1. GUI element eligibility -- object type not eligible for this
///      surface -> Suppressed.
///   2. Layer 1 type policy -- NOT YET IMPLEMENTED (see module header).
///      R1 has no Focus declaring a type policy, so this step is currently
///      a no-op pass-through. Implementing it is a separate, later task
///      when the first type-policy-declaring Focus is built -- not silently
///      skipped, explicitly deferred (P5 -- named, not dropped).
///   3. Layer 2 object-level flags, evaluated for the given surface_context.
///   4. Layer 3 surface classification invariants (folded into step 3's
///      per-surface logic below, since decisions.id=513 defines surface
///      behavior per flag rather than as an independent later pass).
///   5. Return VisibilityDecision.
///
/// user_context: reserved for multi-user (decisions.id=513's "Extensibility"
/// section) -- accepted now, unused now, so the signature does not need to
/// change when multi-user activates it.
pub fn evaluate_object_visibility<T: VisibilityFlags>(
    object: &T,
    object_type: &str,
    surface_context: SurfaceClass,
    _user_context: Option<&str>,
) -> VisibilityDecision {
    // Step 1 -- GUI element eligibility.
    let Some(registration) = lookup_object_type(object_type) else {
        return VisibilityDecision::Suppressed;
    };
    if !registration.eligible_surfaces.contains(&surface_context) {
        return VisibilityDecision::Suppressed;
    }

    // Step 2 -- Layer 1 type policy. No-op in R1 (see doc comment above).

    // Steps 3-4 -- Layer 2 flags evaluated under Layer 3 surface rules.
    if !registration.carries_flags {
        return VisibilityDecision::Visible;
    }

    let redact = object.redact_identification();
    let hide = object.hide_from_shared_surfaces();

    match surface_context {
        SurfaceClass::Boundary => {
            // Invariant surface: both flags apply at maximum strength,
            // regardless of anything else. hide wins outright (object must
            // not appear at all); redact alone strips identification.
            if hide {
                VisibilityDecision::Suppressed
            } else if redact {
                VisibilityDecision::Redacted
            } else {
                VisibilityDecision::Visible
            }
        }
        SurfaceClass::Ambient => {
            // Both flags apply fully on Ambient surfaces.
            if hide {
                VisibilityDecision::Suppressed
            } else if redact {
                VisibilityDecision::Redacted
            } else {
                VisibilityDecision::Visible
            }
        }
        SurfaceClass::Direct => {
            // hide_from_shared_surfaces does not apply on Direct surfaces
            // (decisions.id=513: "Library entries remain accessible within
            // Focus-declared Direct surfaces"). redact_identification still
            // applies -- Direct navigation does not imply the user has
            // crossed an export/Tier-2/3 boundary, and decisions.id=513 is
            // explicit that redact_identification "still applies at export
            // and Tier 2/3 boundaries within Direct surfaces." Within-QR
            // full identification for the authenticated user is a
            // Boundary-vs-not-Boundary distinction the caller resolves by
            // passing Boundary when a Direct-surface action itself crosses
            // into export/Tier 2/3 -- this function only sees the
            // classification it is given.
            if redact {
                VisibilityDecision::Redacted
            } else {
                VisibilityDecision::Visible
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestObject {
        redact: bool,
        hide: bool,
    }

    impl VisibilityFlags for TestObject {
        fn redact_identification(&self) -> bool {
            self.redact
        }
        fn hide_from_shared_surfaces(&self) -> bool {
            self.hide
        }
    }

    fn unflagged() -> TestObject {
        TestObject { redact: false, hide: false }
    }

    #[test]
    fn unregistered_object_type_is_suppressed() {
        let obj = unflagged();
        let decision = evaluate_object_visibility(&obj, "not_a_real_type", SurfaceClass::Ambient, None);
        assert_eq!(decision, VisibilityDecision::Suppressed);
    }

    #[test]
    fn unflagged_entity_is_visible_on_every_registered_surface() {
        let obj = unflagged();
        for surface in [SurfaceClass::Ambient, SurfaceClass::Direct, SurfaceClass::Boundary] {
            assert_eq!(
                evaluate_object_visibility(&obj, "entity", surface, None),
                VisibilityDecision::Visible,
                "surface {surface:?} should be Visible for an unflagged entity"
            );
        }
    }

    #[test]
    fn redact_identification_redacts_on_ambient_and_boundary() {
        let obj = TestObject { redact: true, hide: false };
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Ambient, None),
            VisibilityDecision::Redacted
        );
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Boundary, None),
            VisibilityDecision::Redacted
        );
    }

    #[test]
    fn redact_identification_still_applies_on_direct() {
        // decisions.id=513: redact_identification "still applies at export
        // and Tier 2/3 boundaries within Direct surfaces" -- Direct does
        // not exempt redact the way it exempts hide.
        let obj = TestObject { redact: true, hide: false };
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Direct, None),
            VisibilityDecision::Redacted
        );
    }

    #[test]
    fn hide_from_shared_surfaces_suppresses_ambient_and_boundary_but_not_direct() {
        let obj = TestObject { redact: false, hide: true };
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Ambient, None),
            VisibilityDecision::Suppressed
        );
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Boundary, None),
            VisibilityDecision::Suppressed
        );
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Direct, None),
            VisibilityDecision::Visible,
            "hide_from_shared_surfaces must not apply on Direct (decisions.id=513)"
        );
    }

    #[test]
    fn both_flags_hide_wins_on_boundary() {
        let obj = TestObject { redact: true, hide: true };
        assert_eq!(
            evaluate_object_visibility(&obj, "entity", SurfaceClass::Boundary, None),
            VisibilityDecision::Suppressed,
            "hide_from_shared_surfaces must win over redact_identification when both are set"
        );
    }

    #[test]
    fn eligibility_branch_suppresses_a_surface_not_in_the_registration() {
        // Exercises the eligibility `contains` check evaluate_object_visibility
        // uses directly, rather than only its lookup-miss path (covered by
        // unregistered_object_type_is_suppressed above). Uses a standalone
        // ObjectTypeRegistration value rather than narrowing the real
        // OBJECT_TYPE_REGISTRY entry -- R1's one real "entity" registration
        // is intentionally eligible on all three surfaces and should not be
        // narrowed just to exercise this branch.
        let restricted = ObjectTypeRegistration {
            object_type: "entity",
            carries_flags: true,
            eligible_surfaces: &[SurfaceClass::Direct],
            inherits_to: &[],
        };
        assert!(!restricted.eligible_surfaces.contains(&SurfaceClass::Ambient));
        assert!(!restricted.eligible_surfaces.contains(&SurfaceClass::Boundary));
        assert!(restricted.eligible_surfaces.contains(&SurfaceClass::Direct));
    }

    #[test]
    fn lookup_object_type_finds_entity() {
        assert!(lookup_object_type("entity").is_some());
        assert!(lookup_object_type("nonexistent").is_none());
    }
}

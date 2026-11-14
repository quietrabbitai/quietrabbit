// src-tauri/src/conductor/model_block/config.rs
//
// items.id=894 part 1 (decisions.id=941 point 3, decisions.id=942): the COMMON
// configuration every model block carries, the range it may run in, and the one
// function (tighten) that narrows an envelope.
//
// What is in the common configuration, and what can move it:
//   need             the task type, difficulty and output size. Focus author only.
//   declared_inputs  the names the block's template may name. Tighten-only.
//   range            quality floor (min and max), highest tier, destinations.
//                    Tighten-only.
//   class_preference an optional ranking hint (small, medium, large). It is never
//                    a model id (decisions.id=875: no model is named in code) and
//                    never a filter.
//
// The four tighten-only things form the Envelope. EnvelopeOverride has exactly
// those four fields and deny_unknown_fields, so an override that names anything
// else does not parse, and tighten() can only ever remove: a destination or an
// input the base lacks is never added, a tier above the base is never reached, a
// floor never drops below the base's. No field here can switch a gate off: the
// pinned key list in admission_tests.rs fails on any key not reviewed.

use serde::{Deserialize, Serialize};

use crate::conductor::tokens::{is_system_token, ExternalAccess};

// ---------------------------------------------------------------------------
// Closed vocabularies. An unknown value is a parse error naming the value.
// ---------------------------------------------------------------------------

/// Where a block's call may go. hosted_api and manual_handoff are external.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Local,
    HostedApi,
    ManualHandoff,
}

/// An ordered named quality bar. The numbers behind the names come from the
/// calibration (B2, B6b) and Jason's floors; three bars are a placeholder count.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum QualityBar {
    Basic,
    Standard,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum OutputSize {
    Short,
    Medium,
    Long,
}

/// A ranking preference for the resolver (B2). Never a model id, never a filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ModelClass {
    Small,
    Medium,
    Large,
}

/// The task types providers/evaluation.rs measures. The old "validated against
/// task_types.yaml" comment on ActionDefinition.task_type was never true: that
/// file is archived and nothing reads it. A model block uses this closed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ModelTaskType {
    Summarization,
    StructuredOutput,
    Code,
    Reasoning,
    Research,
    LongContext,
    CreativeWriting,
    QuickResponse,
}

impl ModelTaskType {
    /// The string ActionDefinition.task_type carries (what select_model matches).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarization => "summarization",
            Self::StructuredOutput => "structured_output",
            Self::Code => "code",
            Self::Reasoning => "reasoning",
            Self::Research => "research",
            Self::LongContext => "long_context",
            Self::CreativeWriting => "creative_writing",
            Self::QuickResponse => "quick_response",
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct TaskNeed {
    pub task_type: ModelTaskType,
    pub difficulty: Difficulty,
    pub output_size: OutputSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct QualityFloor {
    pub min: QualityBar,
    pub max: QualityBar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ModelRange {
    pub quality_floor: QualityFloor,
    pub highest_tier: ExternalAccess,
    pub destinations: Vec<Destination>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct CommonConfig {
    pub display_name: String,
    pub output_var: String,
    pub need: TaskNeed,
    /// Exactly the names the block's generated template names (checked at LOAD).
    pub declared_inputs: Vec<String>,
    pub range: ModelRange,
    pub class_preference: Option<ModelClass>,
}

/// What a model block asks of whoever runs it. Carried on the expanded step
/// (ActionDefinition.requirement) for the resolver (B2) and the run plan (B4).
/// The executor does not read it in items.id=894: model choice stays on
/// executor::select_model until B2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRequirement {
    pub block_stable_id: String,
    pub need: TaskNeed,
    /// Already tightened to the Focus ceiling.
    pub range: ModelRange,
    pub class_preference: Option<ModelClass>,
}

// ---------------------------------------------------------------------------
// Envelope and tighten()
// ---------------------------------------------------------------------------

/// The four things that only ever narrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub quality_floor: QualityFloor,
    pub highest_tier: ExternalAccess,
    pub destinations: Vec<Destination>,
    pub declared_inputs: Vec<String>,
}

/// An override of the envelope, from a persona override row or a per-run
/// override (later builds), or the Focus ceiling (expansion). Exactly the four
/// tighten-only things, each optional, and nothing else parses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnvelopeOverride {
    pub quality_floor: Option<QualityFloor>,
    pub highest_tier: Option<ExternalAccess>,
    pub destinations: Option<Vec<Destination>>,
    pub declared_inputs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TightenError {
    /// The quality floor's min rose above its max.
    EmptyQualityRange,
    /// No destination is left.
    NoDestination,
}

impl std::fmt::Display for TightenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyQualityRange => write!(
                f,
                "the quality floor range is empty after tightening (min is above max)"
            ),
            Self::NoDestination => write!(f, "no destination is left after tightening"),
        }
    }
}

impl Envelope {
    /// An override that reproduces this envelope exactly (for idempotence tests).
    pub fn as_override(&self) -> EnvelopeOverride {
        EnvelopeOverride {
            quality_floor: Some(self.quality_floor),
            highest_tier: Some(self.highest_tier),
            destinations: Some(self.destinations.clone()),
            declared_inputs: Some(self.declared_inputs.clone()),
        }
    }
}

/// THE function that narrows an envelope. Every override of a model block's
/// range goes through it: quality floor min is the larger of the two and max the
/// smaller, the tier is the lower, destinations and declared inputs are the
/// intersection in the base's order. An override can never add an element the
/// base lacks or raise a bound the base set (property-tested).
pub fn tighten(base: &Envelope, over: &EnvelopeOverride) -> Result<Envelope, TightenError> {
    let quality_floor = match over.quality_floor {
        Some(o) => QualityFloor {
            min: base.quality_floor.min.max(o.min),
            max: base.quality_floor.max.min(o.max),
        },
        None => base.quality_floor,
    };
    if quality_floor.min > quality_floor.max {
        return Err(TightenError::EmptyQualityRange);
    }
    let highest_tier = match over.highest_tier {
        Some(t) => ExternalAccess::min(base.highest_tier, t),
        None => base.highest_tier,
    };
    let destinations: Vec<Destination> = match &over.destinations {
        Some(o) => base
            .destinations
            .iter()
            .copied()
            .filter(|d| o.contains(d))
            .collect(),
        None => base.destinations.clone(),
    };
    if destinations.is_empty() {
        return Err(TightenError::NoDestination);
    }
    let declared_inputs: Vec<String> = match &over.declared_inputs {
        Some(o) => base
            .declared_inputs
            .iter()
            .filter(|n| o.contains(n))
            .cloned()
            .collect(),
        None => base.declared_inputs.clone(),
    };
    Ok(Envelope {
        quality_floor,
        highest_tier,
        destinations,
        declared_inputs,
    })
}

// ---------------------------------------------------------------------------
// LOAD validation of the common configuration
// ---------------------------------------------------------------------------

/// A name a template token or a declared input can take: [a-z_][a-z0-9_]*, the
/// shape executor::find_tokens recognises, at most 64 characters.
pub fn is_slug(s: &str) -> bool {
    let mut chars = s.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_lowercase());
    first_ok
        && s.len() <= 64
        && chars.all(|c| c == '_' || c.is_ascii_digit() || c.is_ascii_lowercase())
}

impl CommonConfig {
    /// The envelope of this configuration.
    pub fn envelope(&self) -> Envelope {
        Envelope {
            quality_floor: self.range.quality_floor,
            highest_tier: self.range.highest_tier,
            destinations: self.range.destinations.clone(),
            declared_inputs: self.declared_inputs.clone(),
        }
    }

    /// Everything about the common configuration that can be checked without the
    /// block's own spec. `external_allowed` is EXTERNAL_MODEL_BLOCKS_ALLOWED,
    /// passed in so a test can exercise both settings without flipping the
    /// constant.
    pub fn validate(&self, external_allowed: bool) -> Result<(), String> {
        if self.display_name.trim().is_empty() {
            return Err("common: display_name is empty".to_owned());
        }
        if !is_slug(&self.output_var) {
            return Err(format!(
                "common: output_var '{}' must be a lower-case name (letters, digits, underscore)",
                self.output_var
            ));
        }
        if is_system_token(&self.output_var) {
            return Err(format!(
                "common: output_var '{}' collides with a system token",
                self.output_var
            ));
        }
        if crate::conductor::declared_inputs::bound_names().contains(&self.output_var.as_str()) {
            return Err(format!(
                "common: output_var '{}' is the name of a session or case variable",
                self.output_var
            ));
        }
        let mut seen = Vec::new();
        for name in &self.declared_inputs {
            if !is_slug(name) {
                return Err(format!(
                    "common: declared input '{name}' must be a lower-case name"
                ));
            }
            if seen.contains(&name) {
                return Err(format!("common: declared input '{name}' is listed twice"));
            }
            seen.push(name);
        }
        if self.declared_inputs.contains(&self.output_var) {
            return Err(format!(
                "common: output_var '{}' is also a declared input (the block would read what it writes)",
                self.output_var
            ));
        }
        validate_range(&self.range, external_allowed)
    }
}

/// The range rules: a non-empty set of distinct destinations, an ordered floor,
/// every destination reachable at the highest tier, and, while external model
/// blocks are not allowed, nothing but local at local_only.
pub fn validate_range(range: &ModelRange, external_allowed: bool) -> Result<(), String> {
    if range.quality_floor.min > range.quality_floor.max {
        return Err("range: quality_floor.min is above quality_floor.max".to_owned());
    }
    if range.destinations.is_empty() {
        return Err("range: destinations is empty".to_owned());
    }
    for (i, d) in range.destinations.iter().enumerate() {
        if range.destinations[..i].contains(d) {
            return Err(format!("range: destination {d:?} is listed twice"));
        }
    }
    let tier = range.highest_tier;
    if range.destinations.contains(&Destination::HostedApi)
        && tier < ExternalAccess::AnonymousPreferred
    {
        return Err(format!(
            "range: destination hosted_api is unreachable at highest_tier '{}' (a hosted \
             provider is not anonymous, so it needs anonymous_preferred or unrestricted)",
            tier.as_str()
        ));
    }
    if range.destinations.contains(&Destination::ManualHandoff)
        && tier < ExternalAccess::AnonymousRequired
    {
        return Err(format!(
            "range: destination manual_handoff is unreachable at highest_tier '{}'",
            tier.as_str()
        ));
    }
    if !external_allowed {
        if let Some(d) = range
            .destinations
            .iter()
            .find(|d| **d != Destination::Local)
        {
            return Err(format!(
                "range: destination {d:?} is not allowed yet (EXTERNAL_MODEL_BLOCKS_ALLOWED is \
                 false: hosted_api waits for the outbound review of items.id=884, manual_handoff \
                 for items.id=B5)"
            ));
        }
        if tier != ExternalAccess::LocalOnly {
            return Err(format!(
                "range: highest_tier '{}' is not allowed yet (EXTERNAL_MODEL_BLOCKS_ALLOWED is \
                 false: a model block runs at local_only)",
                tier.as_str()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(min: QualityBar, max: QualityBar, tier: ExternalAccess) -> Envelope {
        Envelope {
            quality_floor: QualityFloor { min, max },
            highest_tier: tier,
            destinations: vec![Destination::Local, Destination::HostedApi],
            declared_inputs: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        }
    }

    #[test]
    fn tighten_narrows_and_never_adds() {
        let base = env(
            QualityBar::Basic,
            QualityBar::High,
            ExternalAccess::Unrestricted,
        );
        let over = EnvelopeOverride {
            quality_floor: Some(QualityFloor {
                min: QualityBar::Standard,
                max: QualityBar::High,
            }),
            highest_tier: Some(ExternalAccess::LocalOnly),
            destinations: Some(vec![Destination::HostedApi, Destination::ManualHandoff]),
            declared_inputs: Some(vec!["c".to_owned(), "z".to_owned()]),
        };
        let t = tighten(&base, &over).unwrap();
        assert_eq!(t.quality_floor.min, QualityBar::Standard);
        assert_eq!(t.highest_tier, ExternalAccess::LocalOnly);
        assert_eq!(t.destinations, vec![Destination::HostedApi]);
        assert_eq!(t.declared_inputs, vec!["c".to_owned()]);
    }

    #[test]
    fn tighten_errors_when_a_range_empties() {
        let base = env(
            QualityBar::Basic,
            QualityBar::Standard,
            ExternalAccess::LocalOnly,
        );
        let over = EnvelopeOverride {
            quality_floor: Some(QualityFloor {
                min: QualityBar::High,
                max: QualityBar::High,
            }),
            ..Default::default()
        };
        assert_eq!(tighten(&base, &over), Err(TightenError::EmptyQualityRange));
        let over = EnvelopeOverride {
            destinations: Some(vec![Destination::ManualHandoff]),
            ..Default::default()
        };
        assert_eq!(tighten(&base, &over), Err(TightenError::NoDestination));
    }

    #[test]
    fn an_override_naming_anything_else_does_not_parse() {
        for bad in [
            r#"{"need": {}}"#,
            r#"{"class_preference": "small"}"#,
            r#"{"output_var": "x"}"#,
            r#"{"gate1": false}"#,
        ] {
            assert!(
                serde_json::from_str::<EnvelopeOverride>(bad).is_err(),
                "{bad} must not parse"
            );
        }
        assert!(
            serde_json::from_str::<EnvelopeOverride>(r#"{"highest_tier": "local_only"}"#).is_ok()
        );
    }

    // -- the property test ---------------------------------------------------
    //
    // No property-test crate (a new dependency would be a cargo-deny question):
    // an exhaustive pass over every single-axis value plus a seeded linear
    // congruential sample of random (base, override) pairs.

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn coin(&mut self) -> bool {
            self.next() & 1 == 0
        }
        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[(self.next() as usize) % xs.len()]
        }
        fn subset<T: Clone>(&mut self, xs: &[T]) -> Vec<T> {
            let mask = self.next();
            xs.iter()
                .enumerate()
                .filter(|(i, _)| mask >> i & 1 == 1)
                .map(|(_, x)| x.clone())
                .collect()
        }
    }

    const BARS: [QualityBar; 3] = [QualityBar::Basic, QualityBar::Standard, QualityBar::High];
    const TIERS: [ExternalAccess; 4] = [
        ExternalAccess::LocalOnly,
        ExternalAccess::AnonymousRequired,
        ExternalAccess::AnonymousPreferred,
        ExternalAccess::Unrestricted,
    ];
    const DESTS: [Destination; 3] = [
        Destination::Local,
        Destination::HostedApi,
        Destination::ManualHandoff,
    ];
    const NAMES: [&str; 4] = ["a", "b", "c", "d"];

    fn random_floor(r: &mut Lcg) -> QualityFloor {
        let (x, y) = (r.pick(&BARS), r.pick(&BARS));
        QualityFloor {
            min: x.min(y),
            max: x.max(y),
        }
    }

    fn random_names(r: &mut Lcg) -> Vec<String> {
        r.subset(&NAMES).into_iter().map(str::to_owned).collect()
    }

    fn random_base(r: &mut Lcg) -> Envelope {
        let mut destinations = r.subset(&DESTS);
        if destinations.is_empty() {
            destinations.push(Destination::Local);
        }
        Envelope {
            quality_floor: random_floor(r),
            highest_tier: r.pick(&TIERS),
            destinations,
            declared_inputs: random_names(r),
        }
    }

    fn random_override(r: &mut Lcg) -> EnvelopeOverride {
        EnvelopeOverride {
            quality_floor: r.coin().then(|| random_floor(r)),
            highest_tier: r.coin().then(|| r.pick(&TIERS)),
            destinations: r.coin().then(|| r.subset(&DESTS)),
            declared_inputs: r.coin().then(|| random_names(r)),
        }
    }

    fn within(narrow: &Envelope, wide: &Envelope) -> bool {
        narrow.quality_floor.min >= wide.quality_floor.min
            && narrow.quality_floor.max <= wide.quality_floor.max
            && narrow.highest_tier <= wide.highest_tier
            && narrow
                .destinations
                .iter()
                .all(|d| wide.destinations.contains(d))
            && narrow
                .declared_inputs
                .iter()
                .all(|n| wide.declared_inputs.contains(n))
    }

    #[test]
    fn tighten_property_never_exceeds_base_or_override_and_is_idempotent() {
        let mut r = Lcg(0x05ee_d894);
        let mut oks = 0;
        let mut errs = 0;
        for _ in 0..20_000 {
            let base = random_base(&mut r);
            let o1 = random_override(&mut r);
            let o2 = random_override(&mut r);

            let t = tighten(&base, &o1);
            match &t {
                Ok(t) => {
                    oks += 1;
                    // Never above the base on any axis.
                    assert!(within(t, &base), "{base:?} {o1:?} -> {t:?}");
                    // Never above the override on any axis it names.
                    if let Some(f) = o1.quality_floor {
                        assert!(t.quality_floor.min >= f.min && t.quality_floor.max <= f.max);
                    }
                    if let Some(tier) = o1.highest_tier {
                        assert!(t.highest_tier <= tier);
                    }
                    if let Some(d) = &o1.destinations {
                        assert!(t.destinations.iter().all(|x| d.contains(x)));
                    }
                    if let Some(n) = &o1.declared_inputs {
                        assert!(t.declared_inputs.iter().all(|x| n.contains(x)));
                    }
                    // Idempotent: applying the result as an override changes nothing.
                    assert_eq!(tighten(t, &t.as_override()).as_ref(), Ok(t));
                    // Applying the same override again changes nothing.
                    assert_eq!(tighten(t, &o1).as_ref(), Ok(t));
                    // Two overrides in either order give the same envelope or both fail.
                    let ab = tighten(t, &o2);
                    let ba = tighten(&base, &o2).and_then(|x| tighten(&x, &o1));
                    // (Which error comes first can differ, so compare success and value.)
                    assert_eq!(ab.ok(), ba.ok(), "order matters: {base:?} {o1:?} {o2:?}");
                    // An empty override is the identity.
                    assert_eq!(
                        tighten(&base, &EnvelopeOverride::default()).as_ref(),
                        Ok(&base)
                    );
                }
                Err(_) => {
                    errs += 1;
                    // An error only ever comes from an emptied range.
                    let destinations_empty = o1
                        .destinations
                        .as_ref()
                        .is_some_and(|o| !base.destinations.iter().any(|d| o.contains(d)));
                    let floor_empty = o1.quality_floor.is_some_and(|f| {
                        base.quality_floor.min.max(f.min) > base.quality_floor.max.min(f.max)
                    });
                    assert!(destinations_empty || floor_empty, "{base:?} {o1:?}");
                }
            }
        }
        // The sample must exercise both outcomes or the test proves little.
        assert!(oks > 1_000 && errs > 1_000, "oks={oks} errs={errs}");
    }

    #[test]
    fn tighten_exhaustive_single_axis() {
        // Every tier pair: the result is the lower.
        for a in TIERS {
            for b in TIERS {
                let base = Envelope {
                    quality_floor: QualityFloor {
                        min: QualityBar::Basic,
                        max: QualityBar::High,
                    },
                    highest_tier: a,
                    destinations: vec![Destination::Local],
                    declared_inputs: vec![],
                };
                let t = tighten(
                    &base,
                    &EnvelopeOverride {
                        highest_tier: Some(b),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(t.highest_tier, a.min(b));
            }
        }
        // Every pair of floors.
        for (a0, a1) in [(0usize, 0usize), (0, 1), (0, 2), (1, 1), (1, 2), (2, 2)] {
            for (b0, b1) in [(0usize, 0usize), (0, 1), (0, 2), (1, 1), (1, 2), (2, 2)] {
                let base = Envelope {
                    quality_floor: QualityFloor {
                        min: BARS[a0],
                        max: BARS[a1],
                    },
                    highest_tier: ExternalAccess::LocalOnly,
                    destinations: vec![Destination::Local],
                    declared_inputs: vec![],
                };
                let over = EnvelopeOverride {
                    quality_floor: Some(QualityFloor {
                        min: BARS[b0],
                        max: BARS[b1],
                    }),
                    ..Default::default()
                };
                let (lo, hi) = (a0.max(b0), a1.min(b1));
                match tighten(&base, &over) {
                    Ok(t) => {
                        assert!(lo <= hi);
                        assert_eq!(
                            (t.quality_floor.min, t.quality_floor.max),
                            (BARS[lo], BARS[hi])
                        );
                    }
                    Err(e) => {
                        assert!(lo > hi);
                        assert_eq!(e, TightenError::EmptyQualityRange);
                    }
                }
            }
        }
    }

    // -- validation ---------------------------------------------------------

    fn range(dest: Vec<Destination>, tier: ExternalAccess) -> ModelRange {
        ModelRange {
            quality_floor: QualityFloor {
                min: QualityBar::Basic,
                max: QualityBar::High,
            },
            highest_tier: tier,
            destinations: dest,
        }
    }

    #[test]
    fn range_rules() {
        let local = range(vec![Destination::Local], ExternalAccess::LocalOnly);
        assert!(validate_range(&local, false).is_ok());
        assert!(validate_range(&local, true).is_ok());

        // External destinations and tiers are refused while the constant is false.
        let hosted = range(
            vec![Destination::Local, Destination::HostedApi],
            ExternalAccess::AnonymousPreferred,
        );
        assert!(validate_range(&hosted, false)
            .unwrap_err()
            .contains("not allowed yet"));
        assert!(validate_range(&hosted, true).is_ok());
        let tier_only = range(vec![Destination::Local], ExternalAccess::AnonymousRequired);
        assert!(validate_range(&tier_only, false)
            .unwrap_err()
            .contains("not allowed yet"));

        // Unreachable destinations are refused even when external is allowed.
        let unreachable = range(
            vec![Destination::HostedApi],
            ExternalAccess::AnonymousRequired,
        );
        assert!(validate_range(&unreachable, true)
            .unwrap_err()
            .contains("unreachable"));
        let unreachable = range(vec![Destination::ManualHandoff], ExternalAccess::LocalOnly);
        assert!(validate_range(&unreachable, true)
            .unwrap_err()
            .contains("unreachable"));

        // Empty, duplicated, inverted.
        assert!(validate_range(&range(vec![], ExternalAccess::LocalOnly), true).is_err());
        assert!(validate_range(
            &range(
                vec![Destination::Local, Destination::Local],
                ExternalAccess::LocalOnly
            ),
            true
        )
        .is_err());
        let mut inverted = local.clone();
        inverted.quality_floor = QualityFloor {
            min: QualityBar::High,
            max: QualityBar::Basic,
        };
        assert!(validate_range(&inverted, true).is_err());
    }

    #[test]
    fn slug_shape() {
        assert!(is_slug("content_brief"));
        assert!(is_slug("_x1"));
        for bad in ["", "Brief", "a-b", "1a", "a b", &"a".repeat(65)] {
            assert!(!is_slug(bad), "{bad}");
        }
    }
}

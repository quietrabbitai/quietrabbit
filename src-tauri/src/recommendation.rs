// src-tauri/src/recommendation.rs
//
// items.id=607 (decisions.id=852, decisions.id=854): onboarding Step 1
// recommendation engine. `recommend(profile, catalog)` is pure -- no DB, no
// IPC, no I/O -- so every hardware class is unit-testable on constructed
// profiles. commands::system::get_provider_recommendation builds the catalog
// from the providers table and wraps it.
//
// NO PROVIDER OR MODEL IDS in this module: the candidate set and preferred
// order come from providers-table fields only (qr_recommendation_rank,
// hardware_requirement, is_local, qr_recommended). Tests use neutral fixture
// ids for the same reason.
//
// NO USER COPY: the result carries ids and enums only; the frontend maps
// them to i18n keys.
//
// RULES (decisions.id=852 B/C):
//   * Local candidate on a usable GPU whose VRAM covers min_vram: fit is
//     Comfortable (VRAM >= 1.5x the requirement) or Tight. Nothing else is
//     ever Comfortable or Tight for a local model.
//   * Every other local case is CPU (no GPU, Unusable, Unknown, integrated
//     with no dedicated VRAM, or the model does not fit the GPU): fit is
//     Slow, and the model is "fitting" only if ram_class >= min_ram_class.
//   * Hosted candidates are not hardware-bound: fit is always Comfortable.
//   * weak_hardware = no usable GPU AND RamClass::Low. It makes a hosted
//     candidate the primary; fitting local candidates stay as alternatives.
//   * Candidates are ordered by qr_recommendation_rank only (NULL last).
//     Fit is a label, not a sort key. Rank is compared only within a kind.
//
// NOT HANDLED: an integrated GPU Usable by Ollama (unified memory) reports
// no vram_mb, so it takes the CPU path here until the macOS/Windows probe
// items (608/609) define how its shared memory should be sized.

use serde::Serialize;
use specta::Type;

use crate::hardware_probe::{HardwareProfile, OllamaUsability, RamClass};

/// VRAM at or above this multiple of a model's requirement is Comfortable,
/// below it (but still sufficient) is Tight. Expressed as a fraction to stay
/// in integer arithmetic.
const COMFORT_NUM: u64 = 3;
const COMFORT_DEN: u64 = 2;

/// Maximum alternatives returned alongside the primary pick.
const MAX_ALTERNATIVES: usize = 4;
/// Minimum alternatives to reach by pulling in non-fitting local candidates.
const MIN_ALTERNATIVES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    LocalModel,
    HostedApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Comfortable,
    Tight,
    Slow,
}

/// One catalog entry, already reduced from a providers row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider_id: String,
    pub kind: CandidateKind,
    /// providers.qr_recommendation_rank: lower = more preferred, None = no
    /// stated order (sorts last within its kind).
    pub rank: Option<u32>,
    /// Local candidates only.
    pub min_ram_class: Option<RamClass>,
    /// Local candidates only, decimal megabytes (curated min_vram_gb * 1000).
    pub min_vram_mb: Option<u32>,
    /// Local: weights already pulled. Hosted: always false.
    pub installed: bool,
    /// Hosted: an active API key already exists. Local: ignored.
    pub has_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct Recommendation {
    pub provider_id: String,
    pub kind: CandidateKind,
    pub fit: Fit,
    pub needs_install: bool,
    pub needs_api_key: bool,
    pub installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ProviderRecommendation {
    /// None only when the catalog is empty.
    pub recommended: Option<Recommendation>,
    /// 0 to 4 entries; fewer than 2 only when the catalog has fewer than 3
    /// candidates. Never an error.
    pub alternatives: Vec<Recommendation>,
    pub weak_hardware: bool,
}

fn ram_order(c: RamClass) -> u8 {
    match c {
        RamClass::Low => 0,
        RamClass::Medium => 1,
        RamClass::High => 2,
        RamClass::VeryHigh => 3,
    }
}

fn has_usable_gpu(profile: &HardwareProfile) -> bool {
    profile
        .primary_gpu
        .as_ref()
        .is_some_and(|g| g.usable_by_ollama == OllamaUsability::Usable)
}

/// Fit of a local candidate, or None when it fits neither the GPU nor RAM.
fn local_fit(profile: &HardwareProfile, c: &Candidate) -> Option<Fit> {
    let gpu_vram = profile
        .primary_gpu
        .as_ref()
        .filter(|g| g.usable_by_ollama == OllamaUsability::Usable)
        .and_then(|g| g.vram_mb);
    if let (Some(vram), Some(need)) = (gpu_vram, c.min_vram_mb) {
        if vram >= need {
            let comfortable = u64::from(vram) * COMFORT_DEN >= u64::from(need) * COMFORT_NUM;
            return Some(if comfortable {
                Fit::Comfortable
            } else {
                Fit::Tight
            });
        }
    }
    match c.min_ram_class {
        Some(min) if ram_order(profile.ram_class) >= ram_order(min) => Some(Fit::Slow),
        _ => None,
    }
}

fn to_recommendation(c: &Candidate, fit: Fit) -> Recommendation {
    let local = c.kind == CandidateKind::LocalModel;
    Recommendation {
        provider_id: c.provider_id.clone(),
        kind: c.kind,
        fit,
        needs_install: local && !c.installed,
        needs_api_key: !local && !c.has_key,
        installed: local && c.installed,
    }
}

pub fn recommend(profile: &HardwareProfile, catalog: &[Candidate]) -> ProviderRecommendation {
    let weak_hardware = !has_usable_gpu(profile) && profile.ram_class == RamClass::Low;

    // Rank NULL sorts last. provider_id breaks ties for determinism only; it
    // is not a preference. Hosted candidates also sort key-already-configured
    // first, again below rank.
    let mut local_fit_list: Vec<(&Candidate, Fit)> = Vec::new();
    let mut local_unfit: Vec<&Candidate> = Vec::new();
    let mut hosted: Vec<&Candidate> = Vec::new();
    for c in catalog {
        match c.kind {
            CandidateKind::LocalModel => match local_fit(profile, c) {
                Some(fit) => local_fit_list.push((c, fit)),
                None => local_unfit.push(c),
            },
            CandidateKind::HostedApi => hosted.push(c),
        }
    }
    local_fit_list.sort_by_key(|(c, _)| (c.rank.is_none(), c.rank, c.provider_id.clone()));
    local_unfit.sort_by_key(|c| (c.rank.is_none(), c.rank, c.provider_id.clone()));
    hosted.sort_by_key(|c| (c.rank.is_none(), c.rank, !c.has_key, c.provider_id.clone()));

    let locals: Vec<Recommendation> = local_fit_list
        .iter()
        .map(|(c, fit)| to_recommendation(c, *fit))
        .collect();
    let hosteds: Vec<Recommendation> = hosted
        .iter()
        .map(|c| to_recommendation(c, Fit::Comfortable))
        .collect();

    // Cross-kind order: strong hardware lists local before hosted, weak
    // hardware lists hosted before local.
    let mut ordered: Vec<Recommendation> = if weak_hardware {
        hosteds.into_iter().chain(locals).collect()
    } else {
        locals.into_iter().chain(hosteds).collect()
    };

    // Non-fitting local candidates are used only to reach the minimum, and
    // as a last-resort primary; they are flagged Slow.
    let mut spill = local_unfit.iter().map(|c| to_recommendation(c, Fit::Slow));

    if ordered.is_empty() {
        if let Some(first) = spill.next() {
            ordered.push(first);
        }
    }
    if ordered.is_empty() {
        return ProviderRecommendation {
            recommended: None,
            alternatives: Vec::new(),
            weak_hardware,
        };
    }
    let recommended = ordered.remove(0);
    let mut alternatives = ordered;
    while alternatives.len() < MIN_ALTERNATIVES {
        match spill.next() {
            Some(r) => alternatives.push(r),
            None => break,
        }
    }
    alternatives.truncate(MAX_ALTERNATIVES);

    ProviderRecommendation {
        recommended: Some(recommended),
        alternatives,
        weak_hardware,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_probe::{GpuInfo, GpuVendor, PROFILE_VERSION};

    fn gpu(usable: OllamaUsability, vram_mb: Option<u32>, integrated: bool) -> GpuInfo {
        GpuInfo {
            vendor: GpuVendor::Other,
            name: None,
            architecture: None,
            is_integrated: integrated,
            vram_mb,
            usable_by_ollama: usable,
        }
    }

    fn profile(ram_mb: u32, gpu: Option<GpuInfo>) -> HardwareProfile {
        HardwareProfile {
            profile_version: PROFILE_VERSION,
            ram_mb,
            ram_class: RamClass::from_mb(ram_mb),
            cpu_cores: 8,
            gpus: gpu.iter().cloned().collect(),
            primary_gpu: gpu,
        }
    }

    fn local(id: &str, rank: Option<u32>, ram: RamClass, vram_mb: u32) -> Candidate {
        Candidate {
            provider_id: id.into(),
            kind: CandidateKind::LocalModel,
            rank,
            min_ram_class: Some(ram),
            min_vram_mb: Some(vram_mb),
            installed: false,
            has_key: false,
        }
    }

    fn hosted(id: &str, rank: Option<u32>, has_key: bool) -> Candidate {
        Candidate {
            provider_id: id.into(),
            kind: CandidateKind::HostedApi,
            rank,
            min_ram_class: None,
            min_vram_mb: None,
            installed: false,
            has_key,
        }
    }

    /// Three local candidates (large, mid, small) plus two hosted, neutral ids.
    fn catalog() -> Vec<Candidate> {
        vec![
            local("local-a", Some(1), RamClass::Medium, 6_000),
            local("local-b", Some(2), RamClass::Medium, 6_000),
            local("local-c", Some(3), RamClass::Low, 3_000),
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(1), false),
        ]
    }

    fn ids(r: &ProviderRecommendation) -> Vec<&str> {
        r.recommended
            .iter()
            .chain(r.alternatives.iter())
            .map(|x| x.provider_id.as_str())
            .collect()
    }

    #[test]
    fn cpu_only_30gib_is_local_slow() {
        // This dev machine: MemTotal 31,988,204 kB = ~32,756 decimal MB
        // (VeryHigh), no GPU Ollama can use.
        let p = profile(
            32_756,
            Some(gpu(OllamaUsability::Unusable, Some(8_573), false)),
        );
        let r = recommend(&p, &catalog());
        assert!(!r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-a");
        assert_eq!(rec.fit, Fit::Slow);
        assert!(rec.needs_install);
        assert!(!rec.needs_api_key);
        assert_eq!(r.alternatives.len(), 4);
        assert!(r.alternatives.iter().all(|a| a.fit != Fit::Tight));
    }

    #[test]
    fn low_ram_no_gpu_is_weak_and_hosted_primary() {
        let p = profile(7_900, None);
        let r = recommend(&p, &catalog());
        assert!(r.weak_hardware);
        let rec = r.recommended.clone().unwrap();
        assert_eq!(rec.kind, CandidateKind::HostedApi);
        assert!(rec.needs_api_key);
        // The small local model still fits low RAM and stays an alternative,
        // flagged Slow; the larger ones do not fit and are not listed.
        let local_alts: Vec<_> = r
            .alternatives
            .iter()
            .filter(|a| a.kind == CandidateKind::LocalModel)
            .collect();
        assert_eq!(local_alts.len(), 1);
        assert_eq!(local_alts[0].provider_id, "local-c");
        assert_eq!(local_alts[0].fit, Fit::Slow);
        // Weak hardware lists hosted before local.
        assert_eq!(ids(&r), vec!["hosted-a", "hosted-b", "local-c"]);
    }

    #[test]
    fn weak_hardware_without_hosted_uses_best_local_slow() {
        let p = profile(7_900, None);
        let cat: Vec<_> = catalog()
            .into_iter()
            .filter(|c| c.kind == CandidateKind::LocalModel)
            .collect();
        let r = recommend(&p, &cat);
        assert!(r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-c");
        assert_eq!(rec.fit, Fit::Slow);
        // Non-fitting locals are pulled in to reach two alternatives.
        assert_eq!(r.alternatives.len(), 2);
        assert!(r.alternatives.iter().all(|a| a.fit == Fit::Slow));
    }

    #[test]
    fn low_ram_with_usable_gpu_is_not_weak() {
        let p = profile(
            7_900,
            Some(gpu(OllamaUsability::Usable, Some(12_000), false)),
        );
        let r = recommend(&p, &catalog());
        assert!(!r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-a");
        assert_eq!(rec.fit, Fit::Comfortable);
    }

    #[test]
    fn usable_gpu_comfortable_vs_tight_split() {
        // 1.5x of 6,000 = 9,000.
        let comfy = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(9_000), false)),
        );
        let tight = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(8_999), false)),
        );
        let cat = vec![local("local-a", Some(1), RamClass::Medium, 6_000)];
        assert_eq!(
            recommend(&comfy, &cat).recommended.unwrap().fit,
            Fit::Comfortable
        );
        assert_eq!(recommend(&tight, &cat).recommended.unwrap().fit, Fit::Tight);
    }

    #[test]
    fn gpu_too_small_falls_back_to_ram_and_is_slow() {
        let p = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(4_000), false)),
        );
        let cat = vec![local("local-a", Some(1), RamClass::Medium, 6_000)];
        let rec = recommend(&p, &cat).recommended.unwrap();
        assert_eq!(rec.fit, Fit::Slow);
    }

    #[test]
    fn unknown_and_unusable_gpus_are_cpu() {
        for usable in [OllamaUsability::Unknown, OllamaUsability::Unusable] {
            let p = profile(16_000, Some(gpu(usable, Some(24_000), false)));
            let rec = recommend(&p, &catalog()).recommended.unwrap();
            assert_eq!(rec.fit, Fit::Slow, "{usable:?}");
        }
    }

    #[test]
    fn usable_gpu_without_vram_figure_is_cpu() {
        let p = profile(16_000, Some(gpu(OllamaUsability::Usable, None, true)));
        let rec = recommend(&p, &catalog()).recommended.unwrap();
        assert_eq!(rec.fit, Fit::Slow);
    }

    #[test]
    fn order_is_rank_only_not_fit() {
        // local-x outranks local-y but is only Slow (needs more VRAM than
        // the GPU has), while local-y would be Comfortable. Rank wins.
        let p = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(4_000), false)),
        );
        let cat = vec![
            local("local-y", Some(2), RamClass::Low, 2_000),
            local("local-x", Some(1), RamClass::Medium, 6_000),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(r.recommended.as_ref().unwrap().provider_id, "local-x");
        assert_eq!(r.alternatives[0].provider_id, "local-y");
        assert_eq!(r.alternatives[0].fit, Fit::Comfortable);
    }

    #[test]
    fn null_rank_sorts_last_and_id_breaks_ties() {
        let p = profile(32_756, None);
        let cat = vec![
            local("local-z", None, RamClass::Low, 1_000),
            local("local-b", Some(5), RamClass::Low, 1_000),
            local("local-a", Some(5), RamClass::Low, 1_000),
        ];
        assert_eq!(
            ids(&recommend(&p, &cat)),
            vec!["local-a", "local-b", "local-z"]
        );
    }

    #[test]
    fn hosted_order_is_rank_then_key_then_id() {
        let p = profile(32_756, None);
        let cat = vec![
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(1), true),
            hosted("hosted-c", Some(0), false),
            hosted("hosted-d", None, true),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(
            ids(&r),
            vec!["hosted-c", "hosted-b", "hosted-a", "hosted-d"]
        );
        // Key-already-configured is reported through needs_api_key.
        assert!(!r.alternatives[0].needs_api_key);
        assert!(r.alternatives[1].needs_api_key);
    }

    #[test]
    fn strong_hardware_lists_local_before_hosted() {
        let p = profile(32_756, None);
        let r = recommend(&p, &catalog());
        assert_eq!(
            ids(&r),
            vec!["local-a", "local-b", "local-c", "hosted-a", "hosted-b"]
        );
    }

    #[test]
    fn alternatives_capped_at_four() {
        let p = profile(32_756, None);
        let mut cat = catalog();
        cat.push(hosted("hosted-c", Some(1), false));
        let r = recommend(&p, &cat);
        assert_eq!(r.alternatives.len(), 4);
    }

    #[test]
    fn installed_local_reports_state() {
        let p = profile(32_756, None);
        let mut c = local("local-a", Some(1), RamClass::Low, 1_000);
        c.installed = true;
        let rec = recommend(&p, &[c]).recommended.unwrap();
        assert!(rec.installed);
        assert!(!rec.needs_install);
    }

    #[test]
    fn small_catalog_never_errors() {
        let p = profile(32_756, None);
        let empty = recommend(&p, &[]);
        assert!(empty.recommended.is_none());
        assert!(empty.alternatives.is_empty());

        let one = recommend(&p, &[hosted("hosted-a", Some(1), false)]);
        assert_eq!(one.recommended.unwrap().provider_id, "hosted-a");
        assert!(one.alternatives.is_empty());

        let two = recommend(
            &p,
            &[
                hosted("hosted-a", Some(1), false),
                hosted("hosted-b", Some(1), false),
            ],
        );
        assert_eq!(two.alternatives.len(), 1);
    }

    #[test]
    fn nothing_fits_still_returns_a_primary() {
        let p = profile(7_900, None);
        let cat = vec![
            local("local-a", Some(1), RamClass::High, 9_000),
            local("local-b", Some(2), RamClass::High, 9_000),
            local("local-c", Some(3), RamClass::High, 9_000),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(r.recommended.unwrap().fit, Fit::Slow);
        assert_eq!(r.alternatives.len(), 2);
    }
}

// src-tauri/src/spend/mod.rs
//
// items.id=710 (decisions.id=875 points 3 and 4, decisions.id=872 points 3
// and 6, PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 8d and 8e): consent,
// warnings, limits, the spend ledger, the cost estimate and their enforcement
// for hosted providers.
//
// Nothing here names a provider or a model (decisions.id=872). Money is
// integer micro-USD everywhere (`money`); prices are integer micro-USD per
// million tokens (`price`). Every user-facing statement is a stable code plus
// structured parameters (`codes`), never prose: the copy renders through the
// frontend's i18n when the consuming screens land (items.id=693, 580, 696).

pub mod calibration;
pub mod codes;
pub mod disclosure;
pub mod estimate;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod gate;
pub mod money;
pub mod price;

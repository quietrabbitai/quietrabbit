// src-tauri/src/persistence/shared_db_residual.rs
//
// The exact contents of the unencrypted shared.db (items.id=661,
// decisions.id=862): every table, every column, and WHY it may be readable
// without the password. This is the machine-checked source for the privacy
// floor's one carve-out (decisions.id=597 member 1 as amended by
// decisions.id=862) and for items.id=662's floor text.
//
// persistence::shared_db_privacy_tests compares a fresh install's real schema
// to this list. A new table or column FAILS that test until someone adds it
// here with a class, which is the point: shared.db is readable from disk, so
// adding to it is a decision, not a side effect.
//
// CLASSES
//   Login          read to show the login list and check a password: users
//                  (display name for the login list, role, idle timeout) and
//                  the failure and lockout bookkeeping keyed by display name.
//   KeyMaterial    key-derivation salts and parameters, the password wrap and
//                  recovery seal (ciphertext and public values only), the
//                  re-key state machine (wrapped keys; rel_path holds paths
//                  that embed Persona ids, no names), the public sharing key.
//   InstanceConfig instance_config (lockout switch, nightly batch times, the
//                  hardware profile), schema_version, migration_lock.
//   GlobalCatalog  curated data with no user content: providers and models
//                  (including per-machine install flags), the catalog state and
//                  history, shipped Focus definitions and their blocks. The
//                  pre-login nightly sweep reads focuses.
//   StructuralIds  ids, membership, status and timestamps: personas(id,
//                  created_at) as the FK anchor, user_personas, auth_sessions,
//                  user_capabilities, artifact_versions, group_departures
//                  (group id, Persona id, left or removed).
//   Mailbox        pending_group_invitations, pending_group_key_rotations,
//                  pending_persona_shares: a sender writes a row for a
//                  recipient who may be offline, so it cannot live in the
//                  recipient's encrypted store. Ids, status, timestamps and
//                  ciphertext only; names and labels are inside the ciphertext.
//   KnownResidual  accepted and listed for items.id=662, NOT clean:
//                  focus_settings and focus_settings_friction_decisions are
//                  keyed (persona_id, focus_id), so they show which Focuses
//                  each Persona uses (Jason, items.id=661 review, option b).
//                  focus_block_customization holds per-Persona Focus
//                  customization JSON; it has no writer yet, and when the Focus
//                  builder writes it, it must move to an encrypted store.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualClass {
    Login,
    KeyMaterial,
    InstanceConfig,
    GlobalCatalog,
    StructuralIds,
    Mailbox,
    KnownResidual,
}

#[derive(Debug, Clone, Copy)]
pub struct ResidualTable {
    pub name: &'static str,
    pub class: ResidualClass,
    pub columns: &'static [&'static str],
}

pub const RESIDUAL_SHARED_SCHEMA: &[ResidualTable] = &[
    // ip_address is plaintext in this table (and in auth_sessions, with
    // user_agent). No current writer fills it: the failure insert at
    // commands/auth.rs records the display name and the time only. If one ever
    // does, the address of a failed login is readable from disk without the
    // password, so that is a decision for items.id=662's list, not a default.
    ResidualTable {
        name: "auth_failures",
        class: ResidualClass::Login,
        columns: &["id", "display_name", "attempted_at", "ip_address"],
    },
    ResidualTable {
        name: "auth_lockouts",
        class: ResidualClass::Login,
        columns: &["display_name", "locked_until", "failure_count", "locked_at"],
    },
    ResidualTable {
        name: "users",
        class: ResidualClass::Login,
        columns: &[
            "id",
            "display_name",
            "role",
            "is_primary",
            "auth_enabled",
            "password_hash",
            "idle_timeout_minutes",
            "created_at",
            "extra_metadata",
        ],
    },
    ResidualTable {
        name: "user_key_rotation_items",
        class: ResidualClass::KeyMaterial,
        columns: &["rotation_id", "rel_path", "kind", "state", "bytes", "error"],
    },
    ResidualTable {
        name: "user_key_rotations",
        class: ResidualClass::KeyMaterial,
        columns: &[
            "rotation_id",
            "user_id",
            "kind",
            "direction",
            "state",
            "from_epoch",
            "to_epoch",
            "started_at",
            "finished_at",
            "last_error",
            "old_dek_under_new_hex",
            "staged_salt_hex",
            "staged_kdf_memory_kib",
            "staged_kdf_iterations",
            "staged_kdf_parallelism",
            "staged_format_version",
            "staged_password_wrap_hex",
            "staged_recovery_public_key_hex",
            "staged_recovery_seal_hex",
            "staged_sharing_public_key_hex",
        ],
    },
    ResidualTable {
        name: "user_key_wrappers",
        class: ResidualClass::KeyMaterial,
        columns: &[
            "user_id",
            "key_epoch",
            "format_version",
            "password_wrap_hex",
            "recovery_public_key_hex",
            "recovery_seal_hex",
            "pending_rotation",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "user_salts",
        class: ResidualClass::KeyMaterial,
        columns: &[
            "user_id",
            "salt_hex",
            "kdf_algorithm",
            "kdf_memory_kib",
            "kdf_iterations",
            "kdf_parallelism",
            "created_at",
        ],
    },
    ResidualTable {
        name: "user_sharing_keys",
        class: ResidualClass::KeyMaterial,
        columns: &[
            "user_id",
            "public_key_hex",
            "created_at",
            "key_epoch",
            "rotated_at",
        ],
    },
    ResidualTable {
        name: "instance_config",
        class: ResidualClass::InstanceConfig,
        columns: &["key", "value"],
    },
    ResidualTable {
        name: "migration_lock",
        class: ResidualClass::InstanceConfig,
        columns: &["id", "locked_at", "locked_by"],
    },
    ResidualTable {
        name: "schema_version",
        class: ResidualClass::InstanceConfig,
        columns: &["version", "applied_at", "description"],
    },
    ResidualTable {
        name: "building_blocks",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "stable_id",
            "display_name",
            "invocation_mode",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "focus_block_compositions",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "id",
            "focus_id",
            "action_id",
            "block_stable_id",
            "sequence_index",
            "schedule_trigger_anchor_field",
            "schedule_trigger_offset",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "focus_provider_criteria",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "focus_id",
            "require_is_local",
            "require_is_anonymous",
            "require_not_trains_on_data",
            "allow_provider_ids",
            "deny_provider_ids",
            "seeded_from_policy",
            "created_at",
            "updated_at",
        ],
    },
    // User-authored Focuses (display_name, description) must NOT be written
    // here. This table holds shipped Focus definitions only. The Focus builder
    // (items.id=38) stores authored Focuses and per-Persona settings in an
    // encrypted per-user store (decisions.id=862).
    ResidualTable {
        name: "focuses",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "focus_id",
            "display_name",
            "description",
            "version",
            "max_routing_tier",
            "output_type",
            "suggest_in_focuses",
            "multi_source_validation",
            "generic_title_template",
            "high_priority_trigger_anchor_field",
            "high_priority_trigger_offset",
            "status",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "provider_catalog_history",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "version",
            "sha256",
            "document",
            "undo",
            "applied_at",
            "rolled_back_at",
        ],
    },
    ResidualTable {
        name: "provider_catalog_state",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "id",
            "applied_version",
            "applied_sha256",
            "applied_at",
            "declined_version",
        ],
    },
    ResidualTable {
        name: "provider_models",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "id",
            "provider_id",
            "model_id",
            "context_window_tokens",
            "is_default",
            "created_at",
            "pricing",
            "request_options",
            "token_factor",
            "output_overhead",
            "status",
            "retires_at",
            "requires_paid",
            "qr_recommended",
            "qr_recommendation_rank",
            "retains_data_override",
            "trains_on_data_override",
            "retention_days_override",
            "context_window_source",
            "context_window_as_of",
        ],
    },
    ResidualTable {
        name: "providers",
        class: ResidualClass::GlobalCatalog,
        columns: &[
            "id",
            "display_name",
            "provider_type",
            "mode",
            "launch_url",
            "activation_status",
            "documentation_gate",
            "last_reviewed_at",
            "review_trigger_note",
            "created_at",
            "is_local",
            "is_anonymous",
            "retains_data",
            "trains_on_data_by_default",
            "login_required",
            "qr_internal_eligible",
            "privacy_guardian_default_level",
            "risk_rating",
            "hardware_requirement",
            "user_privacy_summary",
            "qr_recommended",
            "privacy_commitment_basis",
            "performance_profile",
            "preference_tier",
            "installed",
            "qr_disabled_by_user",
            "local_model_tag",
            "local_model_digest",
            "installed_at",
            "focus_eligible",
            "cloud_chat_visible",
            "qr_recommendation_rank",
            "api_dialect",
            "base_url_template",
            "auth_scheme",
            "extra_headers",
            "models_path",
            "user_variables",
            "free_tier",
            "plan_inclusion",
            "retention_days",
            "jurisdiction",
            "verification_status",
            "credential_acquisition",
            "spend_limit_url",
            "logo_key",
            "required_request_options",
            // items.id=873: declared login hosts, curated catalog data with no user content.
            "login_hosts",
        ],
    },
    ResidualTable {
        name: "artifact_versions",
        class: ResidualClass::StructuralIds,
        columns: &[
            "artifact_type",
            "artifact_id",
            "scope",
            "persona_id",
            "version",
            "trust_level",
            "revoked",
            "installed_at",
            "extra_metadata",
        ],
    },
    ResidualTable {
        name: "auth_sessions",
        class: ResidualClass::StructuralIds,
        columns: &[
            "session_id",
            "user_id",
            "created_at",
            "last_active_at",
            "expires_at",
            "ip_address",
            "user_agent",
            "is_remember_me",
        ],
    },
    ResidualTable {
        name: "group_departures",
        class: ResidualClass::StructuralIds,
        columns: &["group_id", "persona_id", "departed_at", "reason"],
    },
    ResidualTable {
        name: "personas",
        class: ResidualClass::StructuralIds,
        columns: &["id", "created_at"],
    },
    ResidualTable {
        name: "user_capabilities",
        class: ResidualClass::StructuralIds,
        columns: &[
            "user_id",
            "persona_id",
            "capability",
            "allowed",
            "created_at",
        ],
    },
    ResidualTable {
        name: "user_personas",
        class: ResidualClass::StructuralIds,
        columns: &["user_id", "persona_id", "joined_at"],
    },
    ResidualTable {
        name: "pending_group_invitations",
        class: ResidualClass::Mailbox,
        columns: &[
            "id",
            "recipient_persona_id",
            "group_id",
            "encrypted_group_key",
            "status",
            "created_at",
            "responded_at",
            "extra_metadata",
        ],
    },
    ResidualTable {
        name: "pending_group_key_rotations",
        class: ResidualClass::Mailbox,
        columns: &[
            "id",
            "recipient_persona_id",
            "group_id",
            "encrypted_group_key",
            "status",
            "created_at",
            "applied_at",
            "extra_metadata",
        ],
    },
    ResidualTable {
        name: "pending_persona_shares",
        class: ResidualClass::Mailbox,
        columns: &[
            "id",
            "recipient_user_id",
            "source_persona_id",
            "payload_schema_version",
            "encrypted_payload",
            "status",
            "created_at",
            "responded_at",
            "extra_metadata",
            "materialized_persona_id",
            "share_type",
            "revoked_at",
        ],
    },
    ResidualTable {
        name: "focus_block_customization",
        class: ResidualClass::KnownResidual,
        columns: &[
            "id",
            "composition_id",
            "persona_id",
            "customization",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "focus_settings",
        class: ResidualClass::KnownResidual,
        columns: &[
            "persona_id",
            "focus_id",
            "context_flow",
            "library_visibility",
            "privacy_tier",
            "max_permitted_tier",
            "focus_profile",
            "voice_override",
            "created_at",
            "updated_at",
        ],
    },
    ResidualTable {
        name: "focus_settings_friction_decisions",
        class: ResidualClass::KnownResidual,
        columns: &[
            "id",
            "persona_id",
            "focus_id",
            "decision",
            "requested_privacy_tier",
            "requested_focus_profile",
            "requested_max_permitted_tier",
            "existing_privacy_tier",
            "existing_focus_profile",
            "existing_max_permitted_tier",
            "created_at",
        ],
    },
];

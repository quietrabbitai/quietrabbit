---
paths:
  - "src-tauri/src/persistence/entity*.rs"
  - "src-tauri/src/persistence/facts_*.rs"
  - "src-tauri/src/commands/my_facts.rs"
  - "frontend/src/navShell/MyFacts*"
  - "frontend/src/navShell/myFacts*"
  - "frontend/src/navShell/StopUsingDialog.tsx"
  - "src-tauri/src/persistence/personal_store.rs"
  - "src-tauri/src/persistence/dedup_store.rs"
  - "src-tauri/src/persistence/source_registry_store.rs"
  - "src-tauri/schema/personal_*.sql"
---

## Entity Model (D6-371–D6-375 — LOCKED)
personal_fields flat key-value model replaced by entities + entity_facts.
Migration: personal_002.sql (complete, entities + entity_facts). D6-459 additions
(source_registry, deduplication, modification_state, soft-delete tombstone) also
complete -- see personal_002.sql. CORRECTED 2026-09-12 (items.id=477's
schema-shape golden-snapshot test build surfaced this while iterating
SCHEMA_FILES): the personal_*.sql sequence was NOT consolidated to 2 files --
personal_005.sql through personal_008.sql are real, embedded in SCHEMA_FILES,
and exercised (an existing migrations.rs test asserts personal.db applies all
8 real versions on first open). The prior "consolidated from 7 planned files
to 2, do not search for those files" note here was accurate as of 2026-08-01
but has since gone stale as personal_005-008.sql were actually built. Current
count: 8 files, personal_001.sql through personal_008.sql, all live.

Key facts:
- entities: self-referential (parent_entity_id — PENDING DROP, not yet scheduled),
  open type vocabulary, aliases as JSON array. status CHECK is active, archived,
  deleted_in_source, user_deleted (personal_002.sql rebuilt it and folded
  'retired' into archived; personal_001.sql's older list is not the live one).
  There is no retired_at column, so decisions.id=416 invariant 13 is not
  implemented: end-dating (valid_until) plus entity status stand in for it
  (decisions.id=969). retire_entity only sets status; it end-dates nothing.
- entity_facts: entity_id nullable (NULL = singleton), temporal validity,
  source field tracks provenance
- entity_relationships: stub table only R1 — no reads, no writes, no IPC
- Singletons (entity_id = NULL) work exactly as personal_fields did
- Full schema: D6-372. All fifteen invariants: D6-373.

## Key Architectural Decisions

- Hierarchical fact model: scope tree (depth tiers) + knowledge graph (D6-459).
  Two distinct layers — do not conflate. Scope tree governs inheritance and access
  control. Knowledge graph governs semantic relationships.
- Source of truth framework (D6-460): source_registry, deduplication candidates,
  modification_state, soft-delete tombstone. First instantiated by Cooking.
- parent_entity_id: DROP in a future migration (not yet scheduled). Confirm no
  Rust code references this column first. entity_relationships is the canonical
  relationship store.

## My Facts: browse, stop using, use again (items.id=959, decisions.id=968, 969)
The Library's Facts button. Add and change are items.id=961, sharing choices items.id=960.
- Code: commands/my_facts.rs (get_facts_tree, get_fact_history, search_facts, list_removed,
  stop_using_fact, stop_using_section, use_again_fact, use_again_section),
  persistence/facts_view_store.rs (reads only, a scan pins it), persistence/facts_stop_store.rs
  (writes), frontend navShell/MyFactsView.tsx, myFactsFormat.ts, StopUsingDialog.tsx.
- ISOLATION: one persona_id per command; writer gate, session-derived user and key, membership
  check, THEN that one Persona's personal.db is opened (open_checked, a scan pins the order).
  Errors are short codes; any read failure is facts_unreadable and the view shows no partial tree.
- VISIBLE = status active AND owner_focus IS NULL AND the parent chain is visible too (invariant
  14 at read time). Facts: valid_until IS NULL, in General (entity_id NULL) or a visible section.
  Every write resolves its id in that same set first, so an owned entity, another Persona's id and
  a section archived by something else are fact_not_found or section_not_found. Both stores run
  their own SQL (always owner_focus IS NULL); search_entities is not widened. Search covers names,
  aliases (json_each), labels (also in spaced form) and values.
- NO DELETE (a scan). STOP MARKER, no column: a stop sets valid_until and writes stopped_at and
  stop_scope ('fact' or 'section') into the row's extra_metadata. A marked row is never an earlier
  value (use again writes the same value back). A stopped section is status archived on the PARENT
  only, with stopped_at in the entity's extra_metadata, which also tells it from an archive made
  elsewhere; its own facts share that timestamp. Children are never written.
- Use again: a NEW row (invariant 8) copying value, sensitivity, abstraction tiers, source and the
  three provenance columns. Refused newer_value_exists when the label has an active value; only the
  label's latest ended row can come back. A section restore is atomic and brings back only the facts
  stopped with it. A stop and a use again flip a pristine entity to user_modified, which is what
  keeps a persona sync from overwriting a stopped synced SECTION.
- Provenance words: user_confirmed, ipc, extract_confirm = "You confirmed this"; corrected,
  conversation, extracted have their own; anything else (interview, synced_share) shows none.
  Production writers set ipc (commands/personal.rs) and extract_confirm (conductor/extract.rs);
  dedup_store's merge insert omits source, so it gets the column default interview.
- extra_metadata TRAP: personal_store::create_entity_fact_with_policy_conn (and so
  create_entity_fact_with_provenance*) stores the JSON text `null`, not `{}`, when no metadata is
  passed (Option<Value>::unwrap_or_default is Value::Null). json_set and json_insert on it are
  SILENT no-ops. Anything that writes a key into entity_facts.extra_metadata must first normalise:
  CASE WHEN json_type(extra_metadata) = 'object' THEN extra_metadata ELSE '{}' END. A reader must
  not assume an object.

### Who still reads after a stop (audit at items.id=959; re-check before changing the copy)
- Every production loader of entity_facts VALUES filters valid_until IS NULL, including the
  context-assembly load (personal_store::load_entity_facts_for_context, decisions.id=424), the
  singleton field reads and export, list_pending_cross_persona_exports, the share, sync and
  view-sync fact loader and dedup. The one read of ended rows outside My Facts is
  entity_store::check_entity_cascade's superseded COUNT (no production caller). entity_purge counts
  and deletes owned rows only.
- NOT cascaded: load_entity_facts_for_context and list_pending_cross_persona_exports do not read
  entities.status, and persona share, sync and view-sync list status='active' entities one by one,
  not by parent chain. So a CHILD of a stopped section is hidden in My Facts but its active facts
  are still loaded and still shared. The stop-section confirm says so; do not claim otherwise
  until those loaders apply invariant 14. (The Gate 1 bridge takes singletons only.)
- Entity reads with no status filter: get_entity_conn by id (source_registry refresh verdict and
  mark-modified, persona_sync's exists check, dedup's load_record_conn), check_entity_cascade,
  source_registry::list_record_ids_for_source, entity_purge's owned-entity list.
- A stop does not reach chat history, saved outputs, a run snapshot's approved disclosure buffers
  (work already started), copies a share already made, or a view-only recipient's cache before its
  next sync.
- OPEN DEFECT, persona sync: persona_sync::engine::apply_synced_fact inserts under the owner's row
  id. A stopped synced GENERAL fact keeps that id, so while the owner still sends it every pull
  fails (UNIQUE entity_facts.id) and the WHOLE update rolls back; when the owner changes the value
  the fact returns. Seen in a throwaway test. No IPC command creates a synced share today.

### UI copy rule
navShell.facts.* never says "QR", never promises that Quiet Rabbit will not use a stopped value, and
states the limits above (myFactsFormat.test.ts pins it).

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
The Library's Facts button. Add and change are items.id=961; sharing choices are the next section.
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
  elsewhere. Its facts AND the active facts of every descendant section (any status, never into an
  owned entity) are end-dated with that one timestamp and scope 'section'. No descendant's status
  or metadata is written (invariant 14); the end-dating is what stops the loaders below reading
  them. SectionView.stop_fact_count is that same walk, and the confirm states it.
- Use again: a NEW row (invariant 8) copying value, sensitivity, abstraction tiers, source and the
  three provenance columns. Refused newer_value_exists when the label has an active value; only the
  label's latest ended row can come back. A section restore is atomic and brings back only the facts
  stopped with it, descendants' included, found by the shared timestamp and not by today's parent
  chain; a fact or child section stopped on its own earlier stays stopped. A stop and a use again
  flip each pristine entity whose facts they touch (the section always) to user_modified, which is
  what keeps a persona sync from writing a stopped synced section's or descendant's facts back.
- use_again_section is refused (section_not_found, nothing written) unless the section's whole
  parent chain is in the visible set, so a child's facts never come back active under a stopped or
  owned ancestor. This supersedes the earlier "use again by id still works" under a stopped parent.
  The check is on the section asked for: facts restored by stamp are not re-checked against where
  their entity sits now.
- LIMIT of the cascade: it ends what is active at stop time. A fact written to a descendant later
  is read as usual.
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
- Invariant 14 is NOT applied by the loaders: load_entity_facts_for_context and
  list_pending_cross_persona_exports do not read entities.status, and persona share, sync and
  view-sync list status='active' entities one by one, not by parent chain. A section stop therefore
  stops its descendants' facts by end-dating them (above); a section archived by anything that does
  NOT end-date them leaves them loaded and shared. (The Gate 1 bridge takes singletons only.)
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

## My Facts: sharing choices (items.id=960, decisions.id=953, 968 addendum 2)
The Sharing block in a fact's panel, over get_fact_policies and set_fact_policy. How a stored choice
applies to a step is in personal-fields.md ("WHICH CHOICE APPLIES").
- Code (frontend navShell): factSharing.ts (pure: what the block is, which lines show, the partly
  shared mark), factSharingController.ts (pure: read, write, re-read), FactSharing.tsx (controlled, no
  state, no command), MyFactsView.tsx (one controller per Persona; binds the two commands). It is the
  only writer of the policy in the app (a source pin). No DOM test runner: the wiring is pinned on source.
- WHO HAS A BLOCK: a General fact whose STORED name is exactly one of the four catalogued names (the
  bridge's own rule). "City" typed under General shows as "City" like location_city but has no block;
  mapping a typed label to a catalogued name belongs to items.id=961. Nothing in the app creates a fact
  under the four names yet, so the block shows only for rows that already exist.
- FIXED, no control: FactView.copied_in (source_persona_id is not this Persona, a missing one included;
  the bridge's test, NOT cross_persona_export), a sensitivity other than general or personal, or
  not_permitted. The fact's own floors are decided before the policies load, so they hold when it fails.
- ROUTES: anonymous (tier 2) offers Share when can_share, Describe, Keep back; signed-in (tier 3) offers
  Describe and Keep back only. A control's value is `effective`, never `choice`. A stored ask (no screen
  offers it) is shown as itself with buttons that replace it with Describe or Keep back.
- FAILS CLOSED: an unreadable read, a missing entry or ONE word a route cannot hold fails the whole block:
  a line and Try again, no control. The tree and Stop using stay usable. An unreadable policy table is
  not distinguishable: the command answers choice omit, so the block shows Keep back and a write fails.
- WRITES: no optimistic value. Every write is followed by a read, whatever it answered
  (set_fact_policy can fail after storing); the write's returned entry is not used; a failed re-read drops
  the old values; one write at a time per Persona; only the newest read applies; nothing after dispose.
- A choice is per field NAME: it survives a new value and stop / use again. A run in progress takes the
  STRICTER of its snapshot and the table before each hosted step; a parked run resumed after a
  loosening builds its track again and takes the new choice.
- COPY: navShell.facts.sharing.* and fact_policy.* (design record section 10a; en.json is the home).
  Pinned: no "QR", no "always" or "never", no claim of where a value stays, no tier or digit; the limits
  line (exact text, typed chat, other words, under four characters, this saved fact only) shows under
  every fixed or choosable block; the anonymous scope note only while that choice is not Keep back; the
  Describe note wherever Describe is offered.

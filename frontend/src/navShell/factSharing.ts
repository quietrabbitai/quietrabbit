// items.id=960 unit 1 (decisions.id=953, 968 addendum 2): what the Sharing block of a
// fact's panel is, as pure, React-free code, so the hard floors, the default and the
// failure direction are tested on Node's built-in TypeScript support (the
// myFactsFormat.ts precedent). Nothing here reads or writes: the component and its
// write-then-re-read controller are unit 2.
//
// RULES:
// - Only a catalogued name in the General row, matched exactly, has a block: the Gate 1
//   bridge skips every other fact ("City" is not location_city).
// - HARD FLOORS give the fixed block, with no control: a copied-in fact, a stored or
//   catalogue sensitivity that is not general or personal (an unknown one included), or
//   a route the resolver reports as not_permitted. The floors a fact carries itself are
//   decided before the policies are looked at, so they hold while loading or failed.
// - A route's value comes from `effective`, NEVER from `choice`: a stored word the
//   resolver refuses is not what applies.
// - FAILS CLOSED: policies that could not be read, no entry for the field, or an
//   effective word the route cannot hold give the failed block, with no control. It is
//   never shown as Keep back or Share.
// - The signed-in route offers Describe and Keep back only, whatever can_share says
//   (no v1 field may be passed there). The anonymous route offers Share only when
//   can_share.
// - A stored `ask` (the command accepts it, no screen offers it) releases the value
//   behind the outbound review: the anonymous route shows it as itself and lets the
//   user REPLACE it with Describe or Keep back, never Share and never ask again, so
//   sharing can always be made stricter.
//
// FOR UNIT 2'S COPY: Share on the anonymous route has no effect on a step that may
// reach signed-in services (that step applies the signed-in choice, unit 0), so the
// block must not promise otherwise. Unit 2 adds a placeholder key for a scope note
// under the anonymous route (navShell.facts.sharing.anonymousScope); Chat-BRAND words
// it.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { FactPolicyInfo, FactPolicyTier, FactView } from '../bindings.ts'
import type { T } from './myFactsFormat.ts'

/** The closed catalogue's names (block_data/personal_fact_fields.json; a test pins
 *  the two lists equal). Held here so a block can say it failed to load. */
export const CATALOGUED_FIELDS: readonly string[] = [
  'location_city',
  'job_title',
  'employer',
  'industry',
]

/** The sensitivities that are open to a user's choice. */
const OPEN_SENSITIVITIES: readonly string[] = ['general', 'personal']

export type SharingWord = 'pass' | 'summarize' | 'omit'

/** What a stored ask may be replaced with. */
const ASK_REPLACEMENTS: readonly SharingWord[] = ['summarize', 'omit']

export type RouteControl =
  | { kind: 'choose'; value: SharingWord; options: SharingWord[] }
  /** A stored ask, shown as itself. `options` are what may replace it. */
  | { kind: 'ask'; options: SharingWord[] }

export interface SharingRoute {
  route: 'anonymous' | 'signedIn'
  /** The tier number set_fact_policy takes. Never shown. */
  tier: 2 | 3
  control: RouteControl
}

export type PoliciesLoad =
  | { kind: 'loading' }
  | { kind: 'failed'; code: string }
  | { kind: 'ready'; policies: FactPolicyInfo[] }

export type SharingBlock =
  | { kind: 'none' }
  | { kind: 'fixed' }
  | { kind: 'loading' }
  | { kind: 'failed'; code: string }
  | {
      kind: 'choices'
      fieldName: string
      routes: [SharingRoute, SharingRoute]
      /** Both routes are at the default: no stored choice, and Keep back applies. */
      defaultNote: boolean
    }

/** What one route shows, or null when its effective word is not one it can hold. */
function routeControl(
  tier: FactPolicyTier,
  options: SharingWord[],
  askAllowed: boolean,
): RouteControl | null {
  if (tier.effective === 'ask') {
    return askAllowed ? { kind: 'ask', options: [...ASK_REPLACEMENTS] } : null
  }
  const value = options.find((word) => word === tier.effective)
  return value === undefined ? null : { kind: 'choose', value, options }
}

/** The Sharing block of one fact. `inGeneral` is true for a fact of the General row
 *  (FactsTree.general), false for a fact inside a section. */
export function sharingBlock(
  fact: Pick<FactView, 'label' | 'sensitivity' | 'copied_in'>,
  inGeneral: boolean,
  load: PoliciesLoad,
): SharingBlock {
  if (!inGeneral || !CATALOGUED_FIELDS.includes(fact.label)) return { kind: 'none' }
  // Anything but a clear "not copied in" is the floor.
  if (fact.copied_in !== false || !OPEN_SENSITIVITIES.includes(fact.sensitivity)) {
    return { kind: 'fixed' }
  }
  if (load.kind === 'loading') return { kind: 'loading' }
  if (load.kind === 'failed') return { kind: 'failed', code: load.code }

  const entry = load.policies.find((p) => p.field_name === fact.label)
  if (entry === undefined) return { kind: 'failed', code: 'catalogue_unavailable' }
  if (
    !OPEN_SENSITIVITIES.includes(entry.sensitivity) ||
    entry.tier2.effective === 'not_permitted' ||
    entry.tier3.effective === 'not_permitted'
  ) {
    return { kind: 'fixed' }
  }
  const anonymous = routeControl(
    entry.tier2,
    entry.tier2.can_share === true ? ['pass', 'summarize', 'omit'] : ['summarize', 'omit'],
    true,
  )
  const signedIn = routeControl(entry.tier3, ['summarize', 'omit'], false)
  if (anonymous === null || signedIn === null) {
    return { kind: 'failed', code: 'fact_policy_unavailable' }
  }
  return {
    kind: 'choices',
    fieldName: fact.label,
    routes: [
      { route: 'anonymous', tier: 2, control: anonymous },
      { route: 'signedIn', tier: 3, control: signedIn },
    ],
    defaultNote: [entry.tier2, entry.tier3].every(
      (tier) => tier.choice === null && tier.effective === 'omit',
    ),
  }
}

/** Which of the block's lines show (design record section 10a, display
 *  conditions). The loading and failed blocks show none of them. */
export interface SharingNotes {
  /** What Keep back cannot catch: under a fixed block and under the choices. */
  limits: boolean
  /** Both routes are at the default. */
  defaultNote: boolean
  /** When a choice takes hold. */
  next: boolean
  /** What Describe sends: wherever Describe is offered. */
  describeNote: boolean
  /** That an anonymous Share does not reach signed-in work: only while the
   *  anonymous choice is not Keep back (a stored ask is not). */
  anonymousScope: boolean
}

export function sharingNotes(block: SharingBlock): SharingNotes {
  const choices = block.kind === 'choices' ? block : null
  const anonymous = choices?.routes[0].control
  return {
    limits: block.kind === 'fixed' || choices !== null,
    defaultNote: choices?.defaultNote === true,
    next: choices !== null,
    describeNote:
      choices?.routes.some((r) => r.control.options.includes('summarize')) === true,
    anonymousScope:
      anonymous !== undefined && (anonymous.kind === 'ask' || anonymous.value !== 'omit'),
  }
}

/** Whether a fact is marked as partly shared: some route is not at Keep back. A fixed,
 *  loading or failed block is never marked. */
export function isPartlyShared(block: SharingBlock): boolean {
  return (
    block.kind === 'choices' &&
    block.routes.some((r) => r.control.kind === 'ask' || r.control.value !== 'omit')
  )
}

/** The user's word for a stored policy word. */
export function sharingWordText(t: T, word: SharingWord | 'ask'): string {
  switch (word) {
    case 'pass':
      return t('fact_policy.policy.pass')
    case 'summarize':
      return t('fact_policy.policy.summarize')
    case 'omit':
      return t('fact_policy.policy.omit')
    case 'ask':
      return t('fact_policy.policy.ask')
  }
}

/** The sentence for a failed read or write (commands/fact_policy.rs ERROR_CODES). An
 *  unknown code, or a rejected call, reads as unavailable: never a raw string. */
export function sharingErrorText(t: T, code: string): string {
  switch (code) {
    case 'persona_not_found':
      return t('fact_policy.error.persona_not_found')
    case 'unknown_field':
      return t('fact_policy.error.unknown_field')
    case 'invalid_tier':
      return t('fact_policy.error.invalid_tier')
    case 'invalid_policy':
      return t('fact_policy.error.invalid_policy')
    case 'pass_not_allowed':
      return t('fact_policy.error.pass_not_allowed')
    case 'tier3_not_selectable':
      return t('fact_policy.error.tier3_not_selectable')
    case 'catalogue_unavailable':
      return t('fact_policy.error.catalogue_unavailable')
    default:
      return t('fact_policy.error.fact_policy_unavailable')
  }
}

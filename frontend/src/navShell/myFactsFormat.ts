// items.id=959 unit 3: the pure, React-free helpers of the Library Facts view (field
// labels, the section nesting, the counts a stop confirm states, provenance and error
// wording), so they can be tested on Node's built-in TypeScript support (same
// precedent as sharedLogFormat.ts). Every word comes through `t`, and the t() call
// sites stay LITERAL here so the i18n consistency test sees each key.
//
// COPY HONESTY (decisions.id=968, 969): the stop wording must not claim more than the
// build does. See the unit 4 notes in .claude/rules/entity-model.md.
//
// CHAT-BRAND REVIEW: all copy under navShell.facts.* is placeholder wording.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { FactProvenance, SectionView } from '../bindings.ts'

export type T = (key: string, params?: Record<string, unknown>) => string

/** The shown label of a stored field name. The four catalogued General fields have
 *  words of their own; anything else is the stored name with spaces for
 *  underscores. */
export function fieldLabel(t: T, name: string): string {
  switch (name) {
    case 'location_city':
      return t('navShell.facts.field.locationCity')
    case 'job_title':
      return t('navShell.facts.field.jobTitle')
    case 'employer':
      return t('navShell.facts.field.employer')
    case 'industry':
      return t('navShell.facts.field.industry')
    default: {
      const spaced = name.replace(/_/g, ' ')
      return spaced.charAt(0).toUpperCase() + spaced.slice(1)
    }
  }
}

/** Where a fact came from, or null: a source the backend does not map shows no line. */
export function provenanceText(t: T, provenance: FactProvenance | null): string | null {
  switch (provenance) {
    case 'you_confirmed':
      return t('navShell.facts.source.youConfirmed')
    case 'you_corrected':
      return t('navShell.facts.source.youCorrected')
    case 'from_chat':
      return t('navShell.facts.source.fromChat')
    case 'found_by_quiet_rabbit':
      return t('navShell.facts.source.found')
    default:
      return null
  }
}

/** The tag shown on a sensitive fact, as text (never colour alone). */
export function sensitivityTag(t: T, sensitivity: string): string | null {
  switch (sensitivity) {
    case 'medical':
      return t('navShell.facts.tag.medical')
    case 'financial':
      return t('navShell.facts.tag.financial')
    default:
      return null
  }
}

export interface SectionNode {
  section: SectionView
  children: SectionView[]
}

/** One level of nesting: each top-level section with all its descendants as one flat
 *  block under it (the list arrives root first, then its descendants). A section
 *  whose parent is absent stands on its own rather than disappearing. */
export function nestSections(sections: SectionView[]): SectionNode[] {
  const nodes: SectionNode[] = []
  const rootOf = new Map<string, SectionNode>()
  for (const section of sections) {
    const node = section.parent_id === null ? undefined : rootOf.get(section.parent_id)
    if (node === undefined) {
      const own: SectionNode = { section, children: [] }
      nodes.push(own)
      rootOf.set(section.id, own)
    } else {
      node.children.push(section)
      rootOf.set(section.id, node)
    }
  }
  return nodes
}

/** How many sections sit anywhere inside `id`. */
export function descendantCount(sections: SectionView[], id: string): number {
  const inside = new Set([id])
  let count = 0
  for (const s of sections) {
    if (s.parent_id !== null && inside.has(s.parent_id)) {
      inside.add(s.id)
      count += 1
    }
  }
  return count
}

/** The sentence for a stop-section confirm. `factCount` is every fact the stop ends,
 *  those of the sections inside included (SectionView.stop_fact_count). */
export function stopSectionBody(t: T, factCount: number, hasInside: boolean): string {
  if (hasInside) {
    if (factCount === 0) return t('navShell.facts.stop.treeBodyNone')
    return factCount === 1
      ? t('navShell.facts.stop.treeBodyOne')
      : t('navShell.facts.stop.treeBodyOther', { count: factCount })
  }
  if (factCount === 0) return t('navShell.facts.stop.sectionBodyNone')
  return factCount === 1
    ? t('navShell.facts.stop.sectionBodyOne')
    : t('navShell.facts.stop.sectionBodyOther', { count: factCount })
}

export function factCountText(t: T, count: number): string {
  return count === 1
    ? t('navShell.facts.countOne')
    : t('navShell.facts.countOther', { count })
}

export function earlierLinkText(t: T, count: number): string {
  return count === 1
    ? t('navShell.facts.earlierOne')
    : t('navShell.facts.earlierOther', { count })
}

/** The sentence for a failed stop or use again. An unknown code reads as the generic
 *  failure, never as a raw string. */
export function writeErrorText(t: T, code: string): string {
  switch (code) {
    case 'newer_value_exists':
      return t('navShell.facts.error.newer')
    case 'fact_not_found':
    case 'section_not_found':
      return t('navShell.facts.error.gone')
    default:
      return t('navShell.facts.error.generic')
  }
}

/** A stored timestamp as the user's own date, or the stored text if unreadable. */
export function shortDate(iso: string, locale?: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime())
    ? iso
    : d.toLocaleDateString(locale, { day: 'numeric', month: 'short', year: 'numeric' })
}

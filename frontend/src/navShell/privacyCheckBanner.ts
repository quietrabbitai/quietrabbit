// items.id=618: whether the nav shell shows its privacy-check banner for the
// Privacy Filter status that get_health reports. Extracted from NavShell.tsx so it can
// be tested React-free on Node's built-in TypeScript support (same precedent
// as keepActiveChatForPersona.ts).
//
// CHAT-BRAND REVIEW: every state shares one placeholder string for now (the
// t() call stays literal in NavShell.tsx so the i18n consistency test sees it). The
// per-state wording (downloading with percent, verifying, unavailable with a
// retry button), severity and placement are Chat-BRAND's; items.id=678 feeds
// the Downloading and Verifying states.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { PrivacyFilterStatus } from '../bindings.ts'

/** False when the check is ready or the status has not arrived yet. */
export function showPrivacyCheckBanner(status: PrivacyFilterStatus | null): boolean {
  if (status === null) return false
  switch (status.kind) {
    case 'ready':
      return false
    case 'downloading':
    case 'verifying':
    case 'unavailable':
      return true
  }
}

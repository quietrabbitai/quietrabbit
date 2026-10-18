// Pure helpers for recovery-phrase words (items.id=769 stage 3). No React; no
// side effects; covered by recoveryWords.test.ts. Used by the new-phrase
// confirmation now and by the restore form (stage 4).
//
// The backend normalises the same way (trim, lowercase, collapse whitespace:
// commands/key_rotation.rs normalise_phrase / normalise_word), so this is for
// the UI's own checks -- word counts and "is every box filled" -- not for
// deciding whether a phrase is right. Nothing here validates against the BIP39
// word list; only the backend knows whether the words open an account.

import type { WordAnswer } from '../bindings.ts'

export const PHRASE_WORD_COUNT = 24

/** Split typed or pasted text into lowercase words. Any run of whitespace
 *  separates words -- spaces, tabs, CR, LF (a CRLF paste from a Windows text
 *  file), and the non-breaking spaces some editors insert. */
export function splitWords(text: string): string[] {
  return text
    .split(/\s+/u)
    .filter((word) => word.length > 0)
    .map((word) => word.toLowerCase())
}

/** One typed word, as the backend will compare it. */
export function normaliseWord(word: string): string {
  return word.trim().toLowerCase()
}

/** True when the text has exactly 24 words made only of letters. Enough to
 *  enable a submit button; the backend decides whether they are a valid
 *  phrase for the account (BIP39 English words are all a-z). */
export function looksLikePhrase(text: string): boolean {
  const words = splitWords(text)
  return words.length === PHRASE_WORD_COUNT && words.every((word) => /^[a-z]+$/u.test(word))
}

/** The phrase re-joined with single spaces, for sending to the backend. */
export function joinWords(text: string): string {
  return splitWords(text).join(' ')
}

/** True once every asked-for position has a non-empty answer. */
export function answersComplete(positions: number[], entered: Record<number, string>): boolean {
  return positions.length > 0 && positions.every((p) => normaliseWord(entered[p] ?? '').length > 0)
}

/** The answers in the order of `positions`, normalised, ready for
 *  confirmNewRecoveryPhrase. */
export function buildAnswers(positions: number[], entered: Record<number, string>): WordAnswer[] {
  return positions.map((position) => ({ position, word: normaliseWord(entered[position] ?? '') }))
}

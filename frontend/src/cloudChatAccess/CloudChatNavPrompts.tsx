// items.id=873 (decisions.id=934): the Cloud Chat navigation prompts.
//
// The Rust pane host cancels a load that is off the provider's declared hosts and
// raises one of three events (cloud_chat_gpu_pane/nav_confirm.rs):
//   cloud-chat-nav-confirm            {request_id, provider_id, host}
//   cloud-chat-nav-confirm-cancelled  {provider_id}
//   cloud-chat-nav-notice             {provider_id, scheme}
// The confirm names the host only. Open in your browser and Cancel answer through
// commands.answerCloudChatNavConfirm with the request id and the decision: the URL
// was kept in Rust, so nothing here supplies what gets opened. Wording is
// placeholder copy until Chat-BRAND (the items.id=853/854 pattern).
//
// Hand-declared payload types, not generated: event payloads, not command args,
// same convention as CloudChatAccessPane.tsx's popup payloads.
import { useEffect, useReducer, useRef } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands } from '../bindings'
import { acquireBlockingOverlay } from '../auth/blockingOverlay'
import { initialNavPromptState, reduceNavPrompt } from './navPromptState'
import './CloudChatNavPrompts.css'

interface NavConfirmPayload {
  request_id: string
  provider_id: string
  host: string
}
interface NavConfirmCancelledPayload {
  provider_id: string
}
interface NavNoticePayload {
  provider_id: string
  scheme: string | null
}

const NOTICE_MS = 8000

export function CloudChatNavPrompts() {
  const { t } = useTranslation()
  const [state, dispatch] = useReducer(reduceNavPrompt, initialNavPromptState)
  const { confirm, notice } = state

  // Tauri listeners detached on unmount (CLAUDE.md): the cancelled/unlisten idiom.
  useEffect(() => {
    const unlisteners: UnlistenFn[] = []
    let cancelled = false
    const keep = (p: Promise<UnlistenFn>) =>
      p.then((unlisten) => {
        if (cancelled) {
          unlisten()
        } else {
          unlisteners.push(unlisten)
        }
      })

    keep(
      listen<NavConfirmPayload>('cloud-chat-nav-confirm', (event) => {
        const { request_id, provider_id, host } = event.payload
        dispatch({
          type: 'confirm',
          confirm: { requestId: request_id, providerId: provider_id, host },
        })
      }),
    )
    keep(
      listen<NavConfirmCancelledPayload>('cloud-chat-nav-confirm-cancelled', (event) => {
        dispatch({ type: 'confirm-cancelled', providerId: event.payload.provider_id })
      }),
    )
    keep(
      listen<NavNoticePayload>('cloud-chat-nav-notice', (event) => {
        dispatch({
          type: 'notice',
          notice: { providerId: event.payload.provider_id, scheme: event.payload.scheme },
        })
      }),
    )

    return () => {
      cancelled = true
      for (const unlisten of unlisteners) unlisten()
    }
  }, [])

  // A native provider pane draws above the webview, so the confirm holds the
  // panes hidden while it is open (blockingOverlay.ts).
  const confirmOpen = confirm !== null
  useEffect(() => {
    if (!confirmOpen) return undefined
    return acquireBlockingOverlay()
  }, [confirmOpen])

  // Focus lands on Cancel when the confirm opens, and Escape cancels it. Held at
  // window level in the capture phase while the confirm is open, like
  // PopupHitLayer's key handling, so no key reaches a page underneath.
  const cancelRef = useRef<HTMLButtonElement>(null)
  const confirmRequestId = confirm?.requestId ?? null
  useEffect(() => {
    if (confirmRequestId === null) return undefined
    cancelRef.current?.focus()
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        dispatch({ type: 'answered', requestId: confirmRequestId })
        void commands.answerCloudChatNavConfirm(confirmRequestId, false)
      }
    }
    window.addEventListener('keydown', onKeyDown, true)
    return () => window.removeEventListener('keydown', onKeyDown, true)
  }, [confirmRequestId])

  useEffect(() => {
    if (notice === null) return undefined
    const timer = window.setTimeout(() => dispatch({ type: 'notice-dismissed' }), NOTICE_MS)
    return () => window.clearTimeout(timer)
  }, [notice])

  const answer = (requestId: string, openInBrowser: boolean) => {
    dispatch({ type: 'answered', requestId })
    void commands.answerCloudChatNavConfirm(requestId, openInBrowser)
  }

  return (
    <>
      {confirm && (
        <div className="cloud-chat-nav-prompt__overlay" role="presentation">
          <dialog
            open
            className="cloud-chat-nav-prompt"
            aria-labelledby="cloud-chat-nav-prompt-title"
            aria-describedby="cloud-chat-nav-prompt-body"
          >
            <h2 id="cloud-chat-nav-prompt-title" className="cloud-chat-nav-prompt__title">
              {t('navShell.cloudChatAccessPane.navConfirmTitle')}
            </h2>
            <p id="cloud-chat-nav-prompt-body" className="cloud-chat-nav-prompt__body">
              {t('navShell.cloudChatAccessPane.navConfirmBody')}{' '}
              <span className="cloud-chat-nav-prompt__host">{confirm.host}</span>
            </p>
            <div className="cloud-chat-nav-prompt__actions">
              <button
                type="button"
                ref={cancelRef}
                className="cloud-chat-nav-prompt__button"
                onClick={() => answer(confirm.requestId, false)}
              >
                {t('navShell.cloudChatAccessPane.navConfirmCancel')}
              </button>
              <button
                type="button"
                className="cloud-chat-nav-prompt__button cloud-chat-nav-prompt__button--primary"
                onClick={() => answer(confirm.requestId, true)}
              >
                {t('navShell.cloudChatAccessPane.navConfirmOpen')}
              </button>
            </div>
          </dialog>
        </div>
      )}
      {notice && (
        <output className="cloud-chat-nav-prompt__notice">
          <span>
            {notice.scheme === null
              ? t('navShell.cloudChatAccessPane.navNoticeMalformed')
              : t('navShell.cloudChatAccessPane.navNoticeScheme', { scheme: notice.scheme })}
          </span>
          <button
            type="button"
            className="cloud-chat-nav-prompt__notice-dismiss"
            onClick={() => dispatch({ type: 'notice-dismissed' })}
          >
            {t('navShell.cloudChatAccessPane.navNoticeDismiss')}
          </button>
        </output>
      )}
    </>
  )
}

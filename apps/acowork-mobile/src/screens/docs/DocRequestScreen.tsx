/**
 * One review request: what the agent wants to write, and the two buttons
 * that decide it (§10).
 *
 * The action set is binary by design — 通过 / 驳回. Inline comment review is
 * a Desktop affordance; on a phone the question is only "accept this or
 * not", and offering a third button that cannot be filled in one-handed
 * produces either an abandoned draft or a note typed into the wrong place.
 *
 * Approving merges `content` over the document at `base_version`. The
 * version is shown because a stale base is the one failure the user can
 * still reason about before accepting.
 */

import { t as tr } from '../../i18n'
import { useCallback, useState } from 'react'
import { useNavStore } from '../../stores/navStore'
import { useDocStore } from '../../stores/docStore'
import { useForegroundRefresh } from '../../lib/foreground'
import { Banner, ListRow } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { formatListTime } from '../../lib/time'

export function DocRequestScreen() {
  const req = useDocStore((s) => s.activeRequest)
  const loading = useDocStore((s) => s.requestsLoading)
  const acting = useDocStore((s) => s.acting)
  const error = useDocStore((s) => s.error)
  const openRequest = useDocStore((s) => s.openRequest)
  const review = useDocStore((s) => s.review)
  const pop = useNavStore((s) => s.pop)

  const [note, setNote] = useState<string | null>(null)

  const reload = useCallback(() => {
    if (req) void openRequest(req.request_id)
  }, [req, openRequest])
  useForegroundRefresh(reload)

  const run = async (approved: boolean) => {
    if (!req) return
    const err = await review(req.request_id, approved)
    if (err) setNote(err)
    else pop()
  }

  if (!req) {
    return (
      <EdgeSwipe onBack={() => pop()}>
        <div className="screen">
          <header className="navbar">
            <button type="button" className="navbar-back" onClick={() => pop()} aria-label={tr('返回')}>{tr('返回')}</button>
            <span className="navbar-title">{tr('审阅')}</span>
            <span className="navbar-trail" />
          </header>
          <div className="empty-state">{loading ? tr('载入中…') : tr('该请求已不在队列中')}</div>
        </div>
      </EdgeSwipe>
    )
  }

  const pending = req.status === 'pending'

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label={tr('返回')}>{tr('返回')}</button>
          <span className="navbar-title">{tr('审阅')}</span>
          <span className="navbar-trail" />
        </header>

        <div className="scroll">
          <ListRow label={tr('文档')} value={req.path} />
          <ListRow label={tr('提交者')} value={req.submitted_by} hint={formatListTime(Date.parse(req.created_at))} />
          <ListRow label={tr('基线版本')} value={`v${req.base_version}`} />
          <ListRow label={tr('状态')} value={req.status} />

          <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) 0' }}>
            {tr('提交内容')}
          </div>
          <pre className="req-diff">{req.content}</pre>

          {error || note ? (
            <div style={{ padding: 'var(--space-2) var(--space-4)' }}>
              <Banner tone={note && !error ? 'info' : 'error'}>{error ?? note}</Banner>
            </div>
          ) : null}

          {pending ? (
            <div className="review-actions">
              <button type="button" className="review-btn reject" disabled={acting} onClick={() => void run(false)}>
                {tr('驳回')}
              </button>
              <button type="button" className="review-btn approve" disabled={acting} onClick={() => void run(true)}>
                {tr('通过')}
              </button>
            </div>
          ) : (
            <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
              <Banner tone="info">{tr('该请求已处理，仅桌面端可查看历史审阅记录')}</Banner>
            </div>
          )}
        </div>
      </div>
    </EdgeSwipe>
  )
}

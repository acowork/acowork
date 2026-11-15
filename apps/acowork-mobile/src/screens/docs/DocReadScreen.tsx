/**
 * Document reader (§10): read-only Markdown.
 *
 * The same renderer as chat bubbles, which is the whole point of keeping it
 * dependency-free — a document and a message are both Markdown text on a
 * narrow screen, and shipping two renderers means two sets of bugs.
 *
 * Editing (TipTap on Desktop) is deliberately absent: v1 mobile is the
 * reading surface. The nav right button offers the only mutation that is
 * safe to do one-handed, which is none — so it is metadata instead.
 */

import { t as tr } from '../../i18n'
import { useCallback } from 'react'
import { useNavStore } from '../../stores/navStore'
import { useDocStore } from '../../stores/docStore'
import { useForegroundRefresh } from '../../lib/foreground'
import { Banner } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { renderMarkdown } from '../../lib/markdown'
import { formatListTime } from '../../lib/time'

export function DocReadScreen() {
  const doc = useDocStore((s) => s.activeDoc)
  const loading = useDocStore((s) => s.docLoading)
  const error = useDocStore((s) => s.error)
  const openDoc = useDocStore((s) => s.openDoc)
  const pop = useNavStore((s) => s.pop)

  const reload = useCallback(() => {
    if (doc) void openDoc(doc.meta.doc_id)
  }, [doc, openDoc])
  useForegroundRefresh(reload)

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label={tr('返回')}>{tr('返回')}</button>
          <span className="navbar-title">{doc?.meta.name ?? tr('文档')}</span>
          <span className="navbar-trail" />
        </header>

        {error ? (
          <div style={{ padding: 'var(--space-2) var(--space-4)' }}>
            <Banner tone="error">{error}</Banner>
          </div>
        ) : null}

        {loading && !doc ? <div className="empty-state">{tr('载入中…')}</div> : null}
        {!loading && !doc ? <div className="empty-state">{tr('文档不存在或已被删除')}</div> : null}

        {doc ? (
          <div className="scroll">
            <div className="doc-meta">
              v{doc.meta.version} · {formatListTime(Date.parse(doc.meta.updated_at))}
              {doc.meta.import ? tr(' · 来自 {{path}}', { path: doc.meta.import.workspace_path }) : ''}
            </div>
            <article className="doc-body">{renderMarkdown(doc.content)}</article>
          </div>
        ) : null}
      </div>
    </EdgeSwipe>
  )
}

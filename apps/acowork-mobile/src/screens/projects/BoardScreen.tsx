/**
 * The board, degraded (§9): the four kanban COLUMNS become a horizontal
 * status chip filter over one list.
 *
 * This is the design's explicit v1 call, not an omission — a drag-and-drop
 * column board is unusable at 390pt, and a horizontally scrolling set of
 * vertical columns is worse. The chips keep the same information (status and
 * how many) with a tap target that actually works on a thumb.
 *
 * Tasks are one flat list from `GET /projects/{pid}/tasks`, which already
 * carries `depth` / `parent_id`, so a subtask is indented from the same
 * payload rather than re-fetched per parent.
 */

import { t as tr } from '../../i18n'
import { useMemo, useState } from 'react'
import { useNavStore } from '../../stores/navStore'
import { usePmStore } from '../../stores/pmStore'
import { ListRow, Banner } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { formatListTime } from '../../lib/time'
import type { PmTask } from '../../lib/api'

const STATUS_LABEL: Record<string, string> = {
  pending: '待处理',
  in_progress: '进行中',
  submitted: '待审核',
  done: '已完成',
  rejected: '已拒绝',
  cancelled: '已取消',
}

const PRIORITY_LABEL: Record<string, string> = { low: '低', normal: '普通', high: '高', urgent: '紧急' }

/** The chip order the Desktop board uses, so the two ends read alike. */
const CHIPS: { id: 'all' | PmTask['status']; label: string }[] = [
  { id: 'all', label: '全部' },
  { id: 'pending', label: '待处理' },
  { id: 'in_progress', label: '进行中' },
  { id: 'submitted', label: '待审核' },
  { id: 'done', label: '已完成' },
  { id: 'rejected', label: '已拒绝' },
]

export function BoardScreen() {
  const pid = usePmStore((s) => s.activeProjectId)
  const project = usePmStore((s) => s.projects.find((p) => p.id === s.activeProjectId))
  const tasks = usePmStore((s) => (pid ? s.tasksByProject[pid] : undefined))
  const loading = usePmStore((s) => s.loadingProject)
  const error = usePmStore((s) => s.error)
  const openTask = usePmStore((s) => s.openTask)
  const pop = useNavStore((s) => s.pop)
  const push = useNavStore((s) => s.push)

  const [chip, setChip] = useState<'all' | PmTask['status']>('all')

  const counts = useMemo(() => {
    const c: Record<string, number> = { all: tasks?.length ?? 0 }
    for (const t of tasks ?? []) c[t.status] = (c[t.status] ?? 0) + 1
    return c
  }, [tasks])

  const visible = useMemo(() => (tasks ?? []).filter((t) => chip === 'all' || t.status === chip), [tasks, chip])

  const open = (t: PmTask) => {
    void openTask(t.id)
    push('projects/task')
  }

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label={tr('返回')}>
            {tr('返回')}
          </button>
          <span className="navbar-title">{project?.title ?? tr('看板')}</span>
          <span className="navbar-trail" />
        </header>

        <div className="board-nav" role="tablist" aria-label={tr('状态筛选')}>
          {CHIPS.map((c) => (
            <button
              key={c.id}
              type="button"
              role="tab"
              aria-selected={chip === c.id}
              className={`board-chip${chip === c.id ? ' is-active' : ''}`}
              onClick={() => setChip(c.id)}
            >
              {tr(c.label)} <span className="board-chip-n">{counts[c.id] ?? 0}</span>
            </button>
          ))}
        </div>

        {error ? (
          <div style={{ padding: 'var(--space-2) var(--space-4)' }}>
            <Banner tone="error">{error}</Banner>
          </div>
        ) : null}

        <div className="scroll">
          {loading && !tasks ? <ListRow label={tr('载入中…')} /> : null}
          {visible.map((t) => (
            <ListRow
              key={t.id}
              arrow
              onClick={() => open(t)}
              label={
                <span className="row-title">
                  {t.depth > 0 ? <span className="task-indent">└ </span> : null}
                  {t.title}
                  {t.is_blocked ? <span className="badge">{tr('被阻塞')}</span> : null}
                  {t.review_status === 'pending' ? <span className="badge">{tr('需审核')}</span> : null}
                </span>
              }
              hint={`${tr(STATUS_LABEL[t.status] ?? t.status)} · ${tr(PRIORITY_LABEL[t.priority] ?? t.priority)}${
                t.assignee ? ` · ${assigneeLabel(t.assignee)}` : ''
              }`}
              value={formatListTime(Date.parse(t.due_at ?? t.updated_at))}
            />
          ))}
          {tasks && visible.length === 0 ? <ListRow label={tr('该筛选下暂无任务')} /> : null}
        </div>
      </div>
    </EdgeSwipe>
  )
}

/**
 * `assignee` is a human string or an agent INSTANCE id (ADR-073). An
 * instance id is unreadable on a phone, so it is shortened rather than
 * resolved — resolving it would need a directory join the list endpoint
 * does not do.
 */
function assigneeLabel(a: string): string {
  if (a === 'human') return tr('我')
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(a) ? a.slice(0, 8) : a
}

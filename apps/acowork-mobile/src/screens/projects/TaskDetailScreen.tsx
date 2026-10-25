/**
 * Task detail + 任务流转 (§9).
 *
 * The board is read-only, but a human on a phone CAN still move a task:
 * claim it, submit a result, approve or reject a pending review. Those are
 * the three transitions that need no keyboard and no file browser, which is
 * exactly the set the design keeps (§2.1 "只读看板 + 任务流转").
 *
 * Which buttons appear is derived from the SERVER's state, never from a
 * guess about the user: `status` decides claim/submit, and
 * `review_status === 'pending'` decides review. A task that is not in
 * `in_progress` cannot be submitted, so the button is absent rather than
 * disabled-and-confusing.
 */

import { useCallback, useEffect, useState } from 'react'
import { useNavStore } from '../../stores/navStore'
import { usePmStore } from '../../stores/pmStore'
import { useForegroundRefresh } from '../../lib/foreground'
import { Banner, ListRow } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { renderMarkdown } from '../../lib/markdown'

const STATUS_LABEL: Record<string, string> = {
  pending: '待处理',
  in_progress: '进行中',
  submitted: '已提交待审核',
  done: '已完成',
  rejected: '已拒绝',
  cancelled: '已取消',
}

const PRIORITY_LABEL: Record<string, string> = { low: '低', normal: '普通', high: '高', urgent: '紧急' }

const TYPE_LABEL: Record<string, string> = {
  task: '任务',
  bug: '缺陷',
  feature: '功能',
  chore: '杂务',
  checkpoint: '检查点',
  milestone: '里程碑',
}

export function TaskDetailScreen() {
  const task = usePmStore((s) => s.activeTask)
  const acting = usePmStore((s) => s.acting)
  const error = usePmStore((s) => s.error)
  const openTask = usePmStore((s) => s.openTask)
  const claim = usePmStore((s) => s.claim)
  const submit = usePmStore((s) => s.submit)
  const review = usePmStore((s) => s.review)
  const pop = useNavStore((s) => s.pop)

  const [note, setNote] = useState<string | null>(null)
  const [submitText, setSubmitText] = useState('')
  const [submitOpen, setSubmitOpen] = useState(false)

  const reload = useCallback(() => {
    if (task) void openTask(task.id)
  }, [task, openTask])
  useForegroundRefresh(reload)

  useEffect(() => setNote(null), [task?.id])

  const run = async (fn: () => Promise<string | null>) => {
    const err = await fn()
    setNote(err ?? '已完成')
  }

  if (!task) {
    return (
      <EdgeSwipe onBack={() => pop()}>
        <div className="screen">
          <header className="navbar">
            <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
            <span className="navbar-title">任务</span>
            <span className="navbar-trail" />
          </header>
          <div className="empty-state">任务不存在或已被删除</div>
        </div>
      </EdgeSwipe>
    )
  }

  const canClaim = task.status === 'pending'
  const canSubmit = task.status === 'in_progress'
  const canReview = task.review_status === 'pending'

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
          <span className="navbar-title">{task.id}</span>
          <span className="navbar-trail" />
        </header>

        <div className="scroll">
          <div className="list-section" role="group" aria-label="任务">
            <div className="task-title">{task.title}</div>
            <ListRow label="状态" value={STATUS_LABEL[task.status] ?? task.status} />
            <ListRow label="优先级" value={PRIORITY_LABEL[task.priority] ?? task.priority} />
            <ListRow label="类型" value={TYPE_LABEL[task.type] ?? task.type} />
            <ListRow label="负责人" value={task.assignee ?? '未指派'} />
            {task.start_at ? <ListRow label="开始" value={new Date(task.start_at).toLocaleDateString('zh-CN')} /> : null}
            {task.due_at ? <ListRow label="截止" value={new Date(task.due_at).toLocaleDateString('zh-CN')} /> : null}
            {task.is_blocked ? <ListRow label="被阻塞于" value={(task.blocked_by ?? []).join('、')} /> : null}
          </div>

          {task.description ? (
            <div className="list-section" role="group" aria-label="描述">
              <div className="list-section-title">描述</div>
              <div className="task-desc">{renderMarkdown(task.description)}</div>
            </div>
          ) : null}

          {task.result ? (
            <div className="list-section" role="group" aria-label="提交结果">
              <div className="list-section-title">提交结果</div>
              <div className="task-desc">{renderMarkdown(task.result.text)}</div>
            </div>
          ) : null}

          {error || note ? (
            <div style={{ padding: 'var(--space-2) var(--space-4)' }}>
              <Banner tone={note && !error ? 'info' : 'error'}>{error ?? note}</Banner>
            </div>
          ) : null}

          {canClaim || canSubmit || canReview ? (
            <div className="list-section" role="group" aria-label="流转">
              <div className="list-section-title">流转</div>
              {canClaim ? (
                <ListRow
                  disabled={acting}
                  label={<span className="row-title tint">认领此任务</span>}
                  onClick={() => void run(() => claim(task.id))}
                />
              ) : null}
              {canSubmit ? (
                <>
                  <ListRow
                    disabled={acting}
                    label={<span className="row-title tint">提交结果</span>}
                    onClick={() => setSubmitOpen((v) => !v)}
                  />
                  {submitOpen ? (
                    <div style={{ padding: 'var(--space-2) var(--space-4)' }}>
                      <textarea
                        className="composer-input"
                        rows={3}
                        placeholder="结果说明"
                        aria-label="结果说明"
                        value={submitText}
                        onChange={(e) => setSubmitText(e.target.value)}
                      />
                      <button
                        type="button"
                        className="retry-link"
                        style={{ color: 'var(--color-tint)' }}
                        disabled={!submitText.trim() || acting}
                        onClick={() => {
                          void run(async () => {
                            const err = await submit(task.id, submitText.trim())
                            if (!err) {
                              setSubmitOpen(false)
                              setSubmitText('')
                            }
                            return err
                          })
                        }}
                      >
                        提交
                      </button>
                    </div>
                  ) : null}
                </>
              ) : null}
              {canReview ? (
                <div className="review-actions">
                  <button type="button" className="review-btn reject" disabled={acting} onClick={() => void run(() => review(task.id, false))}>
                    驳回
                  </button>
                  <button type="button" className="review-btn approve" disabled={acting} onClick={() => void run(() => review(task.id, true))}>
                    通过
                  </button>
                </div>
              ) : null}
            </div>
          ) : null}

          <div className="list-section" role="group" aria-label="编辑">
            <ListRow label="编辑任务" hint="移动端 v1 不提供，请在桌面端" value={<span className="badge">桌面端</span>} />
          </div>
        </div>
      </div>
    </EdgeSwipe>
  )
}

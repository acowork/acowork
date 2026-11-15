/**
 * Projects tab state (design §9).
 *
 * The board is READ-ONLY by design (§2.1): no drag-to-reorder, no create, no
 * edit — those stay on Desktop. What mobile DOES expose is 任务流转,
 * claim/submit/review, because those are one-tap decisions a human makes
 * away from the keyboard, which is the entire point of having a phone app.
 *
 * Tasks are cached per project. `TaskResponse` already carries `depth` and
 * `parent_id`, so the tree is rebuilt from the one list call instead of
 * walking `/tasks/{tid}/children` recursively — N+1 on a phone network.
 */

import { t as tr } from '../i18n'
import { create } from 'zustand'
import {
  fetchProjectTasks,
  fetchProjects,
  fetchTask,
  pmClaimTask,
  pmReviewTask,
  pmSubmitTask,
  type PmProject,
  type PmTask,
} from '../lib/api'

interface PmStore {
  projects: PmProject[]
  projectsLoaded: boolean
  /** Tasks per project, as one flat list (the tree is derived in the view). */
  tasksByProject: Record<string, PmTask[]>
  activeProjectId: string | null
  activeTask: PmTask | null
  loadingProject: boolean
  /** A transition is in flight — the buttons must not double-fire. */
  acting: boolean
  error: string | null

  refreshProjects(): Promise<void>
  openProject(projectId: string): Promise<void>
  openTask(taskId: string): Promise<void>
  /** claim / submit / review; returns the message to show on failure. */
  claim(taskId: string): Promise<string | null>
  submit(taskId: string, text: string): Promise<string | null>
  review(taskId: string, approved: boolean): Promise<string | null>
}

/**
 * A transition returns the fresh `Task`; fold it back into the cache.
 *
 * It must be a MERGE, not a replace. claim/submit/review answer a bare
 * `Task`, while the list endpoint answers `TaskResponse` = `Task` + the
 * derived `depth` / `is_blocked` / `blocked_by`. Replacing wholesale drops
 * those three, so a blocked task loses its 被阻塞于 row the moment it is
 * touched — the badge would vanish and reappear only on a refetch.
 */
export function mergeTask(state: PmStore['tasksByProject'], tid: string, next: PmTask): PmStore['tasksByProject'] {
  const out: PmStore['tasksByProject'] = {}
  for (const [pid, list] of Object.entries(state)) {
    out[pid] = list.some((t) => t.id === tid) ? list.map((t) => (t.id === tid ? { ...t, ...next } : t)) : list
  }
  return out
}

/** Zustand's setter, narrowed to what `act` actually needs. */
type SetFn = (partial: Partial<PmStore> | ((s: PmStore) => Partial<PmStore>)) => void

/** Run a transition, mapping a rejection into a readable line, not a throw. */
async function act(fn: () => Promise<PmTask>, set: SetFn, tid: string): Promise<string | null> {
  set({ acting: true })
  try {
    const next = await fn()
    set((s) => ({
      tasksByProject: mergeTask(s.tasksByProject, tid, next),
      activeTask: s.activeTask?.id === tid ? { ...s.activeTask, ...next } : s.activeTask,
    }))
    return null
  } catch (e) {
    return e instanceof Error ? e.message : tr('操作失败')
  } finally {
    set({ acting: false })
  }
}

export const usePmStore = create<PmStore>((set, get) => ({
  projects: [],
  projectsLoaded: false,
  tasksByProject: {},
  activeProjectId: null,
  activeTask: null,
  loadingProject: false,
  acting: false,
  error: null,

  refreshProjects: async () => {
    try {
      set({ projects: await fetchProjects(), projectsLoaded: true, error: null })
    } catch (e) {
      set({ error: e instanceof Error ? e.message : tr('项目列表加载失败') })
    }
  },

  openProject: async (projectId) => {
    set({ activeProjectId: projectId, loadingProject: true, error: null })
    try {
      const tasks = await fetchProjectTasks(projectId)
      // Guard the switch race: a slow response for project A must not paint
      // over project B if the user moved on.
      if (get().activeProjectId === projectId) set((s) => ({ tasksByProject: { ...s.tasksByProject, [projectId]: tasks } }))
    } catch (e) {
      set({ error: e instanceof Error ? e.message : tr('任务加载失败') })
    } finally {
      set({ loadingProject: false })
    }
  },

  openTask: async (taskId) => {
    try {
      set({ activeTask: await fetchTask(taskId), error: null })
    } catch (e) {
      set({ error: e instanceof Error ? e.message : tr('任务加载失败') })
    }
  },

  claim: (tid) => act(() => pmClaimTask(tid), set, tid),
  submit: (tid, text) => act(() => pmSubmitTask(tid, text), set, tid),
  review: (tid, approved) => act(() => pmReviewTask(tid, approved), set, tid),
}))

/**
 * Doc library store (§10).
 *
 * The service returns ONE tree level per call (`GET /tree?dir_id=`), never a
 * recursive tree. The prototype shows an expandable tree, so the store
 * lazily fetches a directory's children the first time it is expanded and
 * caches them — same visual, one request per directory actually opened.
 *
 * `flatten()` is exported separately because it is the whole rendering
 * contract: given the cache and the set of open directories, what rows
 * appear and at what indent. Keeping it pure means it can be tested without
 * a gateway, which matters more than testing the fetch.
 */

import { t as tr } from '../i18n'
import { create } from 'zustand'
import {
  approveDocRequest,
  fetchDoc,
  fetchDocRequest,
  fetchDocRequests,
  fetchDocTree,
  rejectDocRequest,
  searchDocs,
} from '../lib/api'
import type {
  DocContent,
  DocMeta,
  DocSearchHit,
  DocTreeNode,
  DocUpdateRequest,
  DirMeta,
} from '../lib/api'
import { useAuthStore } from './authStore'

/** What one directory contains — the part of `DocTreeNode` we cache. */
export interface DirLevel {
  files: DocMeta[]
  dirs: DirMeta[]
}

export type FlatRow =
  | { kind: 'dir'; key: string; dir_id: string; name: string; depth: number; open: boolean; loading: boolean; count: number | null }
  | { kind: 'doc'; key: string; doc_id: string; name: string; depth: number; updated_at: string }

/** Root dir id is the empty string, which means "no dir_id parameter". */
export const ROOT_ID = ''

export function levelKey(dirId: string): string {
  return dirId || ROOT_ID
}

/**
 * Depth-first flatten of the cached tree, honouring the open set. A
 * directory whose children are not cached yet still renders (and renders
 * empty underneath) — the row is what triggers the fetch, so it must never
 * depend on the fetch having already happened.
 */
export function flatten(
  levels: Record<string, DirLevel>,
  open: ReadonlySet<string>,
  loading: ReadonlySet<string> = new Set(),
): FlatRow[] {
  const out: FlatRow[] = []
  const walk = (dirId: string, depth: number) => {
    const level = levels[levelKey(dirId)]
    if (!level) return
    for (const d of level.dirs) {
      const isOpen = open.has(d.dir_id)
      out.push({
        kind: 'dir',
        key: `d:${d.dir_id}`,
        dir_id: d.dir_id,
        name: d.name,
        depth,
        open: isOpen,
        loading: loading.has(d.dir_id),
        count: levels[d.dir_id] ? levels[d.dir_id]!.files.length + levels[d.dir_id]!.dirs.length : null,
      })
      if (isOpen) walk(d.dir_id, depth + 1)
    }
    for (const f of level.files) {
      out.push({ kind: 'doc', key: `f:${f.doc_id}`, doc_id: f.doc_id, name: f.name, depth, updated_at: f.updated_at })
    }
  }
  walk(ROOT_ID, 0)
  return out
}

interface DocState {
  levels: Record<string, DirLevel>
  open: Set<string>
  loadingDirs: Set<string>
  rootLoading: boolean
  query: string
  hits: DocSearchHit[] | null
  searching: boolean
  error: string | null

  activeDoc: DocContent | null
  docLoading: boolean

  requests: DocUpdateRequest[] | null
  requestsLoading: boolean
  activeRequest: DocUpdateRequest | null
  acting: boolean

  refresh: () => Promise<void>
  /** Expand or collapse a directory, fetching its children on first open. */
  toggle: (dirId: string) => Promise<void>
  runSearch: (q: string) => Promise<void>
  clearSearch: () => void

  openDoc: (docId: string) => Promise<void>
  refreshRequests: () => Promise<void>
  openRequest: (id: string) => Promise<void>
  review: (id: string, approved: boolean) => Promise<string | null>
}

function toLevel(t: DocTreeNode): DirLevel {
  return { files: t.files.filter((f) => !f.deleted), dirs: t.dirs.filter((d) => !d.deleted) }
}

export const useDocStore = create<DocState>((set, get) => ({
  levels: {},
  open: new Set(),
  loadingDirs: new Set(),
  rootLoading: false,
  query: '',
  hits: null,
  searching: false,
  error: null,

  activeDoc: null,
  docLoading: false,

  requests: null,
  requestsLoading: false,
  activeRequest: null,
  acting: false,

  async refresh() {
    set({ rootLoading: true, error: null })
    try {
      const root = await fetchDocTree()
      set((s) => ({ levels: { ...s.levels, [ROOT_ID]: toLevel(root) }, rootLoading: false }))
    } catch (e) {
      set({ rootLoading: false, error: (e as Error).message || tr('文档库加载失败') })
    }
  },

  async toggle(dirId) {
    const open = new Set(get().open)
    if (open.has(dirId)) {
      open.delete(dirId)
      set({ open })
      return
    }
    open.add(dirId)
    set({ open })
    if (get().levels[dirId]) return
    set((s) => ({ loadingDirs: new Set(s.loadingDirs).add(dirId) }))
    try {
      const level = toLevel(await fetchDocTree(dirId))
      set((s) => ({ levels: { ...s.levels, [dirId]: level }, loadingDirs: del(s.loadingDirs, dirId) }))
    } catch (e) {
      // Collapse it back: an open directory that never fills is a spinner
      // that never ends, and the user cannot tell the difference from slow.
      set((s) => ({
        open: del(s.open, dirId),
        loadingDirs: del(s.loadingDirs, dirId),
        error: (e as Error).message || tr('目录加载失败'),
      }))
    }
  },

  async runSearch(q) {
    set({ query: q })
    if (!q.trim()) {
      set({ hits: null })
      return
    }
    set({ searching: true })
    try {
      const hits = await searchDocs(q.trim())
      // A response to a query the user has since retyped is dropped, not
      // rendered — the list would otherwise flicker back to stale results.
      if (get().query === q) set({ hits, searching: false })
      else set({ searching: false })
    } catch (e) {
      set({ searching: false, error: (e as Error).message || tr('搜索失败') })
    }
  },

  clearSearch() {
    set({ query: '', hits: null })
  },

  async openDoc(docId) {
    set({ docLoading: true, activeDoc: null, error: null })
    try {
      set({ activeDoc: await fetchDoc(docId), docLoading: false })
    } catch (e) {
      set({ docLoading: false, error: (e as Error).message || tr('文档加载失败') })
    }
  },

  async refreshRequests() {
    set({ requestsLoading: true, error: null })
    try {
      set({ requests: await fetchDocRequests('pending'), requestsLoading: false })
    } catch (e) {
      set({ requestsLoading: false, error: (e as Error).message || tr('审阅队列加载失败') })
    }
  },

  async openRequest(id) {
    set({ activeRequest: null, error: null })
    try {
      set({ activeRequest: await fetchDocRequest(id) })
    } catch (e) {
      set({ error: (e as Error).message || tr('审阅详情加载失败') })
    }
  },

  async review(id, approved) {
    const me = useAuthStore.getState().me?.user_id
    // The reviewer identity is recorded as who accepted the change, so it
    // must be this account. An agent that submitted a request cannot be the
    // one approving it — that separation is the entire reason the queue
    // exists instead of letting the agent write the document directly.
    if (!me) return tr('未登录，无法审阅')
    set({ acting: true, error: null })
    try {
      // Approve wraps (`{request, doc_version}`), reject does not — see api.ts.
      const next = approved ? (await approveDocRequest(id, me)).request : await rejectDocRequest(id, me)
      set({
        acting: false,
        activeRequest: next,
        requests: (get().requests ?? []).filter((r) => r.request_id !== id),
      })
      return null
    } catch (e) {
      set({ acting: false, error: (e as Error).message || tr('审阅失败') })
      return e instanceof Error ? e.message : tr('审阅失败')
    }
  },
}))

function del<T>(s: Set<T>, v: T): Set<T> {
  const n = new Set(s)
  n.delete(v)
  return n
}
/**
 * The pure parts of the Docs and Settings tabs: the tree flatten (what rows
 * appear given a cache and an open set) and appearance resolution.
 *
 * Both are the failure-prone half of those screens — the fetch is a
 * one-liner, but a tree that renders a directory inside itself, or a theme
 * that follows the OS when the user explicitly chose light, is silent
 * corruption the eye catches only after it is shipped.
 */

import { beforeEach, describe, expect, it } from 'vitest'
import { flatten, ROOT_ID, type DirLevel, type FlatRow } from '../stores/docStore'
import { ACCENTS, applyAppearance, darken, ACCENT_STRONG, loadAccentId, loadThemePref, resolveTheme } from '../lib/theme'
import { mergeTask } from '../stores/pmStore'
import type { PmTask } from '../lib/api'

const doc = (id: string, name: string) => ({
  doc_id: id,
  name,
  version: 1,
  created_at: '2025-01-01T00:00:00Z',
  updated_at: '2025-01-01T00:00:00Z',
  deleted: false,
})
const dir = (id: string, name: string) => ({ dir_id: id, name, updated_at: '2025-01-01T00:00:00Z', deleted: false })

/** Narrow to a directory row: `count` and `loading` only exist on one arm of
 *  the union, and asserting on the wrong arm is a test that silently checks
 *  nothing. */
function dirRow(rows: FlatRow[], name: string) {
  const r = rows.find((x) => x.kind === 'dir' && x.name === name)
  if (!r || r.kind !== 'dir') throw new Error(`no directory row named ${name}`)
  return r
}

describe('doc tree flatten', () => {
  const root: DirLevel = { files: [doc('f0', '根文档')], dirs: [dir('d1', '设计'), dir('d2', '未缓存')] }

  it('renders root files and directories at depth 0, directories first', () => {
    const rows = flatten({ [ROOT_ID]: root }, new Set())
    expect(rows.map((r) => r.name)).toEqual(['设计', '未缓存', '根文档'])
    expect(rows.every((r) => r.depth === 0)).toBe(true)
  })

  it('splices an open directory children in directly after its row', () => {
    const rows = flatten(
      { [ROOT_ID]: root, d1: { files: [doc('f1', '子文档')], dirs: [dir('d1a', '孙目录')] } },
      new Set(['d1']),
    )
    expect(rows.map((r) => `${r.depth}:${r.name}`)).toEqual([
      '0:设计',
      '1:孙目录',
      '1:子文档',
      '0:未缓存',
      '0:根文档',
    ])
  })

  it('counts children only once a directory has been fetched', () => {
    // A count of 0 and no count mean different things: "this directory is
    // empty" versus "nobody has looked yet". Rendering 0 before the fetch
    // would tell the user their library is empty.
    const rows = flatten({ [ROOT_ID]: root }, new Set())
    expect(dirRow(rows, '未缓存').count).toBeNull()
    const fetched = flatten({ [ROOT_ID]: root, d1: { files: [], dirs: [] } }, new Set())
    expect(dirRow(fetched, '设计').count).toBe(0)
  })

  it('marks the row loading so the caret is not the only feedback', () => {
    const rows = flatten({ [ROOT_ID]: root }, new Set(['d2']), new Set(['d2']))
    expect(dirRow(rows, '未缓存').loading).toBe(true)
  })

  it('cannot recurse into itself: an open set naming the root adds nothing', () => {
    const rows = flatten({ [ROOT_ID]: root }, new Set([ROOT_ID]))
    expect(rows.length).toBe(3)
  })

  it('hides children of a closed directory even when cached', () => {
    const levels = { [ROOT_ID]: root, d1: { files: [doc('f1', '子文档')], dirs: [] } }
    expect(flatten(levels, new Set()).some((r) => r.name === '子文档')).toBe(false)
    expect(flatten(levels, new Set(['d1'])).some((r) => r.name === '子文档')).toBe(true)
  })
})

describe('appearance', () => {
  beforeEach(() => {
    localStorage.clear()
    document.documentElement.removeAttribute('data-theme')
    document.documentElement.removeAttribute('style')
  })

  it('treats an unset preference as 跟随系统', () => {
    expect(loadThemePref()).toBe('system')
    expect(loadAccentId()).toBe('blue')
  })

  it('falls back to the default when storage holds a value it no longer offers', () => {
    localStorage.setItem('acowork.theme', 'sepia')
    localStorage.setItem('acowork.accent', 'chartreuse')
    expect(loadThemePref()).toBe('system')
    expect(loadAccentId()).toBe('blue')
  })

  it('an explicit choice beats the OS', () => {
    // jsdom has no matchMedia, so systemDark() is false: light is the
    // system answer here, and the explicit value must win over it.
    expect(resolveTheme('dark')).toBe('dark')
    expect(resolveTheme('light')).toBe('light')
    expect(resolveTheme('system')).toBe('light')
  })

  it('applies the theme and the accent of the resolved theme', () => {
    applyAppearance('dark', 'pink')
    expect(document.documentElement.dataset.theme).toBe('dark')
    const pink = ACCENTS.find((a) => a.id === 'pink')!
    expect(document.documentElement.style.getPropertyValue('--color-tint')).toBe(pink.dark)
    // Two derived roles from one accent: the highlight colour stays the raw
    // accent (it must match the settings swatch), while filled surfaces under
    // white text use the darkened variant.
    const strong = darken(pink.dark, ACCENT_STRONG)
    expect(document.documentElement.style.getPropertyValue('--color-tint-strong')).toBe(strong)
  })

  it('uses the light accent when the light theme is forced', () => {
    applyAppearance('light', 'orange')
    expect(document.documentElement.style.getPropertyValue('--color-tint')).toBe(
      ACCENTS.find((a) => a.id === 'orange')!.light,
    )
  })

  it('every accent, in both themes, keeps white text on filled accent surfaces at WCAG AA', () => {
    // The point of darkening is legibility, so assert the ratio itself: a
    // weakened ACCENT_STRONG or a revert to raw accents fails here.
    const lum = (hex: string) => {
      const [r = 0, g = 0, b = 0] = [1, 3, 5]
        .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
        .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
      return 0.2126 * r + 0.7152 * g + 0.0722 * b
    }
    const vsWhite = (hex: string) => 1.05 / (lum(hex) + 0.05)
    for (const a of ACCENTS) {
      for (const shade of [a.light, a.dark]) {
        const bg = darken(shade, ACCENT_STRONG)
        // Assert on the ratio so the failure names the accent that regressed.
        expect(vsWhite(bg), `${a.id} ${shade} -> ${bg} vs #ffffff`).toBeGreaterThanOrEqual(4.5)
      }
    }
  })

  it('an unknown accent id degrades to the default instead of throwing', () => {
    applyAppearance('system', 'no-such-colour')
    expect(document.documentElement.style.getPropertyValue('--color-tint')).toBe(ACCENTS[0]!.light)
  })
})

describe('pm task merge', () => {
  const listed = {
    id: 't-1',
    project_id: 'p-1',
    title: '同步文档',
    type: 'task',
    status: 'pending',
    review_status: 'not_required',
    priority: 'normal',
    created_by: 'human',
    created_at: '2025-01-01T00:00:00Z',
    updated_at: '2025-01-01T00:00:00Z',
    depth: 2,
    is_blocked: true,
    blocked_by: ['t-0'],
  } as unknown as PmTask

  it('keeps the derived board fields a transition response omits', () => {
    // claim/submit/review answer a bare Task: no depth, no is_blocked.
    const { depth: _depth, is_blocked: _blocked, blocked_by: _by, ...taskOnly } = listed
    const bare = { ...taskOnly, status: 'in_progress' } as PmTask

    const row = mergeTask({ 'p-1': [listed] }, 't-1', bare)['p-1']![0]!
    expect(row.status).toBe('in_progress')
    expect(row.depth).toBe(2)
    expect(row.is_blocked).toBe(true)
    expect(row.blocked_by).toEqual(['t-0'])
  })

  it('leaves other projects untouched', () => {
    const bare = { ...listed, status: 'done' } as PmTask
    const merged = mergeTask({ 'p-1': [listed], 'p-2': [{ ...listed, id: 't-9' }] }, 't-1', bare)
    expect(merged['p-2']![0]!.status).toBe('pending')
    expect(merged['p-1']![0]!.status).toBe('done')
  })
})

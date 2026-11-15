/**
 * ProjectIcon — the mobile twin of Desktop's `ProjectAvatar` rendering
 * priority: custom bytes → builtin bundle → default project icon.
 *
 * `renderToStaticMarkup` is enough here: the branch is chosen synchronously
 * from `metadata`, and the only async part (`AuthImage`'s authenticated fetch)
 * is exactly what must NOT paint a broken image before the bytes arrive.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { ProjectIcon } from '../components/ProjectIcon'
import { BUILTIN_ICONS } from '../lib/builtinIcons'
import { DEFAULT_PROJECT_ICON } from '../lib/projectIcon'
import type { PmProject } from '../lib/api'

function project(metadata?: unknown): PmProject {
  return {
    id: 'p-1',
    title: 'acowork',
    description: '',
    status: 'active',
    created_by: 'u1',
    created_at: '2026-09-01T00:00:00Z',
    updated_at: '2026-10-01T00:00:00Z',
    ...(metadata === undefined ? {} : { metadata }),
  } as PmProject
}

const markup = (metadata?: unknown) =>
  renderToStaticMarkup(<ProjectIcon project={project(metadata)} baseUrl="http://gw.test" size={22} />)

describe('ProjectIcon', () => {
  it('paints a bundled builtin icon by id', () => {
    const html = markup({ icon: { kind: 'builtin', id: 'icon-01' } })
    expect(html).toContain(BUILTIN_ICONS['icon-01'])
    expect(html).toContain('project-icon')
    expect(html).toContain('width:22px')
  })

  it('falls back to the default icon when no icon is set', () => {
    expect(markup()).toContain(DEFAULT_PROJECT_ICON)
    expect(markup({})).toContain(DEFAULT_PROJECT_ICON)
  })

  it('falls back to the default icon for an unknown or malformed ref', () => {
    expect(markup({ icon: { kind: 'builtin', id: 'icon-99' } })).toContain(DEFAULT_PROJECT_ICON)
    expect(markup({ icon: 'icon-01' })).toContain(DEFAULT_PROJECT_ICON)
  })

  it('shows the default icon, not a bare <img>, while custom bytes load', () => {
    // AuthImage fetches the authed icon-file URL in an effect; before it
    // resolves there is no blob URL, so painting the default tile is the
    // contract (a raw <img src> would 401 and show a broken image).
    expect(markup({ icon: { kind: 'custom', path: 'projects/p-1/icon/icon-01.png' } }))
      .toContain(DEFAULT_PROJECT_ICON)
  })
})

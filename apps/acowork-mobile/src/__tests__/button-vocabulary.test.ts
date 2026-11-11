/**
 * The mobile button vocabulary is closed — the same contract as the desktop
 * app's `styles/buttonVariants.test.ts`: solid fills only (`btn-solid`,
 * `btn-accent`), no bordered tone may creep back in.
 *
 * Why this exists here too: the mobile `.btn` default *was* an accent
 * outline, and its comment claimed desktop parity — the parity had drifted
 * the other way and desktop had already deleted `btn-outline`. Nothing
 * renders a stylesheet in jsdom, so the class name itself is the spec.
 *
 * `?raw` is NOT used for the stylesheets: vitest stubs CSS (even raw) in
 * this project's jsdom environment, so the files are read from disk —
 * vitest's runtime is Node; jsdom is only the DOM.
 */
import { describe, expect, it } from 'vitest'
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve, join } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const SRC = resolve(__dirname, '..')

const VARIANTS = ['solid', 'accent'] as const

function tsxFiles(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry)
    const st = statSync(p)
    if (st.isDirectory()) {
      if (entry !== '__tests__' && entry !== 'node_modules') tsxFiles(p, out)
    } else if (entry.endsWith('.tsx')) out.push(p)
  }
  return out
}

describe('closed button vocabulary (desktop parity)', () => {
  const appCss = readFileSync(resolve(SRC, 'styles/app.css'), 'utf8')
  const mobileCss = readFileSync(resolve(SRC, 'styles/mobile.css'), 'utf8')

  it('the .btn base rule carries no border', () => {
    const base = appCss.match(/\.btn \{[^}]*\}/)
    expect(base, '.btn base rule exists').not.toBeNull()
    expect(base![0]).toContain('border: none')
    expect(base![0]).not.toMatch(/border: 1px/)
  })

  it('no deleted tone is re-registered in either stylesheet', () => {
    for (const dead of ['btn-filled', 'btn-outline']) {
      for (const sheet of [appCss, mobileCss]) {
        expect(sheet).not.toMatch(new RegExp(`\\.${dead}\\s*\\{`))
      }
    }
  })

  it('call sites use only the registered tones (plus the size modifier)', () => {
    const files = tsxFiles(SRC)
    expect(files.length).toBeGreaterThan(10)
    const allowed = (VARIANTS as readonly string[]).concat(['sm'])
    for (const file of files) {
      const src = readFileSync(file, 'utf8')
      for (const tone of [...src.matchAll(/btn-(\w+)/g)].map((m) => m[1]!)) {
        expect(allowed, `${file} uses btn-${tone}`).toContain(tone)
      }
    }
  })
})

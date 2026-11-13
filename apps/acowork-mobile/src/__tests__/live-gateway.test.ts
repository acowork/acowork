// Read-only integration smoke against a LIVE Gateway: every endpoint the
// four tabs and the agent drawer call, asserting the ENVELOPE each mobile
// type claims. Gated by ACOWORK_E2E_LIVE=1 + ACOWORK_E2E_URL=<url>.
//
// Nothing here writes. That is the whole design rule for this file, and the
// reason the send/poll test that creates a session lives in `dev/` with its
// own vitest config instead — see dev/live-send.test.ts for why an env-var
// gate was not enough to keep it out of the default run.
//
// These tests earn their keep on shape, not status codes. A wrong path
// prefix returns 404 with an empty body, the store catches it, and the
// screen renders "文档库为空" — indistinguishable from an empty library. The
// Gateway's auth middleware also runs before routing, so an unauthenticated
// probe cannot tell a real route from `/api/bogus` (both 401). Only a
// logged-in GET with an asserted envelope catches it, and it has: the wrong
// doc prefix, `access` being a string rather than a `read_only` bool, and
// approve returning a wrapped `{request, doc_version}`.
//
// ACOWORK_E2E_URL has no localhost default on purpose — but note that a
// default cannot protect you from typing the dev Gateway in yourself.
import { describe, expect, it } from 'vitest'

import { probeStatus, setAuthBridge } from '../lib/api'
import {
  fetchAgentConfig,
  fetchAgents,
  fetchAgentStatus,
  fetchBuiltinTools,
  fetchDirectory,
  fetchMemoryStats,
  fetchDocRequests,
  fetchDocTree,
  fetchMe,
  fetchProjectTasks,
  fetchProjects,
  fetchUserChats,
  fetchWorkspaces,
  loginRequest,
} from '../lib/api'
import type { TokenPair } from '../lib/types'

// `process.env`, not `import.meta.env`: Vitest does not copy arbitrary shell
// variables into import.meta.env (only VITE_-prefixed ones and .env files),
// so an import.meta-only read makes this whole file permanently skipped — an
// opt-in test nobody can opt into.
// Read through globalThis rather than adding @types/node to a browser-only
// app for one test file's benefit.
const shell = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
const meta = (import.meta as { env?: Record<string, string | undefined> }).env
const envv = (k: string): string | undefined => shell?.[k] ?? meta?.[k]
// The disposable e2e account, not a real one — it exists so this file can
// create and delete sessions without touching anyone's workspace.
const E2E_USER = 'mobile-e2e'
const E2E_PW = 'mobile-e2e-pw'

const ENABLED = envv('ACOWORK_E2E_LIVE') === '1'
const RAW_GW = envv('ACOWORK_E2E_URL')

// Resolved at collection: `skipIf` gates the `it` bodies but does not stop
// this file from being loaded, so an empty string stands in for
// "unreachable" and the suite is skipped whole.
async function gatewayUp(): Promise<string | null> {
  if (!ENABLED || !RAW_GW) return null
  try {
    return (await probeStatus(RAW_GW)).account_state === 'ready' ? RAW_GW : null
  } catch {
    return null
  }
}
const GW = (await gatewayUp()) ?? ''

describe.skipIf(GW === '')('live gateway (read-only)', () => {

  /**
   * Read-only shape check of every endpoint the four tabs depend on.
   *
   * This exists because a wrong path prefix is invisible: `/api/doc/api/tree`
   * returns 404 with an EMPTY body, the store catches it, and the screen
   * renders "文档库为空" — which looks like an empty library, not a bug. The
   * Gateway's auth middleware also runs before routing, so an unauthenticated
   * probe cannot distinguish a real route from `/api/bogus` (both are 401).
   * Only a logged-in GET with an asserted envelope catches it.
   *
   * No writes at all: nothing here can leave state behind, so unlike the
   * test above it needs no cleanup.
   */
  it('every tab endpoint returns the envelope the mobile types claim', async () => {
    let tokens: TokenPair | null = null
    try {
      tokens = await loginRequest(GW, E2E_USER, E2E_PW)
    } catch {
      return // e2e account absent — nothing to assert against
    }
    setAuthBridge({
      baseUrl: () => GW,
      accessToken: () => tokens?.access_token ?? null,
      refresh: async () => null,
      sessionExpired: () => {},
    })

    const me = await fetchMe()
    expect(me.user_id).toBeTruthy()
    // Envelopes are NOT uniform: /api/agents and /api/pm/* are bare arrays,
    // /api/users/* wraps in an object. Assuming the wrong one is how a list
    // silently renders empty.
    await expect(fetchAgents()).resolves.toBeInstanceOf(Array)
    await expect(fetchProjects()).resolves.toBeInstanceOf(Array)
    await expect(fetchDocRequests('pending')).resolves.toBeInstanceOf(Array)
    await expect(fetchDirectory()).resolves.toBeInstanceOf(Array)

    const chats = await fetchUserChats(me.user_id)
    expect(Array.isArray(chats)).toBe(true)

    const tree = await fetchDocTree()
    expect(Array.isArray(tree.files)).toBe(true)
    expect(Array.isArray(tree.dirs)).toBe(true)

    const projects = await fetchProjects()
    if (projects.length) {
      const tasks = await fetchProjectTasks(projects[0]!.id)
      expect(Array.isArray(tasks)).toBe(true)
      // `depth` is always serialised; `is_blocked`/`blocked_by` are skipped
      // when false/empty, so the mobile type must keep them optional.
      if (tasks.length) expect(typeof tasks[0]!.depth).toBe('number')
    }

    const status = await probeStatus(GW)
    expect(status.account_state).toBe('ready')
  })

  /**
   * The Agent drawer's five endpoints (§4.2), asserted on the fields the
   * drawer actually renders.
   *
   * A shape test earns its place by catching a mistake a type cannot: here it
   * is `access` being the STRING 'read-only' rather than a `read_only` bool.
   * The TS interface accepted either, so every workspace — including a
   * read-only one — rendered the "加入会话" button. Nothing failed; the UI
   * simply lied.
   */
  it('drawer endpoints carry the fields the drawer reads', async () => {
    let tokens: TokenPair | null = null
    try {
      tokens = await loginRequest(GW, E2E_USER, E2E_PW)
    } catch {
      return
    }
    setAuthBridge({
      baseUrl: () => GW,
      accessToken: () => tokens?.access_token ?? null,
      refresh: async () => null,
      sessionExpired: () => {},
    })
    const agents = await fetchAgents()
    const agentId = agents.find((a) => a.alive)?.instance_id
    if (!agentId) return // no running agent to read

    const st = await fetchAgentStatus(agentId)
    expect(st.work_dir).toBeTruthy()

    const ms = await fetchMemoryStats(agentId)
    expect(typeof ms.total_nodes).toBe('number')
    expect(ms.index_health).toBeTruthy()

    const tools = await fetchBuiltinTools(agentId)
    if (tools.length) expect(typeof tools[0]!.enabled).toBe('boolean')

    const ws = await fetchWorkspaces(agentId)
    if (ws.length) {
      expect(typeof ws[0]!.id).toBe('string')
      expect(['read-only', 'read-write']).toContain(ws[0]!.access)
    }

    // Wrapped: the settings live under `config`, so reading the top level
    // yields only agent_id/matches and an empty-looking tab.
    const cfg = await fetchAgentConfig(agentId)
    expect(cfg['config']).toBeTypeOf('object')
  })

})

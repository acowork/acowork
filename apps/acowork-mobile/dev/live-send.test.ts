/**
 * The billable live test — deliberately OUTSIDE `src/`, because `npm test`
 * and a plain `npx vitest run` use the src-only include glob in vite.config.ts,
 * so they must
 * never be able to reach a path that creates a session.
 *
 * Run it explicitly:
 *
 *   ACOWORK_E2E_URL=https://<throwaway-gateway> \
 *   ACOWORK_E2E_CONFIRM=https://<throwaway-gateway> \
 *   npx vitest run --config dev/vitest.send.config.ts
 *
 * Why this is a script and not a gated test in the suite: an env-var gate
 * only stops the accidental run, and the accident that actually happened
 * was deliberate-looking — someone (me) pointed `ACOWORK_E2E_URL` at the
 * dev Gateway on 127.0.0.1 and re-ran the file three times chasing an
 * unrelated failure, creating seven billable sessions inside the very
 * runtime hosting the conversation. A flag I can type is not a guard. So
 * the guard here is structural (not in the default include) plus a
 * paste-it-twice confirm plus the work_dir print below, which is the part
 * that makes "which machine am I writing to" impossible to miss.
 *
 * It still drives the SAME store code the app runs — that is the reason it
 * is a vitest file rather than a curl script. The regression it covers is
 * the fast-reply race the transition-only poll rule missed (fixed by the
 * message_count freshness check in chatStore).
 */

import { afterAll, describe, expect, it } from 'vitest'
import {
  fetchAgentStatus,
  fetchAgents,
  httpChatTransport,
  loginRequest,
  probeStatus,
  setAuthBridge,
} from '../src/lib/api'
import { POLL_INTERVAL_MS, setChatTransport, useChatStore } from '../src/stores/chatStore'
import { setDirectorySource, useAgentStore } from '../src/stores/agentStore'
import type { TokenPair } from '../src/lib/types'

const RAW_GW = process.env.ACOWORK_E2E_URL ?? ''
const CONFIRM = process.env.ACOWORK_E2E_CONFIRM ?? ''
const E2E_USER = 'mobile-e2e'
const E2E_PW = 'mobile-e2e-pw'

// The URL must be supplied twice, identically. Cheap, but unlike a boolean
// flag, copying a URL twice makes you look at the URL.
// ponytail: this is friction, not a fence. The fence is the config include;
// this only helps a hand-typed run avoid the wrong Gateway.
const ARMED = RAW_GW !== '' && CONFIRM === RAW_GW

const created: { agentId: string; sid: string }[] = []

describe.skipIf(!ARMED)('live send (billable)', () => {
  afterAll(async () => {
    const failed: string[] = []
    for (const { agentId, sid } of created.splice(0)) {
      try {
        await useChatStore.getState().deleteSession(agentId, sid)
      } catch (e) {
        failed.push(`${sid} (${e instanceof Error ? e.message : String(e)})`)
      }
    }
    if (failed.length) throw new Error(`leaked ${failed.length} session(s): ${failed.join(', ')}`)
  }, 60_000)

  it('login → create → send → poll sees the reply land', async () => {
    const status = await probeStatus(RAW_GW)
    expect(status.account_state).toBe('ready')

    const tokens: TokenPair = await loginRequest(RAW_GW, E2E_USER, E2E_PW)
    setAuthBridge({
      baseUrl: () => RAW_GW,
      accessToken: () => tokens.access_token,
      refresh: async () => null,
      sessionExpired: () => {},
    })
    setDirectorySource({
      loadAgents: async () =>
        (await fetchAgents())
          .filter((r) => r.alive)
          .map((r) => ({ id: r.instance_id, name: r.display_name ?? r.name, icon: null, status: 'idle' as const })),
      loadUsers: async () => [],
    })
    setChatTransport(httpChatTransport)

    expect(await useAgentStore.getState().refreshDirectory()).toBe(true)
    const agentId = useAgentStore.getState().agentList[0]?.id
    if (!agentId) throw new Error('no alive agent on this Gateway — nothing to test against')

    // Printed before anything is created: the session lands in THIS
    // directory, on whatever machine answers this URL. If that is the
    // machine running you, stop.
    const st = await fetchAgentStatus(agentId)
    console.log(`\n  target gateway : ${RAW_GW}`)
    console.log(`  writing session into agent ${agentId.slice(0, 8)}\n                   work_dir = ${st.work_dir}\n`)

    await useAgentStore.getState().refreshSessions(agentId)
    const sid = await useChatStore.getState().createSession(agentId, 'mobile live-send (throwaway)')
    expect(sid).toBeTruthy()
    created.push({ agentId, sid: sid! })

    expect(await useChatStore.getState().sendMessage(agentId, sid!, 'mobile store-level e2e ping')).toBe(true)

    useChatStore.getState().startWatching(agentId, sid!)
    const deadline = Date.now() + 90_000
    let got = false
    while (Date.now() < deadline && !got) {
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS))
      const v = useChatStore.getState().agentStates[agentId]
      got = !!v && v.messages.some((m) => m.role === 'assistant' && m.content.length > 0)
    }
    useChatStore.getState().stopWatching()
    expect(got).toBe(true)
  }, 120_000)
})

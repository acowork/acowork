/**
 * Presence channel tests (v1.2 §8.1): `AgentStatus` envelope decoding and the
 * single global subscription that flips list rows — CONNECT → SUBSCRIBE
 * `acowork/agents/+/status` → PUBLISH routing, the QoS 1 acknowledgement, the
 * fail-closed CONNACK path and the teardown on logout.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { PKT } from '../lib/mqtt-wire'
import { decodePresence, wBytes, wStr, wVarint } from '../lib/proto-wire'
import {
  PRESENCE_TOPIC, presenceConnected, setPresenceDeps, startPresence, stopPresence,
} from '../lib/presence'
import type { WebSocketLike } from '../lib/realtime'
import { setDirectorySource, useAgentStore } from '../stores/agentStore'
import type { AgentSummary, SessionInfo, UserSummary } from '../lib/types'

/* ---------------- envelope reader ---------------- */

// DataEnvelope{version=1, agent_status=9}: AgentStatus{online=2, instance_id=4,
// state=6, detail=7}. proto3 omits defaults, so an absent field means false/0.
const envPresence = (inner: number[]) => Uint8Array.from([...wVarint(1, 1), ...wBytes(9, inner)])
const stOnline = (...extra: number[]) => envPresence([...wVarint(2, 1), ...extra])

describe('decodePresence', () => {
  it('maps ADR-085 states onto the directory vocabulary', () => {
    const up = decodePresence(envPresence([...wVarint(2, 1), ...wStr(4, 'i-1'), ...wVarint(6, 4)]))
    expect(up).toEqual({ instanceId: 'i-1', online: true, state: 4, detail: '', status: 'idle' })

    // FAILED carries the reason the row shows instead of a generic verdict.
    const failed = decodePresence(envPresence([...wVarint(2, 1), ...wVarint(6, 5), ...wStr(7, 'port in use')]))
    expect(failed?.status).toBe('error')
    expect(failed?.detail).toBe('port in use')

    // STARTING is still "up" — the same call the HTTP directory makes.
    expect(decodePresence(stOnline(...wVarint(6, 2)))?.status).toBe('idle')
  })

  it('treats an absent `online` as offline (LWT / invariant 6)', () => {
    const off = decodePresence(envPresence([...wVarint(6, 1)]))
    expect(off).toEqual({ instanceId: '', online: false, state: 1, detail: '', status: 'offline' })
    // UNSPECIFIED (0) gates like offline for CAPABILITY checks, and the
    // boolean is what decides the dot: online=false wins either way.
    expect(decodePresence(envPresence([]))?.status).toBe('offline')
  })

  it('returns null for an envelope that is not an AgentStatus', () => {
    expect(decodePresence(Uint8Array.from([...wVarint(1, 1), ...wBytes(18, [])]))).toBeNull()
    expect(decodePresence(new Uint8Array())).toBeNull()
  })
})

/* ---------------- channel engine ---------------- */

class FakeSocket implements WebSocketLike {
  binaryType = 'arraybuffer'
  sent: Uint8Array[] = []
  readyState = 1
  onopen: (() => void) | null = null
  onmessage: ((ev: { data: ArrayBuffer | Uint8Array }) => void) | null = null
  onerror: (() => void) | null = null
  onclose: (() => void) | null = null
  send(d: Uint8Array) { this.sent.push(d) }
  close() { this.readyState = 3; this.onclose?.() }
  emitOpen() { this.onopen?.() }
  emit(bytes: Uint8Array) { this.onmessage?.({ data: bytes }) }
}

const connackOk = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 0])
const connackBad = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 5])
const subackGranted = Uint8Array.from([PKT.SUBACK << 4, 3, 0, 1, 1])
const subackRefused = Uint8Array.from([PKT.SUBACK << 4, 3, 0, 1, 0x80])

function statusPublish(agentId: string, inner: number[], qos1 = true): Uint8Array {
  const topic = new TextEncoder().encode(`acowork/agents/${agentId}/status`)
  const payload = envPresence(inner)
  const head = qos1 ? [0, 3] : []
  const body = Uint8Array.from([0, topic.length, ...topic, ...head, ...payload])
  return Uint8Array.from([(PKT.PUBLISH << 4) | (qos1 ? 0x02 : 0), body.length, ...body])
}

let sockets: FakeSocket[] = []
let onUpdate: ReturnType<typeof vi.fn>

beforeEach(() => {
  sockets = []
  onUpdate = vi.fn()
  setPresenceDeps({
    wsUrl: () => 'ws://192.168.1.7:19876/mqtt',
    connectOpts: async () => ({ clientId: 'user:alice:mobile:dev-1-presence', username: 'alice', password: 'tok' }),
    onUpdate,
    makeWs: () => { const s = new FakeSocket(); sockets.push(s); return s },
  })
})

afterEach(() => {
  stopPresence()
  setPresenceDeps({ wsUrl: () => null, connectOpts: async () => null, makeWs: undefined })
})

const flush = () => new Promise((r) => setTimeout(r, 0))

describe('presence channel', () => {
  it('subscribes the fleet-wide filter and routes each verdict to onUpdate', async () => {
    startPresence()
    await flush()
    const sock = sockets[0]!
    expect(sock.sent.length).toBe(0) // nothing sent before the socket opens
    sock.emitOpen()
    expect((sock.sent[0]![0] ?? 0) >> 4).toBe(PKT.CONNECT)
    sock.emit(connackOk)
    sock.emit(subackGranted)
    expect(presenceConnected()).toBe(true)
    const subText = new TextDecoder().decode(sock.sent[1])
    expect(subText).toContain(PRESENCE_TOPIC)
    expect(subText).not.toContain('sessions/') // presence never touches session topics

    sock.emit(statusPublish('i-1', [...wVarint(2, 1), ...wStr(4, 'i-1'), ...wVarint(6, 4)]))
    expect(onUpdate).toHaveBeenCalledWith('i-1', expect.objectContaining({ status: 'idle' }))

    // The LWT copy carries no instance_id: the topic path is the identity.
    sock.emit(statusPublish('i-2', []))
    expect(onUpdate).toHaveBeenCalledWith('i-2', expect.objectContaining({ status: 'offline' }))
  })

  it('acknowledges a QoS 1 publish so the broker stops redelivering', async () => {
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    sock.emit(subackGranted)
    sock.emit(statusPublish('i-1', [...wVarint(2, 1)]))
    const puback = sock.sent.find((b) => (b[0] ?? 0) >> 4 === PKT.PUBACK)
    expect(puback).toBeDefined()
    expect(((puback![2] ?? 0) * 256 + (puback![3] ?? 0))).toBe(3)
  })

  it('a refused SUBACK never reports a live channel', async () => {
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    sock.emit(subackRefused)
    expect(presenceConnected()).toBe(false)
  })

  it('a rejected CONNACK (fail-closed Gateway) retries instead of spinning', async () => {
    startPresence()
    await flush()
    sockets[0]!.emitOpen()
    sockets[0]!.emit(connackBad)
    expect(presenceConnected()).toBe(false)
    await vi.waitFor(() => expect(sockets.length).toBeGreaterThanOrEqual(2), { timeout: 3000 })
  })

  it('stopPresence closes the socket and stops the retry loop', async () => {
    startPresence()
    await flush()
    stopPresence()
    expect(presenceConnected()).toBe(false)
    const seen = sockets.length
    // A reconnect attempt after a close would open a second socket; the
    // backoff is 1 s, so 1.2 s of waiting is a decisive check.
    await new Promise((r) => setTimeout(r, 1200))
    expect(sockets.length).toBe(seen)
  })

  it('with no credentials the channel stays quiet and retries slowly', async () => {
    setPresenceDeps({ connectOpts: async () => null })
    startPresence()
    await flush()
    expect(sockets.length).toBe(0)
    expect(presenceConnected()).toBe(false)
  })
})

/* ---------------- store integration ---------------- */

function seedAgents(list: AgentSummary[]) {
  setDirectorySource({
    loadAgents: async () => list,
    loadUsers: async (): Promise<UserSummary[]> => [],
    loadNodes: async () => [],
  })
  useAgentStore.setState({ agents: {}, agentList: [], users: [] })
  useAgentStore.getState().setAgents(list)
}

describe('applyPresence', () => {
  afterEach(() => useAgentStore.setState({ agents: {}, agentList: [], users: [] }))

  it('flips both the row list and the per-agent slot, in place', async () => {
    seedAgents([
      { id: 'i-1', name: 'Senior', node_id: 'n-1', status: 'offline' },
      { id: 'i-2', name: 'PM', node_id: 'n-1', status: 'idle' },
    ])
    useAgentStore.getState().applyPresence('i-1', 'idle')
    const s = useAgentStore.getState()
    expect(s.agentList.find((a) => a.id === 'i-1')?.status).toBe('idle')
    expect(s.agents['i-1']?.info.status).toBe('idle')
    // untouched neighbour keeps its own verdict
    expect(s.agentList.find((a) => a.id === 'i-2')?.status).toBe('idle')
  })

  it('ignores a verdict for an agent the directory never listed', () => {
    seedAgents([{ id: 'i-1', name: 'Senior', node_id: 'n-1', status: 'offline' }])
    useAgentStore.getState().applyPresence('ghost', 'idle')
    const s = useAgentStore.getState()
    expect(s.agentList.map((a) => a.id)).toEqual(['i-1'])
    expect(s.agents['ghost']).toBeUndefined()
  })

  it('does not churn state when the verdict is unchanged', () => {
    seedAgents([{ id: 'i-1', name: 'Senior', node_id: 'n-1', status: 'idle' }])
    const before = useAgentStore.getState().agents['i-1']
    useAgentStore.getState().applyPresence('i-1', 'idle')
    expect(useAgentStore.getState().agents['i-1']).toBe(before)
  })

  it('keeps already-loaded sessions while it rewrites the info row', () => {
    seedAgents([{ id: 'i-1', name: 'Senior', node_id: 'n-1', status: 'offline' }])
    const sessions: SessionInfo[] = [{ session_id: 's-1', title: 'T' } as SessionInfo]
    useAgentStore.getState().setSessions('i-1', sessions)
    useAgentStore.getState().applyPresence('i-1', 'idle')
    // `setSessions` normalises the preview field, so compare identity, not shape.
    expect(useAgentStore.getState().agents['i-1']?.sessions.map((x) => x.session_id)).toEqual(['s-1'])
  })
})

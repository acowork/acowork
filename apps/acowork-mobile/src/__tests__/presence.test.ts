/**
 * Background channel tests (v1.2 §8.1, ADR-085, design doc 25 §8): `AgentStatus`
 * envelope decoding and the single global subscription that flips list rows —
 * CONNECT → SUBSCRIBE `acowork/agents/+/status` → PUBLISH routing, the QoS 1
 * acknowledgement, the fail-closed CONNACK path and the teardown on logout.
 *
 * The same socket also carries the user-chat doorbell, so the second describe
 * pins the half where the payload is EMPTY: one SUBSCRIBE for both filters,
 * per-filter grant codes, exact-topic routing and the state report that arms or
 * stands down the store's fallback poll.
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
/** Grant codes for the connection that subscribed both filters — index-based,
 *  so the codes must line up with `[PRESENCE_TOPIC, chatTopic]`. */
const suback2 = (codes: number[]) =>
  Uint8Array.from([PKT.SUBACK << 4, 2 + codes.length, 0, 1, ...codes])

/** The last SUBSCRIBE as text. Position alone does not work: a live connection
 *  also sends PUBACKs between the subscribes. */
function lastSubscribe(s: FakeSocket): string {
  const packet = s.sent.filter((b) => (b[0] ?? 0) >> 4 === PKT.SUBSCRIBE).at(-1)
  if (!packet) throw new Error('no SUBSCRIBE was sent')
  return new TextDecoder().decode(packet)
}

/** The doorbell as `acowork-user` sends it: QoS 1, non-retained, NO payload. */
function chatPublish(topic: string, pkid = 7): Uint8Array {
  const t = new TextEncoder().encode(topic)
  const body = Uint8Array.from([0, t.length, ...t, (pkid >> 8) & 0xff, pkid & 0xff])
  return Uint8Array.from([(PKT.PUBLISH << 4) | 0x02, body.length, ...body])
}

function statusPublish(agentId: string, inner: number[], qos1 = true, pkid = 3): Uint8Array {
  const topic = new TextEncoder().encode(`acowork/agents/${agentId}/status`)
  const payload = envPresence(inner)
  const head = qos1 ? [(pkid >> 8) & 0xff, pkid & 0xff] : []
  const body = Uint8Array.from([0, topic.length, ...topic, ...head, ...payload])
  return Uint8Array.from([(PKT.PUBLISH << 4) | (qos1 ? 0x02 : 0), body.length, ...body])
}

const catBytes = (...parts: Uint8Array[]) => {
  const out = new Uint8Array(parts.reduce((a, p) => a + p.length, 0))
  let o = 0
  for (const p of parts) { out.set(p, o); o += p.length }
  return out
}

let sockets: FakeSocket[] = []
let onUpdate: ReturnType<typeof vi.fn>
let onChatWake: ReturnType<typeof vi.fn>
let onChatChannel: ReturnType<typeof vi.fn>

/** The signed-in account's doorbell topic — the shape `lib/api.ts` builds. */
const ME = 'bbbbbbbb-0000-4000-8000-000000000002'
const CHAT_TOPIC = `acowork/users/${ME}/chat/changed`
const CHAT_TOPIC_OTHER = 'acowork/users/cccccccc-0000-4000-8000-000000000003/chat/changed'

beforeEach(() => {
  sockets = []
  onUpdate = vi.fn()
  onChatWake = vi.fn()
  onChatChannel = vi.fn()
  setPresenceDeps({
    wsUrl: () => 'ws://192.168.1.7:19876/mqtt',
    connectOpts: async () => ({ clientId: 'user:alice:mobile:dev-1-presence', username: 'alice', password: 'tok' }),
    onUpdate,
    // No account by default: the presence tests must keep seeing exactly one
    // filter, and the chat half reads the topic per CONNECT like the real app.
    chatTopic: () => null,
    onChatWake,
    onChatChannel,
    makeWs: () => { const s = new FakeSocket(); sockets.push(s); return s },
  })
})

afterEach(() => {
  stopPresence()
  // `setPresenceDeps` merges, so anything left set would leak into the next
  // file's expectations (the module holds one global deps object).
  setPresenceDeps({
    wsUrl: () => null, connectOpts: async () => null, makeWs: undefined,
    chatTopic: undefined, onChatWake: undefined, onChatChannel: undefined,
  })
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

  it('acks EVERY publish when the byte pipe coalesces several into one frame', async () => {
    // Device failure this pins: the wildcard subscribe replays the whole fleet,
    // rumqttd writes the burst in one TCP send, the bridge delivers it as ONE
    // WebSocket frame. Decoding only the first packet left pkid 2 un-acked, so
    // the next ack looked out-of-order and the broker dropped the connection —
    // a reconnect plus full replay every 5 s, forever.
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    sock.emit(subackGranted)
    sock.emit(catBytes(
      statusPublish('i-1', [...wVarint(2, 1)], true, 2),
      statusPublish('i-2', [...wVarint(2, 1)], true, 3),
    ))
    const acks = sock.sent
      .filter((b) => (b[0] ?? 0) >> 4 === PKT.PUBACK)
      .map((b) => (b[2] ?? 0) * 256 + (b[3] ?? 0))
    expect(acks).toEqual([2, 3]) // in the order the broker sent them
    expect(onUpdate).toHaveBeenCalledTimes(2)
    expect(onUpdate).toHaveBeenNthCalledWith(2, 'i-2', expect.objectContaining({ status: 'idle' }))
  })

  it('reassembles a publish split across two frames', async () => {
    // The mirror case: one packet bigger than the broker's write, so its tail
    // arrives in the next frame. Neither half may be treated as a packet.
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    sock.emit(subackGranted)
    const full = statusPublish('i-1', [...wVarint(2, 1), ...wStr(4, 'i-1')])
    const cut = Math.floor(full.length / 2)
    sock.emit(full.slice(0, cut))
    expect(onUpdate).not.toHaveBeenCalled()
    sock.emit(full.slice(cut))
    expect(onUpdate).toHaveBeenCalledWith('i-1', expect.objectContaining({ status: 'idle' }))
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

/* ---------------- the chat doorbell on the same socket ---------------- */

describe('chat wake-up half of the channel', () => {
  /** Bring the socket up with an account, subscribed and granted. */
  async function connected(): Promise<FakeSocket> {
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    sock.emit(suback2([1, 1]))
    return sock
  }

  it('sends both filters in the ONE SUBSCRIBE the CONNACK answers', async () => {
    setPresenceDeps({ chatTopic: () => CHAT_TOPIC })
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    // Two round trips would leave a half-awake channel — presence up, mailbox
    // silently never subscribed — with nothing in the UI to notice.
    const subs = sock.sent.filter((b) => (b[0] ?? 0) >> 4 === PKT.SUBSCRIBE)
    expect(subs).toHaveLength(1)
    const text = lastSubscribe(sock)
    expect(text).toContain(PRESENCE_TOPIC)
    expect(text).toContain(CHAT_TOPIC)
  })

  it('reports the doorbell up, then routes the empty publish to onChatWake', async () => {
    setPresenceDeps({ chatTopic: () => CHAT_TOPIC })
    const sock = await connected()
    expect(onChatChannel).toHaveBeenCalledWith(true)

    sock.emit(chatPublish(CHAT_TOPIC))

    expect(onChatWake).toHaveBeenCalledTimes(1)
    // QoS 1 means the broker expects an ack; missing it is a redelivery loop.
    const puback = sock.sent.find((b) => (b[0] ?? 0) >> 4 === PKT.PUBACK)
    expect(((puback?.[2] ?? 0) * 256 + (puback?.[3] ?? 0))).toBe(7)
    // Routed BEFORE the presence decoder: an empty payload would otherwise be
    // dropped as an undecodable envelope, which is a silent no-op, not an error.
    expect(onUpdate).not.toHaveBeenCalled()
  })

  it('never wakes the inbox for another account\'s doorbell', async () => {
    setPresenceDeps({ chatTopic: () => CHAT_TOPIC })
    const sock = await connected()

    // The broker delivers anything under a matched prefix; the topic is also the
    // social graph, so reacting to a neighbour's nudge would be both a privacy
    // leak of attention and a read storm.
    sock.emit(chatPublish(CHAT_TOPIC_OTHER))
    sock.emit(chatPublish('acowork/users/' + ME + '/chat/changed/extra'))
    expect(onChatWake).not.toHaveBeenCalled()

    sock.emit(chatPublish(CHAT_TOPIC))
    expect(onChatWake).toHaveBeenCalledTimes(1)
  })

  it('re-derives the topic per CONNECT, so a rotated account cannot be left deaf', async () => {
    let topic: string | null = CHAT_TOPIC
    setPresenceDeps({ chatTopic: () => topic })
    const sock = await connected()
    sock.emit(chatPublish(CHAT_TOPIC))
    expect(onChatWake).toHaveBeenCalledTimes(1)

    // Sign in as somebody else without tearing the channel down.
    topic = CHAT_TOPIC_OTHER
    sock.emit(connackOk)
    const text = lastSubscribe(sock)
    expect(text).toContain(CHAT_TOPIC_OTHER)
    sock.emit(suback2([1, 1]))

    sock.emit(chatPublish(CHAT_TOPIC)) // the stale topic must be ignored now
    expect(onChatWake).toHaveBeenCalledTimes(1)
    sock.emit(chatPublish(CHAT_TOPIC_OTHER, 9))
    expect(onChatWake).toHaveBeenCalledTimes(2)
  })

  it('a refused chat filter degrades the channel without killing presence', async () => {
    setPresenceDeps({ chatTopic: () => CHAT_TOPIC })
    const sock = await connected()
    expect(onChatChannel).toHaveBeenCalledTimes(1)

    // The account system was switched off between connections: the mailbox is
    // refused (0x80) while the fleet filter is still granted. Grant is per
    // filter, not per packet — treating it as one verdict would either stop the
    // fleet dots or leave the inbox thinking push works while nothing arrives.
    sock.emit(connackOk)
    sock.emit(suback2([1, 0x80]))

    expect(onChatChannel).toHaveBeenLastCalledWith(false)
    expect(onChatChannel).toHaveBeenCalledTimes(2)
    expect(presenceConnected()).toBe(true)
    sock.emit(statusPublish('i-1', [...wVarint(2, 1), ...wStr(4, 'i-1')]))
    expect(onUpdate).toHaveBeenCalledWith('i-1', expect.objectContaining({ status: 'idle' }))
  })

  it('reports the channel state once per change, not once per reconnect', async () => {
    setPresenceDeps({ chatTopic: () => CHAT_TOPIC })
    const sock = await connected()

    // The store stops its fallback timer on `true`; a flapping report would
    // stop and re-arm it on every reconnect and the timer would never idle.
    sock.emit(connackOk)
    sock.emit(suback2([1, 1]))
    sock.emit(connackOk)
    sock.emit(suback2([1, 1]))
    expect(onChatChannel).toHaveBeenCalledTimes(1)

    sock.emit(subackRefused)
    expect(onChatChannel).toHaveBeenCalledTimes(2)
  })

  it('with no account it subscribes the fleet filter alone', async () => {
    // `chatTopic` is null in the default deps (boot / logged out).
    startPresence()
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackOk)
    const subs = sock.sent.filter((b) => (b[0] ?? 0) >> 4 === PKT.SUBSCRIBE)
    expect(subs).toHaveLength(1)
    expect(lastSubscribe(sock)).not.toContain('acowork/users/')

    sock.emit(subackGranted)
    expect(presenceConnected()).toBe(true)
    expect(onChatChannel).not.toHaveBeenCalled()
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

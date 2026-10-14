/**
 * Realtime channel tests (v1.2 §8): MQTT packet codec round-trips, protobuf
 * envelope decoding, and the dual-channel state machine (relay → WS primary,
 * drop → polling fallback, retained-question guard).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import {
  encodeConnect, encodeSubscribe, encodePuback, encodePingreq, encodeDisconnect,
  decodePacket, PKT,
} from '../lib/mqtt-wire'
import { decodeEnvelope, wStr, wVarint, wBytes } from '../lib/proto-wire'
import {
  setRealtimeDeps, startWatching, stopWatching, channelState, pollNetworkState,
  POLL_INTERVAL_MS, type WebSocketLike,
} from '../lib/realtime'
import type { SessionSnapshot } from '../lib/types'

/* ---------------- wire codec ---------------- */

describe('mqtt-wire', () => {
  it('CONNECT carries client id, username, password', () => {
    const buf = encodeConnect({ clientId: 'user:n:mobile:d', username: 'n', password: 'tok' })
    expect((buf[0] ?? 0) >> 4).toBe(PKT.CONNECT)
    const text = new TextDecoder().decode(buf)
    expect(text).toContain('MQTT')
    expect(text).toContain('user:n:mobile:d')
    expect(text).toContain('tok')
  })

  it('remaining-length varint survives >127 byte bodies', () => {
    const big = 'x'.repeat(300)
    const buf = encodeConnect({ clientId: big, username: 'u', password: 'p' })
    // Second varint byte has the continuation bit set on the first.
    expect((buf[1] ?? 0) & 0x80).toBe(0x80)
  })

  it('SUBSCRIBE lists every topic with its QoS byte', () => {
    const buf = encodeSubscribe(['a/b', 'c/d'], [1, 0], 7)
    const text = new TextDecoder().decode(buf)
    expect(text).toContain('a/b')
    expect(text).toContain('c/d')
    expect((buf[0] ?? 0) >> 4).toBe(PKT.SUBSCRIBE)
    expect((buf[0] ?? 0) & 0x0f).toBe(2) // QoS1 flag required by spec
  })

  it('decodes CONNACK / SUBACK / PUBLISH frames', () => {
    const connack = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 0])
    expect(decodePacket(connack)).toEqual({ type: PKT.CONNACK, code: 0 })

    const suback = Uint8Array.from([PKT.SUBACK << 4, 4, 0, 7, 1, 0])
    const d = decodePacket(suback)
    expect(d?.type).toBe(PKT.SUBACK)
    if (d?.type === PKT.SUBACK) { expect(d.packetId).toBe(7); expect(d.codes).toEqual([1, 0]) }

    const topic = new TextEncoder().encode('t/1')
    const payload = new TextEncoder().encode('hi')
    const body = Uint8Array.from([0, topic.length, ...topic, ...payload])
    const publish = Uint8Array.from([(PKT.PUBLISH << 4) | 0, body.length, ...body])
    const p = decodePacket(publish)
    expect(p?.type).toBe(PKT.PUBLISH)
    if (p?.type === PKT.PUBLISH) {
      expect(p.packet.topic).toBe('t/1')
      expect(new TextDecoder().decode(p.packet.payload)).toBe('hi')
      expect(p.packet.qos).toBe(0)
    }
  })

  it('rejects malformed frames instead of throwing', () => {
    expect(decodePacket(Uint8Array.from([]))).toBeNull()
    expect(decodePacket(Uint8Array.from([0xff, 0xff]))).toBeNull()
  })

  it('PINGREQ / PUBACK / DISCONNECT are minimal', () => {
    expect(encodePingreq().length).toBe(2)
    expect(encodePuback(9)[2] ?? 0).toBe(0)
    expect(encodePuback(9)[3] ?? 0).toBe(9)
    expect((encodeDisconnect()[0] ?? 0) >> 4).toBe(PKT.DISCONNECT)
  })
})

/* ---------------- protobuf envelopes ---------------- */

// DataEnvelope{version=1 varint; oneof payload: session_message=15, session_state=18}
const envState = (st: number[]) => Uint8Array.from([...wVarint(1, 1), ...wBytes(18, st)])
const envMsg = (ev: number[]) => Uint8Array.from([...wVarint(1, 1), ...wBytes(15, ev)])

describe('proto-wire', () => {
  it('decodes DataEnvelope{session_state}', () => {
    // Wire-faithful: status is a serde tagged-enum JSON string.
    const ev = decodeEnvelope(envState([...wStr(3, '{"status":"idle"}'), ...wVarint(4, 7)]))
    expect(ev).toEqual({ kind: 'state', status: 'idle', messageCount: 7 })
    // Bare enum name tolerated (publisher drift guard).
    const bare = decodeEnvelope(envState([...wStr(3, 'idle'), ...wVarint(4, 2)]))
    expect(bare).toEqual({ kind: 'state', status: 'idle', messageCount: 2 })
  })

  it('decodes done / error / stopped', () => {
    expect(decodeEnvelope(envMsg(wBytes(6, []))).kind).toBe('done')
    expect(decodeEnvelope(envMsg(wBytes(8, []))).kind).toBe('stopped')
    const err = decodeEnvelope(envMsg(wBytes(7, wStr(2, 'boom'))))
    expect(err).toEqual({ kind: 'error', error: 'boom' })
  })

  it('decodes ask_question and tool_approval_needed details', () => {
    const q = decodeEnvelope(envMsg(wBytes(9, [...wStr(1, 'r1'), ...wStr(2, '{"question":"Q?"}')])))
    expect(q.kind).toBe('ask_question')
    if (q.kind === 'ask_question') { expect(q.requestId).toBe('r1'); expect(q.questionJson).toContain('Q?') }

    const a = decodeEnvelope(envMsg(wBytes(19, [...wStr(2, 'r2'), ...wStr(3, 'bash'), ...wStr(4, 'rm -rf'), ...wStr(5, 'high')])))
    expect(a.kind).toBe('tool_approval_needed')
    if (a.kind === 'tool_approval_needed') {
      expect(a.requestId).toBe('r2')
      expect(a.toolName).toBe('bash')
      expect(a.riskLevel).toBe('high')
    }
  })

  it('unknown fields and unknown oneofs are skipped, not fatal', () => {
    const inner = [...wStr(99, 'future'), ...wBytes(3, wStr(1, 'chunk'))] // chunk: never rendered
    expect(decodeEnvelope(envMsg(inner)).kind).toBe('ignore')
  })
})

/* ---------------- channel state machine ---------------- */

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
  /* test-side helpers */
  emitOpen() { this.onopen?.() }
  emit(bytes: Uint8Array) { this.onmessage?.({ data: bytes }) }
}

const connack = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 0])
const connackBad = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 4])
// Six topics are subscribed (state + five turn-end signals), so the remaining
// length is 2 packetId bytes + 6 return codes = 8. It used to say 9: a frame
// whose declared length exceeds its bytes is an INCOMPLETE packet, and the
// stream framer (correctly) waits for the missing byte instead of decoding it.
const subackOk = Uint8Array.from([PKT.SUBACK << 4, 8, 0, 1, 1, 1, 1, 1, 1, 1])
const statePublish = (status: string, count: number, qos1 = false): Uint8Array => {
  const topic = new TextEncoder().encode('acowork/agents/a1/sessions/s1/state')
  // Wire-faithful: SessionState.status is a JSON string (serde tagged enum).
  const payload = envState([...wStr(3, JSON.stringify({ status })), ...wVarint(4, count)])
  const head = qos1 ? [0, 1] : []
  const body = Uint8Array.from([0, topic.length, ...topic, ...head, ...payload])
  return Uint8Array.from([(PKT.PUBLISH << 4) | (qos1 ? 0x02 : 0), body.length, ...body])
}

let sockets: FakeSocket[] = []
const snapshot = vi.fn(async (): Promise<SessionSnapshot> => ({ status: { status: 'idle' }, messageCount: 4 }))
let handlers: { onSnapshot: ReturnType<typeof vi.fn>; onEvent: ReturnType<typeof vi.fn>; onChannel: ReturnType<typeof vi.fn> }

beforeEach(() => {
  sockets = []
  snapshot.mockClear()
  handlers = { onSnapshot: vi.fn(), onEvent: vi.fn(), onChannel: vi.fn() }
})

afterEach(() => {
  stopWatching()
  setRealtimeDeps({ wsUrl: () => null, connectOpts: async () => null, snapshot, makeWs: undefined })
})

function wireDeps(over: Parameters<typeof setRealtimeDeps>[0] = {}) {
  setRealtimeDeps({
    wsUrl: () => 'wss://gw.relay.test/mqtt',
    connectOpts: async () => ({ clientId: 'user:u:mobile:d', username: 'u', password: 'tok' }),
    snapshot,
    makeWs: () => { const s = new FakeSocket(); sockets.push(s); return s },
    ...over,
  })
}

const flush = () => new Promise((r) => setTimeout(r, 0))

describe('realtime channel', () => {
  it('LAN shape (no wsUrl) polls and reports the snapshot', async () => {
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await vi.waitFor(() => expect(handlers.onChannel).toHaveBeenCalledWith('polling'))
    await vi.waitFor(() => expect(handlers.onSnapshot).toHaveBeenCalledWith({ status: { status: 'idle' }, messageCount: 4 }))
    expect(channelState()).toBe('polling')
  })

  it('relay shape subscribes precisely and routes state publishes to onSnapshot', async () => {
    wireDeps()
    startWatching('a1', 's1', handlers)
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connack)
    sock.emit(subackOk)
    expect(channelState()).toBe('ws')
    expect(handlers.onChannel).toHaveBeenCalledWith('ws')
    // CONNECT was the first frame; SUBSCRIBE the second.
    expect((sock.sent[0]![0] ?? 0) >> 4).toBe(PKT.CONNECT)
    expect((sock.sent[1]![0] ?? 0) >> 4).toBe(PKT.SUBSCRIBE)
    const subText = new TextDecoder().decode(sock.sent[1])
    expect(subText).toContain('acowork/agents/a1/sessions/s1/state')
    expect(subText).toContain('messages/done')
    expect(subText).toContain('messages/tool_approval_needed')
    expect(subText).not.toContain('messages/chunk') // v1 never subscribes chunks
    expect(subText).not.toContain('sessions/s2')    // foreground session only

    sock.emit(statePublish('thinking', 4, true))
    expect(handlers.onSnapshot).toHaveBeenCalledWith({ status: { status: 'thinking' }, messageCount: 4 })
    // polling is OFF while the WS channel owns freshness (§8.0 exclusivity)
    expect(snapshot).not.toHaveBeenCalled()
  })

  it('a dropped WS falls back to polling immediately', async () => {
    wireDeps()
    startWatching('a1', 's1', handlers)
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connack)
    sock.emit(subackOk)
    expect(channelState()).toBe('ws')
    sock.close() // tunnel dies
    expect(channelState()).toBe('polling')
    expect(handlers.onChannel).toHaveBeenLastCalledWith('polling')
  })

  it('CONNACK rejection (stale token) never strands the session', async () => {
    wireDeps()
    startWatching('a1', 's1', handlers)
    await flush()
    const sock = sockets[0]!
    sock.emitOpen()
    sock.emit(connackBad)
    expect(channelState()).toBe('polling')
  })

  it('a socket that opens but never answers CONNACK times out to polling', async () => {
    vi.useFakeTimers()
    wireDeps()
    startWatching('a1', 's1', handlers)
    await vi.advanceTimersByTimeAsync(0)
    const sock = sockets[0]!
    sock.emitOpen() // open, then silence (black-holed tunnel)
    await vi.advanceTimersByTimeAsync(9000)
    expect(channelState()).toBe('polling')
    vi.useRealTimers()
  })

  it('each (re)connect asks for fresh credentials — token rotation survives', async () => {
    const connectOpts = vi.fn(async () => ({ clientId: 'c', username: 'u', password: 'tok1' }))
    wireDeps({ connectOpts })
    startWatching('a1', 's1', handlers)
    await flush()
    sockets[0]!.emitOpen()
    sockets[0]!.close()
    await vi.waitFor(() => expect(sockets.length).toBeGreaterThanOrEqual(2), { timeout: 3000 })
    expect(connectOpts).toHaveBeenCalledTimes(2)
  })
})

/* ---------------- idle stop (§8.3 "空闲不轮询") ----------------
 * A phone sitting on a finished conversation must not keep hitting the
 * Gateway every 2s. These use fake timers: the rule is about how MANY ticks
 * happen, not about wall-clock latency.
 */
describe('idle stop', () => {
  afterEach(() => {
    vi.useRealTimers()
    snapshot.mockResolvedValue({ status: { status: 'idle' }, messageCount: 4 })
  })
  const ticks = (n: number) => vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS * n)

  it('tears the timer down after two quiet ticks', async () => {
    vi.useFakeTimers()
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await ticks(0) // the immediate first tick establishes the count baseline
    expect(channelState()).toBe('polling')
    await ticks(2)
    expect(channelState()).toBe('off')
    const seen = handlers.onSnapshot.mock.calls.length
    await ticks(10)
    expect(handlers.onSnapshot).toHaveBeenCalledTimes(seen)
  })

  it('keeps polling while the agent is working', async () => {
    vi.useFakeTimers()
    snapshot.mockResolvedValue({ status: { status: 'thinking' }, messageCount: 4 })
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await ticks(6)
    expect(channelState()).toBe('polling')
  })

  it('keeps polling while the authoritative count moves', async () => {
    vi.useFakeTimers()
    let n = 4
    snapshot.mockImplementation(async () => ({ status: { status: 'idle' }, messageCount: (n += 1) }))
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await ticks(6)
    expect(channelState()).toBe('polling')
  })

  it('a session unknown to the Runtime (404 → null/null) goes quiet without an offline verdict', async () => {
    vi.useFakeTimers()
    snapshot.mockResolvedValue({ status: null, messageCount: null })
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await ticks(3)
    expect(channelState()).toBe('off')
    expect(pollNetworkState()).toEqual({ online: true, fails: 0 })
  })

  it('a local send restarts the loop after an idle stop', async () => {
    vi.useFakeTimers()
    wireDeps({ wsUrl: () => null })
    startWatching('a1', 's1', handlers)
    await ticks(4)
    expect(channelState()).toBe('off')
    startWatching('a1', 's1', handlers) // what chatStore.sendMessage does
    await ticks(1)
    expect(channelState()).toBe('polling')
  })
})

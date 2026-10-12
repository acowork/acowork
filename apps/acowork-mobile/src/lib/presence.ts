/**
 * Agent presence channel (v1.2 §8.1, ADR-085).
 *
 * Subscribes the whole fleet in one wildcard filter — `acowork/agents/+/status`
 * — so an agent going up, failing or being stopped flips the list row within
 * milliseconds instead of on the next directory poll. The Runtime publishes that
 * topic Retained with a 5 s heartbeat and an LWT (`online = false`), so a fresh
 * subscribe immediately replays the last known verdict for every agent, and a
 * device that was offline cannot miss a transition permanently.
 *
 * Why a SECOND MQTT connection instead of sharing `realtime`'s: that one is
 * scoped to the open session and is torn down when the user leaves the chat
 * screen, while presence has to be live on the list screen. MQTT 3.1.1 keys a
 * session by client id and a duplicate CONNECT takes it over, so this client
 * connects with its own id (`main.tsx` appends `-presence` to the device
 * segment — the Gateway's `remote_client_shape` forbids a fourth colon).
 *
 * There is deliberately no polling fallback in here: the directory poll already
 * carries presence (`alive` / `lifecycle`), this channel only makes the flip
 * instant. If the bridge is unreachable the list keeps its last polled state —
 * which is exactly the pre-phase-2 behaviour.
 */
import {
  encodeConnect, encodeDisconnect, encodePingreq, encodePuback,
  encodeSubscribe, PacketFramer, PKT, type ConnectOpts, type Decoded,
} from './mqtt-wire'
import { decodePresence, type PresenceUpdate } from './proto-wire'
import type { WebSocketLike } from './realtime'

/** One wildcard filter for the whole fleet (QoS 1: the status topic is QoS 1). */
export const PRESENCE_TOPIC = 'acowork/agents/+/status'

const RECONNECT_FAST = [1000, 2000, 4000]
const RECONNECT_SLOW_MS = 30_000
const PING_INTERVAL_MS = 25_000
const STALE_MS = 70_000
/** A socket that opens but never answers CONNACK (black-holed bridge, or a
 *  Gateway without a published user verifier) must not be waited on forever. */
const CONNECT_TIMEOUT_MS = 8_000

export interface PresenceDeps {
  /** MQTT-over-WS bridge URL, or null when the shape has none. */
  wsUrl?(): string | null
  /** Fresh CONNECT credentials with THIS channel's client id; null = no session. */
  connectOpts?(): Promise<ConnectOpts | null>
  /** One agent's verdict flipped. Called for every status publish. */
  onUpdate(agentId: string, update: PresenceUpdate): void
  /** Injectable for tests; defaults to the global WebSocket. */
  makeWs?(url: string): WebSocketLike
}

let deps: PresenceDeps | null = null
export function setPresenceDeps(d: Partial<PresenceDeps>): void {
  deps = { ...(deps ?? {}), ...d } as PresenceDeps
}

/* ---------------- module state (one global subscription) ---------------- */

let started = false
let sock: WebSocketLike | null = null
let alive = false
let retryTimer: ReturnType<typeof setTimeout> | null = null
let pingTimer: ReturnType<typeof setInterval> | null = null
let connectTimer: ReturnType<typeof setTimeout> | null = null
let lastRx = 0
let retries = 0
let packetId = 1
/** The bridge is a byte pipe: one frame can carry several status publishes
 *  (a wildcard subscribe replays the whole fleet). Reassemble before decoding,
 *  or a dropped tail means a missing PUBACK and rumqttd kills the session. */
const framer = new PacketFramer()

export function presenceConnected(): boolean {
  return alive
}

/* ---------------- public API ---------------- */

/** Idempotent: called on every `ready` phase (login, token rotation, reconnect). */
export function startPresence(): void {
  if (started) return
  started = true
  retries = 0
  void open()
}

export function stopPresence(): void {
  started = false
  retries = 0
  if (retryTimer !== null) { clearTimeout(retryTimer); retryTimer = null }
  closeSocket()
}

/* ---------------- engine ---------------- */

function closeSocket(): void {
  // Detach FIRST: `close()` fires `onclose` synchronously in some WebViews and
  // the `sock === s` guard would otherwise re-enter onDown.
  const s = sock
  sock = null
  alive = false
  if (pingTimer !== null) { clearInterval(pingTimer); pingTimer = null }
  if (connectTimer !== null) { clearTimeout(connectTimer); connectTimer = null }
  if (s) {
    if (s.readyState === 1) {
      try { s.send(encodeDisconnect()) } catch { /* socket already dying */ }
    }
    try { s.close() } catch { /* already gone */ }
  }
}

async function open(): Promise<void> {
  if (!started || !deps) return
  const url = deps.wsUrl?.() ?? null
  if (!url) { onDown(); return } // no bridge for this shape — stay quiet
  const opts = await deps.connectOpts?.()
  if (!started) return // logged out while the credential lookup was in flight
  if (!opts) { slowRetry(); return }
  closeSocket()
  framer.reset() // a fresh TCP stream cannot continue a half-read packet
  const s = deps.makeWs
    ? deps.makeWs(url)
    : (new WebSocket(url, ['mqtt']) as unknown as WebSocketLike)
  sock = s
  s.binaryType = 'arraybuffer'
  connectTimer = setTimeout(() => { if (sock === s && !alive) onDown() }, CONNECT_TIMEOUT_MS)
  s.onopen = () => {
    if (sock !== s) return
    s.send(encodeConnect(opts))
  }
  s.onmessage = (ev) => {
    if (sock !== s) return
    lastRx = Date.now()
    const bytes = ev.data instanceof Uint8Array ? ev.data : new Uint8Array(ev.data)
    for (const d of framer.feed(bytes)) handlePacket(d, s)
  }
  s.onerror = () => { if (sock === s) onDown() }
  s.onclose = () => { if (sock === s) onDown() }
}

function handlePacket(d: Decoded, s: WebSocketLike): void {
  if (!deps) return
  if (d.type === PKT.CONNACK) {
    if (d.code === 0) s.send(encodeSubscribe([PRESENCE_TOPIC], [1], packetId++))
    else onDown() // rejected (stale token / fail-closed verifier): retry later
    return
  }
  if (d.type === PKT.SUBACK) {
    if (d.codes.some((c) => c !== 0x80)) {
      alive = true
      retries = 0
      if (connectTimer !== null) { clearTimeout(connectTimer); connectTimer = null }
      if (pingTimer !== null) clearInterval(pingTimer)
      pingTimer = setInterval(() => {
        if (Date.now() - lastRx > STALE_MS) { onDown(); return }
        try { s.send(encodePingreq()) } catch { onDown() }
      }, PING_INTERVAL_MS)
    }
    return
  }
  if (d.type === PKT.PUBLISH) {
    const p = d.packet
    if (p.qos === 1 && p.packetId !== undefined) {
      try { s.send(encodePuback(p.packetId)) } catch { /* racing close */ }
    }
    const update = decodePresence(p.payload)
    if (!update) return
    // The topic path is the instance identity (ADR-073) and it is the key the
    // directory lists rows by; the envelope's `instance_id` is the fallback for
    // a publisher that fills the payload but not the path.
    const seg = p.topic.split('/')
    const agentId = update.instanceId || seg[2] || ''
    if (agentId) deps.onUpdate(agentId, update)
  }
}

/** Backoff after a loss: fast while the user is presumably watching a flip,
 *  then slow — a Gateway without the account system refuses forever. */
function onDown(): void {
  if (!started) return
  alive = false
  closeSocket()
  const delay = retries < RECONNECT_FAST.length
    ? (RECONNECT_FAST[retries] ?? RECONNECT_SLOW_MS)
    : RECONNECT_SLOW_MS
  retries += 1
  schedule(delay)
}

/** Not a loss — a missing credential (logged out mid-flight, rotation failed).
 *  Keep the channel alive without burning the fast-retry budget. */
function slowRetry(): void {
  if (!started) return
  alive = false
  closeSocket()
  schedule(RECONNECT_SLOW_MS)
}

function schedule(delay: number): void {
  if (retryTimer !== null) clearTimeout(retryTimer)
  retryTimer = setTimeout(() => {
    retryTimer = null
    void open()
  }, delay)
}

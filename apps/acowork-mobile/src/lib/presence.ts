/**
 * The background channel: fleet presence + the user-chat wake-up signal
 * (v1.2 §8.1, ADR-085, design doc 25 §8).
 *
 * Two independent reasons to keep a socket open while the user is nowhere near
 * the thing that changed:
 *
 *  - `acowork/agents/+/status`: one wildcard filter for the whole fleet, so an
 *    agent going up, failing or being stopped flips the list row within
 *    milliseconds instead of on the next directory poll. The Runtime publishes
 *    it Retained with a 5 s heartbeat and an LWT (`online = false`), so a fresh
 *    subscribe immediately replays the last known verdict for every agent, and
 *    a device that was offline cannot miss a transition permanently.
 *  - `acowork/users/{me}/chat/changed`: an **empty** nudge published by
 *    `acowork-user` whenever one of my threads gets a message or a read cursor
 *    moves (ADR-076 §决策 8). Reacting means re-reading over HTTP; the payload
 *    names nothing, because the embedded broker has no subscription-level ACL.
 *
 * Why a SECOND MQTT connection instead of sharing `realtime`'s: that one is
 * scoped to the open session and is torn down when the user leaves the chat
 * screen, while both of these have to live outside any one screen. MQTT 3.1.1
 * keys a session by client id and a duplicate CONNECT takes it over, so this
 * client connects with its own id (`main.tsx` appends `-presence` to the device
 * segment — the Gateway's `remote_client_shape` forbids a fourth colon).
 *
 * Both filters ride that ONE connection on purpose. A third socket with a third
 * client id would work, but it doubles the handshake, the keepalive and the
 * reconnect-backoff surface, and the bridge is a byte pipe shared by all of
 * them: the framing bug that cost 240 kick-offs in one window (§8.2) would have
 * had twice as many chances to happen again.
 *
 * There is deliberately no polling fallback in here: the directory poll already
 * carries presence (`alive` / `lifecycle`), this channel only makes the flip
 * instant. If the bridge is unreachable the list keeps its last polled state —
 * which is exactly the pre-phase-2 behaviour. Chat does have one, and it is the
 * store's decision, taken from [`PresenceDeps.onChatChannel`].
 */
import {
  encodeConnect, encodeDisconnect, encodePingreq, encodePuback,
  encodeSubscribe, PacketFramer, PKT, type ConnectOpts, type Decoded,
} from './mqtt-wire'
import { decodeEnvelope, decodePresence, type PresenceUpdate } from './proto-wire'
import type { WebSocketLike } from './realtime'

/** One wildcard filter for the whole fleet (QoS 1: the status topic is QoS 1). */
export const PRESENCE_TOPIC = 'acowork/agents/+/status'
/** Fleet-wide session working states (retained QoS1 — a fresh subscribe
 *  replays every session's current status, same as Desktop's mqtt_client).
 *  Drives the inbox's breathing working dot: the presence wire above only
 *  carries lifecycle (offline/error/idle), never `busy`. */
export const SESSION_STATE_TOPIC = 'acowork/agents/+/sessions/+/state'
/** Prefix of the per-account chat signals — routes a PUBLISH to the right
 *  consumer without re-deriving the account id. Mirrors
 *  `acowork_core::mqtt_proto::user_chat_changed_topic`. */
const USER_CHAT_PREFIX = 'acowork/users/'

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
  /** One session's lifecycle status pushed/retained (`.../sessions/{sid}/state`).
   *  Raw Runtime status string; the store decides busy vs idle. */
  onSessionState?(agentId: string, sessionId: string, status: string): void
  /**
   * The account's chat signal topic, or null while there is no account (boot,
   * logged out). Read per CONNECT, so a rotation that lands during a reconnect
   * is reflected on the next subscribe without tearing the channel down.
   */
  chatTopic?(): string | null
  /** A chat signal arrived. Coalescing and the actual reload are the store's. */
  onChatWake?(): void
  /**
   * Whether the chat half of this channel is currently able to deliver. The
   * 5 s fallback poll is armed only while this is false — the same exclusivity
   * `realtime.ts` applies — so a refused subscribe degrades instead of going
   * silent, and a healthy push path never also polls.
   */
  onChatChannel?(up: boolean): void
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
/** The filters sent in the last SUBSCRIBE, in return-code order, plus the chat
 *  one this connection is actually listening to. Kept per connection: the
 *  account can change between two CONNECTs, and routing a PUBLISH against a
 *  stale topic would silently drop the wake-up. */
let subTopics: string[] = [PRESENCE_TOPIC, SESSION_STATE_TOPIC]
let chatSub: string | null = null
/** Last value handed to `onChatChannel`, so a reconnect does not re-arm a poll
 *  that is already armed (and the reverse). */
let chatUp = false
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
  reportChat(false)
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
    if (d.code === 0) {
      // ONE SUBSCRIBE carrying both filters: the packet id space is per
      // connection, and two round trips would add a way to end up subscribed
      // to presence only, half-awake, with nothing logging it.
      const chat = deps.chatTopic?.() ?? null
      chatSub = chat
      subTopics = chat
        ? [PRESENCE_TOPIC, SESSION_STATE_TOPIC, chat]
        : [PRESENCE_TOPIC, SESSION_STATE_TOPIC]
      s.send(encodeSubscribe(subTopics, subTopics.map(() => 1), packetId++))
    } else onDown() // rejected (stale token / fail-closed verifier): retry later
    return
  }
  if (d.type === PKT.SUBACK) {
    // Grant is per filter, not per packet: a Gateway whose account system is
    // off can hand back 0x80 for the mailbox while still delivering presence.
    const presenceOk = granted(d.codes, PRESENCE_TOPIC)
    const stateOk = granted(d.codes, SESSION_STATE_TOPIC)
    const chatOk = chatSub !== null && granted(d.codes, chatSub)
    if (presenceOk || stateOk || chatOk) {
      alive = true
      retries = 0
      if (connectTimer !== null) { clearTimeout(connectTimer); connectTimer = null }
      if (pingTimer !== null) clearInterval(pingTimer)
      pingTimer = setInterval(() => {
        if (Date.now() - lastRx > STALE_MS) { onDown(); return }
        try { s.send(encodePingreq()) } catch { onDown() }
      }, PING_INTERVAL_MS)
    }
    reportChat(chatOk)
    return
  }
  if (d.type === PKT.PUBLISH) {
    const p = d.packet
    if (p.qos === 1 && p.packetId !== undefined) {
      try { s.send(encodePuback(p.packetId)) } catch { /* racing close */ }
    }
    if (p.topic.startsWith(USER_CHAT_PREFIX)) {
      // Routed before the presence decoder: the chat payload is EMPTY, so
      // letting it fall through would be a silent no-op instead of a wake-up.
      // Exact match, not just the prefix — a wildcard subscribe on this tree is
      // never sent, and if one ever were, another account's signal must not
      // refresh this device.
      if (chatSub !== null && p.topic === chatSub) deps.onChatWake?.()
      return
    }
    if (p.topic.startsWith('acowork/agents/') && p.topic.endsWith('/state')) {
      // `acowork/agents/{instance}/sessions/{sid}/state` — working signal.
      const ev = decodeEnvelope(p.payload)
      if (ev.kind === 'state') {
        const seg = p.topic.split('/')
        const agentId = seg[2]
        const sessionId = seg[4]
        if (agentId && sessionId) deps.onSessionState?.(agentId, sessionId, ev.status)
      }
      return
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

/** Did the broker accept one of the filters we just sent? `0x80` is the only
 *  failure code MQTT 3.1.1 defines; a short code list means the packet did not
 *  line up with our subscribe order and nothing may be assumed. */
function granted(codes: number[], topic: string): boolean {
  const i = subTopics.indexOf(topic)
  if (i < 0) return false
  const code = codes[i]
  return code !== undefined && code !== 0x80
}

/** Only on a change — a reconnect cycle that re-reports the same state would
 *  otherwise stop and re-arm the fallback poll every time. */
function reportChat(up: boolean): void {
  if (chatUp === up) return
  chatUp = up
  deps?.onChatChannel?.(up)
}

/** Backoff after a loss: fast while the user is presumably watching a flip,
 *  then slow — a Gateway without the account system refuses forever. */
function onDown(): void {
  if (!started) return
  alive = false
  reportChat(false)
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
  reportChat(false)
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

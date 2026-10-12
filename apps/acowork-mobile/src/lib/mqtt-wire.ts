/**
 * Minimal MQTT 3.1.1 packet codec over WebSocket (v1.2 §8.1).
 *
 * Scope discipline: only the four client packets (CONNECT / SUBSCRIBE /
 * UNSUBSCRIBE / PINGREQ / DISCONNECT) and the four server packets we must
 * react to (CONNACK / SUBACK / PUBLISH / PINGRESP). No shared subscriptions,
 * no auth extension, no will. QoS 1 flows are handled by the spec-mandated
 * PUBACK round-trip so retained QoS1 events are not redelivered forever.
 *
 * A WebSocket frame is NOT an MQTT packet. Both bridges are raw byte pipes
 * (`core/acowork-gateway/src/relay/remote_listener.rs`), so whatever the broker
 * coalesces into one TCP write arrives as one frame, and one big publish can
 * arrive split across frames. Inbound frames therefore go through
 * [PacketFramer], which reassembles the stream and hands back whole packets.
 */

export const PKT = {
  CONNECT: 1, CONNACK: 2, PUBLISH: 3, PUBACK: 4,
  SUBSCRIBE: 8, SUBACK: 9, UNSUBSCRIBE: 10, UNSUBACK: 11,
  PINGREQ: 12, PINGRESP: 13, DISCONNECT: 14,
} as const

/* ---------------- encode ---------------- */

function len(n: number): Uint8Array {
  // MQTT remaining-length varint (up to 4 bytes).
  const out: number[] = []
  do {
    let b = n % 128
    n = Math.floor(n / 128)
    if (n > 0) b |= 0x80
    out.push(b)
  } while (n > 0)
  return Uint8Array.from(out)
}

function pkt(type: number, flags: number, body: Uint8Array): Uint8Array {
  const rl = len(body.length)
  const h = new Uint8Array(1 + rl.length)
  h[0] = (type << 4) | flags
  h.set(rl, 1)
  const all = new Uint8Array(h.length + body.length)
  all.set(h); all.set(body, h.length)
  return all
}

function str(s: string): Uint8Array {
  const b = new TextEncoder().encode(s)
  const out = new Uint8Array(2 + b.length)
  out[0] = (b.length >> 8) & 0xff
  out[1] = b.length & 0xff
  out.set(b, 2)
  return out
}

function cat(...parts: Uint8Array[]): Uint8Array {
  const n = parts.reduce((a, p) => a + p.length, 0)
  const out = new Uint8Array(n)
  let o = 0
  for (const p of parts) { out.set(p, o); o += p.length }
  return out
}

export interface ConnectOpts {
  clientId: string
  username: string
  password: string
  keepAliveSecs?: number
}

export function encodeConnect(o: ConnectOpts): Uint8Array {
  const var0 = new Uint8Array(10)
  var0.set([0, 4], 0); var0.set(new TextEncoder().encode('MQTT'), 2)
  var0[6] = 4 // protocol level 3.1.1
  var0[7] = 0xc2 // flags: username | password | clean session
  var0[8] = ((o.keepAliveSecs ?? 30) >> 8) & 0xff
  var0[9] = (o.keepAliveSecs ?? 30) & 0xff
  const body = cat(var0, str(o.clientId), str(o.username), str(o.password))
  return pkt(PKT.CONNECT, 0, body)
}

/** topics[i] pairs with qoss[i]; packetId is 1..65535. */
export function encodeSubscribe(topics: string[], qoss: number[], packetId: number): Uint8Array {
  const pid = Uint8Array.from([(packetId >> 8) & 0xff, packetId & 0xff])
  const body = cat(pid, ...topics.map((t, i) => cat(str(t), Uint8Array.from([qoss[i] ?? 0]))))
  return pkt(PKT.SUBSCRIBE, 2, body)
}

export function encodeUnsubscribe(topics: string[], packetId: number): Uint8Array {
  const pid = Uint8Array.from([(packetId >> 8) & 0xff, packetId & 0xff])
  const body = cat(pid, ...topics.map(str))
  return pkt(PKT.UNSUBSCRIBE, 2, body)
}

export function encodePuback(packetId: number): Uint8Array {
  return pkt(PKT.PUBACK, 0, Uint8Array.from([(packetId >> 8) & 0xff, packetId & 0xff]))
}

export const encodePingreq = (): Uint8Array => pkt(PKT.PINGREQ, 0, new Uint8Array(0))
export const encodeDisconnect = (): Uint8Array => pkt(PKT.DISCONNECT, 0, new Uint8Array(0))

/* ---------------- decode ---------------- */

export interface PublishPacket {
  topic: string
  payload: Uint8Array
  qos: number
  retain: boolean
  dup: boolean
  packetId?: number
}
export type Decoded =
  | { type: typeof PKT.CONNACK; code: number }
  | { type: typeof PKT.SUBACK; packetId: number; codes: number[] }
  | { type: typeof PKT.PUBLISH; packet: PublishPacket }
  | { type: typeof PKT.PUBACK; packetId: number }
  | { type: typeof PKT.PINGRESP }

/** Decode exactly one complete control packet. Returns null on malformed
 *  input — a broken packet must not take the session down. Anything reading a
 *  byte stream must go through [PacketFramer] instead: this function has no way
 *  to see that a frame carries more than the packet it declares. */
export function decodePacket(frame: Uint8Array): Decoded | null {
  try {
    const fixed = frame[0] ?? 0
    const type = fixed >> 4
    const flags = fixed & 0x0f
    let p = 1
    let mult = 1
    let rl = 0
    for (;;) {
      const b = frame[p++] ?? 0
      rl += (b & 0x7f) * mult
      if (!(b & 0x80)) break
      mult *= 128
    }
    const body = frame.subarray(p, p + rl)
    switch (type) {
      case PKT.CONNACK:
        return { type, code: body[1] ?? 1 }
      case PKT.SUBACK:
        return {
          type,
          packetId: (body[0] ?? 0) * 256 + (body[1] ?? 0),
          codes: Array.from(body.subarray(2)),
        }
      case PKT.PUBLISH: {
        const qos = (flags >> 1) & 3
        const retain = (flags & 1) === 1
        const dup = ((flags >> 3) & 1) === 1
        const tl = (body[0] ?? 0) * 256 + (body[1] ?? 0)
        const topic = new TextDecoder().decode(body.subarray(2, 2 + tl))
        let o = 2 + tl
        let packetId: number | undefined
        if (qos > 0) { packetId = (body[o] ?? 0) * 256 + (body[o + 1] ?? 0); o += 2 }
        return { type, packet: { topic, payload: body.slice(o), qos, retain, dup, packetId } }
      }
      case PKT.PUBACK:
        return { type, packetId: (body[0] ?? 0) * 256 + (body[1] ?? 0) }
      case PKT.PINGRESP:
        return { type }
      default:
        return null
    }
  } catch {
    return null
  }
}

/* ---------------- stream reassembly ---------------- */

/** Defensive ceiling: a header claiming more than this is a broken stream, and
 *  the only sane answer is to drop the buffer (the normal failure path will
 *  tear the socket down). */
const MAX_PACKET_BYTES = 4 * 1024 * 1024

/**
 * Reassembles the MQTT byte stream out of WebSocket frames.
 *
 * Why this exists: handing a coalesced frame straight to `decodePacket`
 * silently discards everything after the first packet. The lost packet is
 * usually a QoS 1 PUBLISH, so its PUBACK is never sent; rumqttd then sees the
 * NEXT ack as out-of-order and drops the connection (real device: the presence
 * wildcard was killed and re-subscribed every 5 s, matching the Runtime's
 * status heartbeat). Feed each frame here and process what comes back in order.
 */
export class PacketFramer {
  // Annotated, not inferred: the literal `new Uint8Array(0)` pins the generic
  // to `ArrayBuffer`, which then rejects the `ArrayBufferLike` views `cat()`
  // and `subarray()` hand back.
  private buf: Uint8Array = new Uint8Array(0)

  /** Consume one incoming frame; returns every packet that is now complete. */
  feed(frame: Uint8Array): Decoded[] {
    if (frame.length > 0) this.buf = cat(this.buf, frame)
    const out: Decoded[] = []
    for (;;) {
      const bytes = this.take()
      if (!bytes) break
      const d = decodePacket(bytes)
      if (d) out.push(d) // an unparseable packet is skipped, never fatal
    }
    return out
  }

  /** Drop all buffered bytes. One instance per socket, reset on each new
   *  connection: a fresh TCP stream cannot continue a half-read packet. */
  reset(): void {
    this.buf = new Uint8Array(0)
  }

  /** Cut the first complete packet off the buffer, or return null while it is
   *  still incomplete (the bytes stay buffered for the next frame). */
  private take(): Uint8Array | null {
    const b = this.buf
    if (b.length < 2) return null
    let p = 1
    let mult = 1
    let rl = 0
    for (;;) {
      const x = b[p]
      if (x === undefined) return null // remaining-length varint still in flight
      p += 1
      rl += (x & 0x7f) * mult
      if ((x & 0x80) === 0) break
      mult *= 128
      if (p > 4) { this.reset(); return null } // a 5th varint byte is illegal
    }
    // Checked after the loop as well: a legal 4-byte varint can still claim
    // 256 MB, and buffering that forever is a memory leak, not a packet.
    if (rl > MAX_PACKET_BYTES) { this.reset(); return null }
    const total = p + rl
    if (b.length < total) return null
    const packet = b.slice(0, total)
    this.buf = b.subarray(total)
    return packet
  }
}

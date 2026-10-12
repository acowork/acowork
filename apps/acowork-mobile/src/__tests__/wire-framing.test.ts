/**
 * MQTT stream reassembly over the WS byte pipe (v1.2 §8.1).
 *
 * The bridges (`core/acowork-gateway/src/relay/remote_listener.rs`) forward
 * bytes, not messages: whatever the broker coalesces into one TCP send arrives
 * as ONE WebSocket frame, and one large packet can arrive split. Treating a
 * frame as one packet silently loses the tail — on a real device that was a
 * QoS 1 `AgentStatus` publish whose PUBACK never went out, so the NEXT ack
 * looked out-of-order and rumqttd dropped the presence connection every 5 s
 * (the Runtime's status heartbeat). These tests pin the framing contract both
 * channels rely on.
 */
import { describe, it, expect } from 'vitest'
import { PacketFramer, PKT, type Decoded } from '../lib/mqtt-wire'

/* ---------------- packet builders ---------------- */

/** MQTT remaining-length varint (needed to build a >127-byte packet). */
function lenBytes(n: number): number[] {
  const out: number[] = []
  do {
    let b = n % 128
    n = Math.floor(n / 128)
    if (n > 0) b |= 0x80
    out.push(b)
  } while (n > 0)
  return out
}

function publish(topic: string, payload: string, pkid = 1, qos = 1): Uint8Array {
  const t = new TextEncoder().encode(topic)
  const p = new TextEncoder().encode(payload)
  const head = qos > 0 ? [(pkid >> 8) & 0xff, pkid & 0xff] : []
  const body = Uint8Array.from([(t.length >> 8) & 0xff, t.length & 0xff, ...t, ...head, ...p])
  return Uint8Array.from([(PKT.PUBLISH << 4) | (qos << 1), ...lenBytes(body.length), ...body])
}

const CONNACK = Uint8Array.from([PKT.CONNACK << 4, 2, 0, 0])
const SUBACK = Uint8Array.from([PKT.SUBACK << 4, 3, 0, 1, 1])
const PINGRESP = Uint8Array.from([PKT.PINGRESP << 4, 0])
/** A reserved packet type: decoders must skip it, not stall on it. */
const GARBAGE = Uint8Array.from([0xf0, 0])

const cat = (...parts: Uint8Array[]) => {
  const out = new Uint8Array(parts.reduce((a, p) => a + p.length, 0))
  let o = 0
  for (const p of parts) { out.set(p, o); o += p.length }
  return out
}

const topicsOf = (packets: Decoded[]) =>
  packets.map((d) => (d.type === PKT.PUBLISH ? d.packet.topic : d.type))

describe('PacketFramer', () => {
  it('splits a frame that carries two coalesced publishes', () => {
    const f = new PacketFramer()
    const out = f.feed(cat(
      publish('acowork/agents/i-1/status', 'a', 2),
      publish('acowork/agents/i-2/status', 'b', 3),
    ))
    expect(out).toHaveLength(2)
    expect(topicsOf(out)).toEqual(['acowork/agents/i-1/status', 'acowork/agents/i-2/status'])
    // The pkids are what the channel must ack IN ORDER — an out-of-order ack is
    // what the broker punished with a disconnect.
    expect(out.map((d) => (d.type === PKT.PUBLISH ? d.packet.packetId : null))).toEqual([2, 3])
  })

  it('yields every packet of a mixed burst, in order', () => {
    const f = new PacketFramer()
    const out = f.feed(cat(CONNACK, SUBACK, PINGRESP, publish('t/u', 'x', 4)))
    expect(out.map((d) => d.type)).toEqual([PKT.CONNACK, PKT.SUBACK, PKT.PINGRESP, PKT.PUBLISH])
  })

  it('buffers a partial packet and completes it on the next frame', () => {
    const f = new PacketFramer()
    const full = publish('acowork/agents/i-1/status', 'hello world', 5)
    const cut = Math.floor(full.length / 2)
    expect(f.feed(full.slice(0, cut))).toEqual([])
    const out = f.feed(full.slice(cut))
    expect(out).toHaveLength(1)
    expect(out[0]!.type).toBe(PKT.PUBLISH)
  })

  it('survives a remaining-length varint split across frames', () => {
    // A topic long enough that the length is a two-byte varint: frame 1 ends
    // between the two varint bytes, so the packet is not even self-describing yet.
    const f = new PacketFramer()
    const full = publish('acowork/'.padEnd(200, 'x'), 'body', 6)
    expect(full[1]).toBeGreaterThan(127) // confirms the two-byte varint case
    expect(f.feed(full.slice(0, 2))).toEqual([])
    const out = f.feed(full.slice(2))
    expect(out).toHaveLength(1)
    expect(out[0]!.type).toBe(PKT.PUBLISH)
  })

  it('skips a packet it cannot decode and keeps the rest of the frame', () => {
    const f = new PacketFramer()
    const out = f.feed(cat(GARBAGE, publish('t/keep', 'y', 7)))
    expect(out.map((d) => d.type)).toEqual([PKT.PUBLISH])
  })

  it('treats an empty frame as a no-op', () => {
    const f = new PacketFramer()
    expect(f.feed(new Uint8Array(0))).toEqual([])
    expect(f.feed(CONNACK)).toHaveLength(1) // an empty frame must not desync it
    expect(f.feed(new Uint8Array(0))).toEqual([])
  })

  it('drops a half-read packet on reset (a new socket is a new stream)', () => {
    const f = new PacketFramer()
    const full = publish('t/u', 'abcdef', 8)
    expect(f.feed(full.slice(0, 4))).toEqual([]) // incomplete, held for more
    f.reset()
    // Those bytes are gone. Without the reset they would be re-interpreted as a
    // header and swallow the next socket's first real packet.
    expect(f.feed(CONNACK)).toHaveLength(1)
  })

  it('refuses an absurd remaining length instead of buffering forever', () => {
    const f = new PacketFramer()
    // A legal 4-byte varint claiming ~256 MB: no real packet, so the buffer is
    // dropped — otherwise a hostile frame grows memory without bound.
    expect(f.feed(Uint8Array.from([PKT.PUBLISH << 4, 0xff, 0xff, 0xff, 0x7f]))).toEqual([])
    expect(f.feed(SUBACK)).toHaveLength(1) // and the next frame parses cleanly
  })
})

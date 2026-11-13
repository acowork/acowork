/**
 * `lib/gatewayUrl.ts` is the single source of truth for "which Gateway is
 * this Desktop talking to" (ADR-115). Every other module — the store, the
 * boot path, the address picker, the title-bar switcher — reads its verdict
 * instead of re-deriving it, which means a wrong answer here is wrong
 * EVERYWHERE and silently so: the UI would look consistent while pointing at
 * the wrong connection semantics.
 *
 * These cases are therefore tested directly rather than through a component.
 * The loopback table is the important one: it decides whether Desktop may
 * spawn a local Gateway, and getting `::ffff:127.0.0.1` wrong (a dual-stack
 * listener reports a local IPv4 connection that way — see ADR-114 §2.3)
 * would classify this machine as remote and refuse to boot it.
 */
import { describe, expect, it } from "vitest";
import {
  LOCAL_GATEWAY_URL,
  deriveGatewayMode,
  gatewayAddressOptions,
  hasHttpScheme,
  isHttpsUrl,
  isLoopbackGatewayUrl,
  normalizeGatewayUrl,
} from "./gatewayUrl";

describe("normalizeGatewayUrl", () => {
  it("trims and strips trailing slashes", () => {
    expect(normalizeGatewayUrl("  http://10.0.0.5:19876///  ")).toBe(
      "http://10.0.0.5:19876",
    );
  });

  it("treats empty / nullish input as empty", () => {
    expect(normalizeGatewayUrl("")).toBe("");
    expect(normalizeGatewayUrl("   ")).toBe("");
    // @ts-expect-error — the store can receive an undefined from a stale key
    expect(normalizeGatewayUrl(undefined)).toBe("");
  });
});

describe("isLoopbackGatewayUrl", () => {
  it.each([
    "http://localhost:19876",
    "http://127.0.0.1:19876",
    // 127/8 is entirely this machine — Windows and browsers accept the rest.
    "http://127.0.0.2:19876",
    "http://127.1.2.3:19876",
    "http://[::1]:19876",
    "http://[0:0:0:0:0:0:0:1]:19876",
    // Dual-stack listeners report a local IPv4 connection as IPv4-mapped.
    // `new URL()` normalizes the dotted form to hex (`::ffff:7f00:1`), so
    // both spellings have to be recognised — see the loopback table comment.
    "http://[::ffff:127.0.0.1]:19876",
    "http://[::ffff:7f00:1]:19876",
    "http://[::ffff:127.0.0.2]:19876",
    "https://localhost",
  ])("%s is this machine", (url) => {
    expect(isLoopbackGatewayUrl(url)).toBe(true);
  });

  it.each([
    "http://192.168.1.10:19876",
    "http://10.0.0.5:19876",
    // IPv4-mapped but routable — a real LAN peer, not loopback.
    "http://[::ffff:192.168.1.10]:19876",
    // Same address in the hex form the URL parser normalizes it to.
    "http://[::ffff:c0a8:10a]:19876",
    "https://0f1e2d3c.relay.example.com",
    "http://127.0.0.1.example.com:19876",
  ])("%s is remote", (url) => {
    expect(isLoopbackGatewayUrl(url)).toBe(false);
  });

  it("fails closed for scheme-less / unparseable input", () => {
    // The manual host extraction still recognises a bare loopback host...
    expect(isLoopbackGatewayUrl("127.0.0.1:19876")).toBe(true);
    expect(isLoopbackGatewayUrl("[::1]:19876")).toBe(true);
    // ...anything it cannot classify must NOT be treated as local, because
    // "local" is what authorises spawning a child process.
    expect(isLoopbackGatewayUrl("abc")).toBe(false);
    expect(isLoopbackGatewayUrl("")).toBe(false);
  });
});

describe("deriveGatewayMode", () => {
  it("maps loopback to local and everything else to remote", () => {
    expect(deriveGatewayMode(LOCAL_GATEWAY_URL, false)).toBe("local");
    expect(deriveGatewayMode("http://10.0.0.5:19876", false)).toBe("remote");
  });

  it("lets an explicit relay opt-in win", () => {
    expect(deriveGatewayMode("https://gw.relay.example.com", true)).toBe("relay");
    // Even for a loopback URL: relayActive is only ever set together with an
    // https device domain (`commitGatewayUrl`), so this combination is not
    // reachable from the UI — the derivation still has to be total.
    expect(deriveGatewayMode(LOCAL_GATEWAY_URL, true)).toBe("relay");
  });
});

describe("scheme checks", () => {
  it("isHttpsUrl accepts only https", () => {
    expect(isHttpsUrl("https://gw.relay.example.com")).toBe(true);
    expect(isHttpsUrl("HTTPS://gw.relay.example.com")).toBe(true);
    expect(isHttpsUrl("http://127.0.0.1:19876")).toBe(false);
    expect(isHttpsUrl("gw.relay.example.com")).toBe(false);
  });

  it("hasHttpScheme accepts http and https, nothing else", () => {
    expect(hasHttpScheme("http://127.0.0.1:19876")).toBe(true);
    expect(hasHttpScheme("https://gw.relay.example.com")).toBe(true);
    // A bare host is what a user types when they forget the scheme — it would
    // become a `base_url` that can never answer, so it must be rejected.
    expect(hasHttpScheme("192.168.1.5:19876")).toBe(false);
    expect(hasHttpScheme("abc")).toBe(false);
    expect(hasHttpScheme("")).toBe(false);
    expect(hasHttpScheme("ws://127.0.0.1:19876")).toBe(false);
  });
});

describe("gatewayAddressOptions", () => {
  const LAN = "http://10.0.0.5:19876";
  const OTHER = "http://192.168.3.10:19876";

  it("always offers localhost first, even with no history", () => {
    expect(gatewayAddressOptions(LAN, [], null)).toEqual([LOCAL_GATEWAY_URL, LAN]);
  });

  it("orders localhost → last-good → current → history", () => {
    expect(gatewayAddressOptions(LAN, [OTHER], OTHER)).toEqual([
      LOCAL_GATEWAY_URL,
      OTHER,
      LAN,
    ]);
  });

  it("deduplicates and normalizes before comparing", () => {
    expect(
      gatewayAddressOptions(`${LAN}/`, [LAN, "  http://192.168.3.10:19876/  "], LAN),
    ).toEqual([LOCAL_GATEWAY_URL, LAN, OTHER]);
  });

  it("drops scheme-less entries instead of offering an address that cannot connect", () => {
    expect(
      gatewayAddressOptions(LAN, ["abc", "10.0.0.9:19876"], "nonsense"),
    ).toEqual([LOCAL_GATEWAY_URL, LAN]);
  });

  it("collapses to a single row when the current address is localhost", () => {
    expect(gatewayAddressOptions(LOCAL_GATEWAY_URL, [], null)).toEqual([
      LOCAL_GATEWAY_URL,
    ]);
  });
});

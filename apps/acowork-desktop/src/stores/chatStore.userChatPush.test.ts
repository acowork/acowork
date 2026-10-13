/**
 * The Desktop half of the user-chat wake-up path (design doc 25 §8).
 *
 * Wire contract: `acowork-user` publishes an EMPTY, non-retained doorbell on
 * `acowork/users/{user_id}/chat/changed` after every send and every read. The
 * Rust eventloop forwards it as the Tauri `user-chat-changed` event with the
 * account segment taken from the topic (see `parse_user_chat_changed_topic` in
 * `src-tauri/src/commands/chat_mqtt.rs`), and [chatStore](chatStore.ts) turns
 * one delivery into one store re-read — for the signed-in account only.
 *
 * Why the "is this mine" test lives in the webview:
 *  - `ALL_TOPIC_FILTERS` is a compile-time table re-subscribed on every CONNACK,
 *    while the account can change hands without a reconnect, so what is
 *    subscribed is the wildcard `acowork/users/+/chat/changed`;
 *  - `connect_mqtt`'s `user_id` is the MQTT *client id* (the plain transport
 *    hardcodes `default`), not the account, so Rust cannot make that call;
 *  - the wildcard reveals nothing new: the Desktop already lists every account
 *    id through `GET /api/users/directory`.
 *
 * This file pins the wiring. The reload rule itself lives in
 * [userChatStore.test.ts](userChatStore.test.ts):
 *  1. the listener is registered on the channel the Rust side emits;
 *  2. my signal re-reads, someone else's does not — the payload names nothing,
 *     so a foreign doorbell would drive this device's reads for no reason;
 *  3. the connect edge re-reads once, because doorbells that fired while we
 *     were unsubscribed are gone for good (non-retained);
 *  4. the bridge coming up stands the inbox's fallback timer down and a loss
 *     re-arms it — and neither may empty the inbox it is protecting.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  // `settingsStore` syncs the persisted log level into the module on startup,
  // and the store graph behind `chatStore` pulls it at import time.
  setLevel: () => {},
}));

type Handler = (event: { payload: unknown }) => void;

const { mockInvoke, mockListen, registeredListeners } = vi.hoisted(() => {
  const registeredListeners = new Map<string, Handler[]>();
  const mockInvoke = vi.fn(async (..._args: unknown[]): Promise<unknown> => undefined);
  const mockListen = vi.fn(async (channel: string, handler: Handler) => {
    const arr = registeredListeners.get(channel) ?? [];
    arr.push(handler);
    registeredListeners.set(channel, arr);
    return () => {
      registeredListeners.set(
        channel,
        (registeredListeners.get(channel) ?? []).filter((h) => h !== handler),
      );
    };
  });
  return { mockInvoke, mockListen, registeredListeners };
});

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mockListen }));

import { useAuthStore } from "./authStore";
import { disposeMqttListener, initMqttListener } from "./chatStore";
import { POLL_MS, useUserChatStore } from "./userChatStore";
import type { UserAccount, UserChatMessage, UserChatSummary } from "../lib/types";

const SELF = "bbbbbbbb-0000-4000-8000-000000000002";
const PEER = "aaaaaaaa-0000-4000-8000-000000000001";
const CHAT = `${PEER}__${SELF}`;

const self: UserAccount = {
  user_id: SELF,
  username: "bob",
  display_name: "Bob",
  role: "admin",
  language: "en",
  timezone: "UTC",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

/** Every URL the inbox asked for, so a test can count the re-reads. */
let urls: string[] = [];

/**
 * Answer both inbox endpoints from one frozen snapshot: the thread page always
 * reports the same `total` it is handed, so a wake-up that changes nothing is
 * provably a no-op rather than a resync.
 */
function stubFetch(
  page: { messages?: UserChatMessage[]; total?: number; chats?: UserChatSummary[] } = {},
) {
  const messages = page.messages ?? [];
  const total = page.total ?? messages.length;
  const chats = page.chats ?? [];
  urls = [];
  globalThis.fetch = vi.fn(async (input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input.toString();
    urls.push(url);
    const body = url.endsWith("/chats")
      ? { chats }
      : { chat_id: CHAT, messages, total, offset: 0, limit: 50 };
    return {
      ok: true,
      status: 200,
      json: async () => body,
    } as unknown as Response;
  }) as unknown as typeof fetch;
}

/** Chat-list reads only — `…/chats/{id}/messages` must not blur in. */
const chatReads = () => urls.filter((u) => u.endsWith("/chats")).length;

function emit(channel: string, payload: unknown = {}): void {
  for (const handler of registeredListeners.get(channel) ?? []) {
    handler({ payload });
  }
}

const signalFor = (userId: string) => emit("user-chat-changed", { user_id: userId });

const signedIn = () =>
  useAuthStore.setState({
    mode: "multi_user",
    status: "logged_in",
    account: self,
    accessToken: "at",
  });

const signedOut = () => useAuthStore.setState({ mode: "local", status: "disabled", account: null });

beforeEach(() => {
  vi.useRealTimers();
  // Sign out first: the fallback loop is owned by the session, so this is what
  // guarantees no interval survives from the previous test into this one.
  signedOut();
  disposeMqttListener();
  registeredListeners.clear();
  mockInvoke.mockReset();
  mockListen.mockClear();
  // Connected by default, so the init path applies an idle → connected edge and
  // takes the same branch a live Desktop does after the Gateway answers.
  mockInvoke.mockImplementation(async (cmd: unknown) =>
    cmd === "get_mqtt_status" ? { known: true, connected: true } : undefined,
  );
  stubFetch();
  signedIn();
});

afterEach(() => {
  vi.useRealTimers();
  signedOut();
  disposeMqttListener();
});

describe("user-chat wake-up fanout", () => {
  it("registers a listener on the channel the Rust eventloop emits", async () => {
    await initMqttListener();
    expect(registeredListeners.get("user-chat-changed")?.length ?? 0).toBeGreaterThanOrEqual(1);
  });

  it("re-reads the inbox when MY account's signal arrives", async () => {
    await initMqttListener();
    // The connect edge already read once, so take a baseline the emit can move.
    await vi.waitFor(() => expect(chatReads()).toBeGreaterThan(0));
    const before = chatReads();

    signalFor(SELF);

    await vi.waitFor(() => expect(chatReads()).toBe(before + 1));
  });

  it("ignores a signal that belongs to another account", async () => {
    await initMqttListener();
    await vi.waitFor(() => expect(chatReads()).toBeGreaterThan(0));
    const before = chatReads();

    // The wildcard filter delivers every account's doorbell. Acting on one
    // would let anyone's traffic drive this device's reads — and because the
    // payload is empty, nothing would ever look wrong.
    signalFor(PEER);
    signalFor(PEER);

    expect(chatReads()).toBe(before);
    await new Promise((r) => setTimeout(r, 0));
    expect(chatReads()).toBe(before);
  });

  it("ignores every signal while nobody is signed in", async () => {
    signedOut();
    await initMqttListener();
    urls = [];

    signalFor(SELF);

    expect(urls).toHaveLength(0);
  });

  it("re-reads on the connect edge, for the doorbells missed while unsubscribed", async () => {
    await initMqttListener();
    await vi.waitFor(() => expect(chatReads()).toBeGreaterThan(0));
    const connected = chatReads();

    emit("mqtt-status", { connected: false, reconnecting: true });
    emit("mqtt-status", { connected: true });

    await vi.waitFor(() => expect(chatReads()).toBeGreaterThan(connected));
  });

  it("dispose unregisters the listener, so a remount cannot double-read", async () => {
    await initMqttListener();
    await vi.waitFor(() => expect(chatReads()).toBeGreaterThan(0));
    disposeMqttListener();
    const afterDispose = chatReads();

    signalFor(SELF);

    expect(chatReads()).toBe(afterDispose);
    await new Promise((r) => setTimeout(r, 0));
    expect(chatReads()).toBe(afterDispose);
  });
});

describe("push channel vs the inbox fallback poll", () => {
  it("the bridge coming up stands the timer down, and a loss re-arms it", async () => {
    // Start from the degraded state the poll exists for: signed in, no bridge.
    signedOut(); // stops whatever loop the beforeEach armed, on real timers
    vi.useFakeTimers();
    stubFetch();
    signedIn(); // re-arms it, under the fake clock, and reads once immediately

    await vi.advanceTimersByTimeAsync(POLL_MS * 2);
    expect(chatReads()).toBeGreaterThan(1); // the timer, not a signal, is reading

    // MQTT connects: the doorbell takes over, so the timer must stop.
    await initMqttListener();
    const afterConnect = chatReads();
    expect(afterConnect).toBeGreaterThan(1); // and it read once to catch up

    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(chatReads()).toBe(afterConnect); // no tick fires behind the push path

    // The bridge drops. The degraded path has to converge again, just slower.
    emit("mqtt-status", { connected: false });
    const afterLoss = chatReads();
    await vi.advanceTimersByTimeAsync(POLL_MS * 2);
    expect(chatReads()).toBeGreaterThan(afterLoss);
  });

  it("a bridge that comes up does not empty the inbox it is replacing", async () => {
    // The bug this forecloses: standing the poll down used to be the same
    // branch as signing out, so a successful MQTT connect cleared the thread
    // the reader was looking at. Only a session change may reset the store.
    signedOut();
    vi.useFakeTimers();
    const held: UserChatMessage = { ts: 10, from: PEER, kind: "text", body: "hi" };
    const row: UserChatSummary = {
      chat_id: CHAT,
      peer_user_id: PEER,
      peer_display_name: "Alice",
      last_active_at: 10,
      last_message_preview: "hi",
      unread_count: 0,
    };
    stubFetch({ messages: [held], total: 1, chats: [row] });
    signedIn();
    useUserChatStore.setState({
      chats: [row],
      activePeerId: PEER,
      messages: [held],
      messagesTotal: 1,
      unseenCount: 0,
    });

    await initMqttListener();
    await vi.advanceTimersByTimeAsync(0); // let the catch-up read settle

    const s = useUserChatStore.getState();
    expect(s.activePeerId).toBe(PEER);
    expect(s.messages).toEqual([held]); // the page is still on screen
    expect(s.chats).toHaveLength(1); // and the inbox still has the thread
  });
});

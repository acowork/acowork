/**
 * The execution-session bubble for an AGENT-ORIGIN turn (design 27 §5.4.5).
 *
 * Why this test exists (the reported defect): an execution session writes every
 * inbound turn as a `user` row, and for an agent-origin intent that row's
 * `content` is the MACHINE envelope — `[intent:…:action] {"project_id":…}` plus
 * the reply coordinates the model needs. The bubble printed it verbatim, so a
 * PM's transcript showed JSON where the peer had written a brief.
 *
 * The Runtime now resolves the human projection once, at the transport boundary,
 * and returns it as `display`; the client only transports and paints it. The
 * contract pinned here:
 *   1. `display.body` is what a person reads, rendered as markdown — even though
 *      the row's role is `user` and a person's own words are literal text;
 *   2. the envelope stays reachable, folded, because it is the only text holding
 *      the reply coordinates;
 *   3. the skin follows the words, not the role: an Agent's markdown in the accent
 *      bubble is the "user text turned black" defect (`speakerBubble.ts`);
 *   4. a row with no display (a human turn, or one written before the projection
 *      existed) keeps today's literal rendering, unchanged.
 */
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import type { ChatMessage, IntentDisplay } from "../../lib/types";

// The avatars are not under test, and `AgentAvatar` resolves its image through
// the gateway's avatar-file endpoint, which no test network serves.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <span data-testid="agent-avatar" />,
}));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <span data-testid="user-avatar" />,
  BUILTIN_ICONS: {},
}));

const ENVELOPE =
  '[intent:2f3c8a11:create_dev_tasks] {"project_id":"p-66e68d1a","request":"拆任务"}\n' +
  "[conversation_id: sess-1] [message_id: msg-9]";

const DISPLAY: IntentDisplay = {
  origin: "request",
  action: "create_dev_tasks",
  summary: "拆解 v0.2 任务",
  body: "### 拆解 v0.2 任务\n\n- **project_id**: p-66e68d1a\n- **request**: 拆任务",
};

function intentRow(display: IntentDisplay | undefined): ChatMessage {
  return {
    id: "u1",
    type: "user",
    content: ENVELOPE,
    timestamp: 1_700_000_000_000,
    senderDisplayName: "PM",
    display,
  };
}

function renderRow(display: IntentDisplay | undefined) {
  return render(
    <MessageBubble
      message={intentRow(display)}
      currentSessionId="s1"
      liveUserName="PM"
      // The row's role is `user`, so the surface resolves the speaker as a person.
      // The display has to override that on its own.
      speakerKind="user"
      senderAgentId={null}
    />,
  );
}

describe("an agent-origin intent turn in the execution transcript", () => {
  it("prints the display body as markdown instead of the machine envelope", () => {
    const { container } = renderRow(DISPLAY);

    expect(container.querySelector(".prose")).not.toBeNull();
    expect(container.querySelector("h3")?.textContent).toContain("拆解 v0.2 任务");
    expect(container.querySelector("strong")?.textContent).toContain("project_id");
    // The painted prose is the projection — the envelope is nowhere in it.
    const prose = container.querySelector(".prose")!;
    expect(prose.textContent).not.toContain("[intent:2f3c8a11");
    expect(prose.textContent).not.toContain('"project_id":"p-66e68d1a"');
    expect(prose.textContent).toContain("拆解 v0.2 任务");
  });

  it("keeps the envelope one click away — it holds the reply coordinates", () => {
    const { container } = renderRow(DISPLAY);

    const details = container.querySelector("details");
    expect(details).not.toBeNull();
    // Folded, not shown: the machine format is the model's, not the reader's.
    expect(details!.hasAttribute("open")).toBe(false);
    expect(details!.textContent).toContain("[intent:2f3c8a11");
    expect(container.querySelector("details pre")?.textContent).toContain("sess-1");
  });

  it("takes the agent skin — markdown in the accent bubble is the black-text defect", () => {
    const { container } = renderRow(DISPLAY);

    expect(container.querySelector(".bg-chat-bubble")).not.toBeNull();
    expect(container.querySelector(".bg-chat-user")).toBeNull();
    // Still on the initiator's side: only the skin follows the writer.
    expect(
      container.querySelector<HTMLElement>(".bg-chat-bubble")!.className,
    ).toContain("rounded-br-sm");
  });

  it("says so when the producer cut the body", () => {
    const { container } = renderRow({ ...DISPLAY, truncated: true });
    expect(container.textContent).toContain("chatView.block.displayTruncated");
  });

  it("leaves a row without a display on the literal rendering it had before", () => {
    // A human turn, or any row written before the projection existed.
    const { container } = renderRow(undefined);

    expect(container.querySelector(".prose")).toBeNull();
    expect(container.querySelector("details")).toBeNull();
    expect(container.textContent).toContain("[intent:2f3c8a11");
    expect(container.querySelector(".bg-chat-user")).not.toBeNull();
  });
});

/**
 * GroupResponderCard — the group's **default responder** as a settings card
 * (ADR-105, design 27 §5.3.1 revision), sharing the right-panel card grammar
 * (`ListBox` box card + `ExpandableRow` collapsible header) with the sibling
 * members card and [GroupChatPromptCard](GroupChatPromptCard.tsx).
 *
 * The fact it edits is one field of the chat session
 * (`acowork-user`, `chat-sessions/{id}/responder`): which agent instance an
 * un-@'d human message in this group is delivered to. The bug it closes is a
 * silence — a human answering the agent that just spoke, without typing "@",
 * had their message archived and nothing answered it.
 *
 * Ownership of the decision: the STORE validates every write (kind is a group,
 * caller is a participant, caller is owner-or-admin, the named id is a typed
 * agent member *right now*), and the Gateway re-reads the role against the live
 * roster before delivering. The card therefore only mirrors the server's
 * `editable` answer instead of re-deriving the governance rule client-side —
 * one authority, no drift.
 *
 * Read is open to every participant (a member should be able to see who picks
 * up an un-addressed message); write renders disabled for the rest, which is
 * the same covenant `chat-md` offers.
 *
 * The candidate list is the caller's: the panels already resolve participant
 * ids to display names through the agent directory, and re-deriving that here
 * would duplicate the resolution (and its fallbacks) on a third surface.
 */
import { useCallback, useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAuthStore } from "../../stores/authStore";
import { getGatewayUrl } from "../../lib/config";
import { showToast } from "../common/ToastProvider";
import { ListBox, ExpandableRow } from "../common/list";
import { Dropdown } from "../common/Dropdown";

/** One agent member of the group, as the panel resolved it. */
export interface ResponderCandidate {
  id: string;
  name: string;
}

interface ResponderResponse {
  responder: string | null;
  editable: boolean;
}

async function loadResponder(sessionId: string): Promise<ResponderResponse> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${sessionId}/responder`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!resp.ok) throw new Error(`GET responder -> ${resp.status}`);
  return resp.json();
}

/** Whole-value replace, `null` = clear (`{"responder": null}`). */
async function saveResponder(
  sessionId: string,
  responder: string | null,
): Promise<ResponderResponse> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${sessionId}/responder`, {
    method: "PUT",
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify({ responder }),
  });
  if (!resp.ok) throw new Error(`PUT responder -> ${resp.status}`);
  return resp.json();
}

export function GroupResponderCard({
  sessionId,
  candidates,
}: {
  sessionId: string;
  candidates: ResponderCandidate[];
}) {
  const { t } = useTranslation();
  const [responder, setResponder] = useState<string | null>(null);
  const [editable, setEditable] = useState(false);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(true);

  const load = useCallback(async () => {
    if (!sessionId) return;
    setLoading(true);
    setError(null);
    // Drop the previous group's answer before the new GET lands, so a slow
    // response never shows one group's responder (or rights) on another.
    setResponder(null);
    setEditable(false);
    try {
      const data = await loadResponder(sessionId);
      setResponder(data.responder ?? null);
      setEditable(!!data.editable);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [sessionId]);

  useEffect(() => {
    void load();
  }, [load]);

  const pick = async (value: string) => {
    const next = value || null;
    setBusy(true);
    try {
      const data = await saveResponder(sessionId, next);
      setResponder(data.responder ?? null);
      showToast({ type: "success", message: t("groupResponder.saved") });
    } catch (cause) {
      // Keep the server's answer (the select snaps back to the stored value):
      // a refused write must not leave the panel claiming the change landed.
      showToast({
        type: "error",
        message: `${t("groupResponder.saveFailed")} (${
          cause instanceof Error ? cause.message : String(cause)
        })`,
      });
      await load();
    } finally {
      setBusy(false);
    }
  };

  const named = responder ? candidates.find((c) => c.id === responder) : undefined;
  // A responder that is not (or no longer) an agent member: the store clears
  // the role on removal, so this is only reachable in a narrow window (another
  // device removed the member, or the roster drifted). Say so instead of
  // silently rendering "unset" over a stored id.
  const stale = !!responder && !named;

  const badge = loading || error ? null : (
    <span className="shrink-0 rounded bg-panel-inset px-1.5 py-0.5 text-10 text-text-tertiary">
      {named ? named.name : t("groupResponder.valueNone")}
    </span>
  );

  return (
    <ListBox dividers={false} className="mt-3" data-testid="group-responder">
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={t("groupResponder.title")}
        ariaLabel={t("groupResponder.title")}
        meta={badge}
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
      >
        {loading ? (
          <div className="flex items-center justify-center py-4 text-text-tertiary">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          </div>
        ) : error ? (
          <p className="text-10 text-red-500">{error}</p>
        ) : (
          <div className="space-y-2">
            <p className="text-10 leading-relaxed text-text-tertiary">
              {t(editable ? "groupResponder.hint" : "groupResponder.readOnlyHint")}
            </p>
            <Dropdown
              size="small"
              value={responder ?? ""}
              disabled={!editable || busy}
              onChange={(v) => void pick(v)}
              aria-label={t("groupResponder.title")}
              data-testid="group-responder-select"
              options={[
                { value: "", label: t("groupResponder.none") },
                ...candidates.map((c) => ({ value: c.id, label: c.name })),
                // A stored id that no longer resolves keeps its own option, so
                // the select shows the truth (and the warning below explains
                // it) instead of silently rendering as "none".
                ...(stale && responder ? [{ value: responder, label: responder.slice(0, 8) }] : []),
              ]}
            />
            {stale && <p className="text-10 text-amber-600">{t("groupResponder.missing")}</p>}
          </div>
        )}
      </ExpandableRow>
    </ListBox>
  );
}
/**
 * GroupChatPromptCard — the group's `chat.md` as a unified settings card
 * (design 29 §3.1, ADR-099 extended), sharing the right-panel card grammar
 * (`ListBox` box card + `ExpandableRow` collapsible header) with the agent
 * Settings tab's Agent Info / Parameters cards and the Debug tab's
 * Prompts & Skills card.
 *
 * The whole chat-layer prompt of this group lives in the session's `chat.md`
 * (acowork-user, `chat-sessions/{id}/chat.md`) and replaces the built-in one
 * for every agent that answers here. The backend seeds the file with the
 * built-in default on its first read and hides the non-trimmable `# Safety`
 * tail from the API, so the prompt the card hands to the editor is exactly the
 * editable body.
 *
 * Editing happens in the shared Monaco file tab, not an inline textarea — the
 * same interaction as the agent's own prompt files. The card's "edit" action
 * opens that tab through `fileEditorStore.openGroupChatMd`, and Save routes
 * back through the chat-prompt endpoint. "Restore default" drops the override
 * (`DELETE .../chat-md`) so the next read re-seeds the built-in default, and
 * flips the badge back without leaving the panel.
 *
 * Read is open to every participant (a stranger gets a hidden 404), but the
 * server decides who may write it — the group owner or an admin (ADR-098
 * governance). Only that set sees the edit / restore controls; a plain member
 * reads the status without being handed a save the backend would refuse (403).
 */
import { useCallback, useEffect, useState } from "react";
import { Loader2, Pencil, RotateCcw } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAuthStore } from "../../stores/authStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { getGatewayUrl } from "../../lib/config";
import { showToast } from "../common/ToastProvider";
import { ListBox, ExpandableRow } from "../common/list";

/** Inline pre-check only; the server is the authority (it answers 413). Kept
 *  in sync with `acowork_core::chat_prompt::MAX_CHAT_PROMPT_BYTES`. */
const MAX_BYTES = 16 * 1024;

interface ChatMdResponse {
  content: string;
  is_default: boolean;
  editable: boolean;
}

async function loadChatMd(sessionId: string): Promise<ChatMdResponse> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${sessionId}/chat-md`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!resp.ok) throw new Error(`GET chat-md -> ${resp.status}`);
  return resp.json();
}

/** Reset to the built-in default: 204 has no body, so this is a bare ok-check
 *  (not `loadChatMd`'s JSON read). */
async function resetChatMd(sessionId: string): Promise<void> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${sessionId}/chat-md`, {
    method: "DELETE",
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!resp.ok) throw new Error(`DELETE chat-md -> ${resp.status}`);
}

export function GroupChatPromptCard({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const openGroupChatMd = useFileEditorStore((s) => s.openGroupChatMd);
  const [content, setContent] = useState("");
  const [isDefault, setIsDefault] = useState(true);
  const [editable, setEditable] = useState(false);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(true);

  const load = useCallback(async () => {
    if (!sessionId) return;
    setLoading(true);
    setError(null);
    // Drop the previous session's answer before the new GET lands, so a slow
    // response never lets one group's text or rights show on another.
    setEditable(false);
    try {
      const data = await loadChatMd(sessionId);
      setContent(data.content ?? "");
      setIsDefault(!!data.is_default);
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

  const edit = () => openGroupChatMd(sessionId, content);

  const restore = async () => {
    setBusy(true);
    try {
      await resetChatMd(sessionId);
      await load();
      showToast({ type: "success", message: t("groupChatPrompt.restored") });
    } catch (cause) {
      showToast({
        type: "error",
        message: `${t("groupChatPrompt.restoreFailed")} (${
          cause instanceof Error ? cause.message : String(cause)
        })`,
      });
    } finally {
      setBusy(false);
    }
  };

  const bytes = new TextEncoder().encode(content).length;

  const badge =
    loading || error ? null : (
      <span className="shrink-0 rounded bg-panel-inset px-1.5 py-0.5 text-10 text-text-tertiary">
        {isDefault ? t("groupChatPrompt.isDefault") : t("groupChatPrompt.custom")}
      </span>
    );

  // Edit / restore are owner-or-admin affordances only. They live in the card
  // header (visible whether collapsed or not) and stop propagation so a click
  // never toggles the collapse.
  const trailing = editable ? (
    <span
      className="flex shrink-0 items-center gap-1"
      onClick={(e) => e.stopPropagation()}
      role="group"
      aria-label={t("groupChatPrompt.title")}
    >
      <button
        type="button"
        onClick={edit}
        aria-label={t("groupChatPrompt.edit")}
        title={t("groupChatPrompt.edit")}
        data-testid="group-chat-prompt-edit"
        className="flex h-5 w-5 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-zinc-600 dark:hover:text-zinc-300"
      >
        <Pencil className="h-3 w-3" />
      </button>
      {!isDefault && (
        <button
          type="button"
          disabled={busy}
          onClick={() => void restore()}
          aria-label={t("groupChatPrompt.restore")}
          title={t("groupChatPrompt.restore")}
          data-testid="group-chat-prompt-restore"
          className="flex h-5 w-5 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-zinc-600 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:text-zinc-300"
        >
          {busy ? (
            <Loader2 className="h-3 w-3 animate-spin" />
          ) : (
            <RotateCcw className="h-3 w-3" />
          )}
        </button>
      )}
    </span>
  ) : undefined;

  return (
    <ListBox dividers={false} className="mt-3" data-testid="group-chat-prompt">
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={t("groupChatPrompt.title")}
        ariaLabel={t("groupChatPrompt.title")}
        meta={badge}
        trailing={trailing}
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
              {t(editable ? "groupChatPrompt.hint" : "groupChatPrompt.readOnlyHint")}
            </p>
            <div className="flex items-center justify-between gap-2">
              <span className={`text-10 ${bytes > MAX_BYTES ? "text-red-500" : "text-text-tertiary"}`}>
                {bytes} / {MAX_BYTES}
              </span>
              {editable && (
                <button
                  type="button"
                  onClick={edit}
                  className="inline-flex items-center gap-1 rounded border border-border-outer px-2 py-0.5 text-10 text-text-secondary transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-700"
                >
                  <Pencil className="h-3 w-3" />
                  {t("groupChatPrompt.edit")}
                </button>
              )}
            </div>
          </div>
        )}
      </ExpandableRow>
    </ListBox>
  );
}

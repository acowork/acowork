/**
 * The composer's Send/Stop button decision, as a pure function.
 *
 * One control, two meanings. While a turn is running the button still SENDS
 * when the composer holds a draft — the one-step send: the message is posted
 * with `interrupt` and the Runtime aborts the turn that was running when it
 * arrived (see `SessionManager::send_to_session_superseding`). Only a running
 * turn with an EMPTY composer means "stop".
 *
 * This used to be a three-way inline ternary (queue / send-queue-and-stop /
 * stop) duplicated across the icon, the click handler, the tooltip and the
 * aria-label. Extracting it gives the decision a test boundary of its own —
 * the disabled rule in particular is load-bearing: while the button means
 * STOP it must stay live even when the connection drops or an attachment is
 * still uploading, because it is the only control that can halt a running
 * turn. Those conditions gate SENDING only.
 */

export type ComposerButtonMode = "send" | "stop";

export interface ComposerButtonInput {
  /** Backend phase: a turn is running (`isProcessing(sessionStatus)`). */
  sending: boolean;
  /** The composer holds text or a resolved attachment. */
  hasDraft: boolean;
  /** An attachment is still uploading — the payload cannot be assembled. */
  hasUploading: boolean;
  /** ADR-076 §决策 4: the session is shared with us read-only. */
  readOnly: boolean;
  /** The event connection is up. */
  connected: boolean;
}

export interface ComposerButtonState {
  mode: ComposerButtonMode;
  disabled: boolean;
  /** i18n key under `chatPanel`, used for both the tooltip and aria-label. */
  labelKey: "stop" | "sendAndInterrupt" | "sendMessage";
}

export function composerButtonState(input: ComposerButtonInput): ComposerButtonState {
  const mode: ComposerButtonMode = input.sending && !input.hasDraft ? "stop" : "send";
  // A read-only session can neither send nor stop someone else's stream.
  // Otherwise: STOP is never disabled by upload/connection state; SEND is.
  const disabled =
    input.readOnly || (mode === "stop" ? false : input.hasUploading || !input.connected);
  const labelKey =
    mode === "stop" ? "stop" : input.sending ? "sendAndInterrupt" : "sendMessage";
  return { mode, disabled, labelKey };
}
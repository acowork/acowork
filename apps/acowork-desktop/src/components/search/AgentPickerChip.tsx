/**
 * AgentPickerChip — compact agent scope selector for GlobalSearchDialog.
 *
 * Layout:
 *   - Closed: a chip-style button showing only the agent's avatar and a
 *     chevron-down icon (no name, to keep zone 1 visually quiet).
 *   - Open:   a popover anchored to the chip, listing every alive agent
 *     with its full avatar + display name (mirrors AgentList rows).
 *
 * Click outside / Escape closes the popover without affecting the parent
 * dialog's own Escape-to-close behaviour — the parent's listener runs at
 * the dialog root and we stop propagation in the chip's own key handler.
 */

import { useEffect, useRef, useState } from "react";
import { ChevronDown } from "lucide-react";
import { AgentAvatar } from "../common/AgentAvatar";
import type { AgentInfo } from "../../lib/types";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";

export interface AgentPickerChipProps {
    agents: AgentInfo[];
    value: string; // instance_id
    onChange: (instanceId: string) => void;
}

export function AgentPickerChip({ agents, value, onChange }: AgentPickerChipProps) {
    const [open, setOpen] = useState(false);
    const wrapRef = useRef<HTMLDivElement>(null);

    const selected = agents.find((a) => a.instance_id === value) ?? agents[0];

    // Close on outside click / Escape.
    useEffect(() => {
        if (!open) return;
        const onDown = (e: MouseEvent) => {
            if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
        };
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.stopPropagation();
                setOpen(false);
            }
        };
        // Capture phase: the parent dialog's `onMouseDown` calls
        // stopPropagation on any click inside the dialog, which kills
        // the native event before it bubbles to `window`. Listening in
        // capture runs before the React bubble so the outside-click
        // detector still fires when the user clicks the search input,
        // results list, etc.
        window.addEventListener("mousedown", onDown, true);
        window.addEventListener("keydown", onKey, true);
        return () => {
            window.removeEventListener("mousedown", onDown, true);
            window.removeEventListener("keydown", onKey, true);
        };
    }, [open]);

    return (
        <div ref={wrapRef} className="relative shrink-0">
            <button
                type="button"
                data-testid="agent-picker-chip"
                aria-haspopup="listbox"
                aria-expanded={open}
                onMouseDown={(e) => {
                    // Don't let mousedown bubble to the dialog backdrop
                    // (which would close the dialog).
                    e.stopPropagation();
                }}
                onClick={() => setOpen((p) => !p)}
                className="flex h-full items-center gap-1.5 px-5 py-1.5 min-h-[var(--ui-dialog-zone-h)] outline-none hover:bg-zinc-100 focus-visible:bg-zinc-100 dark:hover:bg-zinc-700/50 dark:focus-visible:bg-zinc-700/50"
            >
                {selected ? (
                    <AgentAvatar
                        agentId={selected.instance_id}
                        avatarUrl={selected.avatar}
                        builtinAvatarId={selected.builtin_avatar}
                        version={selected.version}
                        size={20}
                    />
                ) : null}
                <ChevronDown
                    className={`h-3 w-3 text-text-tertiary transition-transform ${open ? "rotate-180" : ""}`}
                />
            </button>

            {open && (
                <ul
                    role="listbox"
                    data-testid="agent-picker-popover"
                    className="absolute left-0 top-full z-10 mt-1 max-h-72 w-72 overflow-y-auto rounded-md border border-border-outer bg-modal-surface py-1 shadow-lg"
                >
                    {agents.map((a) => {
                        const isSel = a.instance_id === value;
                        return (
                            <li
                                key={a.instance_id}
                                role="option"
                                aria-selected={isSel}
                                data-testid="agent-picker-option"
                                onMouseDown={(e) => {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    onChange(a.instance_id);
                                    setOpen(false);
                                }}
                                className={`flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs ${
                                    isSel
                                        ? `${listRowSelected} text-text`
                                        : `text-text-secondary ${listRowHover}`
                                }`}
                            >
                                <AgentAvatar
                                    agentId={a.instance_id}
                                    avatarUrl={a.avatar}
                                    builtinAvatarId={a.builtin_avatar}
                                    version={a.version}
                                    size={24}
                                />
                                <div className="min-w-0 flex-1">
                                    <div className="truncate font-medium text-text">
                                        {a.display_name || a.name || a.instance_id}
                                    </div>
                                    <div className="truncate font-mono text-10 text-text-tertiary">
                                        {a.agent_id}
                                    </div>
                                </div>
                            </li>
                        );
                    })}
                </ul>
            )}
        </div>
    );
}
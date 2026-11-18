import { type ReactNode } from "react";

interface RadioOption<T> {
    label: ReactNode;
    value: T;
    /** Optional fill for the UNSELECTED dot (e.g. a theme pack's preview
     *  tint, so the row shows what picking it does). The selected dot keeps
     *  the accent treatment — selection must stay louder than preview. */
    dotColor?: string;
    /** Optional glyph shown next to the label (e.g. Sun/Moon/Monitor for the
     *  theme modes) so the row reads as a named state, not a bare radio.
     *  Mirrors the title-bar appearance menu's theme rows. */
    icon?: ReactNode;
}

interface RadioGroupProps<T> {
    name: string;
    value: T;
    options: RadioOption<T>[];
    onChange: (value: T) => void;
    noWrap?: boolean;
}

export function RadioGroup<T>({
    name,
    value,
    options,
    onChange,
    noWrap,
}: RadioGroupProps<T>) {
    return (
        <div className={"flex gap-[var(--ui-radio-gap)] " + (noWrap ? "flex-nowrap" : "flex-wrap")}>
            {options.map((opt) => (
                <label
                    key={String(opt.value)}
                    className="flex cursor-pointer items-center gap-[var(--ui-radio-label-gap)] text-xs"
                >
                    <span className="relative flex items-center justify-center">
                        <input
                            type="radio"
                            name={name}
                            value={String(opt.value)}
                            checked={value === opt.value}
                            onChange={() => onChange(opt.value)}
                            className="peer sr-only"
                        />
                        <span
                            className="block h-[var(--ui-radio-size)] w-[var(--ui-radio-size)] rounded-full bg-[var(--ui-radio-bg)] peer-checked:bg-[var(--color-accent)] peer-checked:ring-2 peer-checked:ring-[var(--color-accent)] peer-checked:ring-offset-1 peer-checked:ring-offset-white dark:peer-checked:ring-offset-zinc-800"
                            // Only while unselected: an inline colour would
                            // outrank the `peer-checked:` accent classes and
                            // silence the selection signal.
                            style={
                                opt.dotColor && value !== opt.value
                                    ? { backgroundColor: opt.dotColor }
                                    : undefined
                            }
                        />
                    </span>
                    {opt.icon && (
                        <span className="shrink-0 text-text-tertiary">{opt.icon}</span>
                    )}
                    {opt.label}
                </label>
            ))}
        </div>
    );
}

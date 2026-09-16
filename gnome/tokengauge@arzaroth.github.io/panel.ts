// The snapshot `tokengauge --json` returns, as this side reads it.
//
// The binary decides what the panel contains; these are the shapes it arrives
// in. Written out by hand rather than generated, because the other side of
// this contract is a Rust struct - which is also why the fields are named
// exactly as the JSON names them, and why only the fields this frontend reads
// are declared.

/// A semantic tier, not a colour. `_toneColor` maps it onto the shell theme.
export type Tone = 'normal' | 'dim' | 'good' | 'warn' | 'critical';

/// How a section's rows are drawn. One builder per kind; adding a kind here is
/// the compiler's way of asking for the builder that goes with it.
export type SectionKind = 'meters' | 'bars' | 'rows';

export interface SectionRow {
    label: string;
    value: string;
    suffix: string;
    badge: string;
    badge_tone: Tone;
    footnote: string;
    /// Bar fill, 0 to 1. Null draws no bar.
    fraction: number | null;
    tone: Tone;
    emphasized: boolean;
    tooltip: string;
}

export interface Section {
    id: string;
    title: string;
    kind: SectionKind;
    rows: SectionRow[];
}

export interface HistoryPoint {
    key: string;
    label: string;
    full_label: string;
    usd: string;
    tokens: string;
    fraction: number;
    /// The step still in progress: short because it is not over, not because
    /// spend collapsed.
    partial: boolean;
    tone: Tone;
}

export interface HistorySeries {
    id: string;
    label: string;
    points: HistoryPoint[];
    total_usd: string;
    total_tokens: string;
    average_usd: string;
    empty: boolean;
}

export interface HistoryPanel {
    series: HistorySeries[];
    covers: string;
    notes: string[];
}

export interface BarTooltipLine {
    label: string;
    value: string;
    tone: Tone;
}

export interface BarTooltip {
    title: string;
    lines: BarTooltipLine[];
}

/// The headline number and its tier, resolved by the core under the configured
/// window. This frontend used to pick the window and own the tier boundaries.
export interface Bar {
    percent: number | null;
    tone: Tone;
}

export interface Theme {
    dim: string;
    separator: string;
    green: string;
    yellow: string;
    red: string;
    neutral: string;
}

export interface FetchError {
    provider: string;
    message: string;
    raw: string;
}

export interface UpdateStatus {
    current: string;
    latest: string | null;
    available: boolean;
}

export interface Row {
    provider: string;
    label: string;
    glyph: string;
    color: string;
    icon_svg: string | null;
    plan_label: string | null;
    source: string | null;
    stale: boolean;
    updated: string | null;
    bar: Bar;
    panel: Section[];
    history: HistoryPanel;
    refresh_hint: string;
    bar_tooltip: BarTooltip;
}

export interface Snapshot {
    version: string;
    rows: Row[];
    errors: FetchError[];
    enabled: string[];
    providers: string[];
    primary: string;
    window: string;
    theme: Partial<Theme>;
    update: UpdateStatus | null;
    /// The few bytes the binary rewrites after every fetch. Watching it is what
    /// makes another frontend's fetch land here at once instead of on the next
    /// poll.
    revision_file: string;
}

/// The fallback palette, for a snapshot that has not arrived yet or one from a
/// binary too old to carry a theme.
export const FALLBACK_THEME: Theme = {
    dim: '#6c7086',
    separator: '#45475a',
    green: '#a6e3a1',
    yellow: '#f9e2af',
    red: '#f38ba8',
    neutral: '#cdd6f4',
};

/// An empty snapshot, for the moment before the first answer arrives.
export function emptySnapshot(): Snapshot {
    return {
        version: '',
        rows: [],
        errors: [],
        enabled: [],
        providers: [],
        primary: '',
        window: '',
        theme: {},
        update: null,
        revision_file: '',
    };
}

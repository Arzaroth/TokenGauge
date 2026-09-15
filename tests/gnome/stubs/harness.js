// What the stubs record, so a harness can answer a command or walk a widget
// tree without the extension having to hand either over.
//
// The GNOME equivalent of tests/qml's Harness.Registry, kept in one module
// because the toolkit stubs all need to reach it.

/// Every subprocess the extension started, newest last.
export const running = [];

/// Every GLib timeout it armed, by source id.
export const timeouts = new Map();

/// Files it asked to be told about.
export const monitors = [];

let nextId = 1;
export function takeId() {
    return nextId++;
}

export function reset() {
    running.length = 0;
    timeouts.clear();
    monitors.length = 0;
}

/// The most recently started command containing `needle`, or null.
export function find(needle) {
    for (let i = running.length - 1; i >= 0; i--) {
        if (running[i].commandLine.includes(needle))
            return running[i];
    }
    return null;
}

/// Fire every timeout armed under `kind`, as GLib would when it expires.
/// Repeating sources are re-armed exactly as the real loop re-arms them.
export function fireTimeouts() {
    for (const [id, entry] of [...timeouts]) {
        const again = entry.handler();
        if (!again)
            timeouts.delete(id);
    }
}

/// Every actor in the tree under `actor`, parents before children.
export function flatten(actor) {
    const out = [];
    const walk = a => {
        out.push(a);
        for (const child of a.children)
            walk(child);
    };
    walk(actor);
    return out;
}

/// Every string a user would read in this tree, in order.
export function texts(actor) {
    return flatten(actor)
        .filter(a => typeof a.text === 'string' && a.text !== '')
        .map(a => a.text);
}

/// The actors carrying `styleClass`, in tree order.
export function byStyle(actor, styleClass) {
    return flatten(actor).filter(a => (a.style_class || '').split(/\s+/).includes(styleClass));
}

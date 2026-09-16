// Resolves the imports GNOME Shell provides to the stubs beside this file.
//
// `gi://St` and `resource:///org/gnome/shell/...` are schemes the shell's own
// module loader understands and Node does not. A resolve hook is the whole
// trick: the compiled extension is imported unmodified, and what it gets back
// is a toolkit that records instead of drawing.

const STUBS = new URL('./stubs/', import.meta.url);

const MAP = {
    'gi://Clutter': 'Clutter.js',
    'gi://Gio': 'Gio.js',
    'gi://GLib': 'GLib.js',
    'gi://GObject': 'GObject.js',
    'gi://St': 'St.js',
    'resource:///org/gnome/shell/ui/main.js': 'shell/main.js',
    'resource:///org/gnome/shell/ui/panelMenu.js': 'shell/panelMenu.js',
    'resource:///org/gnome/shell/ui/popupMenu.js': 'shell/popupMenu.js',
    'resource:///org/gnome/shell/extensions/extension.js': 'shell/extension.js',
};

export function resolve(specifier, context, next) {
    const stub = MAP[specifier];
    if (stub)
        return {url: new URL(stub, STUBS).href, shortCircuit: true};
    return next(specifier, context);
}

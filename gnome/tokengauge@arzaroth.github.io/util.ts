// The two things both entry points need and neither owns.
//
// They were copied into `extension.ts` and `prefs.ts` verbatim, which is one
// copy too many for a function whose whole job is to be exactly right: a
// shell quote that drifts on one side is a command injection on that side
// alone, and nothing would have said so.

import Gio from 'gi://Gio';

/// A string as a single `sh` word. Everything this extension runs goes
/// through `sh -c`, because gnome-shell inherits a session PATH that often
/// lacks the directory the installer wrote the binary into, so every value
/// interpolated into that line has to survive the shell's parser.
export function shellQuote(s: string): string {
    return `'${String(s).replace(/'/g, "'\\''")}'`;
}

/// Whether a caught value is the cancellation GJS raises when a request is
/// superseded. GJS raises GLib.Error, which carries `matches()`; everything
/// else reaching one of these catches is a plain throw, and only its text is
/// ever used.
export function isCancelled(error: unknown): boolean {
    const candidate = error as {matches?: (domain: unknown, code: number) => boolean};
    return candidate?.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED) === true;
}

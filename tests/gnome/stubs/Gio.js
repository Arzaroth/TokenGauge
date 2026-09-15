// Stands in for gi://Gio.
//
// Nothing is spawned and nothing is watched. The harness answers a subprocess
// by command line, so what is under test is what the extension does with an
// answer rather than how the answer arrived - the spawning is the binary's
// side of the boundary, and its own end-to-end tests cover it.

import {running, monitors, takeId} from './harness.js';

export const SubprocessFlags = {NONE: 0, STDOUT_PIPE: 1, STDERR_PIPE: 2};
export const FileMonitorFlags = {NONE: 0};
export const IOErrorEnum = {CANCELLED: 19, FAILED: 0};
export const SettingsBindFlags = {DEFAULT: 0};

/// What the extension throws itself against when it cancels a request. GJS
/// raises GLib.Error, which carries `matches()`; the extension's `isCancelled`
/// is written against exactly that.
export class GLibError extends Error {
    constructor(domain, code) {
        super('cancelled');
        this.domain = domain;
        this.code = code;
    }

    matches(domain, code) {
        return this.domain === domain && this.code === code;
    }
}

export class Cancellable {
    constructor() {
        this.cancelled = false;
    }

    cancel() {
        this.cancelled = true;
    }
}

export class Subprocess {
    constructor(argv) {
        this.argv = argv;
        // What the harness matches on. The extension always spawns
        // `['sh', '-c', <line>]`, and the line is the interesting part.
        this.commandLine = argv.join(' ');
        this._exit = 0;
        this._stdout = '';
        this._stderr = '';
        this._pending = null;
        running.push(this);
    }

    static new(argv, _flags) {
        return new Subprocess(argv);
    }

    communicate_utf8_async(_input, cancellable, callback) {
        this._pending = {cancellable, callback};
    }

    communicate_utf8_finish(_result) {
        if (this._pending?.cancellable?.cancelled)
            throw new GLibError(IOErrorEnum, IOErrorEnum.CANCELLED);
        if (this._thrown)
            throw this._thrown;
        return [true, this._stdout, this._stderr];
    }

    get_successful() {
        return this._exit === 0;
    }

    get_exit_status() {
        return this._exit;
    }

    /// What the harness calls to answer a command that was started.
    answer(stdout, stderr = '', exit = 0) {
        this._stdout = stdout;
        this._stderr = stderr;
        this._exit = exit;
        const pending = this._pending;
        this._pending = null;
        pending?.callback(this, null);
    }

    /// An answer that never came because the request was superseded.
    cancel() {
        this._pending?.cancellable?.cancel();
        const pending = this._pending;
        this._pending = null;
        pending?.callback(this, null);
    }
}

export class FileMonitor {
    constructor(path) {
        this.path = path;
        this.cancelled = false;
        this._handlers = new Map();
        monitors.push(this);
    }

    connect(signal, callback) {
        const id = takeId();
        this._handlers.set(id, {signal, callback});
        return id;
    }

    disconnect(id) {
        this._handlers.delete(id);
    }

    cancel() {
        this.cancelled = true;
    }

    /// What the harness calls to say the file moved.
    changed() {
        for (const {signal, callback} of this._handlers.values()) {
            if (signal === 'changed')
                callback(this);
        }
    }
}

export class File {
    constructor(path) {
        this.path = path;
    }

    static new_for_path(path) {
        return new File(path);
    }

    /// No provider logo exists on a machine with no install, which is the
    /// state the extension already has a fallback icon for.
    query_exists() {
        return false;
    }

    monitor_file(_flags, _cancellable) {
        return new FileMonitor(this.path);
    }
}

export class FileIcon {
    constructor({file}) {
        this.file = file;
    }
}

export class ThemedIcon {
    constructor(name) {
        this.name = name;
    }

    static new(name) {
        return new ThemedIcon(name);
    }
}

/// The extension's own GSettings, backed by a plain object. Only the keys it
/// reads are here; an unknown key is a mistake worth throwing on rather than
/// silently answering with a zero.
export class Settings {
    constructor(values = {}) {
        this.values = {
            'waybar-binary': 'tokengauge-waybar',
            'refresh-interval': 600,
            'show-percent': true,
            ...values,
        };
        this._handlers = new Map();
    }

    _get(key) {
        if (!(key in this.values))
            throw new Error(`the extension read an unknown setting: ${key}`);
        return this.values[key];
    }

    get_string(key) {
        return String(this._get(key));
    }

    get_int(key) {
        return Number(this._get(key));
    }

    get_boolean(key) {
        return Boolean(this._get(key));
    }

    set(key, value) {
        this.values[key] = value;
        for (const {signal, callback} of this._handlers.values()) {
            if (signal === 'changed')
                callback(this, key);
        }
    }

    connect(signal, callback) {
        const id = takeId();
        this._handlers.set(id, {signal, callback});
        return id;
    }

    disconnect(id) {
        this._handlers.delete(id);
    }

    bind() {}
}

export default {
    SubprocessFlags,
    FileMonitorFlags,
    IOErrorEnum,
    SettingsBindFlags,
    Cancellable,
    Subprocess,
    File,
    FileIcon,
    FileMonitor,
    ThemedIcon,
    Settings,
    GLibError,
};

// Stands in for gi://GLib. The main loop is the harness's: a timeout is
// recorded rather than armed, and `fireTimeouts` is what makes the clock move,
// so a test can watch a poll happen without waiting for one.

import {timeouts, takeId} from './harness.js';

export const PRIORITY_DEFAULT = 0;
export const SOURCE_REMOVE = false;
export const SOURCE_CONTINUE = true;

export function timeout_add(_priority, intervalMs, handler) {
    const id = takeId();
    timeouts.set(id, {intervalMs, handler});
    return id;
}

export function timeout_add_seconds(_priority, intervalSecs, handler) {
    return timeout_add(_priority, intervalSecs * 1000, handler);
}

export const Source = {
    remove(id) {
        if (!timeouts.has(id))
            throw new Error(`removed a source that was not armed: ${id}`);
        timeouts.delete(id);
        return true;
    },
};

export default {PRIORITY_DEFAULT, SOURCE_REMOVE, SOURCE_CONTINUE, timeout_add, timeout_add_seconds, Source};

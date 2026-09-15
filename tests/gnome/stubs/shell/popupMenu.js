// Stands in for the shell's PopupMenu. Items are kept in order and the open
// state is a plain flag the harness flips, because `open-state-changed` is
// what arms the extension's short render cycle.

import St from 'gi://St';
import {takeId} from '../harness.js';

export class PopupBaseMenuItem extends St.Widget {
    constructor(props = {}) {
        super(props);
    }
}

export class PopupMenu {
    constructor(sourceActor) {
        this.sourceActor = sourceActor;
        this.isOpen = false;
        this.items = [];
        this._handlers = new Map();
    }

    addMenuItem(item) {
        this.items.push(item);
    }

    connect(signal, callback) {
        const id = takeId();
        this._handlers.set(id, {signal, callback});
        return id;
    }

    disconnect(id) {
        this._handlers.delete(id);
    }

    open() {
        this._setOpen(true);
    }

    close() {
        this._setOpen(false);
    }

    _setOpen(open) {
        if (this.isOpen === open)
            return;
        this.isOpen = open;
        for (const {signal, callback} of this._handlers.values()) {
            if (signal === 'open-state-changed')
                callback(this, open);
        }
    }
}

export default {PopupBaseMenuItem, PopupMenu};

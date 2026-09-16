// Stands in for the shell's PanelMenu.Button - the thing that sits in the top
// bar and owns a popup.
//
// `_init` rather than a constructor, because that is what the real one has,
// and it is the whole reason the extension registers its class from a static
// block: GJS routes `super(...)` here.

import St from 'gi://St';
import {PopupMenu} from './popupMenu.js';

export class Button extends St.Widget {
    constructor(...args) {
        super();
        this._init(...args);
    }

    _init(menuAlignment, nameText, dontCreateMenu = false) {
        this.menuAlignment = menuAlignment;
        this.nameText = nameText;
        this.menu = dontCreateMenu ? null : new PopupMenu(this);
    }

    setSensitive(sensitive) {
        this.reactive = sensitive;
    }

    vfunc_event() {
        return false;
    }
}

export default {Button};

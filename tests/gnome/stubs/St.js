// Stands in for gi://St, the shell's widget toolkit.
//
// Nothing is drawn. What these keep is the tree - who was added to whom, and
// what text and style class each one carries - because that tree is the panel
// the user reads, and it is the thing no other test in this repository can
// see. `panel::tests` can only prove the extension mentions a section kind;
// this proves it draws one.

import Clutter from 'gi://Clutter';
import {takeId} from './harness.js';

class Actor {
    constructor(props = {}) {
        this.children = [];
        this.parent = null;
        this.style_class = '';
        this.style = '';
        this.visible = true;
        this.reactive = false;
        this.track_hover = false;
        this.hover = false;
        this.x_expand = false;
        this.y_expand = false;
        this._handlers = new Map();
        this._destroyed = false;
        Object.assign(this, props);
    }

    add_child(child) {
        this.children.push(child);
        child.parent = this;
    }

    remove_child(child) {
        this.children = this.children.filter(c => c !== child);
        child.parent = null;
    }

    destroy_all_children() {
        for (const child of this.children.splice(0))
            child.destroy();
    }

    destroy() {
        if (this._destroyed)
            return;
        this._destroyed = true;
        this.destroy_all_children();
        if (this.parent)
            this.parent.remove_child(this);
        this.emit('destroy');
    }

    connect(signal, callback) {
        const id = takeId();
        if (!this._handlers.has(signal))
            this._handlers.set(signal, new Map());
        this._handlers.get(signal).set(id, callback);
        return id;
    }

    disconnect(id) {
        for (const handlers of this._handlers.values())
            handlers.delete(id);
    }

    /// What the harness calls to raise a signal the shell would have raised.
    emit(signal, ...args) {
        for (const callback of [...(this._handlers.get(signal)?.values() ?? [])])
            callback(this, ...args);
    }

    /// A hover, which is how every tooltip in this extension is reached.
    setHover(hover) {
        this.hover = hover;
        this.emit('notify::hover');
    }

    set_position(x, y) {
        this.x = x;
        this.y = y;
    }

    get_transformed_position() {
        return [0, 0];
    }

    get_width() {
        return 100;
    }

    get_height() {
        return 20;
    }
}

class ClutterText {
    constructor() {
        this.line_wrap = false;
        this.markup = '';
    }

    set_markup(markup) {
        this.markup = markup;
    }
}

export class Widget extends Actor {}

export class BoxLayout extends Widget {
    constructor(props = {}) {
        super(props);
        // The property the extension feature-detects on. Present here, so the
        // shell-48 branch is the one under test; the older spelling is dead on
        // every shell this still supports.
        if (this.orientation === undefined)
            this.orientation = Clutter.Orientation.HORIZONTAL;
    }
}

export class Label extends Widget {
    constructor(props = {}) {
        super({text: '', ...props});
        this.clutter_text = new ClutterText();
    }
}

export class Icon extends Widget {
    constructor(props = {}) {
        super({icon_name: '', icon_size: 0, gicon: null, ...props});
    }
}

export class Button extends Widget {
    constructor(props = {}) {
        super({label: '', child: null, can_focus: false, accessible_name: '', ...props});
        if (this.child)
            this.add_child(this.child);
    }

    /// What the harness calls to click it.
    click() {
        this.emit('clicked');
    }
}

export class DrawingArea extends Widget {
    constructor(props = {}) {
        super({height: 0, ...props});
    }

    get_surface_size() {
        return [100, 20];
    }

    get_context() {
        // Kept, so a test can read back what the repaint painted.
        this._lastContext = new CairoContext();
        return this._lastContext;
    }

    get_theme_node() {
        return {get_foreground_color: () => ({red: 255, green: 255, blue: 255, alpha: 255})};
    }

    /// Run the repaint the shell would run, so the drawing code is executed
    /// rather than merely defined. What it paints is not asserted - it is
    /// pixels - but a throw in it is a blank bar on a real panel.
    repaint() {
        this.emit('repaint');
    }
}

/// Enough cairo for the two things the extension draws. Every call is
/// recorded, so a harness can prove a bar was filled rather than skipped.
class CairoContext {
    constructor() {
        this.calls = [];
    }

    newSubPath() {
        this.calls.push(['newSubPath']);
    }

    arc(...args) {
        this.calls.push(['arc', ...args]);
    }

    closePath() {
        this.calls.push(['closePath']);
    }

    rectangle(...args) {
        this.calls.push(['rectangle', ...args]);
    }

    setSourceRGBA(...args) {
        this.calls.push(['setSourceRGBA', ...args]);
    }

    fill() {
        this.calls.push(['fill']);
    }

    $dispose() {
        this.calls.push(['$dispose']);
    }
}

export default {Widget, BoxLayout, Label, Icon, Button, DrawingArea};

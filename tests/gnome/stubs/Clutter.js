// Stands in for gi://Clutter. Enumerations and two layout markers; nothing
// here does anything, because nothing in the extension asks Clutter to.

export const Orientation = {HORIZONTAL: 0, VERTICAL: 1};
export const ActorAlign = {FILL: 0, START: 1, CENTER: 2, END: 3};
export const EventType = {NOTHING: 0, BUTTON_PRESS: 4, SCROLL: 6};
export const ScrollDirection = {UP: 0, DOWN: 1, LEFT: 2, RIGHT: 3, SMOOTH: 4};
export const BUTTON_PRIMARY = 1;
export const BUTTON_MIDDLE = 2;
export const BUTTON_SECONDARY = 3;
export const EVENT_PROPAGATE = false;
export const EVENT_STOP = true;

export class BinLayout {}

/// What a harness hands to `vfunc_event`. The real one is a boxed union with
/// accessors, which is why the extension calls methods rather than reading
/// fields, and why this has to as well.
export class Event {
    constructor({type = EventType.NOTHING, button = 0, scroll = ScrollDirection.SMOOTH} = {}) {
        this._type = type;
        this._button = button;
        this._scroll = scroll;
    }

    type() {
        return this._type;
    }

    get_button() {
        return this._button;
    }

    get_scroll_direction() {
        return this._scroll;
    }
}

export default {
    Orientation,
    ActorAlign,
    EventType,
    ScrollDirection,
    BUTTON_PRIMARY,
    BUTTON_MIDDLE,
    BUTTON_SECONDARY,
    EVENT_PROPAGATE,
    EVENT_STOP,
    BinLayout,
    Event,
};

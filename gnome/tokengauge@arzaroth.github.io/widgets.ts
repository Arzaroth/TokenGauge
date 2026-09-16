// The drawing helpers: everything that builds a widget and nothing that knows
// what a snapshot is.
//
// They are here rather than in `extension.ts` because they are the only part
// of this frontend that is pure enough to hold to a value - a clamp, a gap, a
// placement - and a function no test can import is a function no test checks.

import Clutter from 'gi://Clutter';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import type * as Panel from './panel.js';

// St.BoxLayout.vertical was replaced by the Clutter orientation property in
// GNOME 48; both spellings have to work across the supported shell versions,
// and only the newer one is in the typings this builds against.
export function box(vertical: boolean, props: Partial<St.BoxLayout.ConstructorProps> = {}): St.BoxLayout {
    const b = new St.BoxLayout(props);
    if ('orientation' in b)
        b.orientation = vertical ? Clutter.Orientation.VERTICAL : Clutter.Orientation.HORIZONTAL;
    else
        (b as unknown as {vertical: boolean}).vertical = vertical;
    return b;
}

export function label(text: string, styleClass: string, style?: string): St.Label {
    const l = new St.Label({text, style_class: styleClass});
    if (style)
        l.style = style;
    l.clutter_text.line_wrap = true;
    return l;
}

export function spacer(): St.Widget {
    return new St.Widget({x_expand: true});
}

// A fill sized in CSS lands wherever the layout puts it, and one sized from a
// `notify::width` handler is a frame behind the allocation it tracks. Draw it
// instead: the repaint runs with the width the popup actually gave the row.
export function barFill(
    fraction: number | null, radius: number, styleClass: string, style?: string,
): St.DrawingArea {
    const clamped = Math.max(0, Math.min(1, Number(fraction) || 0));
    const area = new St.DrawingArea({
        style_class: styleClass,
        style,
        x_expand: true,
        y_expand: true,
        x_align: Clutter.ActorAlign.FILL,
        y_align: Clutter.ActorAlign.FILL,
    });
    area.connect('repaint', () => {
        const [width, height] = area.get_surface_size();
        const w = Math.round(width * clamped);
        if (w <= 0 || height <= 0)
            return;
        const r = Math.min(radius, w / 2, height / 2);
        const cr = area.get_context();
        cr.newSubPath();
        cr.arc(w - r, r, r, -Math.PI / 2, 0);
        cr.arc(w - r, height - r, r, 0, Math.PI / 2);
        cr.arc(r, height - r, r, Math.PI / 2, Math.PI);
        cr.arc(r, r, r, Math.PI, 1.5 * Math.PI);
        cr.closePath();
        // GNOME 45 has no `cr.setSourceColor`; the components are 8-bit on
        // every shell the extension supports.
        const c = area.get_theme_node().get_foreground_color();
        cr.setSourceRGBA(c.red / 255, c.green / 255, c.blue / 255, c.alpha / 255);
        cr.fill();
        cr.$dispose();
    });
    return area;
}

// Cairo wants components and the snapshot's theme carries hex strings.
export function hexToRgb(hex: string): [number, number, number] {
    const m = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(String(hex || ''));
    if (!m)
        return [1, 1, 1];
    return [parseInt(m[1], 16) / 255, parseInt(m[2], 16) / 255, parseInt(m[3], 16) / 255];
}

// The history chart. Drawn rather than laid out for the same reason `barFill`
// is: the repaint runs with the width the popup actually gave the row, and a
// chart sized from a `notify::width` handler is a frame behind its allocation.
export function historyChart(
    points: Panel.HistoryPoint[],
    colorFor: (point: Panel.HistoryPoint) => string,
    height: number,
): St.DrawingArea {
    const area = new St.DrawingArea({
        style_class: 'tokengauge-history-chart',
        x_expand: true,
        height,
    });
    area.connect('repaint', () => {
        const [width, h] = area.get_surface_size();
        const n = points.length;
        if (n === 0 || width <= 0 || h <= 0)
            return;
        // Wide steps get a gap between them; ninety days of bars have none to
        // spare.
        const gap = n <= 12 ? 2 : (n <= 31 ? 1 : 0);
        const w = Math.max(1, (width - gap * (n - 1)) / n);
        const cr = area.get_context();
        points.forEach((point, i) => {
            const fraction = Math.max(0, Math.min(1, Number(point.fraction) || 0));
            // A floor of one pixel: a step that spent a little must never draw
            // as a step that spent nothing.
            const barHeight = fraction > 0 ? Math.max(1, fraction * h) : 0;
            if (barHeight <= 0)
                return;
            const [r, g, b] = hexToRgb(colorFor(point));
            // The step in progress is short because it is not over, so it is
            // drawn as unfinished rather than as a fall.
            cr.setSourceRGBA(r, g, b, point.partial ? 0.45 : 1);
            cr.rectangle(i * (w + gap), h - barHeight, w, barHeight);
            cr.fill();
        });
        cr.$dispose();
    });
    return area;
}

// St has no tooltip of its own, and the panel spec fills `tooltip` for every
// row whose line is an abbreviation of what it carries: a day's exact tokens,
// a model's split by device, the whole sync sentence behind its badge. The
// label goes in the shell's own layer so the popup cannot clip it.
export function attachTooltip<T extends St.Widget>(
    actor: T, text: string | (() => string), markup = false,
): T {
    // A row's tooltip is fixed for the life of the label that carries it, but
    // the panel button outlives every snapshot - so a function is resolved on
    // each hover rather than once here.
    const resolve = typeof text === 'function' ? text : () => text;
    if (typeof text !== 'function' && !text)
        return actor;
    actor.reactive = true;
    actor.track_hover = true;
    let tip: St.Label | null = null;
    const hide = () => {
        if (tip) {
            tip.destroy();
            tip = null;
        }
    };
    actor.connect('notify::hover', () => {
        hide();
        if (!actor.hover)
            return;
        const body = resolve();
        if (!body)
            return;
        tip = new St.Label({style_class: 'tokengauge-tooltip'});
        if (markup)
            tip.clutter_text.set_markup(body);
        else
            tip.text = body;
        tip.clutter_text.line_wrap = true;
        Main.layoutManager.uiGroup.add_child(tip);
        const [x, y] = actor.get_transformed_position();
        const right = global.stage.width - tip.get_width() - 4;
        // Above the row, unless that leaves the stage: the first rows of the
        // popup sit close enough to the top panel for it to.
        const above = y - tip.get_height() - 6;
        tip.set_position(
            Math.round(Math.max(4, Math.min(x, right))),
            Math.round(above >= 4 ? above : y + actor.get_height() + 6));
    });
    actor.connect('destroy', hide);
    return actor;
}

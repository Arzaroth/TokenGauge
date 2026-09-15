// The extension's pure helpers, held to a value.
//
// `extension.test.mjs` drives the whole indicator and proves a snapshot goes
// in and a panel comes out. It cannot say what happens at a boundary: a
// fraction above 1, a chart with one point, a tooltip that would fall off the
// stage. Those are arithmetic, and arithmetic is what a unit test is for.

import assert from 'node:assert/strict';
import {test} from 'node:test';

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const W = '../../build/frontends/gnome/tokengauge@arzaroth.github.io/widgets.js';
const U = '../../build/frontends/gnome/tokengauge@arzaroth.github.io/util.js';

const {box, label, spacer, barFill, hexToRgb, historyChart, attachTooltip} = await import(W);
const {shellQuote, isCancelled} = await import(U);

/// What a drawing area painted. A repaint that bailed before asking for a
/// context painted nothing, which is a real outcome rather than a failure.
function painted(area) {
    area.repaint();
    return area._lastContext ? area._lastContext.calls : [];
}

/// The rectangles it painted, as [x, y, w, h]. The history chart's bars.
function rectangles(area) {
    return painted(area).filter(c => c[0] === 'rectangle').map(c => c.slice(1));
}

/// The width of the rounded bar `barFill` painted. It is a path of four arcs
/// rather than a rectangle, and the corner radius is clamped against both the
/// fill and the track, so the width has to be read back off the corners: the
/// first arc is centred at `w - r` and the last at `r`.
function filledWidth(area) {
    const arcs = painted(area).filter(c => c[0] === 'arc');
    return arcs.length === 0 ? 0 : arcs[0][1] + arcs[arcs.length - 1][1];
}

test('a shell quote survives every value that reaches a command line', () => {
    assert.equal(shellQuote('tokengauge'), "'tokengauge'");
    assert.equal(shellQuote('claude=true'), "'claude=true'");
    // The whole point: a single quote cannot end the quoting early.
    assert.equal(shellQuote("it's"), "'it'\\''s'");
    assert.equal(shellQuote("'; rm -rf ~; '"), "''\\''; rm -rf ~; '\\'''");
    // A quoted value has no unquoted `;`, `&&`, `$` or backtick left in it.
    for (const hostile of ["a; rm b", 'a && b', '$(id)', '`id`', 'a\nb']) {
        const quoted = shellQuote(hostile);
        assert.ok(quoted.startsWith("'") && quoted.endsWith("'"), quoted);
        assert.ok(!quoted.slice(1, -1).includes("'"), quoted);
    }
});

test('only a GLib cancellation reads as a cancellation', () => {
    const cancelled = {matches: (domain, code) => domain === Gio.IOErrorEnum && code === 19};
    assert.equal(isCancelled(cancelled), true);
    // A different GLib error is a real failure and must be reported.
    assert.equal(isCancelled({matches: () => false}), false);
    // Everything else reaching the catch is a plain throw.
    assert.equal(isCancelled(new Error('boom')), false);
    assert.equal(isCancelled('boom'), false);
    assert.equal(isCancelled(null), false);
    assert.equal(isCancelled(undefined), false);
});

test('a box takes the orientation the shell it is running on understands', () => {
    assert.equal(box(true).orientation, Clutter.Orientation.VERTICAL);
    assert.equal(box(false).orientation, Clutter.Orientation.HORIZONTAL);
    assert.equal(box(false, {style_class: 'x'}).style_class, 'x');
});

test('every label wraps, because a provider writes these strings', () => {
    const l = label('a long window title', 'tokengauge-dim');
    assert.equal(l.text, 'a long window title');
    assert.equal(l.clutter_text.line_wrap, true, 'an unwrapped label truncates a reset note');
    assert.equal(l.style, '', 'no style means no inline style attribute');
    assert.equal(label('x', 'y', 'color: #fff;').style, 'color: #fff;');
});

test('a spacer is what pushes a value to the right edge', () => {
    assert.equal(spacer().x_expand, true);
});

test('a bar fill clamps rather than overflowing its track', () => {
    const width = 100;
    // The fraction is the core's and arrives as a float, or as null for a row
    // with no bar. None of those may paint outside the track.
    const widthOf = fraction => filledWidth(barFill(fraction, 4, 'fill'));
    assert.equal(widthOf(0.5), width / 2);
    assert.equal(widthOf(1), width);
    assert.equal(widthOf(1.4), width, 'a provider over its quota must not paint past the track');
    assert.equal(widthOf(0), 0, 'nothing used paints nothing');
    assert.equal(widthOf(-0.2), 0);
    assert.equal(widthOf(null), 0, 'a row the spec gave no bar draws no bar');
    assert.equal(widthOf(NaN), 0);

    // The corner radius is clamped against the fill as well as the track, or a
    // nearly-empty bar would be drawn as a circle wider than its own value.
    const sliver = painted(barFill(0.02, 4, 'fill')).filter(c => c[0] === 'arc');
    assert.equal(sliver[0][3], 1, 'a 2px fill cannot carry a 4px corner');
});

test('a hex colour that is not one reads as white rather than as nothing', () => {
    assert.deepEqual(hexToRgb('#ffffff'), [1, 1, 1]);
    assert.deepEqual(hexToRgb('#000000'), [0, 0, 0]);
    assert.deepEqual(hexToRgb('f38ba8'), [0xf3 / 255, 0x8b / 255, 0xa8 / 255], 'a missing # is fine');
    assert.deepEqual(hexToRgb('#F38BA8'), [0xf3 / 255, 0x8b / 255, 0xa8 / 255], 'and so is upper case');
    // A theme key the snapshot did not carry must not paint a transparent bar.
    for (const bad of ['', 'red', '#fff', undefined, null])
        assert.deepEqual(hexToRgb(bad), [1, 1, 1], `${bad}`);
});

test('a history chart gives wide steps a gap and dense ones none', () => {
    const points = n => Array.from({length: n}, (_, i) => ({fraction: 1, partial: false, tone: 'normal'}));
    const gapOf = n => {
        const rects = rectangles(historyChart(points(n), () => '#ffffff', 140));
        return rects.length < 2 ? null : rects[1][0] - (rects[0][0] + rects[0][2]);
    };
    assert.equal(gapOf(12), 2, 'a year of bars has room to breathe');
    assert.equal(gapOf(30), 1, 'a month has less');
    assert.equal(gapOf(90), 0, 'ninety days have none to spare');
});

test('a step that spent a little never draws as a step that spent nothing', () => {
    const tiny = [{fraction: 0.0001, partial: false, tone: 'normal'}];
    const [rect] = rectangles(historyChart(tiny, () => '#ffffff', 140));
    assert.ok(rect, 'the step was skipped entirely');
    assert.equal(rect[3], 1, 'the floor is one pixel');

    // A step that really spent nothing draws nothing, or a quiet month would
    // read as a month of even, minimal spend.
    const zero = [{fraction: 0, partial: false, tone: 'normal'}];
    assert.equal(rectangles(historyChart(zero, () => '#ffffff', 140)).length, 0);

    // And an empty range paints nothing at all rather than dividing by zero.
    assert.equal(rectangles(historyChart([], () => '#ffffff', 140)).length, 0);
});

test('the step in progress is drawn as unfinished, not as a fall', () => {
    const series = [
        {fraction: 1, partial: false, tone: 'normal'},
        {fraction: 0.2, partial: true, tone: 'normal'},
    ];
    const area = historyChart(series, () => '#ffffff', 140);
    area.repaint();
    const alphas = area._lastContext.calls
        .filter(c => c[0] === 'setSourceRGBA')
        .map(c => c[4]);
    assert.deepEqual(alphas, [1, 0.45], 'today is the same colour at less than full alpha');
});

test('a bar grows from the bottom, so a tall step is a big number', () => {
    const height = 140;
    const [full] = rectangles(historyChart(
        [{fraction: 1, partial: false, tone: 'normal'}], () => '#fff', height));
    // The stub's surface is 20 tall whatever the requested height; what matters
    // is that y + h lands on the baseline rather than at the top.
    assert.equal(full[1] + full[3], 20, 'the chart is drawn upside down');
});

test('a tooltip goes above the row unless that leaves the stage', () => {
    const roomy = new St.Widget();
    roomy.get_transformed_position = () => [40, 300];
    attachTooltip(roomy, 'a day of tokens');
    roomy.setHover(true);
    const above = Main.layoutManager.uiGroup.children.at(-1);
    // 300 - 20 (label height) - 6
    assert.deepEqual([above.x, above.y], [40, 274]);

    // The first rows of the popup sit close enough to the top panel that
    // there is no room above them.
    const tight = new St.Widget();
    tight.get_transformed_position = () => [40, 10];
    attachTooltip(tight, 'a day of tokens');
    tight.setHover(true);
    const below = Main.layoutManager.uiGroup.children.at(-1);
    assert.equal(below.y, 10 + 20 + 6, 'it should have dropped below the row');
});

test('a tooltip never runs off the right edge or the left', () => {
    const far = new St.Widget();
    far.get_transformed_position = () => [5000, 300];
    attachTooltip(far, 'a day of tokens');
    far.setHover(true);
    const clamped = Main.layoutManager.uiGroup.children.at(-1);
    // stage 1920 - label 100 - 4
    assert.equal(clamped.x, 1816);

    const negative = new St.Widget();
    negative.get_transformed_position = () => [-200, 300];
    attachTooltip(negative, 'a day of tokens');
    negative.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.at(-1).x, 4, 'the left edge has a margin too');
});

test('a row with nothing more to say grows no tooltip at all', () => {
    const bare = new St.Widget();
    const before = Main.layoutManager.uiGroup.children.length;
    attachTooltip(bare, '');
    assert.equal(bare.reactive, false, 'an empty tooltip must not make the row hoverable');
    bare.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.length, before);
});

test('a tooltip resolved on each hover reads the value it has now', () => {
    const live = new St.Widget();
    live.get_transformed_position = () => [40, 300];
    let body = 'first';
    attachTooltip(live, () => body);

    live.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.at(-1).text, 'first');
    live.setHover(false);

    body = 'second';
    live.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.at(-1).text, 'second');

    // And a function that answers with nothing shows nothing, which is how the
    // bar icon stays quiet while its own popup is open.
    live.setHover(false);
    const before = Main.layoutManager.uiGroup.children.length;
    body = '';
    live.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.length, before);
});

test('a tooltip is taken down with the row that carried it', () => {
    const row = new St.Widget();
    row.get_transformed_position = () => [40, 300];
    attachTooltip(row, 'a day of tokens');
    row.setHover(true);
    const shown = Main.layoutManager.uiGroup.children.length;

    row.setHover(false);
    assert.equal(Main.layoutManager.uiGroup.children.length, shown - 1, 'unhover left it on screen');

    row.setHover(true);
    row.destroy();
    assert.equal(
        Main.layoutManager.uiGroup.children.length,
        shown - 1,
        'a popup rebuilt under the pointer would leak one label per render',
    );
});

test('markup is only parsed as markup when it was asked for', () => {
    const plain = new St.Widget();
    plain.get_transformed_position = () => [40, 300];
    attachTooltip(plain, '<b>not bold</b>');
    plain.setHover(true);
    const asText = Main.layoutManager.uiGroup.children.at(-1);
    assert.equal(asText.text, '<b>not bold</b>');
    assert.equal(asText.clutter_text.markup, '');

    const rich = new St.Widget();
    rich.get_transformed_position = () => [40, 300];
    attachTooltip(rich, '<b>bold</b>', true);
    rich.setHover(true);
    assert.equal(Main.layoutManager.uiGroup.children.at(-1).clutter_text.markup, '<b>bold</b>');
});

// The GNOME extension, loaded in Node against a recorded panel.
//
// It is the largest frontend in the repository and the one furthest from a
// compiler's reach even now that it is typechecked: TypeScript proves the
// fields it reads exist, not that it draws them. This proves the second half -
// that a snapshot goes in and a panel comes out, with the sections the core
// resolved, in the order it resolved them.

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {test} from 'node:test';

import Gio from 'gi://Gio';
import Clutter from 'gi://Clutter';
import * as Harness from './stubs/harness.js';
import {registered} from './stubs/GObject.js';

const EXTENSION = '../../build/frontends/gnome/tokengauge@arzaroth.github.io/extension.js';
const PANEL = new URL('../qml/fixtures/panel.json', import.meta.url);

const {default: TokenGaugeExtension} = await import(EXTENSION);
const panelJson = readFileSync(PANEL, 'utf8');
const panel = JSON.parse(panelJson);

/// An enabled extension with an indicator in the top bar, as `enable()` leaves
/// it: one snapshot already asked for and nothing answered yet.
function enabled(settings = {}) {
    Harness.reset();
    const extension = new TokenGaugeExtension();
    extension.settings = new Gio.Settings(settings);
    extension.enable();
    return {extension, indicator: extension._indicator};
}

/// Answer the snapshot request the extension has in flight.
function answer(body = panelJson, stderr = '', exit = 0) {
    const asked = Harness.find('--json');
    assert.ok(asked, 'nothing asked for a snapshot');
    asked.answer(body, stderr, exit);
    return asked;
}

/// The menu content box, which is where the whole panel is drawn.
function content(indicator) {
    return indicator._content;
}

/// Open the popup, which is what makes the extension render into it.
function open(indicator) {
    indicator.menu.open();
    return content(indicator);
}

/// The tab buttons carrying their own label. The provider strip draws its
/// tabs as an icon beside a label in a child box rather than as a label, so
/// this is what tells the two strips sharing a style class apart.
function labelledTabs(box) {
    return Harness.byStyle(box, 'tokengauge-tab').filter(b => b.label !== '');
}

test('the indicator registers itself as a GObject class', () => {
    assert.ok(
        registered.length > 0,
        'a class GNOME Shell never registered is one it refuses to instantiate',
    );
});

test('it asks the binary for a snapshot and nothing else', () => {
    enabled();

    assert.equal(Harness.running.length, 1, 'one command, not one per provider');
    const [asked] = Harness.running;
    assert.deepEqual(asked.argv.slice(0, 2), ['sh', '-c']);

    const line = asked.argv[2];
    assert.ok(
        line.startsWith('export PATH="$HOME/.local/bin'),
        'gnome-shell inherits a session PATH that often lacks the install dir',
    );
    assert.ok(line.includes("'tokengauge-waybar'"), 'the binary is named and quoted');
    assert.ok(line.includes('--json'));
    assert.ok(!line.includes('http'), 'no frontend names a provider endpoint');
});

test('the bar shows the pinned provider under the configured window', () => {
    const {indicator} = enabled();
    assert.equal(indicator._panelPercent.text, '—', 'no snapshot, no number');

    answer();

    assert.equal(indicator._panelPercent.text, '68%');
    assert.equal(
        indicator._panelPercent.style,
        `color: ${panel.theme.yellow};`,
        'the tier is the core\'s and the colour is the snapshot theme\'s',
    );
    assert.equal(indicator._panelPercent.visible, true);
});

test('the panel it draws is the panel the core resolved', () => {
    const {indicator} = enabled();
    answer();
    const box = open(indicator);

    const titles = Harness.byStyle(box, 'tokengauge-section-title').map(l => l.text);
    assert.deepEqual(
        titles,
        ['LIMITS', 'COST', 'Pin to bar'],
        'the spec\'s sections in the spec\'s order, then the chrome that is ours',
    );

    // Meters: a label, a value, a full-width bar, and the pace badge.
    const meters = Harness.byStyle(box, 'tokengauge-meter');
    assert.equal(meters.length, 2, 'one per limit window the row reported');
    const limits = panel.rows[0].panel.find(s => s.id === 'limits');
    assert.deepEqual(
        Harness.byStyle(box, 'tokengauge-meter-label').map(l => l.text),
        limits.rows.map(r => r.label),
    );
    assert.deepEqual(
        Harness.byStyle(box, 'tokengauge-meter-value').map(l => l.text),
        limits.rows.map(r => r.value),
    );
    assert.equal(Harness.byStyle(box, 'tokengauge-meter-fill').length, 2);

    // Rows: label and value on one line, no bar.
    const cost = panel.rows[0].panel.find(s => s.id === 'cost');
    const costLabels = Harness.byStyle(box, 'tokengauge-cost-label').map(l => l.text);
    for (const row of cost.rows)
        assert.ok(costLabels.includes(row.label), `${row.label} is not drawn`);
});

test('a meter fill is painted rather than merely laid out', () => {
    const {indicator} = enabled();
    answer();
    const box = open(indicator);

    const [fill] = Harness.byStyle(box, 'tokengauge-meter-fill');
    assert.ok(fill, 'no fill to paint');
    fill.repaint();
    // The repaint runs with the width the popup actually gave the row, which is
    // the whole reason this is drawn instead of sized in CSS.
    assert.ok(
        Harness.flatten(box).length > 0 && fill.get_surface_size()[0] > 0,
        'the fill had no surface to paint on',
    );
});

test('hovering the bar icon says what the panel says', () => {
    const {indicator} = enabled();
    answer();

    const markup = indicator._barTooltipMarkup();
    const tip = panel.rows[0].bar_tooltip;
    assert.ok(markup.startsWith(`<b>${tip.title}</b>`), markup);
    for (const line of tip.lines)
        assert.ok(markup.includes(line.label), `${line.label} is missing from the hover`);
    assert.ok(
        markup.includes(`<span color="${panel.theme.yellow}">`),
        'a tier that is not `normal` is tinted',
    );

    // The popup carries all of this and sits directly under the button, so a
    // tooltip over it would be the same figures twice on one surface.
    indicator.menu.open();
    assert.equal(indicator._barTooltipMarkup(), '');
});

test('a provider name that carries markup cannot corrupt the hover', () => {
    const {indicator} = enabled();
    const hostile = JSON.parse(panelJson);
    hostile.rows[0].bar_tooltip.title = 'A & B <img src=x>';
    answer(JSON.stringify(hostile));

    const markup = indicator._barTooltipMarkup();
    assert.ok(markup.includes('A &amp; B &lt;img src=x&gt;'), markup);
    assert.ok(!markup.includes('<img'), 'Pango would have rendered that');
});

test('the second screen is the history the core resolved', () => {
    const {indicator} = enabled();
    answer();
    open(indicator);

    indicator._historyOpen = true;
    indicator._render();
    const box = content(indicator);

    const titles = Harness.byStyle(box, 'tokengauge-section-title').map(l => l.text);
    assert.deepEqual(titles, ['History'], 'the history replaces the panel, it does not join it');

    const ranges = labelledTabs(box).map(b => b.label);
    assert.deepEqual(ranges, panel.rows[0].history.series.map(s => s.label));

    // A machine with nothing recorded yet says so, rather than drawing an
    // empty chart that reads as a month of zero spend.
    assert.ok(
        Harness.texts(box).includes('Nothing spent in this range.'),
        Harness.texts(box).join(' | '),
    );
});

test('scrolling the bar moves to the next provider and stays on it', () => {
    const {indicator} = enabled();
    answer();
    assert.equal(indicator._panelPercent.text, '68%', 'the pin leads');

    indicator._onScroll(new Clutter.Event({scroll: Clutter.ScrollDirection.DOWN}));

    assert.equal(indicator._selectedProviderId, panel.rows[1].provider);
    assert.equal(indicator._panelPercent.text, `${panel.rows[1].bar.percent}%`);

    // The selection follows the provider, not the slot: a row dropping out
    // would otherwise slide a different provider's numbers under the user.
    const fewer = JSON.parse(panelJson);
    fewer.rows = [panel.rows[1]];
    answer(JSON.stringify(fewer));
    assert.equal(indicator._panelPercent.text, `${panel.rows[1].bar.percent}%`);
});

test('a click runs the flag and the read in one subprocess', () => {
    const {indicator} = enabled();
    answer();
    open(indicator);

    Harness.reset();
    const pin = labelledTabs(content(indicator)).find(b => b.label === 'Claude');
    assert.ok(pin, 'no pin button for the provider the panel is showing');
    pin.click();

    const asked = Harness.find('--set-primary');
    assert.ok(asked, 'repinning ran no command');
    const line = asked.argv[2];
    assert.ok(line.includes("--set-primary 'claude'"), line);
    assert.ok(
        line.indexOf('--json') > line.indexOf('--set-primary'),
        'the read must be chained behind the write, or the panel renders stale',
    );
    assert.ok(line.includes('&&'), 'and only if the write succeeded');
});

test('middle click refreshes without opening the menu', () => {
    const {indicator} = enabled();
    answer();
    Harness.reset();

    const handled = indicator.vfunc_event(
        new Clutter.Event({type: Clutter.EventType.BUTTON_PRESS, button: Clutter.BUTTON_MIDDLE}),
    );

    assert.equal(handled, Clutter.EVENT_STOP, 'the shell would have opened the menu too');
    assert.ok(Harness.find('--refresh'), 'nothing was refreshed');
    assert.equal(indicator.menu.isOpen, false);
});

test('output that is not a snapshot is reported, not drawn', () => {
    const {indicator} = enabled();
    answer();
    open(indicator);
    const good = Harness.byStyle(content(indicator), 'tokengauge-meter-value').map(l => l.text);

    indicator._reload();
    answer('not json at all');

    assert.ok(indicator._lastError.startsWith('parse error:'), indicator._lastError);
    assert.deepEqual(
        Harness.byStyle(content(indicator), 'tokengauge-meter-value').map(l => l.text),
        good,
        'the last good figures must stay on screen',
    );
    assert.ok(Harness.byStyle(content(indicator), 'tokengauge-error').length > 0);
});

test('a command that fails reports its stderr', () => {
    const {indicator} = enabled();
    answer('', 'tokengauge-waybar: not found\n', 127);
    assert.equal(indicator._lastError, 'tokengauge-waybar: not found');
});

test('it watches the revision file the binary rewrites', () => {
    const {indicator} = enabled();
    answer();

    assert.equal(indicator._revisionFile, panel.revision_file);
    const [monitor] = Harness.monitors;
    assert.ok(monitor, 'nothing is watching, so another frontend\'s fetch lands on the next poll');

    // One in-place rewrite raises more than one event; they coalesce into a
    // single re-read rather than one subprocess each.
    Harness.reset();
    monitor.changed();
    monitor.changed();
    monitor.changed();
    assert.equal(Harness.running.length, 0, 'the re-read is behind a settle timer');
    Harness.fireTimeouts();
    assert.equal(Harness.running.length, 1, 'three events, one re-read');
});

test('an open menu re-renders on a short cycle that asks no provider anything', () => {
    const {indicator} = enabled();
    answer();
    Harness.reset();

    indicator.menu.open();

    // Opening re-reads at once, and arms the cycle that keeps the countdowns
    // moving: a reset time is measured at render, so a menu left open would
    // otherwise keep the countdown it opened with.
    assert.equal(Harness.running.length, 1);
    answer();
    const armed = [...Harness.timeouts.values()].map(t => t.intervalMs);
    assert.ok(armed.includes(30000), `no 30s render cycle: ${armed}`);
});

test('a refresh in flight is not cancelled by a poll', () => {
    const {indicator} = enabled();
    answer();

    indicator._applyUpdate();
    assert.equal(indicator._updating, true);
    const inflight = Harness.find('--update');
    assert.ok(inflight);

    // The poll would cancel the in-flight read and strand the button on
    // "Updating…" until the next tick.
    Harness.fireTimeouts();
    assert.equal(indicator._updating, true, 'a poll disarmed an update in flight');

    inflight.answer(panelJson, '', 0);
    assert.equal(indicator._updating, false);
});

test('disabling puts back everything it took', () => {
    const {extension, indicator} = enabled();
    answer();
    indicator.menu.open();
    assert.ok(Harness.timeouts.size > 0);
    assert.ok(Harness.monitors.length > 0);

    extension.disable();

    assert.equal(extension._indicator, null);
    assert.equal(Harness.timeouts.size, 0, 'a timeout left armed fires into a destroyed indicator');
    assert.ok(Harness.monitors.every(m => m.cancelled), 'a file monitor outlived the extension');
});

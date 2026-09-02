import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension, gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';

// How often the open menu re-reads the snapshot. See `_setLive`.
const LIVE_INTERVAL_SECS = 30;

const FALLBACK_THEME = {
    red: '#f38ba8',
    yellow: '#f9e2af',
    green: '#a6e3a1',
    dim: '#6c7086',
};

// St.BoxLayout.vertical was replaced by the Clutter orientation property in
// GNOME 48; both spellings have to work across the supported shell versions.
function box(vertical, props = {}) {
    const b = new St.BoxLayout(props);
    if ('orientation' in b)
        b.orientation = vertical ? Clutter.Orientation.VERTICAL : Clutter.Orientation.HORIZONTAL;
    else
        b.vertical = vertical;
    return b;
}

function shellQuote(s) {
    return `'${String(s).replace(/'/g, "'\\''")}'`;
}

function label(text, styleClass, style) {
    const l = new St.Label({text, style_class: styleClass});
    if (style)
        l.style = style;
    l.clutter_text.line_wrap = true;
    return l;
}

function spacer() {
    return new St.Widget({x_expand: true});
}

// A fill sized in CSS lands wherever the layout puts it, and one sized from a
// `notify::width` handler is a frame behind the allocation it tracks. Draw it
// instead: the repaint runs with the width the popup actually gave the row.
function barFill(fraction, radius, styleClass, style) {
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
function hexToRgb(hex) {
    const m = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(String(hex || ''));
    if (!m)
        return [1, 1, 1];
    return [parseInt(m[1], 16) / 255, parseInt(m[2], 16) / 255, parseInt(m[3], 16) / 255];
}

// The history chart. Drawn rather than laid out for the same reason `barFill`
// is: the repaint runs with the width the popup actually gave the row, and a
// chart sized from a `notify::width` handler is a frame behind its allocation.
function historyChart(points, colorFor, height) {
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
function attachTooltip(actor, text) {
    if (!text)
        return actor;
    actor.reactive = true;
    actor.track_hover = true;
    let tip = null;
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
        tip = new St.Label({style_class: 'tokengauge-tooltip', text});
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

const Indicator = GObject.registerClass(
class TokenGaugeIndicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.5, 'TokenGauge');

        this._extension = extension;
        this._settings = extension.getSettings();
        this._snapshot = {rows: [], errors: [], enabled: [], providers: []};
        this._lastError = '';
        // The selection follows the provider id, not the slot it sits in: a
        // row that appears or drops out on a refresh would otherwise slide a
        // different provider's numbers under whatever the user was reading.
        // Empty means nothing chosen, so the pin still leads.
        this._selectedProviderId = '';
        this._updating = false;
        // The history screen is a second screen over the panel: a year of bars
        // does not belong above the limit gauges. Every range is already on the
        // row, so cycling one is a re-render rather than another `--json`.
        this._historyOpen = false;
        this._historyRange = 0;
        this._cancellable = null;
        this._requestId = 0;
        this._timeoutId = 0;
        this._liveTimeoutId = 0;
        this._menuDirty = true;
        this._revisionFile = '';
        this._revisionMonitor = null;
        this._revisionSettleId = 0;

        const panelBox = box(false, {style_class: 'panel-status-menu-box tokengauge-panel'});
        this._panelIcon = new St.Icon({style_class: 'system-status-icon'});
        this._panelGlyph = new St.Label({style_class: 'tokengauge-panel-glyph', y_align: Clutter.ActorAlign.CENTER});
        this._panelPercent = new St.Label({style_class: 'tokengauge-panel-percent', y_align: Clutter.ActorAlign.CENTER});
        panelBox.add_child(this._panelIcon);
        panelBox.add_child(this._panelGlyph);
        panelBox.add_child(this._panelPercent);
        this.add_child(panelBox);

        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        this._content = box(true, {style_class: 'tokengauge-menu', x_expand: true});
        item.add_child(this._content);
        this.menu.addMenuItem(item);

        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open && this._menuDirty) {
                this._menuDirty = false;
                this._renderMenu();
            }
            this._setLive(open);
        });
        this.connect('scroll-event', (_actor, event) => this._onScroll(event));
        this._settingsChangedId = this._settings.connect('changed', (_s, key) => {
            if (key === 'refresh-interval')
                this._restartTimer();
            else
                this._render();
        });

        this._render();
        this._restartTimer();
    }

    // Middle click refreshes without opening the menu; the shell's own handling
    // of the other buttons (menu toggle) is left alone.
    vfunc_event(event) {
        if (event.type() === Clutter.EventType.BUTTON_PRESS &&
            event.get_button() === Clutter.BUTTON_MIDDLE) {
            this._action('--refresh');
            return Clutter.EVENT_STOP;
        }
        return super.vfunc_event(event);
    }

    _onScroll(event) {
        const rows = this._snapshot.rows || [];
        if (rows.length < 2)
            return Clutter.EVENT_PROPAGATE;
        const direction = event.get_scroll_direction();
        let delta;
        if (direction === Clutter.ScrollDirection.UP)
            delta = -1;
        else if (direction === Clutter.ScrollDirection.DOWN)
            delta = 1;
        else
            return Clutter.EVENT_PROPAGATE;
        const next = (this._selectedIndex + delta + rows.length) % rows.length;
        this._selectedProviderId = String(rows[next].provider);
        this._render();
        return Clutter.EVENT_STOP;
    }

    // ---- data ---------------------------------------------------------------

    _binary() {
        return this._settings.get_string('waybar-binary') || 'tokengauge-waybar';
    }

    // gnome-shell inherits the session PATH, which often lacks the user bin dirs
    // the installer drops the binaries into.
    // A superseded request must not touch shared state: its cancellation lands
    // after the newer request has already set it up.
    //
    // `onSettled` fires when *this* request ends, superseded or not. Anything
    // that owns a flag for the duration of its command has to clear it there:
    // clearing it in the shared completion path let an unrelated refresh
    // finishing mid-update put the Update button back to "Update".
    _run(command, onDone, onSettled = null) {
        this._cancel();
        const requestId = this._requestId;
        const isCurrent = () => requestId === this._requestId;
        this._cancellable = new Gio.Cancellable();
        const wrapped = `export PATH="$HOME/.local/bin:$HOME/bin:/usr/local/bin:$PATH"; ${command}`;
        let proc;
        try {
            proc = Gio.Subprocess.new(
                ['sh', '-c', wrapped],
                Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
        } catch (e) {
            onSettled?.();
            this._lastError = `${e}`;
            this._render();
            return;
        }
        proc.communicate_utf8_async(null, this._cancellable, (source, result) => {
            let stdout, stderr;
            try {
                [, stdout, stderr] = source.communicate_utf8_finish(result);
            } catch (e) {
                onSettled?.();
                if (!isCurrent())
                    return;
                if (!e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED)) {
                    this._lastError = `${e}`;
                    this._render();
                }
                return;
            }
            onSettled?.();
            if (!isCurrent())
                return;
            onDone(source.get_successful(), stdout ?? '', stderr ?? '');
        });
    }

    _refreshSnapshot(command, onSettled = null) {
        this._run(command, (successful, stdout, stderr) => {
            if (!successful) {
                this._lastError = (stderr || '').trim() || _('snapshot command failed');
                this._render();
                return;
            }
            try {
                const parsed = JSON.parse(stdout);
                this._snapshot = parsed;
                this._lastError = '';
            } catch (e) {
                this._lastError = `parse error: ${e}`;
            }
            this._watchRevision();
            this._render();
        }, onSettled);
    }

    // Watch the few bytes the binary rewrites after every fetch, so a fetch by
    // the daemon or another frontend lands here at once instead of on the next
    // poll. The snapshot itself still only ever comes from `--json`.
    _watchRevision() {
        const path = this._snapshot?.revision_file || '';
        if (path === '' || path === this._revisionFile)
            return;
        this._stopWatchingRevision();
        this._revisionFile = path;
        try {
            this._revisionMonitor = Gio.File.new_for_path(path).monitor_file(
                Gio.FileMonitorFlags.NONE, null);
            // One in-place rewrite raises more than one event; coalesce them
            // into a single re-read.
            this._revisionMonitor.connect('changed', () => {
                if (this._revisionSettleId)
                    GLib.Source.remove(this._revisionSettleId);
                this._revisionSettleId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
                    this._revisionSettleId = 0;
                    if (!this._updating)
                        this._reload();
                    return GLib.SOURCE_REMOVE;
                });
            });
        } catch (e) {
            // No watcher: the poll timer still carries the panel.
            this._revisionMonitor = null;
            this._revisionFile = '';
        }
    }

    _stopWatchingRevision() {
        if (this._revisionSettleId) {
            GLib.Source.remove(this._revisionSettleId);
            this._revisionSettleId = 0;
        }
        if (this._revisionMonitor) {
            this._revisionMonitor.cancel();
            this._revisionMonitor = null;
        }
        this._revisionFile = '';
    }

    _reload() {
        this._refreshSnapshot(`${shellQuote(this._binary())} --json`);
    }

    _action(flag, arg) {
        const bin = shellQuote(this._binary());
        const suffix = arg === undefined ? '' : ` ${shellQuote(arg)}`;
        this._refreshSnapshot(`${bin} ${flag}${suffix} && ${bin} --json`);
    }

    // `--sync-setup` returns as soon as it has spawned a terminal, so the
    // snapshot read chained behind it is not waiting on the user. `&&` and a
    // kept stderr, exactly as `_applyUpdate` does it: with `;` the compound
    // command exits 0 whatever setup did, and "no terminal found" would be
    // dropped along with the exit status. Nothing needs refreshing after a
    // failure anyway - setup changes nothing until the user acts in the TUI.
    _openSyncSetup() {
        const bin = shellQuote(this._binary());
        this._refreshSnapshot(`${bin} --sync-setup >/dev/null && ${bin} --json`);
    }

    // --update's human-readable stdout is discarded so only the JSON payload
    // reaches JSON.parse; stderr still surfaces a failed update.
    _applyUpdate() {
        this._updating = true;
        this._render();
        const bin = shellQuote(this._binary());
        this._refreshSnapshot(`${bin} --update >/dev/null && ${bin} --json`, () => {
            this._updating = false;
            this._render();
        });
    }

    get _selectedIndex() {
        const rows = this._rows;
        const chosen = rows.findIndex(row => String(row.provider) === this._selectedProviderId);
        if (chosen >= 0)
            return chosen;
        // Nothing chosen, or the chosen provider has gone: follow the pin. The
        // panel reports its percentage, and opening the menu on a different
        // provider reads as a bug.
        const pinned = String(this._snapshot.primary || '').toLowerCase();
        if (pinned !== '') {
            const pin = rows.findIndex(row => String(row.provider).toLowerCase() === pinned);
            if (pin >= 0)
                return pin;
        }
        return 0;
    }

    _restartTimer() {
        if (this._timeoutId)
            GLib.Source.remove(this._timeoutId);
        const interval = Math.max(15, this._settings.get_int('refresh-interval'));
        this._timeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, interval, () => {
            // A periodic poll would cancel the in-flight --update read and leave
            // the button stuck on "Updating…" until the next tick.
            if (!this._updating)
                this._reload();
            return GLib.SOURCE_CONTINUE;
        });
        this._reload();
    }

    // The menu is open, so the snapshot is re-read on a much shorter cycle than
    // the poll. A reset time is counted against the clock at render time, which
    // means the countdown only moves when `--json` runs again - a menu left open
    // otherwise keeps the countdown it opened with. The binary serves the
    // snapshot it already has and refetches only once that snapshot has aged
    // past `refresh_secs`, so this costs a subprocess, not a provider call.
    _setLive(live) {
        if (this._liveTimeoutId) {
            GLib.Source.remove(this._liveTimeoutId);
            this._liveTimeoutId = 0;
        }
        if (!live)
            return;
        if (!this._updating)
            this._reload();
        this._liveTimeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, LIVE_INTERVAL_SECS, () => {
            // A poll would cancel an in-flight `--update` read and leave the
            // button stuck on "Updating…", exactly as it would on the long timer.
            if (!this._updating)
                this._reload();
            return GLib.SOURCE_CONTINUE;
        });
    }

    // Also invalidates the in-flight request, so a queued callback cannot render
    // through a destroyed indicator.
    //
    // The subprocess is deliberately left to finish. The only long command here
    // is `--update`, which stages a download and renames it into place; killing
    // it halfway is worse than letting a process we stopped listening to run to
    // completion, and the binary is the one that knows how to do that safely.
    _cancel() {
        this._requestId++;
        if (this._cancellable) {
            this._cancellable.cancel();
            this._cancellable = null;
        }
    }

    // ---- helpers ------------------------------------------------------------

    get _rows() {
        return this._snapshot.rows || [];
    }

    get _row() {
        const rows = this._rows;
        if (rows.length === 0)
            return null;
        return rows[this._selectedIndex];
    }

    _theme() {
        return {...FALLBACK_THEME, ...(this._snapshot.theme || {})};
    }

    // The headline number and its tier come off the row's `bar`, resolved by
    // the core under the configured window. This used to pick the window here
    // and carry its own copy of the 50/80 boundaries to tint it with.
    _bar(row) {
        return row?.bar ?? {percent: null, tone: 'dim'};
    }

    _providerGicon(row) {
        if (!row?.icon_svg)
            return null;
        const file = Gio.File.new_for_path(row.icon_svg);
        if (!file.query_exists(null))
            return null;
        return new Gio.FileIcon({file});
    }

    _providerIcon(row, size) {
        const gicon = this._providerGicon(row);
        return gicon ? new St.Icon({gicon, icon_size: size}) : null;
    }

    // ---- rendering ----------------------------------------------------------

    _render() {
        this._renderPanel();
        if (this.menu.isOpen) {
            this._menuDirty = false;
            this._renderMenu();
        } else {
            this._menuDirty = true;
        }
    }

    _renderPanel() {
        const row = this._row;
        const bar = this._bar(row);
        const gicon = this._providerGicon(row);

        this._panelIcon.visible = gicon !== null;
        if (gicon)
            this._panelIcon.gicon = gicon;
        this._panelGlyph.visible = !gicon && !!row?.glyph;
        if (this._panelGlyph.visible) {
            this._panelGlyph.text = row.glyph;
            this._panelGlyph.style = `color: ${row.color || this._theme().dim};`;
        }
        if (!gicon && !this._panelGlyph.visible) {
            this._panelIcon.visible = true;
            this._panelIcon.gicon = Gio.ThemedIcon.new('utilities-system-monitor-symbolic');
        }

        this._panelPercent.visible = this._settings.get_boolean('show-percent');
        this._panelPercent.text =
            bar.percent === null || bar.percent === undefined ? '—' : `${bar.percent}%`;
        this._panelPercent.style = `color: ${this._toneColor(bar.tone)};`;
    }

    _renderMenu() {
        this._content.destroy_all_children();
        this._content.add_child(this._header());

        const errors = this._snapshot.errors || [];
        if (this._lastError || errors.length > 0) {
            const text = this._lastError ||
                errors.map(e => `${e.provider || '?'}: ${e.message || e.raw || 'error'}`).join('\n');
            this._content.add_child(label(text, 'tokengauge-error', `color: ${this._theme().red};`));
        }

        if (this._snapshot.update?.available)
            this._content.add_child(this._updateBanner());

        if (this._rows.length > 0)
            this._content.add_child(this._tabStrip());

        const row = this._row;
        if (!row) {
            this._content.add_child(label(_('No provider data yet.'), 'tokengauge-dim'));
            return;
        }

        this._content.add_child(this._providerCard(row));

        if (this._historyOpen) {
            this._content.add_child(this._historyScreen(row));
        } else {
            // The core hands over an ordered list of sections, each naming its
            // own kind; one builder per kind draws it. A new section in the
            // core appears here with no edit to this file.
            for (const section of row.panel || [])
                this._content.add_child(this._section(section));

            this._content.add_child(this._pinSection());
        }

        if (row.updated)
            this._content.add_child(label(`${_('Updated')} ${row.updated}`, 'tokengauge-footer'));
    }

    /// The history screen. Every string comes off the row; the chart is the
    /// only part this file decides.
    _historyScreen(row) {
        const history = row.history || {};
        const screen = box(true, {style_class: 'tokengauge-section', x_expand: true});
        screen.add_child(label(_('History'), 'tokengauge-section-title'));

        const series = Array.isArray(history.series) ? history.series : [];
        const index = Math.min(this._historyRange, Math.max(0, series.length - 1));

        const strip = box(false, {style_class: 'tokengauge-tabs', x_expand: true});
        series.forEach((entry, at) => {
            const button = new St.Button({
                style_class: at === index
                    ? 'tokengauge-tab tokengauge-tab-active' : 'tokengauge-tab',
                label: entry.label,
                can_focus: true,
            });
            button.connect('clicked', () => {
                this._historyRange = at;
                this._render();
            });
            strip.add_child(button);
        });
        screen.add_child(strip);

        const current = series[index];
        if (!current) {
            screen.add_child(label(_('No history yet.'), 'tokengauge-dim'));
            return screen;
        }

        screen.add_child(label(
            `${current.total_usd}  ·  ${current.total_tokens} ${_('tokens')}` +
            `  ·  ${_('avg')} ${current.average_usd}`,
            'tokengauge-card-title'));

        if (current.empty) {
            screen.add_child(label(_('Nothing spent in this range.'), 'tokengauge-dim'));
        } else {
            // The fill stays the series colour: `partial` already carries the
            // "in progress" signal as reduced alpha, and taking the dim tone
            // as well drew that step as a ghost rather than as data.
            const fill = p => (p.tone === 'critical' ? this._theme().red : this._theme().neutral);
            screen.add_child(historyChart(current.points, fill, 140));
            const edges = box(false, {x_expand: true});
            edges.add_child(label(current.points[0].full_label, 'tokengauge-footer'));
            edges.add_child(spacer());
            edges.add_child(label(
                current.points[current.points.length - 1].full_label, 'tokengauge-footer'));
            screen.add_child(edges);
        }

        const notes = [history.covers].concat(history.notes || []).filter(Boolean).join('  ·  ');
        if (notes)
            screen.add_child(label(notes, 'tokengauge-footer'));
        return screen;
    }

    _iconButton(iconName, tooltip, onClick, hint) {
        const button = new St.Button({
            style_class: 'tokengauge-icon-button',
            child: new St.Icon({icon_name: iconName, icon_size: 16}),
            can_focus: true,
        });
        button.accessible_name = tooltip;
        button.connect('clicked', onClick);
        // The accessible name is what a screen reader says; `hint` is what a
        // pointer gets, for the buttons whose worth depends on something the
        // icon cannot show.
        return hint ? attachTooltip(button, `${tooltip}\n${hint}`) : button;
    }

    _header() {
        const header = box(false, {style_class: 'tokengauge-header', x_expand: true});
        header.add_child(label('TokenGauge', 'tokengauge-title'));
        header.add_child(spacer());
        header.add_child(this._iconButton('view-refresh-symbolic', _('Refresh'),
            () => this._action('--refresh'),
            this._row?.refresh_hint));
        header.add_child(this._iconButton('web-browser-symbolic', _('Open dashboard'),
            () => {
                this.menu.close();
                this._action('--open=dashboard');
            }));
        header.add_child(this._iconButton(
            this._historyOpen ? 'go-previous-symbolic' : 'org.gnome.Settings-usage-symbolic',
            this._historyOpen ? _('Back to the panel') : _('History'),
            () => {
                this._historyOpen = !this._historyOpen;
                this._render();
            }));
        header.add_child(this._iconButton('folder-remote-symbolic', _('Set up fleet sync'),
            () => {
                this.menu.close();
                this._openSyncSetup();
            }));
        header.add_child(this._iconButton('emblem-system-symbolic', _('Settings'),
            () => {
                this.menu.close();
                this._extension.openPreferences();
            }));
        return header;
    }

    _updateBanner() {
        const banner = box(false, {style_class: 'tokengauge-banner', x_expand: true});
        const latest = this._snapshot.update?.latest;
        const text = latest ? `${_('Update available')}: v${latest}` : _('Update available');
        banner.add_child(label(text, 'tokengauge-update-text', `color: ${this._theme().green};`));
        banner.add_child(spacer());
        const button = new St.Button({
            style_class: 'tokengauge-button',
            label: this._updating ? _('Updating…') : _('Update'),
            can_focus: true,
            reactive: !this._updating,
        });
        button.connect('clicked', () => this._applyUpdate());
        banner.add_child(button);
        return banner;
    }

    _tabStrip() {
        const strip = box(false, {style_class: 'tokengauge-tabs', x_expand: true});
        this._rows.forEach((row, index) => {
            const content = box(false, {style_class: 'tokengauge-tab-content'});
            const icon = this._providerIcon(row, 14);
            if (icon)
                content.add_child(icon);
            content.add_child(new St.Label({
                text: row.label || row.provider,
                y_align: Clutter.ActorAlign.CENTER,
            }));
            const selected = index === this._selectedIndex;
            const button = new St.Button({
                style_class: selected ? 'tokengauge-tab tokengauge-tab-active' : 'tokengauge-tab',
                child: content,
                can_focus: true,
            });
            button.connect('clicked', () => {
                this._selectedProviderId = String(row.provider);
                this._render();
            });
            strip.add_child(button);
        });
        return strip;
    }

    _providerCard(row) {
        const card = box(false, {style_class: 'tokengauge-card', x_expand: true});
        const icon = this._providerIcon(row, 22);
        if (icon)
            card.add_child(icon);
        const text = box(true, {x_expand: true});
        text.add_child(label(row.label || row.provider, 'tokengauge-card-title'));
        const subtitle = [row.plan_label, row.source].filter(Boolean).join(' · ');
        if (subtitle)
            text.add_child(label(subtitle, 'tokengauge-dim'));
        card.add_child(text);
        if (row.stale)
            card.add_child(label(_('stale'), 'tokengauge-dim', `color: ${this._theme().yellow};`));
        return card;
    }

    /// A tone name from the core, mapped onto the snapshot theme.
    _toneColor(tone) {
        const t = this._theme();
        switch (tone) {
        case 'good': return t.green;
        case 'warn': return t.yellow;
        case 'critical': return t.red;
        case 'dim': return t.dim;
        default: return t.neutral;
        }
    }

    _section(section) {
        const box_ = box(true, {style_class: 'tokengauge-section', x_expand: true});
        box_.add_child(label(section.title, 'tokengauge-section-title'));
        for (const row of section.rows) {
            switch (section.kind) {
            case 'meters': box_.add_child(this._meter(row)); break;
            case 'bars': box_.add_child(this._barRow(row)); break;
            case 'rows': box_.add_child(this._keyRow(row)); break;
            }
        }
        return box_;
    }

    // Label and value on one line, a full-width bar under it, then the reset
    // note and the pace badge.
    _meter(row) {
        const meter = box(true, {style_class: 'tokengauge-meter', x_expand: true});

        const top = box(false, {x_expand: true});
        top.add_child(label(row.label, 'tokengauge-meter-label'));
        top.add_child(spacer());
        top.add_child(label(row.value, 'tokengauge-meter-value',
            `color: ${this._toneColor(row.tone)};`));
        meter.add_child(top);

        const track = new St.Widget({
            style_class: 'tokengauge-meter-track',
            layout_manager: new Clutter.BinLayout(),
            x_expand: true,
        });
        track.add_child(barFill(row.fraction, 4, 'tokengauge-meter-fill',
            `color: ${this._toneColor(row.tone)};`));
        meter.add_child(track);

        if (row.footnote || row.badge) {
            const trailing = box(false, {x_expand: true});
            if (row.footnote)
                trailing.add_child(label(row.footnote, 'tokengauge-dim'));
            if (row.badge) {
                trailing.add_child(label(`  ·  ${row.badge}`, 'tokengauge-dim',
                    `color: ${this._toneColor(row.badge_tone)};`));
            }
            trailing.add_child(spacer());
            meter.add_child(trailing);
        }
        return attachTooltip(meter, row.tooltip);
    }

    // One line per row with the share bar filling the row behind the text, so a
    // seven-day list and a model breakdown both stay on one screen.
    _barRow(row) {
        const wrap = new St.Widget({
            style_class: 'tokengauge-bar-row',
            layout_manager: new Clutter.BinLayout(),
            x_expand: true,
        });
        wrap.add_child(barFill(row.fraction, 3, 'tokengauge-bar-fill'));

        const line = box(false, {x_expand: true, style_class: 'tokengauge-bar-text'});
        const weight = row.emphasized ? 'font-weight: bold;' : '';
        line.add_child(label(row.label, 'tokengauge-cost-label', weight));
        line.add_child(spacer());
        const value = row.suffix ? `${row.value}  ·  ${row.suffix}` : row.value;
        line.add_child(label(value, 'tokengauge-cost-value', weight));
        wrap.add_child(line);
        return attachTooltip(wrap, row.tooltip);
    }

    // Label and value on one line; a badge and a suffix drop to a caption line
    // under it, because beside the label the two of them leave a sentence
    // fighting over what is left of a narrow popup. The caption tracks the
    // right edge, where the figure it qualifies sits.
    _keyRow(row) {
        const wrap = box(true, {x_expand: true});

        const line = box(false, {x_expand: true});
        line.add_child(label(row.label, 'tokengauge-cost-label'));
        line.add_child(spacer());
        line.add_child(label(row.value, 'tokengauge-cost-value'));
        wrap.add_child(line);

        if (row.badge || row.suffix) {
            const caption = box(false, {x_expand: true});
            caption.add_child(spacer());
            if (row.badge) {
                caption.add_child(label(row.badge, 'tokengauge-dim',
                    `color: ${this._toneColor(row.badge_tone)};`));
            }
            // The separator divides a badge from a suffix, so a row with no
            // badge must not open on one.
            if (row.suffix)
                caption.add_child(label(row.badge ? `  ·  ${row.suffix}` : row.suffix, 'tokengauge-dim'));
            wrap.add_child(caption);
        }
        return attachTooltip(wrap, row.tooltip);
    }

    _pinSection() {
        const section = box(true, {style_class: 'tokengauge-section', x_expand: true});
        section.add_child(label(_('Pin to bar'), 'tokengauge-section-title'));

        const strip = box(false, {style_class: 'tokengauge-tabs', x_expand: true});
        // `--set-primary highest` clears the pin; the bar then shows the first
        // enabled provider, not the busiest one.
        const choices = [{name: 'highest', text: _('Highest usage')}].concat(
            this._rows.map(row => ({
                name: (row.provider || '').toLowerCase(),
                text: row.label || row.provider,
            })));
        const current = this._snapshot.primary || 'highest';
        for (const choice of choices) {
            const active = choice.name === current;
            const button = new St.Button({
                style_class: active ? 'tokengauge-tab tokengauge-tab-active' : 'tokengauge-tab',
                label: choice.text,
                can_focus: true,
            });
            button.connect('clicked', () => this._action('--set-primary', choice.name));
            strip.add_child(button);
        }
        section.add_child(strip);
        return section;
    }

    destroy() {
        if (this._timeoutId) {
            GLib.Source.remove(this._timeoutId);
            this._timeoutId = 0;
        }
        this._setLive(false);
        if (this._settingsChangedId) {
            this._settings.disconnect(this._settingsChangedId);
            this._settingsChangedId = 0;
        }
        this._stopWatchingRevision();
        this._cancel();
        super.destroy();
    }
});

export default class TokenGaugeExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this);
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}

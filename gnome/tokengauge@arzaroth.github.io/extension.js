import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension, gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';

const METER_WIDTH = 288;
const CHART_HEIGHT = 44;

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

function fmtUsd(v) {
    if (v === null || v === undefined)
        return '—';
    return `$${Number(v).toFixed(2)}`;
}

const Indicator = GObject.registerClass(
class TokenGaugeIndicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.5, 'TokenGauge');

        this._extension = extension;
        this._settings = extension.getSettings();
        this._snapshot = {rows: [], errors: [], enabled: [], providers: []};
        this._lastError = '';
        this._selectedIndex = 0;
        this._userSelected = false;
        this._updating = false;
        this._cancellable = null;
        this._requestId = 0;
        this._timeoutId = 0;
        this._menuDirty = true;

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
        if (direction === Clutter.ScrollDirection.UP)
            this._selectedIndex = (this._selectedIndex - 1 + rows.length) % rows.length;
        else if (direction === Clutter.ScrollDirection.DOWN)
            this._selectedIndex = (this._selectedIndex + 1) % rows.length;
        else
            return Clutter.EVENT_PROPAGATE;
        this._userSelected = true;
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
    _run(command, onDone) {
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
            this._updating = false;
            this._lastError = `${e}`;
            this._render();
            return;
        }
        proc.communicate_utf8_async(null, this._cancellable, (source, result) => {
            let stdout, stderr;
            try {
                [, stdout, stderr] = source.communicate_utf8_finish(result);
            } catch (e) {
                if (!isCurrent())
                    return;
                this._updating = false;
                if (!e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED)) {
                    this._lastError = `${e}`;
                    this._render();
                }
                return;
            }
            if (!isCurrent())
                return;
            onDone(source.get_successful(), stdout ?? '', stderr ?? '');
        });
    }

    _refreshSnapshot(command) {
        this._run(command, (successful, stdout, stderr) => {
            this._updating = false;
            if (!successful) {
                this._lastError = (stderr || '').trim() || _('snapshot command failed');
                this._render();
                return;
            }
            try {
                const parsed = JSON.parse(stdout);
                this._snapshot = parsed;
                const n = (parsed.rows || []).length;
                if (!this._userSelected)
                    this._selectedIndex = this._primaryIndex(parsed);
                else if (this._selectedIndex >= n)
                    this._selectedIndex = 0;
                this._lastError = '';
            } catch (e) {
                this._lastError = `parse error: ${e}`;
            }
            this._render();
        });
    }

    _reload() {
        this._refreshSnapshot(`${shellQuote(this._binary())} --json`);
    }

    _action(flag, arg) {
        const bin = shellQuote(this._binary());
        const suffix = arg === undefined ? '' : ` ${shellQuote(arg)}`;
        this._refreshSnapshot(`${bin} ${flag}${suffix} && ${bin} --json`);
    }

    // --update's human-readable stdout is discarded so only the JSON payload
    // reaches JSON.parse; stderr still surfaces a failed update.
    _applyUpdate() {
        this._updating = true;
        this._render();
        const bin = shellQuote(this._binary());
        this._refreshSnapshot(`${bin} --update >/dev/null && ${bin} --json`);
    }

    _primaryIndex(snapshot) {
        const rows = snapshot.rows || [];
        if (snapshot.primary) {
            for (let i = 0; i < rows.length; i++) {
                if ((rows[i].provider || '').toLowerCase() === snapshot.primary)
                    return i;
            }
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

    // Also invalidates the in-flight request, so a queued callback cannot render
    // through a destroyed indicator.
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
        return rows[Math.min(this._selectedIndex, rows.length - 1)];
    }

    _theme() {
        return {...FALLBACK_THEME, ...(this._snapshot.theme || {})};
    }

    _tierColor(pct) {
        const t = this._theme();
        if (pct === null || pct === undefined)
            return t.dim;
        if (pct >= 80)
            return t.red;
        if (pct >= 50)
            return t.yellow;
        return t.green;
    }

    _windowPercent(row) {
        if (!row)
            return null;
        return this._snapshot.window === 'weekly' ? row.weekly_used : row.session_used;
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
        const pct = this._windowPercent(row);
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
        this._panelPercent.text = pct === null || pct === undefined ? '—' : `${pct}%`;
        this._panelPercent.style = `color: ${this._tierColor(pct)};`;
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

        const windowLabels = row.window_labels || [_('Session'), _('Weekly'), _('Tertiary')];
        this._content.add_child(this._meter(windowLabels[0], row.session_used, row.session_reset, row.session_pace));
        if (row.weekly_used !== null && row.weekly_used !== undefined)
            this._content.add_child(this._meter(windowLabels[1], row.weekly_used, row.weekly_reset, row.weekly_pace));
        if (row.tertiary_used !== null && row.tertiary_used !== undefined)
            this._content.add_child(this._meter(windowLabels[2], row.tertiary_used, row.tertiary_reset, null));
        // Skip the windows the provider exposes a slot for but reports nothing
        // in; they are a permanently empty meter here. The waybar tooltip keeps
        // them so its shape does not shift.
        for (const extra of row.extra_windows || []) {
            if (extra.placeholder === true)
                continue;
            this._content.add_child(this._meter(extra.title, extra.used, extra.reset, null));
        }

        if (row.cost) {
            this._content.add_child(this._costSection(row.cost));
            const history = row.cost.weekly_cost_history || [];
            if (history.length > 0)
                this._content.add_child(this._chart(history));
        }

        this._content.add_child(this._pinSection());

        if (row.updated)
            this._content.add_child(label(`${_('Updated')} ${row.updated}`, 'tokengauge-footer'));
    }

    _iconButton(iconName, tooltip, onClick) {
        const button = new St.Button({
            style_class: 'tokengauge-icon-button',
            child: new St.Icon({icon_name: iconName, icon_size: 16}),
            can_focus: true,
        });
        button.accessible_name = tooltip;
        button.connect('clicked', onClick);
        return button;
    }

    _header() {
        const header = box(false, {style_class: 'tokengauge-header', x_expand: true});
        header.add_child(label('TokenGauge', 'tokengauge-title'));
        header.add_child(spacer());
        header.add_child(this._iconButton('view-refresh-symbolic', _('Refresh'),
            () => this._action('--refresh')));
        header.add_child(this._iconButton('web-browser-symbolic', _('Open dashboard'),
            () => {
                this.menu.close();
                this._action('--open=dashboard');
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
            const selected = index === Math.min(this._selectedIndex, this._rows.length - 1);
            const button = new St.Button({
                style_class: selected ? 'tokengauge-tab tokengauge-tab-active' : 'tokengauge-tab',
                child: content,
                can_focus: true,
            });
            button.connect('clicked', () => {
                this._userSelected = true;
                this._selectedIndex = index;
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

    _meter(name, value, reset, pace) {
        const meter = box(true, {style_class: 'tokengauge-meter', x_expand: true});

        const top = box(false, {x_expand: true});
        top.add_child(label(name, 'tokengauge-meter-label'));
        top.add_child(spacer());
        const pct = value === null || value === undefined ? '—' : `${value}%`;
        top.add_child(label(pct, 'tokengauge-meter-value', `color: ${this._tierColor(value)};`));
        meter.add_child(top);

        const track = new St.Widget({
            style_class: 'tokengauge-meter-track',
            style: `width: ${METER_WIDTH}px;`,
            layout_manager: new Clutter.BinLayout(),
        });
        const clamped = Math.max(0, Math.min(100, value || 0));
        const fill = new St.Widget({
            style_class: 'tokengauge-meter-fill',
            style: `width: ${Math.round(METER_WIDTH * clamped / 100)}px; background-color: ${this._tierColor(value)};`,
            x_align: Clutter.ActorAlign.START,
        });
        track.add_child(fill);
        meter.add_child(track);

        const trailing = [reset, pace].filter(t => t && t !== '—').join('  ·  ');
        if (trailing)
            meter.add_child(label(trailing, 'tokengauge-dim'));
        return meter;
    }

    _costRow(name, amount) {
        const row = box(false, {x_expand: true});
        row.add_child(label(name, 'tokengauge-cost-label'));
        row.add_child(spacer());
        row.add_child(label(amount, 'tokengauge-cost-value'));
        return row;
    }

    _costSection(cost) {
        const section = box(true, {style_class: 'tokengauge-section', x_expand: true});
        section.add_child(label(_('Cost'), 'tokengauge-section-title'));
        section.add_child(this._costRow(_('Today'), fmtUsd(cost.today_usd)));
        section.add_child(this._costRow(_('Session'), fmtUsd(cost.session_usd)));
        section.add_child(this._costRow(_('7-day'), fmtUsd(cost.weekly_usd)));
        section.add_child(this._costRow(_('Month'), fmtUsd(cost.monthly_usd)));
        if (cost.burn_rate) {
            section.add_child(this._costRow(_('Burn rate'),
                `${fmtUsd(cost.burn_rate.cost_per_hour)}/hr`));
        }
        return section;
    }

    _chart(history) {
        const section = box(true, {style_class: 'tokengauge-section', x_expand: true});
        section.add_child(label(_('Last 7 days'), 'tokengauge-section-title'));

        const max = history.reduce((m, v) => (v > m ? v : m), 0) || 1;
        const bars = box(false, {style_class: 'tokengauge-chart', x_expand: true});
        for (const usd of history) {
            const column = new St.Widget({
                style_class: 'tokengauge-chart-column',
                layout_manager: new Clutter.BinLayout(),
                x_expand: true,
                height: CHART_HEIGHT,
            });
            const bar = new St.Widget({
                style_class: 'tokengauge-chart-bar',
                style: `height: ${Math.max(2, Math.round(CHART_HEIGHT * usd / max))}px;`,
                y_align: Clutter.ActorAlign.END,
                x_expand: true,
            });
            column.add_child(bar);
            bars.add_child(column);
        }
        section.add_child(bars);
        return section;
    }

    _pinSection() {
        const section = box(true, {style_class: 'tokengauge-section', x_expand: true});
        section.add_child(label(_('Pin to bar'), 'tokengauge-section-title'));

        const strip = box(false, {style_class: 'tokengauge-tabs', x_expand: true});
        // `--set-primary highest` clears the pin; the bar then shows the first
        // enabled provider, not the busiest one.
        const choices = [{name: 'highest', text: _('Auto')}].concat(
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
        if (this._settingsChangedId) {
            this._settings.disconnect(this._settingsChangedId);
            this._settingsChangedId = 0;
        }
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

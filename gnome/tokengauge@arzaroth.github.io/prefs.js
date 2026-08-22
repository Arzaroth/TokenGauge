import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences, gettext as _} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

function shellQuote(s) {
    return `'${String(s).replace(/'/g, "'\\''")}'`;
}

// gnome-shell and gnome-extensions-app both inherit the session PATH, which
// often lacks the user bin dirs the installer drops the binaries into.
function run(command, cancellable, callback) {
    const wrapped = `export PATH="$HOME/.local/bin:$HOME/bin:/usr/local/bin:$PATH"; ${command}`;
    let proc;
    try {
        proc = Gio.Subprocess.new(
            ['sh', '-c', wrapped],
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
    } catch (e) {
        callback(false, '', `${e}`);
        return;
    }
    proc.communicate_utf8_async(null, cancellable, (source, result) => {
        try {
            const [, stdout, stderr] = source.communicate_utf8_finish(result);
            callback(source.get_successful(), stdout ?? '', stderr ?? '');
        } catch (e) {
            if (e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                return;
            callback(false, '', `${e}`);
        }
    });
}

function titleCase(name) {
    return name.charAt(0).toUpperCase() + name.slice(1);
}

export default class TokenGaugePreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();
        // Callbacks touch rows that die with the window.
        const cancellable = new Gio.Cancellable();
        window.connect('close-request', () => cancellable.cancel());
        const page = new Adw.PreferencesPage();
        window.add(page);

        const panel = new Adw.PreferencesGroup({title: _('Panel')});
        page.add(panel);

        const binary = new Adw.EntryRow({
            title: _('tokengauge-waybar binary'),
            show_apply_button: true,
        });
        binary.text = settings.get_string('waybar-binary');
        binary.connect('apply', () => settings.set_string('waybar-binary', binary.text));
        panel.add(binary);

        const interval = new Adw.SpinRow({
            title: _('Refresh interval'),
            subtitle: _('Seconds between snapshot reads'),
            adjustment: new Gtk.Adjustment({lower: 15, upper: 3600, step_increment: 5}),
        });
        settings.bind('refresh-interval', interval, 'value', Gio.SettingsBindFlags.DEFAULT);
        panel.add(interval);

        const showPercent = new Adw.SwitchRow({
            title: _('Show percent in panel'),
            subtitle: _('Off shows only the provider icon'),
        });
        settings.bind('show-percent', showPercent, 'active', Gio.SettingsBindFlags.DEFAULT);
        panel.add(showPercent);

        const providers = new Adw.PreferencesGroup({
            title: _('Providers'),
            description: _('Written to ~/.config/tokengauge/config.toml, shared with the Waybar module'),
        });
        page.add(providers);
        this._fillProviders(settings, providers, cancellable);
    }

    // The provider list and their enabled state live in the shared config, not
    // in GSettings, so both come from the snapshot the binary emits.
    _fillProviders(settings, group, cancellable) {
        const status = new Adw.ActionRow({title: _('Reading providers…')});
        group.add(status);

        const bin = () => shellQuote(settings.get_string('waybar-binary') || 'tokengauge-waybar');

        // Both versions, because the extension and the binary are installed
        // separately: `--update` replaces binaries, and until the extension is
        // reinstalled it keeps driving whatever JavaScript this box already had.
        // Showing only one of them is what made that skew invisible.
        const about = new Adw.PreferencesGroup({title: _('About')});
        const extensionVersion = this.metadata['version-name'] || null;
        const version = new Adw.ActionRow({
            title: _('TokenGauge'),
            subtitle: _('Reading version…'),
        });
        about.add(version);

        run(`${bin()} --version`, cancellable, (ok, stdout, stderr) => {
            if (!ok) {
                version.subtitle = (stderr || '').trim().split('\n')[0] ||
                    _('could not read the version');
                return;
            }
            const binaryVersion = (stdout || '').trim().split(/\s+/).pop();
            if (!extensionVersion || extensionVersion === binaryVersion) {
                version.subtitle = `v${binaryVersion}`;
                return;
            }
            version.subtitle =
                _('extension v%s, binary v%s - reinstall the extension: %s --install-frontend gnome')
                    .format(extensionVersion, binaryVersion, 'tokengauge-waybar');
        });

        run(`${bin()} --json`, cancellable, (successful, stdout, stderr) => {
            if (!successful) {
                status.title = _('Could not read providers');
                status.subtitle = (stderr || '').trim().split('\n')[0] || _('snapshot command failed');
                return;
            }
            let snapshot;
            try {
                snapshot = JSON.parse(stdout);
            } catch (e) {
                status.title = _('Could not read providers');
                status.subtitle = `${e}`;
                return;
            }
            const names = snapshot.providers || [];
            if (names.length === 0) {
                status.title = _('No provider list in the snapshot');
                status.subtitle = _('Update tokengauge-waybar to 0.15.0 or newer');
                return;
            }
            group.remove(status);

            const enabled = snapshot.enabled || [];
            for (const name of names) {
                const row = new Adw.SwitchRow({
                    title: titleCase(name),
                    active: enabled.includes(name),
                });
                // The row stays insensitive until its write lands, so a second
                // toggle cannot finish first and leave the config inverted, and
                // a failed write snaps the switch back to what the config holds.
                let confirmed = row.active;
                let reverting = false;
                row.connect('notify::active', () => {
                    if (reverting)
                        return;
                    const desired = row.active;
                    const arg = shellQuote(`${name}=${desired ? 'true' : 'false'}`);
                    row.sensitive = false;
                    run(`${bin()} --set-provider ${arg}`, cancellable, (ok, _out, err) => {
                        row.sensitive = true;
                        if (!ok) {
                            row.subtitle = (err || '').trim().split('\n')[0] ||
                                _('could not update the config');
                            reverting = true;
                            row.active = confirmed;
                            reverting = false;
                        } else {
                            confirmed = desired;
                            row.subtitle = '';
                        }
                    });
                });
                group.add(row);
            }
        });

        page.add(about);
    }
}

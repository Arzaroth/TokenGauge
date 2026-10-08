// Drives the TokenGauge indicator inside a real shell: opens the popup on the
// provider asked for, writes its geometry to /out/probe.txt, and screenshots
// it scrolled to the top and to the bottom. Writes /out/done when finished,
// which is what ends the run.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const OUT = '/out';
const lines = [];

function record(line) {
    lines.push(line);
    GLib.file_set_contents(`${OUT}/probe.txt`, `${lines.join('\n')}\n`);
}

function finish() {
    GLib.file_set_contents(`${OUT}/done`, '');
}

function later(ms, step) {
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
        try {
            step();
        } catch (e) {
            record(`error: ${e}\n${e.stack}`);
            finish();
        }
        return GLib.SOURCE_REMOVE;
    });
}

function screenshot(name, then) {
    const stream = Gio.File.new_for_path(`${OUT}/${name}.png`)
        .replace(null, false, Gio.FileCreateFlags.NONE, null);
    new Shell.Screenshot().screenshot(false, stream, (shot, result) => {
        try {
            shot.screenshot_finish(result);
        } catch (e) {
            record(`screenshot ${name}: ${e}`);
        }
        stream.close(null);
        then();
    });
}

export default class Probe extends Extension {
    enable() {
        later(6000, () => {
            const indicator = Main.panel.statusArea['tokengauge@arzaroth.github.io'];
            if (!indicator) {
                record('error: the TokenGauge indicator is not in the top bar');
                finish();
                return;
            }
            const provider = GLib.getenv('PROVIDER');
            if (provider) {
                indicator._selectedProviderId = provider;
                indicator._render();
            }
            indicator.menu.open(false);

            later(1500, () => {
                const monitor = Main.layoutManager.primaryMonitor;
                const [, natural] = indicator._content.get_preferred_height(-1);
                record(`monitor: ${monitor.width}x${monitor.height}`);
                record(`popup: y=${indicator.menu.actor.y} height=${indicator.menu.actor.height}`);
                record(`content: natural=${natural} shown=${indicator._content.height}`);
                record(`sections: ${indicator._content.get_children().length}`);
                const popup = indicator.menu.actor;
                if (popup.y + popup.height > monitor.y + monitor.height)
                    record('error: the popup runs off the bottom of the monitor');
                if (indicator._content.height + 1 < natural)
                    record('error: the content is squeezed below its natural height instead of scrolled');
                screenshot('top', () => {
                    const scroll = indicator._scroll;
                    if (!scroll) {
                        record('scroll: none');
                        finish();
                        return;
                    }
                    const adjustment = scroll.vadjustment ?? scroll.vscroll.adjustment;
                    record(`scroll: upper=${adjustment.upper} page=${adjustment.page_size}`);
                    if (adjustment.upper + 1 < natural)
                        record('error: the scroll view cannot reach the bottom of the content');
                    adjustment.value = adjustment.upper - adjustment.page_size;
                    later(500, () => screenshot('bottom', finish));
                });
            });
        });
    }

    disable() {}
}

// Stands in for the shell's Main - the top bar and the layer a tooltip goes
// into, which is the only reason the extension touches it.

import St from 'gi://St';

export const layoutManager = {
    uiGroup: new St.Widget({style_class: 'ui-group'}),
};

export const panel = {
    statusArea: {},
    addToStatusArea(role, indicator) {
        panel.statusArea[role] = indicator;
        return indicator;
    },
};

export default {layoutManager, panel};

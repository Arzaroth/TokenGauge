// Stands in for the shell's Main - the top bar, the layer a tooltip goes
// into, and the monitor the popup sizes its scroll view against.

import St from 'gi://St';

const primaryMonitor = {index: 0, width: 1920, height: 1080};

export const layoutManager = {
    uiGroup: new St.Widget({style_class: 'ui-group'}),
    primaryMonitor,
    findMonitorForActor() {
        return primaryMonitor;
    },
    getWorkAreaForMonitor() {
        return {x: 0, y: 32, width: 1920, height: 1048};
    },
};

export const panel = {
    statusArea: {},
    addToStatusArea(role, indicator) {
        panel.statusArea[role] = indicator;
        return indicator;
    },
};

export default {layoutManager, panel};

// Stands in for the shell's Extension base class and its gettext.
//
// `gettext` is identity: the extension ships no translations yet, and a stub
// that returned anything else would make every assertion about what a user
// reads a test of this file instead of of the extension.

export class Extension {
    constructor(metadata = {}) {
        this.metadata = {uuid: 'tokengauge@arzaroth.github.io', 'version-name': '0.31.0', ...metadata};
        this.uuid = this.metadata.uuid;
        this.settings = null;
        this.preferencesOpened = 0;
    }

    getSettings() {
        return this.settings;
    }

    openPreferences() {
        this.preferencesOpened += 1;
    }
}

export function gettext(text) {
    return text;
}

export default {Extension, gettext};

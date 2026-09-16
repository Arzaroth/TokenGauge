// Stands in for the shell's Extension base class and its gettext.
//
// `gettext` is identity: the extension ships no translations yet, and a stub
// that returned anything else would make every assertion about what a user
// reads a test of this file instead of of the extension.

export class Extension {
    constructor(metadata = {}) {
        // Deliberately not a real version: nothing here compares it to the
        // binary's, and a plausible number would just be one more place a
        // release has to remember to edit.
        this.metadata = {uuid: 'tokengauge@arzaroth.github.io', 'version-name': '0.0.0-stub', ...metadata};
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

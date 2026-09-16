import {register} from 'node:module';

register('./hooks.mjs', import.meta.url);

// `global` in a GNOME Shell extension is the shell itself, not the JS global -
// the only thing read off it here is the stage, which is what a tooltip is
// clamped against so it cannot be placed off screen.
globalThis.stage = {width: 1920, height: 1080};

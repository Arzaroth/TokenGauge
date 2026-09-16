// Stands in for gi://GObject.
//
// `registerClass` is the whole of it: in GJS it installs a GType on the class
// and hands it back, and the extension calls it from a static block precisely
// so the class keeps the constructor it declares. There is no GType here to
// install, so recording that it was called is the useful part - a class that
// was never registered is one GNOME Shell would refuse to instantiate.

export const registered = [];

export function registerClass(...args) {
    const klass = args[args.length - 1];
    registered.push(klass);
    return klass;
}

export class Object {
    _init() {}
}

export default {registerClass, Object, registered};

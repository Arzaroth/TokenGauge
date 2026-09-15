import QtQuick

// The seam between the Plasma applet's halves, checked against its own source.
//
// `main.qml` is a PlasmoidItem and cannot be instantiated outside plasmashell,
// so the harness next door drives `Service.qml` and stops at that line. What
// is left unguarded is the line itself: `CompactRep` and `FullRep` reach the
// applet through `root.<name>`, resolved against main.qml's component scope,
// and QML resolves that at *use* time. A name main.qml stopped providing is
// therefore not a load error and not a lint error - it is an undefined in a
// binding, on a desktop this repository cannot run.
//
// That is the whole risk of having moved the data layer out, so it is the
// thing to hold. Nothing here instantiates anything; it reads the three files
// and compares what one side calls against what the other declares.
Item {
    Check { id: check; name: "plasma-contract" }

    // Names the representations get from PlasmoidItem rather than from
    // main.qml. Spelled out rather than inferred, so growing this list is a
    // decision someone makes on purpose.
    readonly property var fromPlasmoid: ["expanded"]

    function declaredIn(source) {
        var names = []
        // `property var x:`, `readonly property alias x:`, `property Service x:`
        var property = /(?:readonly\s+)?property\s+(?:alias\s+|[A-Za-z_][A-Za-z0-9_.<>]*\s+)([A-Za-z_][A-Za-z0-9_]*)\s*:/g
        var match
        while ((match = property.exec(source)) !== null)
            names.push(match[1])
        var fn = /\bfunction\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/g
        while ((match = fn.exec(source)) !== null)
            names.push(match[1])
        return names
    }

    function rootRefsIn(source) {
        var names = []
        var ref = /\broot\.([A-Za-z_][A-Za-z0-9_]*)/g
        var match
        while ((match = ref.exec(source)) !== null) {
            if (names.indexOf(match[1]) < 0)
                names.push(match[1])
        }
        names.sort()
        return names
    }

    Component.onCompleted: check.readAll({
        "main": "../../plasma/org.tokengauge.plasmoid/contents/ui/main.qml",
        "service": "../../plasma/org.tokengauge.plasmoid/contents/ui/Service.qml",
        "compact": "../../plasma/org.tokengauge.plasmoid/contents/ui/CompactRep.qml",
        "full": "../../plasma/org.tokengauge.plasmoid/contents/ui/FullRep.qml"
    }, function (src) {
        var declared = declaredIn(src.main).concat(fromPlasmoid)
        var used = rootRefsIn(src.compact).concat(rootRefsIn(src.full))

        check.ok("the representations do reach the applet", used.length > 0)
        for (var i = 0; i < used.length; i++) {
            check.ok("main.qml still provides root." + used[i],
                     declared.indexOf(used[i]) >= 0)
        }

        // The seam itself. Service.qml is only drivable because it imports no
        // plasmoid module - `Plasmoid` is an attached type and no QML stub can
        // provide one, so a single import here puts the data layer back out of
        // reach of every test in this directory.
        check.equal("the data layer imports no plasmoid module",
                    src.service.indexOf("org.kde.plasma.plasmoid"), -1)
        check.ok("and owns the executable engine",
                 src.service.indexOf("plasma5support") !== -1)
        check.equal("which main.qml therefore no longer needs",
                    src.main.indexOf("plasma5support"), -1)

        // The representations were not meant to change when the data moved.
        // If one of them starts reaching past `root` into the service, the
        // aliases have stopped being the seam and this check stops meaning
        // anything.
        check.equal("CompactRep goes through root, not through the service",
                    src.compact.indexOf("service."), -1)
        check.equal("FullRep goes through root, not through the service",
                    src.full.indexOf("service."), -1)
    })
}

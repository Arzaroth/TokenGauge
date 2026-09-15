import QtQuick

// A handful of assertions and an exit code, because a QML runtime has no test
// framework and `Qt.exit` is the only thing a script can read. The watchdog is
// not optional: a runtime with nothing on screen will sit there forever if the
// assertions throw before they reach the end.
Item {
    id: check
    property string name: "qml"
    property int failures: 0
    property int checks: 0

    function equal(what, actual, expected) {
        checks += 1
        if (actual === expected) return
        failures += 1
        console.warn("FAIL " + what + ": expected " + JSON.stringify(expected)
                     + ", got " + JSON.stringify(actual))
    }

    function ok(what, condition) { equal(what, !!condition, true) }

    /// Read the recorded panel and hand it over, then run `body` and leave
    /// whatever happens - an assertion that failed, or a throw that never
    /// reached one.
    function run(fixture, body) {
        var xhr = new XMLHttpRequest()
        xhr.onreadystatechange = function () {
            if (xhr.readyState !== XMLHttpRequest.DONE) return
            try {
                body(xhr.responseText)
            } catch (e) {
                failures += 1
                console.warn("THREW in " + name + ": " + e + "\n" + (e.stack || ""))
            }
            done()
        }
        xhr.open("GET", Qt.resolvedUrl(fixture))
        xhr.send()
    }

    function done() {
        console.warn(name + ": " + (checks - failures) + "/" + checks + " checks, "
                     + failures + " failed")
        Qt.exit(failures === 0 ? 0 : 1)
    }

    Timer {
        interval: 10000
        running: true
        onTriggered: {
            console.warn(check.name + ": timed out with " + check.checks + " checks run")
            Qt.exit(2)
        }
    }
}

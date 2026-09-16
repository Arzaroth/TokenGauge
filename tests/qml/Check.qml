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

    /// Read several sources and hand them over as a map keyed by name. Same
    /// contract as `run`, for the checks that read frontend source rather than
    /// driving it.
    function readAll(files, body) {
        var loaded = {}
        var remaining = 0
        for (var key in files) remaining += 1
        if (remaining === 0) { done(); return }
        var finish = function () {
            remaining -= 1
            if (remaining > 0) return
            try {
                body(loaded)
            } catch (e) {
                failures += 1
                console.warn("THREW in " + name + ": " + e + "\n" + (e.stack || ""))
            }
            done()
        }
        for (var which in files) {
            (function (key, path) {
                var xhr = new XMLHttpRequest()
                xhr.onreadystatechange = function () {
                    if (xhr.readyState !== XMLHttpRequest.DONE) return
                    // A source that reads empty is a path that has gone stale,
                    // and every check over it would pass by finding nothing.
                    // `run` cannot have this hole - an empty fixture throws in
                    // JSON.parse - but a source read is just a string.
                    if (!xhr.responseText) {
                        failures += 1
                        console.warn("FAIL " + name + ": read nothing from " + path)
                    }
                    loaded[key] = xhr.responseText
                    finish()
                }
                xhr.open("GET", Qt.resolvedUrl(path))
                xhr.send()
            })(which, files[which])
        }
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

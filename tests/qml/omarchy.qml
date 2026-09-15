import QtQuick
import Harness
import "../../omarchy/arzaroth.tokengauge" as Widget

// The Omarchy widget's data layer, loaded in a plain QML runtime against a
// recorded panel. Nothing is spawned and nothing is drawn: what this covers is
// the half a syntax check cannot reach - the bindings, what the widget reads
// back out of the snapshot, and what it hands the binary on the next call.
Item {
    Check { id: check; name: "omarchy" }
    Widget.Usage { id: usage }

    Component.onCompleted: check.run("fixtures/panel.json", function (panelJson) {
        // Before any answer, no snapshot and no rows - the widget draws its
        // empty state rather than indexing into nothing.
        check.equal("no snapshot yet", usage.snapshot, null)
        check.equal("no rows yet", usage.rows.length, 0)
        check.equal("nothing has failed yet", usage.lastError, "")

        var asked = Registry.find("--json")
        check.ok("the widget asked for a snapshot on its own", asked !== null)
        if (!asked) return

        // What crossed the process boundary: one `sh -c` that puts the user
        // bin dirs on PATH first, because uwsm hands the shell a PATH that
        // often lacks the directory the installer wrote the binary into.
        check.equal("it goes through a shell", asked.command[0], "sh")
        check.equal("as one command", asked.command[1], "-c")
        var line = asked.command[2]
        check.ok("PATH is repaired first", line.indexOf('export PATH="$HOME/.local/bin') === 0)
        check.ok("the binary is named", line.indexOf("tokengauge-waybar") !== -1)
        check.ok("and asked for JSON", line.indexOf("--json") !== -1)
        check.ok("no provider endpoint is named here", line.indexOf("http") === -1)

        asked.answer(panelJson, "", 0)

        // The answer arrived and every derived property followed it.
        check.equal("the snapshot landed", usage.rows.length, 2)
        check.equal("the first row is the pinned one", usage.primary, "claude")
        check.equal("the provider list is the full one", usage.allProviders.length, 5)
        check.equal("enabled is the subset", usage.enabledProviders.length, 2)
        check.equal("no errors", usage.errors.length, 0)
        check.equal("loading cleared", usage.loading, false)
        check.equal("the revision counter moved", usage.revision, 1)
        check.equal("the version came off the snapshot", usage.version, "0.31.0")
        check.ok("the update banner has something to draw", usage.updateStatus.available)

        // The panel is the core's, walked rather than rebuilt. A widget that
        // decided its own section order would pass a syntax check and draw the
        // wrong thing.
        var row = usage.rows[0]
        check.equal("the row carries a resolved panel", row.panel.length, 2)
        check.equal("limits lead", row.panel[0].id, "limits")
        check.equal("and are meters", row.panel[0].kind, "meters")
        check.equal("cost follows", row.panel[1].id, "cost")
        check.equal("as rows", row.panel[1].kind, "rows")
        check.equal("the bar is resolved too", row.bar.percent, 68)
        check.equal("with its tier", row.bar.tone, "warn")
        check.ok("and the hover summary", row.bar_tooltip.lines.length === 3)

        // The second screen. A machine with no store yet says so rather than
        // drawing an empty chart, which is the state every user starts in.
        check.equal("three ranges", row.history.series.length, 3)
        check.equal("the widest is a year", row.history.series[2].id, "12m")
        check.equal("and it knows it is empty", row.history.series[2].empty, true)

        // The widget's own version, read from the manifest beside it. The
        // plugin and the binary install separately, so reporting only one of
        // them is what makes a skew invisible.
        check.ok("the widget read its own version", usage.widgetVersion.length > 0)

        // An action runs the flag and the read in one subprocess, so the panel
        // never renders against pre-action state.
        Registry.clear()
        usage.setPrimary("codex")
        var pinned = Registry.find("--set-primary")
        check.ok("repinning ran a command", pinned !== null)
        if (pinned) {
            var pin = pinned.command[2]
            check.ok("the name is quoted", pin.indexOf("'codex'") !== -1)
            check.ok("and the read is chained behind it", pin.indexOf("&& ") !== -1)
            check.ok("in the same subprocess", pin.indexOf("--json") > pin.indexOf("--set-primary"))
            pinned.answer(panelJson, "", 0)
        }

        // A run already in flight is refused rather than queued, so a button
        // that arms a spinner is told the request never started.
        Registry.clear()
        check.equal("the first run is accepted", usage.run("true"), true)
        check.equal("the second is refused", usage.run("true"), false)
        var inflight = Registry.find("true")
        if (inflight) inflight.answer(panelJson, "", 0)

        // Output that is not a snapshot is reported, not drawn.
        Registry.clear()
        usage.reload()
        var broken = Registry.find("--json")
        check.ok("the reload ran", broken !== null)
        if (broken) {
            broken.answer("not json at all", "", 0)
            check.equal("unreadable output is reported", usage.lastError, "Unreadable snapshot")
            check.equal("and the last good snapshot stays", usage.rows.length, 2)
        }
    })
}

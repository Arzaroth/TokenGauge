import QtQuick
import Harness
import "../../plasma/org.tokengauge.plasmoid/contents/ui" as Widget

// The Plasma applet's data layer, loaded in a plain QML runtime against a
// recorded panel. Plasma is the one that goes out through a data engine keyed
// by the whole command line, so what this covers on top of the Omarchy harness
// is the quoting - a shell command interpolated into a source name - and the
// long poll, which is the applet's answer to a toolkit with no file watcher.
Item {
    Check { id: check; name: "plasma" }
    Widget.Service { id: service; binary: "tokengauge-waybar"; refreshSecs: 600 }

    Component.onCompleted: check.run("fixtures/panel.json", function (panelJson) {
        // Before any answer, an empty snapshot rather than a missing one, so
        // every binding under it resolves instead of throwing.
        check.equal("an empty snapshot, not a null one", service.snapshot.rows.length, 0)
        check.equal("nothing has failed yet", service.lastError, "")
        check.equal("not updating", service.updating, false)

        // The applet parks on the change instead of only polling: QML in a
        // plasmoid has no file watcher, so the wait happens in the binary.
        var watch = Registry.find("--wait-change")
        check.ok("it started the long poll on its own", watch !== null)
        if (!watch) return
        check.ok("the wait is bounded", watch.commandLine.indexOf("--wait-timeout 300") !== -1)
        check.ok("and brings back the new state", watch.commandLine.indexOf("--json") !== -1)
        check.ok("the source is the one it recorded", watch.commandLine === service.watchSource)

        // What crossed the shell: one `sh -c` with everything quoted inside it.
        var line = watch.commandLine
        check.ok("it goes through a shell", line.indexOf("sh -c ") === 0)
        check.ok("with PATH repaired first", line.indexOf('export PATH=') !== -1)
        check.ok("the binary is named", line.indexOf("tokengauge-waybar") !== -1)
        check.ok("nothing unquoted broke out", line.indexOf("; rm ") === -1)

        watch.answer(panelJson, "", 0)

        // The answer landed and the source was released, so the next arming is
        // not refused by the guard that stops two waits overlapping.
        check.equal("the snapshot landed", service.snapshot.rows.length, 2)
        check.equal("no error", service.lastError, "")
        check.equal("the watch slot is free again", service.watchSource, "")
        check.equal("a success does not back off", service.watchFailures, 0)

        // The panel is the core's, walked rather than rebuilt.
        var row = service.snapshot.rows[0]
        check.equal("limits lead", row.panel[0].id, "limits")
        check.equal("and are meters", row.panel[0].kind, "meters")
        check.equal("cost follows", row.panel[1].id, "cost")
        check.equal("the bar is resolved", row.bar.percent, 68)
        check.equal("with its tier", row.bar.tone, "warn")
        check.equal("the hover summary is the core's", row.bar_tooltip.title, "Claude")
        check.equal("the theme came along", service.snapshot.theme.red, "#f38ba8")

        // An action runs the flag and the read in one source, so the applet
        // never renders against pre-action state.
        Registry.clear()
        service.action("--set-primary 'codex'")
        var pinned = Registry.find("--set-primary")
        check.ok("repinning ran a command", pinned !== null)
        if (pinned) {
            check.ok("the read is chained behind it",
                     pinned.commandLine.indexOf("--json") > pinned.commandLine.indexOf("--set-primary"))
            pinned.answer(panelJson, "", 0)
        }

        // Only the update's own completion clears the flag. A refresh landing
        // mid-update used to put the button back to "Update" while the download
        // was still running.
        Registry.clear()
        service.applyUpdate()
        check.equal("the button is armed", service.updating, true)
        var refresh = Registry.find("--json")
        check.ok("the update command was run", refresh !== null)
        service.reload()
        var plain = Registry.find("--json")
        if (plain && plain.commandLine !== service.updateSource) {
            plain.answer(panelJson, "", 0)
            check.equal("an unrelated refresh does not disarm it", service.updating, true)
        }
        var mine = Registry.find("--update")
        if (mine) {
            mine.answer(panelJson, "", 0)
            check.equal("its own completion does", service.updating, false)
        }

        // A wait that fails instead of waiting - no binary on PATH, say - backs
        // off rather than respawning every 200ms forever.
        Registry.clear()
        service.watch()
        var failing = Registry.find("--wait-change")
        check.ok("the wait re-armed", failing !== null)
        if (failing) {
            failing.answer("", "no such file", 127)
            check.equal("a failed wait is counted", service.watchFailures, 1)
            check.equal("and reported", service.lastError, "no such file")
        }

        // Output that is not a snapshot is reported, not drawn.
        Registry.clear()
        service.reload()
        var broken = Registry.find("--json")
        check.ok("the reload ran", broken !== null)
        if (broken) {
            broken.answer("not json at all", "", 0)
            check.ok("unreadable output is reported", service.lastError.indexOf("parse error") === 0)
            check.equal("and the last good snapshot stays", service.snapshot.rows.length, 2)
        }
    })
}

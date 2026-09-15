import QtQuick
import org.kde.plasma.plasma5support as Plasma5Support

// The data side of the applet. Everything the plasmoid draws comes from one
// `tokengauge-waybar --json` snapshot; the QML never reads a credential, a
// cache file, or a provider endpoint itself.
//
// Split out from main.qml so it can be loaded without a plasmoid around it:
// `Plasmoid` is an attached type, which no QML stub can provide, and a data
// layer that cannot be instantiated outside plasmashell cannot be driven by
// tests/qml. Nothing here imports the plasmoid module - the configuration
// arrives as `binary` and `refreshSecs`.
//
// Invisible rather than a QtObject, so the engine and the timers are ordinary
// children - the same shape the Omarchy widget's Usage.qml has.
Item {
    id: service
    visible: false

    property string binary: "tokengauge-waybar"
    property int refreshSecs: 600
    /// The panel is on screen, so the snapshot is re-read on a much shorter
    /// cycle. Set by main.qml from the plasmoid's own expanded state.
    property bool live: false

    // Full snapshot emitted by `tokengauge-waybar --json`.
    property var snapshot: ({ rows: [], errors: [], enabled: [], primary: null, window: "daily", theme: {} })
    property string lastError: ""

    // True while an --update command is in flight; reset when exec completes.
    property bool updating: false
    // Exact exec source of the in-flight update command, so only its own
    // completion clears `updating` (the binary is user-configurable, so a
    // substring match on "--update" isn't reliable).
    property string updateSource: ""

    property string watchSource: ""
    property int watchFailures: 0

    Plasma5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            exec.disconnectSource(source)
            // Clear the in-flight update state only when the update command
            // itself completes, so a periodic refresh finishing mid-update
            // doesn't re-enable the button while --update is still running.
            if (source === service.updateSource) {
                service.updating = false
                service.updateSource = ""
            }
            // Re-arm the long poll from a timer rather than from inside its own
            // newData handler, which is still mid-disconnect. A wait that fails
            // instead of waiting - no binary on PATH, say - would respawn every
            // 200ms forever, so failures back off.
            if (source === service.watchSource) {
                service.watchSource = ""
                if (data["exit code"] === 0) {
                    service.watchFailures = 0
                    rearmWatch.interval = 200
                } else {
                    service.watchFailures = Math.min(service.watchFailures + 1, 6)
                    rearmWatch.interval = 1000 * Math.pow(2, service.watchFailures - 1)
                }
                rearmWatch.restart()
            }
            if (data["exit code"] === 0) {
                try {
                    var parsed = JSON.parse(data.stdout)
                    service.snapshot = parsed
                    service.lastError = ""
                } catch (e) {
                    service.lastError = "parse error: " + e
                }
            } else {
                service.lastError = ((data.stderr || "") + "").trim() || ("exit " + data["exit code"])
            }
        }
    }

    function shellQuote(s) {
        return "'" + String(s).replace(/'/g, "'\\''") + "'"
    }

    // Wrap a command so it runs through a shell with the usual user bin dirs on
    // PATH - plasmashell's session PATH often lacks ~/.local/bin, which is where
    // the installer drops the binary.
    function cmd(c) {
        return "sh -c " + shellQuote('export PATH="$HOME/.local/bin:$HOME/bin:/usr/local/bin:$PATH"; ' + c)
    }

    // Refresh the snapshot.
    function reload() {
        exec.connectSource(cmd(service.binary + " --json"))
    }

    // Run an action flag, then refresh the snapshot.
    function action(flag) {
        exec.connectSource(cmd(service.binary + " " + flag + " && " + service.binary + " --json"))
    }

    // Long-poll for the next change instead of only re-reading on a timer, so a
    // fetch by the daemon or another frontend shows up here at once. QML in a
    // plasmoid has no file watcher, so the wait happens in the binary: it parks
    // on the revision file and exits when the snapshot is rewritten, or after
    // the timeout, and the chained --json brings back the new state either way.
    function watch() {
        if (service.watchSource !== "")
            return
        service.watchSource = cmd(service.binary + " --wait-change --wait-timeout 300 && "
                                  + service.binary + " --json")
        exec.connectSource(service.watchSource)
    }

    Timer {
        id: rearmWatch
        interval: 200
        repeat: false
        onTriggered: service.watch()
    }

    // Download + install the latest release, then refresh so the banner clears.
    // --update's human-readable stdout is discarded so only the --json payload
    // reaches onNewData's JSON.parse.
    function applyUpdate() {
        service.updating = true
        // Discard --update's stdout (keeps the JSON refresh parseable) but keep
        // stderr so a failed update surfaces its error via lastError.
        var updateSource = cmd(service.binary + " --update >/dev/null && " + service.binary + " --json")
        service.updateSource = updateSource
        exec.connectSource(updateSource)
    }

    // Opens the TUI's sync screen in a terminal. `--sync-setup` returns as soon
    // as it has spawned one, so the `--json` chained behind it is not waiting on
    // the user. `&&` and a kept stderr, matching applyUpdate: with `;` the
    // compound command exits 0 whatever setup did, so "no terminal found" would
    // never reach lastError.
    function openSyncSetup() {
        exec.connectSource(cmd(service.binary + " --sync-setup >/dev/null && "
                               + service.binary + " --json"))
    }

    function shutdown() {
        if (service.watchSource !== "")
            exec.disconnectSource(service.watchSource)
    }

    // Fallback beside the long poll: nothing writes the snapshot unless someone
    // asks for it, so with no daemon running this timer is what ages the cache
    // out and triggers the next fetch.
    Timer {
        interval: service.refreshSecs * 1000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: service.reload()
    }

    // While the panel is open, on a much shorter cycle. A reset time is counted
    // against the clock at render time, so the countdown only moves when the
    // snapshot is rendered again - a panel left open otherwise keeps the
    // countdown it opened with. `--json` serves the snapshot it already has and
    // refetches only once that snapshot has aged past `refresh_secs`, so this
    // costs a subprocess, not a provider call.
    Timer {
        interval: 30000
        running: service.live
        repeat: true
        triggeredOnStart: true
        onTriggered: service.reload()
    }

    Component.onCompleted: service.watch()
}

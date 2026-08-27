import QtQuick
import org.kde.plasma.plasmoid
import org.kde.plasma.plasma5support as Plasma5Support

PlasmoidItem {
    id: root

    // Full snapshot emitted by `tokengauge-waybar --json`.
    property var snapshot: ({ rows: [], errors: [], enabled: [], primary: null, window: "daily", theme: {} })
    property var rows: snapshot.rows || []
    property string lastError: ""

    // The selection follows the provider id, not the slot it sits in: a row
    // that appears or drops out on a refresh would otherwise slide a different
    // provider's numbers under whatever the user was reading. Empty means the
    // user has not picked one, so the pin still leads.
    property string selectedProviderId: ""

    readonly property int selectedIndex: {
        for (var i = 0; i < rows.length; i++)
            if (String(rows[i].provider) === selectedProviderId)
                return i
        // Nothing chosen, or the chosen provider has gone: follow the pin. The
        // compact view reports its percentage, and opening the panel on a
        // different provider reads as a bug.
        var pinned = String(snapshot.primary || "").toLowerCase()
        if (pinned !== "")
            for (var j = 0; j < rows.length; j++)
                if (String(rows[j].provider).toLowerCase() === pinned)
                    return j
        return 0
    }

    // Step the selection by one row, wrapping. Used by the compact view's wheel.
    function stepSelection(delta) {
        var n = rows.length
        if (n === 0) return
        var next = ((selectedIndex + delta) % n + n) % n
        root.selectedProviderId = String(rows[next].provider)
    }

    readonly property string waybarBin: Plasmoid.configuration.waybarBinary || "tokengauge-waybar"
    readonly property int refreshSecs: Math.max(15, Plasmoid.configuration.refreshInterval)

    // Cached GitHub release check written by the daemon; see UpdateStatus.
    readonly property var updateInfo: snapshot.update || null
    readonly property bool updateAvailable: !!(updateInfo && updateInfo.available)
    // True while an --update command is in flight; reset when exec completes.
    property bool updating: false
    // Exact exec source of the in-flight update command, so only its own
    // completion clears `updating` (waybarBin is user-configurable, so a
    // substring match on "--update" isn't reliable).
    property string updateSource: ""

    // Row shown in the panel / hovered.
    readonly property var selRow: rows.length > 0 ? rows[selectedIndex] : null

    Plasmoid.icon: "utilities-system-monitor"
    toolTipMainText: selRow ? (selRow.label || selRow.provider) : "TokenGauge"
    toolTipTextFormat: Text.RichText
    toolTipSubText: tooltipSub(selRow)

    // The hover summary: every limit the panel draws, with its tier colour, and
    // today's spend. Read off the same core section list the applet renders, so
    // the two can never disagree about which windows exist.
    function tooltipSub(r) {
        if (!r)
            return lastError !== "" ? lastError : i18n("No provider data yet.")
        var sections = Array.isArray(r.panel) ? r.panel : []
        var lines = []
        for (var i = 0; i < sections.length; i++) {
            if (sections[i].id !== "limits") continue
            var rows = sections[i].rows
            for (var j = 0; j < rows.length; j++)
                lines.push(root.escapeHtml(rows[j].label) + ":&nbsp;<font color=\""
                           + root.toneColor(rows[j].tone) + "\"><b>"
                           + root.escapeHtml(rows[j].value) + "</b></font>")
        }
        // Today's spend comes off the same section list, not off raw
        // today_usd: the local formatter this replaces disagreed with core's
        // money() above a hundred dollars ($312.21 against $312).
        for (var k = 0; k < sections.length; k++) {
            if (sections[k].id !== "cost" || sections[k].rows.length === 0) continue
            var today = sections[k].rows[0]
            lines.push(root.escapeHtml(today.label) + ":&nbsp;<b>"
                       + root.escapeHtml(today.value) + "</b>")
        }
        return lines.join("<br>")
    }

    // ---- data ----------------------------------------------------------------
    Plasma5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            exec.disconnectSource(source)
            // Clear the in-flight update state only when the update command
            // itself completes, so a periodic refresh finishing mid-update
            // doesn't re-enable the button while --update is still running.
            if (source === root.updateSource) {
                root.updating = false
                root.updateSource = ""
            }
            // Re-arm the long poll from a timer rather than from inside its own
            // newData handler, which is still mid-disconnect. A wait that fails
            // instead of waiting - no binary on PATH, say - would respawn every
            // 200ms forever, so failures back off.
            if (source === root.watchSource) {
                root.watchSource = ""
                if (data["exit code"] === 0) {
                    root.watchFailures = 0
                    rearmWatch.interval = 200
                } else {
                    root.watchFailures = Math.min(root.watchFailures + 1, 6)
                    rearmWatch.interval = 1000 * Math.pow(2, root.watchFailures - 1)
                }
                rearmWatch.restart()
            }
            if (data["exit code"] === 0) {
                try {
                    var parsed = JSON.parse(data.stdout)
                    root.snapshot = parsed
                    root.lastError = ""
                } catch (e) {
                    root.lastError = "parse error: " + e
                }
            } else {
                root.lastError = ((data.stderr || "") + "").trim() || ("exit " + data["exit code"])
            }
        }
    }

    // Wrap a command so it runs through a shell with the usual user bin dirs on
    // PATH - plasmashell's session PATH often lacks ~/.local/bin, which is where
    // the installer drops tokengauge-waybar.
    function cmd(c) {
        return "sh -c " + shellQuote('export PATH="$HOME/.local/bin:$HOME/bin:/usr/local/bin:$PATH"; ' + c)
    }

    // Refresh the snapshot.
    function reload() {
        exec.connectSource(cmd(root.waybarBin + " --json"))
    }

    // Run a tokengauge-waybar action flag, then refresh the snapshot.
    function action(flag) {
        exec.connectSource(cmd(root.waybarBin + " " + flag + " && " + root.waybarBin + " --json"))
    }

    // Long-poll for the next change instead of only re-reading on a timer, so a
    // fetch by the daemon or another frontend shows up here at once. QML in a
    // plasmoid has no file watcher, so the wait happens in the binary: it parks
    // on the revision file and exits when the snapshot is rewritten, or after
    // the timeout, and the chained --json brings back the new state either way.
    property string watchSource: ""
    property int watchFailures: 0

    function watch() {
        if (root.watchSource !== "")
            return
        root.watchSource = cmd(root.waybarBin + " --wait-change --wait-timeout 300 && "
                               + root.waybarBin + " --json")
        exec.connectSource(root.watchSource)
    }

    Timer {
        id: rearmWatch
        interval: 200
        repeat: false
        onTriggered: root.watch()
    }

    // Download + install the latest release, then refresh so the banner clears.
    // --update's human-readable stdout is discarded so only the --json payload
    // reaches onNewData's JSON.parse.
    function applyUpdate() {
        root.updating = true
        // Discard --update's stdout (keeps the JSON refresh parseable) but keep
        // stderr so a failed update surfaces its error via root.lastError.
        var updateSource = cmd(root.waybarBin + " --update >/dev/null && " + root.waybarBin + " --json")
        root.updateSource = updateSource
        exec.connectSource(updateSource)
    }

    // Opens the TUI's sync screen in a terminal. `--sync-setup` returns as soon
    // as it has spawned one, so the `--json` chained behind it is not waiting on
    // the user. `&&` and a kept stderr, matching applyUpdate: with `;` the
    // compound command exits 0 whatever setup did, so "no terminal found" would
    // never reach root.lastError.
    function openSyncSetup() {
        exec.connectSource(cmd(root.waybarBin + " --sync-setup >/dev/null && "
                               + root.waybarBin + " --json"))
    }

    function shellQuote(s) {
        return "'" + String(s).replace(/'/g, "'\\''") + "'"
    }

    // Fallback beside the long poll: nothing writes the snapshot unless someone
    // asks for it, so with no daemon running this timer is what ages the cache
    // out and triggers the next fetch.
    Timer {
        interval: root.refreshSecs * 1000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: root.reload()
    }

    // While the panel is open, on a much shorter cycle. A reset time is counted
    // against the clock at render time, so the countdown only moves when the
    // snapshot is rendered again - a panel left open otherwise keeps the
    // countdown it opened with. `--json` serves the snapshot it already has and
    // refetches only once that snapshot has aged past `refresh_secs`, so this
    // costs a subprocess, not a provider call.
    Timer {
        interval: 30000
        running: root.expanded
        repeat: true
        triggeredOnStart: true
        onTriggered: root.reload()
    }

    Component.onCompleted: watch()

    Component.onDestruction: {
        if (root.watchSource !== "")
            exec.disconnectSource(root.watchSource)
    }

    // ---- helpers -------------------------------------------------------------
    // Tier colour for a usage percent, mirroring core color_for_percent.
    // The tooltip is Text.RichText and the labels come from a provider API
    // response, so `&`, `<` and `>` in a window title would corrupt the markup -
    // and Qt's rich text subset accepts tags such as <img src=...>. The waybar
    // surface runs the same data through pango_escape.
    function escapeHtml(value) {
        return String(value === null || value === undefined ? "" : value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;")
    }

    // A tone name from the core, mapped onto the snapshot theme.
    function toneColor(tone) {
        var t = root.snapshot.theme || {}
        switch (String(tone)) {
            case "good": return t.green || "#a6e3a1"
            case "warn": return t.yellow || "#f9e2af"
            case "critical": return t.red || "#f38ba8"
            default: return t.dim || "#6c7086"
        }
    }

    // The headline number and its tier, resolved by the core under the
    // configured window. This used to pick the window here and carry its own
    // copy of the 50/80 boundaries to tint it with.
    function bar(row) {
        return (row && row.bar) ? row.bar : { percent: null, tone: "dim" }
    }

    compactRepresentation: CompactRep {}
    fullRepresentation: FullRep {}
}

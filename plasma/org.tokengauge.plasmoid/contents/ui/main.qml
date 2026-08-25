import QtQuick
import org.kde.plasma.plasmoid
import org.kde.plasma.plasma5support as Plasma5Support

PlasmoidItem {
    id: root

    // Full snapshot emitted by `tokengauge-waybar --json`.
    property var snapshot: ({ rows: [], errors: [], enabled: [], primary: null, window: "daily", theme: {} })
    property var rows: snapshot.rows || []
    property string lastError: ""
    property int selectedIndex: 0
    // Once the user picks a tab / scrolls, stop snapping the selection back to
    // the pinned provider on refresh.
    property bool userSelected: false

    // Row index of the pinned primary provider, or 0 (highest / first).
    function primaryIndex(snap) {
        var rows = snap.rows || []
        if (snap.primary) {
            for (var i = 0; i < rows.length; i++)
                if ((rows[i].provider || "").toLowerCase() === snap.primary)
                    return i
        }
        return 0
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
    readonly property var selRow: rows.length > 0
        ? rows[Math.min(selectedIndex, rows.length - 1)]
        : null

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
        if (r.cost)
            lines.push(i18n("Today") + ":&nbsp;<b>" + root.fmtUsd(r.cost.today_usd) + "</b>")
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
                    var n = (parsed.rows || []).length
                    if (!root.userSelected)
                        root.selectedIndex = root.primaryIndex(parsed)
                    else if (root.selectedIndex >= n)
                        root.selectedIndex = 0
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
    // the user; `;` rather than `&&` so the panel still refreshes if no terminal
    // was found. Only stdout is discarded, so "no terminal found" still reaches
    // root.lastError - the same reason applyUpdate keeps stderr.
    function openSyncSetup() {
        exec.connectSource(cmd(root.waybarBin + " --sync-setup >/dev/null; "
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

    // A tone name from the core, mapped onto the snapshot theme. The compact
    // representation still resolves a bare percentage, so both live here.
    function toneColor(tone) {
        var t = root.snapshot.theme || {}
        switch (String(tone)) {
            case "good": return t.green || "#a6e3a1"
            case "warn": return t.yellow || "#f9e2af"
            case "critical": return t.red || "#f38ba8"
            default: return t.dim || "#6c7086"
        }
    }

    function tierColor(pct) {
        var t = root.snapshot.theme || {}
        if (pct === null || pct === undefined)
            return t.dim || "#6c7086"
        if (pct >= 80)
            return t.red || "#f38ba8"
        if (pct >= 50)
            return t.yellow || "#f9e2af"
        return t.green || "#a6e3a1"
    }

    function windowPercent(row) {
        if (!row) return null
        return root.snapshot.window === "weekly" ? row.weekly_used : row.session_used
    }

    function fmtUsd(v) {
        if (v === null || v === undefined) return "—"
        return "$" + Number(v).toFixed(2)
    }

    compactRepresentation: CompactRep {}
    fullRepresentation: FullRep {}
}

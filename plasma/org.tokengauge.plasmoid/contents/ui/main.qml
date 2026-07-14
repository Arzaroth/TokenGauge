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

    Plasmoid.icon: "utilities-system-monitor"
    toolTipMainText: "TokenGauge"
    toolTipSubText: lastError !== "" ? lastError : (rows.length + " provider(s)")

    // ---- data ----------------------------------------------------------------
    Plasma5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            exec.disconnectSource(source)
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
        exec.connectSource(cmd(root.waybarBin + " " + flag + "; " + root.waybarBin + " --json"))
    }

    function shellQuote(s) {
        return "'" + String(s).replace(/'/g, "'\\''") + "'"
    }

    Timer {
        interval: root.refreshSecs * 1000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: root.reload()
    }

    // ---- helpers -------------------------------------------------------------
    // Tier colour for a usage percent, mirroring core color_for_percent.
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

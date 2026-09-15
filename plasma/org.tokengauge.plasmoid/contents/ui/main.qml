import QtQuick
import org.kde.plasma.plasmoid

PlasmoidItem {
    id: root

    // The data side, which owns the subprocess and the snapshot it brings
    // back. It lives in its own file so it can be instantiated without a
    // plasmoid around it - `Plasmoid` is an attached type and no stub can
    // provide one, which is what kept tests/qml from ever driving this.
    property Service service: Service {
        binary: root.waybarBin
        refreshSecs: root.refreshSecs
        live: root.expanded
    }

    // Full snapshot emitted by `tokengauge-waybar --json`.
    readonly property alias snapshot: root.service.snapshot
    property var rows: snapshot.rows || []
    readonly property alias lastError: root.service.lastError

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
    // True while an --update command is in flight.
    readonly property alias updating: root.service.updating

    // Row shown in the panel / hovered.
    readonly property var selRow: rows.length > 0 ? rows[selectedIndex] : null

    Plasmoid.icon: "utilities-system-monitor"
    toolTipMainText: selRow ? ((selRow.bar_tooltip && selRow.bar_tooltip.title)
                              || selRow.label || selRow.provider) : "TokenGauge"
    toolTipTextFormat: Text.RichText
    toolTipSubText: tooltipSub(selRow)

    // The hover summary, resolved by the core: every limit with its tier
    // colour, then today's spend. `bar_tooltip` is what the tray, the GNOME
    // extension and the Quickshell widget hover with too - this used to pick
    // the lines apart from `panel` here, which is how the other three each
    // ended up saying something different or nothing at all.
    function tooltipSub(r) {
        if (!r)
            return lastError !== "" ? lastError : i18n("No provider data yet.")
        var tip = r.bar_tooltip || {}
        var lines = []
        var rows = Array.isArray(tip.lines) ? tip.lines : []
        for (var i = 0; i < rows.length; i++) {
            // `normal` carries no tier - a spend figure has no threshold to
            // tint against - and wrapping it would read it as dim.
            var value = root.escapeHtml(rows[i].value)
            var body = String(rows[i].tone) === "normal"
                ? "<b>" + value + "</b>"
                : "<font color=\"" + root.toneColor(rows[i].tone) + "\"><b>" + value + "</b></font>"
            lines.push(root.escapeHtml(rows[i].label) + ":&nbsp;" + body)
        }
        return lines.join("<br>")
    }

    // ---- data ----------------------------------------------------------------
    // All of it is the service's; these are the names FullRep and CompactRep
    // already call.
    function reload() { root.service.reload() }
    function action(flag) { root.service.action(flag) }
    function applyUpdate() { root.service.applyUpdate() }
    function openSyncSetup() { root.service.openSyncSetup() }

    Component.onDestruction: root.service.shutdown()

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

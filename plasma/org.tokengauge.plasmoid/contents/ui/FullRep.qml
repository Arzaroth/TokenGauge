import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.plasma.plasmoid
import org.kde.plasma.components as PlasmaComponents
import org.kde.plasma.extras as PlasmaExtras
import org.kde.kirigami as Kirigami

Item {
    id: full

    Layout.minimumWidth: Kirigami.Units.gridUnit * 22
    Layout.minimumHeight: Kirigami.Units.gridUnit * 22
    Layout.preferredWidth: Kirigami.Units.gridUnit * 24
    Layout.preferredHeight: Kirigami.Units.gridUnit * 32

    property bool settingsOpen: false
    property bool historyOpen: false
    // Which range the history pane is showing, as an index into its series.
    // Every range is already on the row, so cycling one is a rebind rather
    // than another `--json`.
    property int historyRange: 0

    // Settings and history are both second screens over the panel, and exactly
    // one of the three is up at a time. A year of bars does not belong above
    // the limit gauges, which is why history is a screen rather than a section.
    readonly property bool showPanel: !full.settingsOpen && !full.historyOpen

    function openScreen(which) {
        full.settingsOpen = which === "settings"
        full.historyOpen = which === "history"
    }

    readonly property var row: root.rows.length > 0 ? root.rows[root.selectedIndex] : null
    readonly property var history: full.row && full.row.history ? full.row.history : null
    readonly property var historySeries: full.history && Array.isArray(full.history.series)
                                         && full.history.series.length > 0
        ? full.history.series[Math.min(full.historyRange, full.history.series.length - 1)]
        : null

    // `toneColor` maps `normal` onto the dim text colour, which is right for a
    // badge and wrong for a chart fill: it would draw every complete step in
    // the same grey as the note under it. A frontend maps a tone onto its own
    // palette, and for a filled bar this one's `normal` is the accent.
    function chartColor(point) {
        return String(point.tone) === "normal"
            ? Kirigami.Theme.highlightColor
            : root.toneColor(point.tone)
    }
    // The core's own list, never a guess: a hardcoded fallback here silently
    // hid every provider added since it was written.
    readonly property var oauthProviders: root.snapshot.providers || []

    // ---- reusable pieces -----------------------------------------------------

    function joinValue(row) {
        var suffix = String(row.suffix || "")
        return suffix === "" ? String(row.value || "") : row.value + "  ·  " + suffix
    }

    // The spec pads a tooltip's sub-tables - a day by model, a day by device -
    // with spaces, so they only line up in a monospace face, and the styled
    // tooltip wraps at 14 gridUnits, which a padded line already brushes
    // against. The attached tooltip carries neither font nor width, hence an
    // instance of our own.
    component MonoToolTip: PlasmaComponents.ToolTip {
        id: tip
        readonly property real maxTextWidth: Kirigami.Units.gridUnit * 30
        font.family: "monospace"
        contentItem: Item {
            implicitWidth: Math.min(tip.maxTextWidth, tipLabel.contentWidth)
            implicitHeight: tipLabel.implicitHeight
            PlasmaComponents.Label {
                id: tipLabel
                text: tip.text
                font: tip.font
                wrapMode: Text.Wrap
                // Device and model names come from a provider API, and rich
                // text would collapse the padding this whole component exists
                // to keep.
                textFormat: Text.PlainText
                Kirigami.Theme.colorSet: Kirigami.Theme.Tooltip
                Kirigami.Theme.inherit: false
                // Only a transport error is long enough to reach this; the
                // tables are far short of it and stay on one line each.
                onLineLaidOut: (line) => {
                    if (line.implicitWidth > tip.maxTextWidth)
                        line.width = tip.maxTextWidth
                }
            }
        }
    }

    // Label and value on one line, a full-width bar under it, then the reset
    // note and the pace badge. The limit gauges.
    component Meter: ColumnLayout {
        required property var modelData
        spacing: 2
        Layout.fillWidth: true

        RowLayout {
            Layout.fillWidth: true
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: modelData.label
                Layout.fillWidth: true
                elide: Text.ElideRight
            }
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: modelData.value
                color: root.toneColor(modelData.tone)
                font.bold: true
            }
        }
        Rectangle {
            Layout.fillWidth: true
            height: Kirigami.Units.gridUnit * 0.5
            radius: height / 2
            color: Kirigami.Theme.backgroundColor
            border.width: 1
            border.color: Kirigami.Theme.disabledTextColor
            Rectangle {
                height: parent.height
                radius: parent.radius
                visible: modelData.fraction !== null && modelData.fraction !== undefined
                width: parent.width * Math.max(0, Math.min(1, Number(modelData.fraction) || 0))
                color: root.toneColor(modelData.tone)
            }
        }
        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing
            visible: String(modelData.footnote || "") !== "" || String(modelData.badge || "") !== ""
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: modelData.footnote
                visible: text !== ""
                opacity: 0.7
                font: Kirigami.Theme.smallFont
            }
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: modelData.badge
                visible: text !== ""
                color: root.toneColor(modelData.badge_tone)
                font: Kirigami.Theme.smallFont
            }
            Item { Layout.fillWidth: true }
        }
    }

    // One line per row with the share bar filling the row behind the text, so a
    // seven-day list and a model breakdown both stay on one screen.
    component BarRow: Item {
        required property var modelData
        Layout.fillWidth: true
        implicitHeight: barLabel.implicitHeight + Kirigami.Units.smallSpacing * 2

        Rectangle {
            anchors.fill: parent
            radius: 3
            color: Kirigami.Theme.alternateBackgroundColor
        }
        Rectangle {
            anchors.left: parent.left
            anchors.top: parent.top
            anchors.bottom: parent.bottom
            width: parent.width * Math.max(0, Math.min(1, Number(modelData.fraction) || 0))
            radius: 3
            opacity: 0.35
            color: Kirigami.Theme.highlightColor
        }
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: Kirigami.Units.smallSpacing
            anchors.rightMargin: Kirigami.Units.smallSpacing
            PlasmaComponents.Label {
                id: barLabel
                textFormat: Text.PlainText
                text: modelData.label
                font.bold: modelData.emphasized === true
                elide: Text.ElideRight
                Layout.fillWidth: true
            }
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: full.joinValue(modelData)
                font.family: "monospace"
                font.bold: modelData.emphasized === true
            }
        }
        MonoToolTip {
            text: modelData.tooltip || ""
            visible: text !== "" && barHover.hovered
            delay: 300
        }
        HoverHandler { id: barHover }
    }

    // Label and value on one line; a badge and a suffix drop to a caption line
    // under it. The cost figures.
    component KeyRow: ColumnLayout {
        id: keyRow
        required property var modelData
        Layout.fillWidth: true
        spacing: 0

        RowLayout {
            Layout.fillWidth: true
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                text: keyRow.modelData.label
                opacity: 0.85
                elide: Text.ElideRight
                Layout.fillWidth: true
            }
            PlasmaComponents.Label { textFormat: Text.PlainText; text: keyRow.modelData.value; font.family: "monospace" }
        }

        // Beside the label the two of them leave a sentence fighting over what
        // is left of a narrow popup. The caption line tracks the right edge,
        // because that is where the figure it qualifies sits.
        RowLayout {
            Layout.fillWidth: true
            visible: keyBadge.text !== "" || keySuffix.text !== ""

            Item { Layout.fillWidth: true }

            PlasmaComponents.Label {
                id: keyBadge
                textFormat: Text.PlainText
                text: keyRow.modelData.badge
                visible: text !== ""
                color: root.toneColor(keyRow.modelData.badge_tone)
                font: Kirigami.Theme.smallFont
                Layout.alignment: Qt.AlignBaseline
            }

            PlasmaComponents.Label {
                id: keySuffix
                textFormat: Text.PlainText
                // The separator divides a badge from a suffix, so a row with no
                // badge must not open on one.
                text: String(keyRow.modelData.suffix || "") === ""
                    ? ""
                    : (keyBadge.visible ? "·  " : "") + keyRow.modelData.suffix
                visible: text !== ""
                opacity: 0.6
                elide: Text.ElideRight
                font: Kirigami.Theme.smallFont
                Layout.alignment: Qt.AlignBaseline
                Layout.maximumWidth: implicitWidth
            }
        }

        // The suffix is the spec's ellipsized copy for surfaces that cannot
        // wrap; the tooltip carries the whole sentence.
        HoverHandler { id: keyHover }
        MonoToolTip {
            text: keyRow.modelData.tooltip || ""
            visible: text !== "" && keyHover.hovered
        }
    }

    QQC2.ButtonGroup { id: tabGroup }

    // ---- layout --------------------------------------------------------------
    ColumnLayout {
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            PlasmaExtras.Heading {
                level: 3
                text: "TokenGauge"
                Layout.fillWidth: true
            }
            PlasmaComponents.ToolButton {
                icon.name: "view-refresh"
                display: QQC2.AbstractButton.IconOnly
                text: i18n("Refresh")
                onClicked: root.action("--refresh")
            }
            PlasmaComponents.ToolButton {
                icon.name: "view-statistics"
                display: QQC2.AbstractButton.IconOnly
                text: i18n("History")
                checkable: true
                checked: full.historyOpen
                onClicked: full.openScreen(full.historyOpen ? "panel" : "history")
            }
            PlasmaComponents.ToolButton {
                icon.name: "configure"
                display: QQC2.AbstractButton.IconOnly
                text: i18n("Settings")
                checkable: true
                checked: full.settingsOpen
                onClicked: full.openScreen(full.settingsOpen ? "panel" : "settings")
            }
        }

        // error banner
        PlasmaComponents.Label {
            textFormat: Text.PlainText
            Layout.fillWidth: true
            visible: root.lastError !== "" || (root.snapshot.errors || []).length > 0
            wrapMode: Text.WordWrap
            color: root.snapshot.theme && root.snapshot.theme.red ? root.snapshot.theme.red : "#f38ba8"
            text: root.lastError !== ""
                ? root.lastError
                : (root.snapshot.errors || []).map(function (e) { return (e.provider || "?") + ": " + (e.message || e.raw || "error") }).join("\n")
        }

        // update-available banner
        RowLayout {
            Layout.fillWidth: true
            visible: root.updateAvailable
            PlasmaComponents.Label {
                textFormat: Text.PlainText
                Layout.fillWidth: true
                wrapMode: Text.WordWrap
                color: root.snapshot.theme && root.snapshot.theme.green ? root.snapshot.theme.green : "#a6e3a1"
                text: root.updateInfo && root.updateInfo.latest
                    ? i18n("Update available: v%1", root.updateInfo.latest)
                    : i18n("Update available")
            }
            PlasmaComponents.Button {
                icon.name: "system-software-update"
                text: root.updating ? i18n("Updating…") : i18n("Update")
                // Disabled while an update is in flight so a double-trigger can't
                // race a second --update; root.updating resets when exec finishes
                // (success or failure), re-enabling on a failed update.
                enabled: !root.updating
                onClicked: root.applyUpdate()
            }
        }

        // provider tab strip
        Flow {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing
            visible: full.showPanel && root.rows.length > 0
            Repeater {
                model: root.rows
                PlasmaComponents.Button {
                    required property int index
                    required property var modelData
                    text: modelData.label || modelData.provider
                    icon.source: modelData.icon_svg ? "file://" + modelData.icon_svg : ""
                    checkable: true
                    QQC2.ButtonGroup.group: tabGroup
                    checked: index === root.selectedIndex
                    highlighted: checked
                    onClicked: root.selectedProviderId = String(modelData.provider)
                }
            }
        }

        QQC2.ScrollView {
            id: scroll
            Layout.fillWidth: true
            Layout.fillHeight: true
            contentWidth: availableWidth

            ColumnLayout {
                width: scroll.availableWidth
                spacing: Kirigami.Units.smallSpacing

                // ---- provider card ----
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    visible: full.showPanel && full.row === null
                    text: i18n("No provider data yet.")
                    opacity: 0.7
                }

                RowLayout {
                    Layout.fillWidth: true
                    visible: full.showPanel && full.row !== null
                    Image {
                        visible: full.row && full.row.icon_svg && status === Image.Ready
                        source: full.row && full.row.icon_svg ? "file://" + full.row.icon_svg : ""
                        fillMode: Image.PreserveAspectFit
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        sourceSize.height: 64
                    }
                    ColumnLayout {
                        Layout.fillWidth: true
                        spacing: 0
                        PlasmaExtras.Heading {
                            level: 4
                            text: full.row ? (full.row.label || full.row.provider) : ""
                        }
                        PlasmaComponents.Label {
                            textFormat: Text.PlainText
                            visible: full.row && (full.row.plan_label || full.row.source)
                            text: full.row ? [full.row.plan_label, full.row.source].filter(Boolean).join(" · ") : ""
                            opacity: 0.7
                            font: Kirigami.Theme.smallFont
                        }
                    }
                    PlasmaComponents.Label {
                        textFormat: Text.PlainText
                        visible: full.row && full.row.stale
                        text: i18n("stale")
                        color: root.snapshot.theme && root.snapshot.theme.yellow ? root.snapshot.theme.yellow : "#f9e2af"
                        font: Kirigami.Theme.smallFont
                    }
                }

                // The core hands over an ordered list of sections, each
                // naming its own kind; one delegate per kind draws it. A new
                // section in the core appears here with no edit to this file.
                Repeater {
                    model: full.showPanel && full.row && Array.isArray(full.row.panel)
                           ? full.row.panel : []

                    ColumnLayout {
                        required property var modelData
                        Layout.fillWidth: true
                        spacing: modelData.kind === "meters" ? Kirigami.Units.smallSpacing : 1

                        Kirigami.Separator { Layout.fillWidth: true }
                        PlasmaComponents.Label {
                            textFormat: Text.PlainText
                            text: modelData.title
                            font.bold: true
                            opacity: 0.85
                        }
                        Repeater {
                            model: modelData.kind === "meters" ? modelData.rows : []
                            Meter {}
                        }
                        Repeater {
                            model: modelData.kind === "bars" ? modelData.rows : []
                            BarRow {}
                        }
                        Repeater {
                            model: modelData.kind === "rows" ? modelData.rows : []
                            KeyRow {}
                        }
                    }
                }

                // ---- history screen ----
                // The series are resolved in the core, every range at once, so
                // this draws a chart and formats nothing.
                ColumnLayout {
                    Layout.fillWidth: true
                    visible: full.historyOpen
                    spacing: Kirigami.Units.smallSpacing

                    Flow {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing
                        Repeater {
                            model: full.historyOpen && full.history ? full.history.series : []
                            PlasmaComponents.Button {
                                required property int index
                                required property var modelData
                                text: modelData.label
                                checkable: true
                                checked: index === full.historyRange
                                highlighted: checked
                                onClicked: full.historyRange = index
                            }
                        }
                    }

                    // No provider row at all, or a snapshot written by a daemon
                    // older than this widget, which carries no `history` field.
                    // Without this the screen hides the panel and draws nothing,
                    // which reads as the widget having broken.
                    PlasmaComponents.Label {
                        Layout.fillWidth: true
                        textFormat: Text.PlainText
                        visible: full.historySeries === null
                        text: i18n("No history yet.")
                        opacity: 0.7
                    }

                    PlasmaComponents.Label {
                        Layout.fillWidth: true
                        textFormat: Text.PlainText
                        visible: full.historySeries !== null
                        text: full.historySeries
                            ? full.historySeries.total_usd + "  ·  "
                              + full.historySeries.total_tokens + " tokens"
                              + "  ·  avg " + full.historySeries.average_usd
                            : ""
                        elide: Text.ElideRight
                    }

                    PlasmaComponents.Label {
                        Layout.fillWidth: true
                        textFormat: Text.PlainText
                        visible: full.historySeries !== null && full.historySeries.empty
                        text: i18n("Nothing spent in this range.")
                        opacity: 0.7
                    }

                    Canvas {
                        id: historyChart
                        Layout.fillWidth: true
                        Layout.preferredHeight: Kirigami.Units.gridUnit * 8
                        visible: full.historySeries !== null && !full.historySeries.empty
                        readonly property var points: full.historySeries && !full.historySeries.empty
                                                      ? full.historySeries.points : []
                        onPointsChanged: requestPaint()
                        onPaint: {
                            var ctx = getContext("2d")
                            ctx.reset()
                            var n = points.length
                            if (n === 0)
                                return
                            // Wide steps get a gap between them; ninety days of
                            // bars do not have the room to spare.
                            var gap = n <= 12 ? 2 : (n <= 31 ? 1 : 0)
                            var w = Math.max(1, (width - gap * (n - 1)) / n)
                            for (var i = 0; i < n; i++) {
                                var p = points[i]
                                // A floor of one pixel: a step that spent
                                // something must never draw as a step that
                                // spent nothing.
                                var h = p.fraction > 0
                                    ? Math.max(1, p.fraction * height)
                                    : 0
                                ctx.fillStyle = full.chartColor(p)
                                // The step in progress is short because it is
                                // not over, so it is drawn as unfinished
                                // rather than as a fall.
                                ctx.globalAlpha = p.partial ? 0.45 : 1.0
                                ctx.fillRect(i * (w + gap), height - h, w, h)
                            }
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        visible: historyChart.visible
                        PlasmaComponents.Label {
                            textFormat: Text.PlainText
                            text: full.historySeries && full.historySeries.points.length > 0
                                ? full.historySeries.points[0].full_label : ""
                            opacity: 0.6
                            font: Kirigami.Theme.smallFont
                        }
                        Item { Layout.fillWidth: true }
                        PlasmaComponents.Label {
                            textFormat: Text.PlainText
                            text: full.historySeries && full.historySeries.points.length > 0
                                ? full.historySeries.points[full.historySeries.points.length - 1].full_label
                                : ""
                            opacity: 0.6
                            font: Kirigami.Theme.smallFont
                        }
                    }

                    PlasmaComponents.Label {
                        Layout.fillWidth: true
                        textFormat: Text.PlainText
                        visible: full.historyOpen && full.history
                        wrapMode: Text.WordWrap
                        opacity: 0.6
                        font: Kirigami.Theme.smallFont
                        text: full.history
                            ? [full.history.covers].concat(full.history.notes || []).join(" — ")
                            : ""
                    }
                }

                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                    horizontalAlignment: Text.AlignRight
                    visible: full.showPanel && full.row && full.row.updated
                    opacity: 0.5
                    font: Kirigami.Theme.smallFont
                    text: full.row && full.row.updated ? i18n("Updated %1", full.row.updated) : ""
                }

                // ---- settings pane ----
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    visible: full.settingsOpen
                    text: i18n("OAuth providers")
                    font.bold: true
                }
                Repeater {
                    model: full.settingsOpen ? full.oauthProviders : []
                    PlasmaComponents.CheckBox {
                        required property var modelData
                        text: modelData.charAt(0).toUpperCase() + modelData.slice(1)
                        checked: (root.snapshot.enabled || []).indexOf(modelData) !== -1
                        onToggled: root.action("--set-provider " + modelData + "=" + (checked ? "true" : "false"))
                    }
                }
                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: full.settingsOpen
                }
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    visible: full.settingsOpen
                    text: i18n("Fleet sync")
                    font.bold: true
                }
                PlasmaComponents.Button {
                    visible: full.settingsOpen
                    icon.name: "folder-sync"
                    text: i18n("Set up sync…")
                    onClicked: root.openSyncSetup()
                }
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                    visible: full.settingsOpen
                    wrapMode: Text.WordWrap
                    opacity: 0.7
                    text: i18n("Adds up tokens and cost across your machines. Opens in a terminal.")
                }
                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: full.settingsOpen
                }
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    visible: full.settingsOpen
                    text: i18n("Pin to bar")
                    font.bold: true
                }
                PlasmaComponents.RadioButton {
                    visible: full.settingsOpen
                    text: i18n("Highest usage")
                    checked: !root.snapshot.primary
                    onToggled: if (checked) root.action("--set-primary highest")
                }
                Repeater {
                    model: full.settingsOpen ? root.rows : []
                    PlasmaComponents.RadioButton {
                        required property var modelData
                        text: modelData.label || modelData.provider
                        checked: root.snapshot.primary === modelData.provider.toLowerCase()
                        onToggled: if (checked) root.action("--set-primary " + modelData.provider.toLowerCase())
                    }
                }
                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: full.settingsOpen
                }
                PlasmaComponents.Label {
                    textFormat: Text.PlainText
                    visible: full.settingsOpen
                    opacity: 0.5
                    font: Kirigami.Theme.smallFont
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                    // Both versions, because the applet and the binary are
                    // installed separately: `--update` replaces binaries, and
                    // until the applet is reinstalled it keeps driving whatever
                    // QML this box already had. Showing only the binary's
                    // version is what made that skew invisible.
                    text: {
                        var binary = root.snapshot.version || ""
                        var applet = ""
                        try {
                            applet = String(Plasmoid.metaData.version || "")
                        } catch (e) {
                            applet = ""
                        }
                        if (binary === "") return "TokenGauge"
                        if (applet === "" || applet === binary) return i18n("TokenGauge v%1", binary)
                        return i18n("applet v%1, binary v%2 - reinstall: tokengauge-waybar --install-frontend plasma",
                                    applet, binary)
                    }
                }
            }
        }
    }
}

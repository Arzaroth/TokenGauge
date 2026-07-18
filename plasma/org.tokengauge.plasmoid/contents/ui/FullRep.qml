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
    readonly property var row: root.rows.length > 0
        ? root.rows[Math.min(root.selectedIndex, root.rows.length - 1)]
        : null
    readonly property var oauthProviders: ["codex", "claude"]

    function chartMax(hist) {
        var m = 0
        for (var i = 0; i < (hist || []).length; i++)
            if (hist[i] > m) m = hist[i]
        return m > 0 ? m : 1
    }

    // ---- reusable pieces -----------------------------------------------------
    component Meter: ColumnLayout {
        property string label: ""
        property var value: null
        property string reset: ""
        property string pace: ""
        spacing: 2
        Layout.fillWidth: true

        RowLayout {
            Layout.fillWidth: true
            PlasmaComponents.Label {
                text: label
                Layout.fillWidth: true
                elide: Text.ElideRight
            }
            PlasmaComponents.Label {
                text: value === null || value === undefined ? "—" : value + "%"
                color: root.tierColor(value)
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
                visible: value !== null && value !== undefined
                width: parent.width * Math.max(0, Math.min(100, value || 0)) / 100
                color: root.tierColor(value)
            }
        }
        PlasmaComponents.Label {
            visible: reset !== "" || pace !== ""
            text: {
                if (pace === "")
                    return reset
                return reset === "" ? pace : reset + "  ·  " + pace
            }
            opacity: 0.7
            font: Kirigami.Theme.smallFont
        }
    }

    component CostRow: RowLayout {
        property string label: ""
        property string amount: ""
        Layout.fillWidth: true
        PlasmaComponents.Label { text: label; opacity: 0.85; Layout.fillWidth: true }
        PlasmaComponents.Label { text: amount; font.family: "monospace" }
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
                icon.name: "configure"
                display: QQC2.AbstractButton.IconOnly
                text: i18n("Settings")
                checkable: true
                checked: full.settingsOpen
                onClicked: full.settingsOpen = !full.settingsOpen
            }
        }

        // error banner
        PlasmaComponents.Label {
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
            visible: !full.settingsOpen && root.rows.length > 0
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
                    onClicked: { root.userSelected = true; root.selectedIndex = index }
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
                    visible: !full.settingsOpen && full.row === null
                    text: i18n("No provider data yet.")
                    opacity: 0.7
                }

                RowLayout {
                    Layout.fillWidth: true
                    visible: !full.settingsOpen && full.row !== null
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
                            visible: full.row && (full.row.plan_label || full.row.source)
                            text: full.row ? [full.row.plan_label, full.row.source].filter(Boolean).join(" · ") : ""
                            opacity: 0.7
                            font: Kirigami.Theme.smallFont
                        }
                    }
                    PlasmaComponents.Label {
                        visible: full.row && full.row.stale
                        text: i18n("stale")
                        color: root.snapshot.theme && root.snapshot.theme.yellow ? root.snapshot.theme.yellow : "#f9e2af"
                        font: Kirigami.Theme.smallFont
                    }
                }

                Meter {
                    visible: !full.settingsOpen && full.row !== null
                    label: full.row && full.row.window_labels ? full.row.window_labels[0] : i18n("Session")
                    value: full.row ? full.row.session_used : null
                    reset: full.row ? full.row.session_reset : ""
                    pace: full.row && full.row.session_pace ? full.row.session_pace : ""
                }
                Meter {
                    visible: !full.settingsOpen && full.row && full.row.weekly_used !== null && full.row.weekly_used !== undefined
                    label: full.row && full.row.window_labels ? full.row.window_labels[1] : i18n("Weekly")
                    value: full.row ? full.row.weekly_used : null
                    reset: full.row ? full.row.weekly_reset : ""
                    pace: full.row && full.row.weekly_pace ? full.row.weekly_pace : ""
                }
                Meter {
                    visible: !full.settingsOpen && full.row && full.row.tertiary_used !== null && full.row.tertiary_used !== undefined
                    label: full.row && full.row.window_labels ? full.row.window_labels[2] : i18n("Tertiary")
                    value: full.row ? full.row.tertiary_used : null
                    reset: full.row ? full.row.tertiary_reset : ""
                }
                Repeater {
                    model: !full.settingsOpen && full.row && full.row.extra_windows ? full.row.extra_windows : []
                    Meter {
                        required property var modelData
                        label: modelData.title
                        value: modelData.used
                        reset: modelData.reset
                    }
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: !full.settingsOpen && full.row && full.row.cost
                }
                PlasmaComponents.Label {
                    visible: !full.settingsOpen && full.row && full.row.cost
                    text: i18n("Cost")
                    font.bold: true
                    opacity: 0.85
                }
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 1
                    visible: !full.settingsOpen && full.row && full.row.cost
                    property var cost: full.row && full.row.cost ? full.row.cost : ({})
                    CostRow { label: i18n("Today"); amount: root.fmtUsd(parent.cost.today_usd) }
                    CostRow { label: i18n("Session"); amount: root.fmtUsd(parent.cost.session_usd) }
                    CostRow { label: i18n("7-day"); amount: root.fmtUsd(parent.cost.weekly_usd) }
                    CostRow { label: i18n("Month"); amount: root.fmtUsd(parent.cost.monthly_usd) }
                    CostRow {
                        visible: parent.cost.burn_rate
                        label: i18n("Burn rate")
                        amount: parent.cost.burn_rate ? root.fmtUsd(parent.cost.burn_rate.cost_per_hour) + "/hr" : "—"
                    }
                }

                PlasmaComponents.Label {
                    visible: !full.settingsOpen && full.row && full.row.cost && (full.row.cost.weekly_cost_history || []).length > 0
                    text: i18n("Last 7 days")
                    font.bold: true
                    opacity: 0.85
                }
                RowLayout {
                    Layout.fillWidth: true
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 3
                    spacing: 2
                    visible: !full.settingsOpen && full.row && full.row.cost && (full.row.cost.weekly_cost_history || []).length > 0
                    Repeater {
                        model: full.row && full.row.cost ? full.row.cost.weekly_cost_history : []
                        Item {
                            required property var modelData
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            Rectangle {
                                anchors.bottom: parent.bottom
                                width: parent.width
                                radius: 2
                                color: Kirigami.Theme.highlightColor
                                height: Math.max(2, parent.height * modelData / full.chartMax(full.row.cost.weekly_cost_history))
                                PlasmaComponents.ToolTip.text: root.fmtUsd(modelData)
                                PlasmaComponents.ToolTip.visible: barHover.hovered
                                PlasmaComponents.ToolTip.delay: 300
                                HoverHandler { id: barHover }
                            }
                        }
                    }
                }

                PlasmaComponents.Label {
                    Layout.fillWidth: true
                    horizontalAlignment: Text.AlignRight
                    visible: !full.settingsOpen && full.row && full.row.updated
                    opacity: 0.5
                    font: Kirigami.Theme.smallFont
                    text: full.row && full.row.updated ? i18n("Updated %1", full.row.updated) : ""
                }

                // ---- settings pane ----
                PlasmaComponents.Label {
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
            }
        }
    }
}

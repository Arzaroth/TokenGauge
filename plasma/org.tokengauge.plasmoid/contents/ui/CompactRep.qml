import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.components as PlasmaComponents
import org.kde.kirigami as Kirigami

MouseArea {
    id: compact

    readonly property var row: root.rows.length > 0
        ? root.rows[Math.min(root.selectedIndex, root.rows.length - 1)]
        : null
    readonly property var pct: root.windowPercent(row)
    readonly property bool vertical: Plasmoid.formFactor === PlasmaCore.Types.Vertical

    Layout.minimumWidth: layout.implicitWidth + Kirigami.Units.smallSpacing * 2
    Layout.preferredWidth: Layout.minimumWidth
    Layout.minimumHeight: layout.implicitHeight + Kirigami.Units.smallSpacing * 2

    acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton | Qt.BackButton
    hoverEnabled: true

    onClicked: (mouse) => {
        if (mouse.button === Qt.LeftButton)
            root.expanded = !root.expanded
        else if (mouse.button === Qt.RightButton)
            root.action("--refresh")
        else if (mouse.button === Qt.MiddleButton)
            root.action("--open=dashboard")
        else if (mouse.button === Qt.BackButton)
            root.action("--open=status")
    }

    onWheel: (wheel) => {
        var n = root.rows.length
        if (n < 2) return
        root.userSelected = true
        if (wheel.angleDelta.y > 0)
            root.selectedIndex = (root.selectedIndex - 1 + n) % n
        else
            root.selectedIndex = (root.selectedIndex + 1) % n
    }

    // Icon beside the percent on a horizontal panel; stacked above it on a
    // vertical panel so a thin strip stays legible.
    readonly property int iconSize: Math.round(Math.min(compact.width, compact.height) * (compact.vertical ? 0.8 : 0.7))

    GridLayout {
        id: layout
        anchors.centerIn: parent
        columns: compact.vertical ? 1 : 2
        rowSpacing: 0
        columnSpacing: Kirigami.Units.smallSpacing

        Image {
            id: logo
            visible: compact.row && compact.row.icon_svg && status === Image.Ready
            source: compact.row && compact.row.icon_svg ? "file://" + compact.row.icon_svg : ""
            fillMode: Image.PreserveAspectFit
            Layout.preferredHeight: compact.iconSize
            Layout.preferredWidth: compact.iconSize
            Layout.alignment: Qt.AlignCenter
            sourceSize.height: 64
            sourceSize.width: 64
        }

        // Glyph fallback when no SVG is installed / available.
        PlasmaComponents.Label {
            visible: !logo.visible && compact.row
            text: compact.row ? (compact.row.glyph || "") : ""
            color: compact.row ? (compact.row.color || Kirigami.Theme.textColor) : Kirigami.Theme.textColor
            font.family: "Symbols Nerd Font"
            font.pixelSize: compact.iconSize
            Layout.alignment: Qt.AlignCenter
        }

        PlasmaComponents.Label {
            visible: Plasmoid.configuration.showPercentInPanel
            text: compact.pct === null || compact.pct === undefined ? "—" : compact.pct + "%"
            color: root.tierColor(compact.pct)
            font.bold: true
            Layout.alignment: Qt.AlignCenter
        }
    }
}

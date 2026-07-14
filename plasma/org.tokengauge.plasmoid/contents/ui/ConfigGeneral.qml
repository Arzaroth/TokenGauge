import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Kirigami.FormLayout {
    id: page

    property alias cfg_waybarBinary: binaryField.text
    property alias cfg_refreshInterval: intervalField.value
    property alias cfg_showPercentInPanel: percentBox.checked

    QQC2.TextField {
        id: binaryField
        Kirigami.FormData.label: i18n("tokengauge-waybar binary:")
        placeholderText: "tokengauge-waybar"
    }

    QQC2.SpinBox {
        id: intervalField
        Kirigami.FormData.label: i18n("Refresh interval (seconds):")
        from: 15
        to: 3600
        stepSize: 15
    }

    QQC2.CheckBox {
        id: percentBox
        Kirigami.FormData.label: i18n("Panel:")
        text: i18n("Show percentage next to the icon")
    }
}

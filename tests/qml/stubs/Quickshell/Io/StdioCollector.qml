import QtQuick

// Stands in for Quickshell's collector: the harness sets `text` and fires
// `streamFinished`, so what is under test is what the widget does with the
// bytes rather than how they arrived.
QtObject {
    property bool waitForEnd: false
    property string text: ""
    signal streamFinished()
}

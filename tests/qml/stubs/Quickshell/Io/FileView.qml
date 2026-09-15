import QtQuick

// Stands in for Quickshell's FileView. Unlike the Process stub this one really
// reads, because the widget takes its own version out of the manifest sitting
// beside it - and a stub that answered nothing would have left the half of the
// version skew the extension owns untested.
//
// `fileChanged` is the harness's to fire: nothing here watches.
QtObject {
    id: view
    property string path: ""
    property bool watchChanges: false
    property bool printErrors: true
    property string contents: ""
    signal loaded()
    signal loadFailed()
    signal fileChanged()

    function text() { return contents }

    onPathChanged: view.reload()

    function reload() {
        if (view.path === "") return
        var xhr = new XMLHttpRequest()
        xhr.onreadystatechange = function () {
            if (xhr.readyState !== XMLHttpRequest.DONE) return
            // A file:// read reports 0 on success and on failure alike; the
            // body is the only signal.
            if (xhr.responseText.length > 0) {
                view.contents = xhr.responseText
                view.loaded()
            } else {
                view.loadFailed()
            }
        }
        xhr.open("GET", view.path.indexOf("file://") === 0 ? view.path : "file://" + view.path)
        xhr.send()
    }
}

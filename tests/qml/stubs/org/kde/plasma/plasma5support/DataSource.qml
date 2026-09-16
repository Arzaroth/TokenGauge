import QtQuick
import Harness

// Stands in for Plasma's executable data engine, which keys a running command
// by its whole command line and hands back one object when it exits. Nothing
// is spawned: the harness answers by command line, so what is under test is
// what the applet does with an answer rather than how the answer arrived.
QtObject {
    id: source
    property string engine: ""
    property var connectedSources: []
    signal newData(string sourceName, var data)

    function connectSource(name) {
        Registry.started({
            "commandLine": name,
            "answer": function (out, err, code) {
                source.newData(name, {"exit code": code, "stdout": out, "stderr": err})
            }
        })
    }

    function disconnectSource(name) {}
}

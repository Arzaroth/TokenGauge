import QtQuick
import Quickshell
import Quickshell.Io

// The data side of the widget. Everything the panel draws comes from one
// `tokengauge-waybar --json` snapshot; the QML never reads a credential, a
// cache file, or a provider endpoint itself.
Item {
  id: root
  visible: false

  property var settings: ({})

  property var snapshot: null
  property string lastError: ""
  property bool loading: false
  property bool updating: false
  property int revision: 0

  readonly property string binary: String(setting("binary", "tokengauge-waybar"))
  readonly property int refreshIntervalSec: Math.max(30, Number(setting("refreshIntervalSec", 600)) || 600)

  readonly property var rows: snapshot && Array.isArray(snapshot.rows) ? snapshot.rows : []
  readonly property var errors: snapshot && Array.isArray(snapshot.errors) ? snapshot.errors : []
  readonly property var enabled: snapshot && Array.isArray(snapshot.enabled) ? snapshot.enabled : []
  // Every toggleable provider, not just the enabled ones - the settings pane
  // needs the full list to draw a switch for each.
  readonly property var allProviders: snapshot && Array.isArray(snapshot.providers) ? snapshot.providers : []
  readonly property string primary: snapshot ? String(snapshot.primary || "") : ""
  readonly property var updateStatus: snapshot ? snapshot.update : null

  function setting(name, fallback) {
    var value = settings ? settings[name] : undefined
    return value === undefined || value === null ? fallback : value
  }

  // uwsm hands the shell a PATH that often lacks ~/.local/bin, which is where
  // the installer drops the binaries. Same wrapper the Plasma applet uses.
  function shellQuote(s) {
    return "'" + String(s).replace(/'/g, "'\\''") + "'"
  }

  function command(tail) {
    return ["sh", "-c", 'export PATH="$HOME/.local/bin:$HOME/bin:/usr/local/bin:$PATH"; ' + tail]
  }

  // Returns false when a run is already in flight, so callers that arm a
  // spinner do not leave it spinning on a request that never started.
  function run(tail) {
    if (snapshotProcess.running) return false
    root.loading = true
    snapshotProcess.command = command(tail)
    snapshotProcess.running = true
    return true
  }

  function reload() {
    run(shellQuote(binary) + " --json")
  }

  // Run an action flag, then re-read the snapshot in the same subprocess so
  // the panel never renders against pre-action state.
  function action(flag) {
    run(shellQuote(binary) + " " + flag + " && " + shellQuote(binary) + " --json")
  }

  function refreshNow() {
    action("--refresh")
  }

  // Both of these rewrite ~/.config/tokengauge/config.toml and reload the
  // daemon, so the follow-up --json in the same subprocess already reflects
  // the change.
  function setProvider(name, enable) {
    action("--set-provider " + shellQuote(name + "=" + (enable ? "true" : "false")))
  }

  function setPrimary(name) {
    action("--set-primary " + shellQuote(name))
  }

  function applyUpdate() {
    // --update's human-readable stdout would break JSON.parse; its stderr is
    // kept so a failed update still surfaces. The flag is armed only once the
    // run is accepted, or a refused one strands the button on "Updating…".
    root.updating = run(shellQuote(binary) + " --update >/dev/null && "
                        + shellQuote(binary) + " --json")
  }

  Process {
    id: snapshotProcess
    running: false

    onExited: function(exitCode) {
      root.loading = false
      root.updating = false
      if (exitCode !== 0 && root.lastError === "")
        root.lastError = "tokengauge-waybar exited " + exitCode
    }

    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.parse(text)
    }

    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var message = String(text || "").trim()
        if (message !== "") console.warn("tokengauge", message)
      }
    }
  }

  function parse(text) {
    var raw = String(text || "").trim()
    if (raw === "") return
    try {
      var parsed = JSON.parse(raw)
      if (parsed && typeof parsed === "object") {
        root.snapshot = parsed
        root.lastError = ""
        root.revision++
      }
    } catch (e) {
      root.lastError = "Unreadable snapshot"
      console.warn("tokengauge", "Ignoring bad snapshot", e)
    }
  }

  Timer {
    interval: root.refreshIntervalSec * 1000
    running: true
    repeat: true
    onTriggered: root.reload()
  }

  Component.onCompleted: reload()
}

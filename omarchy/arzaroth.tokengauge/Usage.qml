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
  readonly property string version: snapshot ? String(snapshot.version || "") : ""

  readonly property var updateStatus: snapshot ? snapshot.update : null

  // A few bytes the binary rewrites after every fetch. Watching it turns the
  // panel from polling into push: a fetch by the daemon, the TUI or another
  // frontend lands here at once instead of on the next timer tick. The
  // snapshot still only ever comes from `--json`.
  readonly property string revisionFile: snapshot ? String(snapshot.revision_file || "") : ""

  FileView {
    path: root.revisionFile
    watchChanges: root.revisionFile !== ""
    printErrors: false
    // One in-place rewrite raises more than one change; coalesce them into a
    // single re-read.
    onFileChanged: revisionSettle.restart()
  }

  Timer {
    id: revisionSettle
    interval: 250
    repeat: false
    // reload() refuses while a run is in flight, and the change that woke us is
    // not repeated; wait out the run rather than dropping the update until the
    // next poll.
    onTriggered: if (!root.reload()) restart()
  }

  // The widget's own version, read from the manifest sitting next to this file.
  // The plugin directory and the binary are installed separately, so the two
  // can drift; reporting only the binary's version hides exactly that.
  property string widgetVersion: ""

  FileView {
    path: Qt.resolvedUrl("manifest.json").toString().replace(/^file:\/\//, "")
    watchChanges: false
    printErrors: false
    onLoaded: {
      try {
        root.widgetVersion = String((JSON.parse(text()) || {}).version || "")
      } catch (e) {
        root.widgetVersion = ""
      }
    }
    onLoadFailed: root.widgetVersion = ""
  }

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
    watchdog.restart()
    return true
  }

  // A run that never returns would pin `running` true and block every later
  // refresh for the life of the shell. SIGTERM first, because a wedged fetch
  // still gets to clean up; SIGKILL only if it ignores that.
  Timer {
    id: watchdog
    interval: 90000
    repeat: false
    onTriggered: {
      if (!snapshotProcess.running) return
      console.warn("tokengauge", "snapshot run timed out, terminating")
      root.lastError = "Snapshot timed out"
      snapshotProcess.running = false
      killTimer.restart()
    }
  }

  Timer {
    id: killTimer
    interval: 5000
    repeat: false
    onTriggered: if (snapshotProcess.running) snapshotProcess.signal(9)
  }

  function reload() {
    return run(shellQuote(binary) + " --json")
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
  // daemon. `--set-provider` fetches a newly enabled provider before it
  // returns, so the --json chained behind it already carries the new row and
  // the chip that switches to it.
  function setProvider(name, enable) {
    action("--set-provider " + shellQuote(name + "=" + (enable ? "true" : "false")))
  }

  function setPrimary(name) {
    action("--set-primary " + shellQuote(name))
  }

  // `--sync-setup` returns as soon as it has spawned a terminal, so the
  // snapshot read chained behind it is not waiting on the user. `;` rather than
  // `&&` so the panel still refreshes when no terminal was found. Only stdout
  // is discarded: stderr carries "no terminal found", which is the whole
  // message the user needs when the button appears to do nothing.
  function openSyncSetup() {
    run(shellQuote(binary) + " --sync-setup >/dev/null; "
        + shellQuote(binary) + " --json")
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
      watchdog.stop()
      killTimer.stop()
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

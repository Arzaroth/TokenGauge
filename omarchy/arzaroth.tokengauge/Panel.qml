import QtQuick
import QtQuick.Controls
import QtQuick.Effects
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "arzaroth.tokengauge"
  ipcTarget: "arzaroth.tokengauge"
  manageIpc: false

  // Ui.Panel does not lift these off the host the way Ui.BarWidget does, and
  // this file is the bar-widget entry point as well as the popup.
  readonly property bool vertical: bar ? bar.vertical : false

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property color track: Style.selectedFillFor(foreground, Color.accent)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  readonly property var providers: usage.rows
  // The selection follows the provider id, not the slot it sits in, so a row
  // that appears or drops out on a refresh cannot swap what you were reading.
  property string selectedProviderId: ""
  readonly property int providerIndex: {
    for (var i = 0; i < providers.length; i++)
      if (String(providers[i].provider) === selectedProviderId) return i
    // Nothing chosen yet, so follow the pinned provider - the bar reports its
    // percentage, and opening the panel on a different one reads as a bug.
    var pinned = present(usage.primary).toLowerCase()
    if (pinned !== "")
      for (var j = 0; j < providers.length; j++)
        if (String(providers[j].provider).toLowerCase() === pinned) return j
    return 0
  }
  readonly property var provider: providers.length > 0 ? providers[providerIndex] : null

  // The panel layout comes from the core, already ordered and formatted, so
  // this file decides how a section looks and never what is in one.
  readonly property var sections: provider && Array.isArray(provider.panel) ? provider.panel : []
  readonly property var headline: {
    for (var i = 0; i < sections.length; i++)
      if (sections[i].id === "limits" && sections[i].rows.length > 0) return sections[i].rows[0]
    return null
  }
  readonly property bool alarming: !!headline && (Number(headline.fraction) || 0) >= 0.9

  property bool settingsOpen: false
  property bool historyOpen: false
  // Which range the history screen is showing. Every range is already on the
  // row, so cycling one is a rebind rather than another `--json`.
  property int historyRange: 0

  // Settings and history are both second screens over the panel, and exactly
  // one of the three is up at a time. A year of bars does not belong above the
  // limit gauges, which is why history is a screen rather than a section.
  readonly property bool showPanel: !root.settingsOpen && !root.historyOpen

  function openScreen(which) {
    root.settingsOpen = which === "settings"
    root.historyOpen = which === "history"
  }

  readonly property var history: provider && provider.history ? provider.history : null
  readonly property var historySeries: history && Array.isArray(history.series)
                                       && history.series.length > 0
    ? history.series[Math.min(root.historyRange, history.series.length - 1)]
    : null
  property real wheelAccumulator: 0

  // The frontend's own read failure - a binary that vanished, a snapshot that
  // will not parse - which is the state that looks exactly like a working
  // panel because the last good numbers stay on screen.
  //
  // Per-provider fetch errors are not folded in here: the Errors repeater at
  // the foot of the panel already lists them, and joining them into this
  // banner printed every one of them twice.
  readonly property string errorText: present(usage.lastError)


  function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)) }
  function alpha(c, a) { return Qt.rgba(c.r, c.g, c.b, a) }

  // The snapshot spells "no value" as an em dash, which reads as content to
  // every `!== ""` check downstream.
  function present(value) {
    var text = String(value === null || value === undefined ? "" : value).trim()
    return text === "" || text === "-" || text === "—" ? "" : text
  }

  // The settings pane is a list of switches with no focus chain of its own:
  // KeyboardPanel hands focus to the key catcher, whose movement keys are
  // already spoken for by the provider tabs and the scroll. Number keys map to
  // the switch rows in order, `p` walks the pin, so nothing in the pane is
  // mouse-only.
  function toggleProviderAt(index) {
    var list = usage.allProviders
    if (index < 0 || index >= list.length) return
    var id = String(list[index])
    usage.setProvider(id, usage.enabled.indexOf(id) < 0)
  }

  function cyclePin() {
    var choices = ["highest"].concat(usage.enabled.map(function(id) { return String(id) }))
    var current = present(usage.primary) === "" ? "highest" : String(usage.primary).toLowerCase()
    var at = choices.indexOf(current)
    usage.setPrimary(choices[(at + 1) % choices.length])
  }

  // `tokengauge-waybar --open` resolves the provider from the config, not from
  // the caller, so it would open the pinned provider rather than the tab you
  // are looking at. The row carries its own URLs instead.
  function openProviderUrl(key) {
    if (!provider) return
    var url = present(provider[key])
    if (url === "") return
    if (bar) bar.run("xdg-open " + JSON.stringify(url))
    else Quickshell.execDetached(["xdg-open", url])
  }

  function selectHistoryRange(index) {
    var series = root.history && Array.isArray(root.history.series) ? root.history.series : []
    if (index >= 0 && index < series.length)
      root.historyRange = index
  }

  function selectProvider(index) {
    if (providers.length === 0) return
    var wrapped = ((index % providers.length) + providers.length) % providers.length
    selectedProviderId = String(providers[wrapped].provider)
  }

  // ------------------------------------------------------------ sections

  // A tone name from the core mapped onto the bar's palette. Omarchy themes
  // carry a foreground and an urgent colour and nothing between them, so the
  // green/yellow tiers the other frontends draw collapse onto the foreground
  // here rather than inventing two colours the theme never picked.
  function toneColor(tone) {
    switch (String(tone)) {
      case "critical": return root.urgent
      case "dim": return root.dim
      default: return root.foreground
    }
  }

  function joinValue(row) {
    var value = present(row.value)
    var suffix = present(row.suffix)
    return suffix === "" ? value : value + "  \u00b7  " + suffix
  }

  // A disabled provider has no row to carry its display name, so the settings
  // pane falls back to the id - which needs the same acronym casing.
  function providerLabel(id) {
    var key = String(id || "").toLowerCase()
    for (var i = 0; i < providers.length; i++) {
      var row = providers[i]
      if (String(row.provider).toLowerCase() === key) return String(row.label || row.provider)
    }
    if (/^(glm|gpt|zai|ai)$/i.test(key)) return key.toUpperCase()
    return key.charAt(0).toUpperCase() + key.slice(1)
  }

  // -------------------------------------------------------- brand marks

  // The installer drops the provider logos next to the binaries; the bar glyph
  // stands in whenever one is missing.
  // Brand colour for the active provider's mark, from the snapshot; the bar
  // foreground stands in when a provider ships no colour.
  readonly property color markColor: provider && present(provider.color) !== ""
    ? provider.color : foreground

  function markSource(row) {
    if (!row) return ""
    var path = root.present(row.icon_svg)
    return path === "" ? "" : (path.indexOf("file://") === 0 ? path : "file://" + path)
  }

  // -------------------------------------------------------------- pieces

  // A label / meter / footnote triple. Every limit row and every history row
  // is one of these, which is what keeps the two sections visually identical.
  component MeterRow: Column {
    id: meterRow

    property string label: ""
    property string value: ""
    property real fraction: 0
    property string footnote: ""
    property string badge: ""
    property color badgeColor: root.dim
    property string tooltip: ""
    property bool emphasized: false
    property color fill: root.foreground

    width: parent ? parent.width : 0
    spacing: Style.space(4)

    Item {
      width: parent.width
      height: labelText.implicitHeight

      Text {
        id: labelText
        textFormat: Text.PlainText
        anchors.left: parent.left
        text: meterRow.label
        color: meterRow.emphasized ? root.foreground : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        font.bold: meterRow.emphasized
      }

      Text {
        textFormat: Text.PlainText
        anchors.right: parent.right
        text: meterRow.value
        color: meterRow.emphasized ? root.foreground : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        font.bold: meterRow.emphasized
      }

      MouseArea {
        id: meterHover
        anchors.fill: parent
        hoverEnabled: meterRow.tooltip !== ""
        acceptedButtons: Qt.NoButton
      }

      PanelToolTip {
        visible: meterHover.containsMouse && meterRow.tooltip !== ""
        text: meterRow.tooltip
        fontFamily: root.fontFamily
      }
    }

    Rectangle {
      width: parent.width
      height: Math.max(Style.space(4), Math.round(Style.spacing.controlHeight * 0.14))
      radius: height / 2
      color: root.track

      Rectangle {
        width: Math.max(parent.height, parent.width * root.clamp(meterRow.fraction, 0, 1))
        height: parent.height
        radius: parent.radius
        color: meterRow.fill
        visible: meterRow.fraction > 0

        Behavior on width {
          NumberAnimation { duration: 160; easing.type: Easing.OutCubic }
        }
      }
    }

    // The badge keeps its own colour rather than joining the footnote string:
    // it carries the pace projection, and "ends ~120%" painted dim reads as a
    // caption instead of the warning it is.
    Row {
      visible: meterRow.footnote !== "" || meterRow.badge !== ""
      spacing: Style.space(6)

      Text {
        textFormat: Text.PlainText
        text: meterRow.footnote
        visible: text !== ""
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
      }

      Text {
        textFormat: Text.PlainText
        text: meterRow.badge
        visible: text !== ""
        color: meterRow.badgeColor
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
      }
    }
  }

  // Model rows read as a table: the share bar fills the row behind the label
  // instead of stacking under it, which keeps the whole panel on one screen.
  component TableRow: Item {
    id: tableRow

    property string label: ""
    property string value: ""
    property real fraction: 0
    property string tooltip: ""
    property bool emphasized: false

    width: parent ? parent.width : 0
    implicitHeight: rowLabel.implicitHeight + Style.spacing.lg

    Rectangle {
      anchors.fill: parent
      radius: Style.cornerRadius
      color: root.alpha(root.foreground, 0.05)
    }

    Rectangle {
      anchors.left: parent.left
      anchors.top: parent.top
      anchors.bottom: parent.bottom
      width: parent.width * root.clamp(tableRow.fraction, 0, 1)
      radius: Style.cornerRadius
      color: root.alpha(root.foreground, 0.14)

      Behavior on width {
        NumberAnimation { duration: 160; easing.type: Easing.OutCubic }
      }
    }

    Text {
      id: rowLabel
      textFormat: Text.PlainText
      text: tableRow.label
      color: root.foreground
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
      font.bold: tableRow.emphasized
      elide: Text.ElideRight
      anchors.left: parent.left
      anchors.leftMargin: Style.space(8)
      anchors.right: rowValue.left
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
    }

    Text {
      id: rowValue
      textFormat: Text.PlainText
      text: tableRow.value
      color: tableRow.emphasized ? root.foreground : root.dim
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
      font.bold: true
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
    }

    MouseArea {
      id: rowHover
      anchors.fill: parent
      hoverEnabled: tableRow.tooltip !== ""
      acceptedButtons: Qt.NoButton
    }

    PanelToolTip {
      visible: rowHover.containsMouse && tableRow.tooltip !== ""
      text: tableRow.tooltip
      fontFamily: root.fontFamily
    }
  }

  Usage {
    id: usage
    settings: root.settings
    // While the panel is up, the snapshot is re-read on a short cycle: the
    // reset countdowns are drawn against the clock at render time, so a panel
    // left open would otherwise sit on the countdown it opened with.
    live: root.opened
  }

  IpcHandler {
    target: root.ipcTarget
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.toggle() }
    function refresh(): string { usage.refreshNow(); return "ok" }
    function next(): string { root.selectProvider(root.providerIndex + 1); return "ok" }
  }

  // The bar sizes each widget from its implicit size, so a root without one
  // occupies no slot at all - the button still answers IPC, it just never
  // paints. Unlike the built-in agents widget this never self-hides: the panel
  // is the only way to reach the provider toggles, so the icon has to survive
  // having nothing enabled.
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  readonly property string barGlyph: provider ? String(provider.glyph || "󰊚") : "󰊚"
  readonly property string barText: headline ? barGlyph + " " + headline.value : barGlyph

  // WidgetButton rather than BarIconButton: the latter pins itself to a
  // one-glyph icon slot, which clips the percentage off.
  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    // A vertical bar has no room for the percentage, but an empty label would
    // leave the button invisible and unclickable - so it keeps the glyph.
    text: root.vertical ? root.barGlyph : root.barText
    hasVisualContent: text !== ""
    active: root.alarming
    // Suppressed because the panel is the detail view: the hero already says
    // the provider and the plan, and a hover that repeats one click's worth of
    // information is noise. Same reasoning as the first-party widgets.
    tooltipText: ""

    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) usage.refreshNow()
      else if (buttonCode === Qt.MiddleButton) root.openProviderUrl("dashboard_url")
      else if (buttonCode === Qt.BackButton) root.openProviderUrl("status_url")
      else root.toggle()
    }

    // One notch is one provider. The accumulator keeps a touchpad's sub-notch
    // deltas from either being dropped or spinning through the whole list.
    onWheelMoved: function(delta) {
      var wheel = Util.wheelSteps(root.wheelAccumulator, delta)
      root.wheelAccumulator = wheel.remainder
      if (wheel.steps === 0) return
      root.selectProvider(root.providerIndex - wheel.steps)
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(380))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(680))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent

      onMoveRequested: function(dx, dy) {
        if (dx !== 0) root.selectProvider(root.providerIndex + dx)
        if (dy !== 0)
          panelFlick.contentY = root.clamp(panelFlick.contentY + dy * Style.space(56), 0,
                                           Math.max(0, panelFlick.contentHeight - panelFlick.height))
      }
      onActivateRequested: usage.refreshNow()
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "r" || t === "R") usage.refreshNow()
        else if (t === ",") root.openScreen(root.settingsOpen ? "panel" : "settings")
        // `.` next to `,`: h and l already move between providers, so the
        // obvious letter was taken.
        else if (t === ".") root.openScreen(root.historyOpen ? "panel" : "history")
        else if (root.historyOpen && /^[1-9]$/.test(t)) root.selectHistoryRange(Number(t) - 1)
        else if (root.settingsOpen && (t === "p" || t === "P")) root.cyclePin()
        else if (root.settingsOpen && (t === "y" || t === "Y")) usage.openSyncSetup()
        else if (root.settingsOpen && /^[1-9]$/.test(t)) root.toggleProviderAt(Number(t) - 1)
        else if (t === "u" || t === "U") root.openProviderUrl("dashboard_url")
        else if (t === "s" || t === "S") root.openProviderUrl("status_url")
      }

      Flickable {
        id: panelFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: column
          width: panelFlick.width
          spacing: Style.space(12)

          // ---------- Update banner ----------
          Rectangle {
            visible: !!usage.updateStatus && usage.updateStatus.available === true
            width: parent.width
            height: updateRow.implicitHeight + Style.space(16)
            radius: Style.cornerRadius
            color: root.alpha(Color.accent, 0.12)

            Row {
              id: updateRow
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              anchors.leftMargin: Style.space(10)
              anchors.rightMargin: Style.space(10)
              spacing: Style.space(10)

              Text {
                textFormat: Text.PlainText
                width: parent.width - updateButton.width - parent.spacing
                anchors.verticalCenter: parent.verticalCenter
                text: usage.updateStatus && usage.updateStatus.latest
                  ? "TokenGauge v" + usage.updateStatus.latest + " is available"
                  : "A TokenGauge update is available"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                wrapMode: Text.WordWrap
              }

              Button {
                id: updateButton
                anchors.verticalCenter: parent.verticalCenter
                text: usage.updating ? "Updating…" : "Update"
                bordered: true
                foreground: root.foreground
                accent: Color.accent
                fontFamily: root.fontFamily
                fontSize: Style.font.bodySmall
                onClicked: if (!usage.updating) usage.applyUpdate()
              }
            }
          }

          // ---------- Hero: brand mark · provider · plan ----------
          PanelHero {
            visible: !!root.provider
            width: parent.width
            title: root.provider ? String(root.provider.label || root.provider.provider) : ""
            meta: root.provider ? String(root.provider.plan_label || "") : ""
            foreground: root.foreground
            fontFamily: root.fontFamily

            // The bundled marks are monochrome on purpose - the popover
            // recolours them to its neutral foreground - so painting the brand
            // colour through the mark's own alpha is what makes them read as
            // logos here. Colourising instead would leave the white ones white.
            iconComponent: Component {
              Item {
                id: heroMark
                width: Style.font.display
                height: Style.font.display

                Image {
                  id: heroMarkImage
                  anchors.fill: parent
                  source: root.markSource(root.provider)
                  sourceSize.width: Style.font.display * 2
                  sourceSize.height: Style.font.display * 2
                  fillMode: Image.PreserveAspectFit
                  visible: false
                }

                Rectangle {
                  id: heroMarkInk
                  anchors.fill: parent
                  color: root.markColor
                  visible: false
                }

                MultiEffect {
                  anchors.fill: parent
                  source: heroMarkInk
                  visible: heroMarkImage.status === Image.Ready
                  maskEnabled: true
                  maskSource: heroMarkImage
                }

                // The bar glyph stands in while the logo is missing or failed.
                Text {
                  textFormat: Text.PlainText
                  anchors.centerIn: parent
                  visible: heroMarkImage.status !== Image.Ready
                  text: root.provider ? String(root.provider.glyph || "") : ""
                  color: root.markColor
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.display
                }
              }
            }

            trailingControl: Component {
              Row {
                spacing: Style.space(8)
                PanelActionButton {
                  iconText: "󰄨"
                  tooltipText: "History  ."
                  foreground: root.historyOpen ? Color.accent : root.dim
                  hoverColor: root.foreground
                  fontFamily: root.fontFamily
                  onClicked: root.openScreen(root.historyOpen ? "panel" : "history")
                }
                PanelActionButton {
                  iconText: "󰒓"
                  tooltipText: "Settings  ,"
                  foreground: root.settingsOpen ? Color.accent : root.dim
                  hoverColor: root.foreground
                  fontFamily: root.fontFamily
                  onClicked: root.openScreen(root.settingsOpen ? "panel" : "settings")
                }
              }
            }
          }

          // ---------- Error banner ----------
          Text {
            textFormat: Text.PlainText
            visible: root.errorText !== ""
            width: parent.width
            text: root.errorText
            color: root.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          // The provider is serving its last good numbers. Urgent rather than
          // dim for the same reason the banner is: the panel otherwise reads as
          // current.
          Text {
            textFormat: Text.PlainText
            visible: !!root.provider && root.provider.stale === true
            width: parent.width
            text: "Showing last known values"
            color: root.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }

          Text {
            textFormat: Text.PlainText
            visible: !root.provider && root.errorText === ""
            width: parent.width
            topPadding: Style.space(24)
            text: "No provider usage yet.\nEnable one in the settings below."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
            horizontalAlignment: Text.AlignHCenter
            wrapMode: Text.WordWrap
          }

          // ---------- Provider switch ----------
          ButtonGroup {
            visible: root.providers.length > 1
            width: parent.width
            foreground: root.foreground
            accent: Color.accent
            fontFamily: root.fontFamily
            options: {
              var out = []
              for (var i = 0; i < root.providers.length; i++) {
                var row = root.providers[i]
                out.push({ value: String(row.provider), label: String(row.label || row.provider) })
              }
              return out
            }
            value: root.provider ? String(root.provider.provider) : ""
            onChanged: function(value) { root.selectedProviderId = value }
          }

          // ---------- Panel sections ----------
          // The core hands over an ordered list of sections; a section knows
          // its own kind, and each kind has exactly one delegate below. A new
          // section in the core appears here with no edit to this file.
          Repeater {
            model: root.sections

            Column {
              required property var modelData
              width: parent ? parent.width : 0
              visible: root.showPanel
              height: visible ? implicitHeight : 0
              spacing: modelData.kind === "meters" ? Style.space(10)
                     : modelData.kind === "bars" ? Style.space(4)
                     : Style.space(6)

              PanelSectionHeader {
                width: parent.width
                text: modelData.title
                foreground: root.foreground
                fontFamily: root.fontFamily
              }

              Repeater {
                model: parent.modelData.kind === "meters" ? parent.modelData.rows : []

                MeterRow {
                  required property var modelData
                  label: modelData.label
                  value: modelData.value
                  fraction: Number(modelData.fraction) || 0
                  fill: root.toneColor(modelData.tone)
                  footnote: root.present(modelData.footnote)
                  badge: root.present(modelData.badge)
                  badgeColor: root.toneColor(modelData.badge_tone)
                  tooltip: root.present(modelData.tooltip)
                }
              }

              Repeater {
                model: parent.modelData.kind === "bars" ? parent.modelData.rows : []

                TableRow {
                  required property var modelData
                  label: modelData.label
                  value: root.joinValue(modelData)
                  fraction: Number(modelData.fraction) || 0
                  emphasized: modelData.emphasized === true
                  tooltip: root.present(modelData.tooltip)
                }
              }

              Repeater {
                model: parent.modelData.kind === "rows" ? parent.modelData.rows : []

                Item {
                  id: keyRow
                  required property var modelData
                  width: parent.width
                  height: keyRowLines.height

                  Column {
                    id: keyRowLines
                    width: parent.width
                    spacing: Style.space(2)

                    Item {
                      width: parent.width
                      height: rowLabelText.implicitHeight

                      Text {
                        id: rowLabelText
                        textFormat: Text.PlainText
                        anchors.left: parent.left
                        text: keyRow.modelData.label
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.body
                      }

                      Text {
                        textFormat: Text.PlainText
                        anchors.right: parent.right
                        text: keyRow.modelData.value
                        color: root.foreground
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.body
                      }
                    }

                    // A badge and a suffix beside the label leave a sentence
                    // fighting over what is left of a narrow popup, so they
                    // drop to a caption line. It tracks the right edge because
                    // that is where the figure it qualifies sits.
                    Item {
                      width: parent.width
                      height: keyRowCaption.height
                      visible: rowBadgeText.text !== "" || rowSuffixText.text !== ""

                      Row {
                        id: keyRowCaption
                        anchors.right: parent.right
                        spacing: Style.space(6)

                        Text {
                          id: rowBadgeText
                          textFormat: Text.PlainText
                          visible: text !== ""
                          text: root.present(keyRow.modelData.badge)
                          color: root.toneColor(keyRow.modelData.badge_tone)
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.caption
                        }

                        Text {
                          id: rowSuffixText
                          textFormat: Text.PlainText
                          visible: text !== ""
                          // The trend badge carries an arrow, which falls back
                          // to a font with taller metrics; top-aligned in a Row
                          // that lands the two strings on different baselines.
                          anchors.baseline: rowBadgeText.visible ? rowBadgeText.baseline : undefined
                          width: Math.min(implicitWidth, keyRow.width
                            - (rowBadgeText.visible ? rowBadgeText.width + keyRowCaption.spacing : 0))
                          elide: Text.ElideRight
                          // The separator divides a badge from a suffix, so a
                          // row with no badge must not open on one.
                          text: root.present(keyRow.modelData.suffix) === ""
                            ? ""
                            : (rowBadgeText.visible ? "\u00b7  " : "")
                              + root.present(keyRow.modelData.suffix)
                          color: root.dim
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.caption
                        }
                      }
                    }
                  }

                  // The suffix is the spec's ellipsized copy for surfaces that
                  // cannot wrap; the tooltip carries the whole sentence.
                  MouseArea {
                    id: keyHover
                    anchors.fill: parent
                    hoverEnabled: root.present(keyRow.modelData.tooltip) !== ""
                    acceptedButtons: Qt.NoButton
                  }

                  PanelToolTip {
                    visible: keyHover.containsMouse && root.present(keyRow.modelData.tooltip) !== ""
                    text: root.present(keyRow.modelData.tooltip)
                    fontFamily: root.fontFamily
                  }
                }
              }
            }
          }


          // ---------- History ----------
          // Every range is resolved in the core and carried on the row, so
          // this draws a chart and formats none of it.
          PanelSectionHeader {
            visible: root.historyOpen
            width: parent.width
            text: "HISTORY"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            visible: root.historyOpen
            height: visible ? implicitHeight : 0
            spacing: Style.space(8)

            Row {
              spacing: Style.space(10)
              Repeater {
                model: root.historyOpen && root.history ? root.history.series : []

                Text {
                  textFormat: Text.PlainText
                  required property int index
                  required property var modelData
                  text: modelData.label
                  color: index === root.historyRange ? root.foreground : root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                  MouseArea {
                    anchors.fill: parent
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.historyRange = parent.index
                  }
                }
              }
            }

            // No provider row at all, or a snapshot written by a daemon older
            // than this widget, which carries no `history` field. Without this
            // the screen hides the panel and draws nothing, which reads as the
            // widget having broken.
            Text {
              textFormat: Text.PlainText
              width: parent.width
              visible: root.historySeries === null
              text: "No history yet."
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
            }

            Text {
              textFormat: Text.PlainText
              width: parent.width
              visible: root.historySeries !== null
              text: root.historySeries
                ? root.historySeries.total_usd + "  ·  "
                  + root.historySeries.total_tokens + " tokens  ·  avg "
                  + root.historySeries.average_usd
                : ""
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.body
              elide: Text.ElideRight
            }

            Text {
              textFormat: Text.PlainText
              width: parent.width
              visible: root.historySeries !== null && root.historySeries.empty
              text: "Nothing spent in this range."
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
            }

            Canvas {
              id: historyChart
              width: parent.width
              // At 120 the chart left most of the popup empty under it.
              height: Style.space(170)
              visible: root.historySeries !== null && !root.historySeries.empty
              readonly property var points: root.historySeries && !root.historySeries.empty
                                            ? root.historySeries.points : []
              onPointsChanged: requestPaint()
              onPaint: {
                var ctx = getContext("2d")
                ctx.reset()
                var n = points.length
                if (n === 0)
                  return
                // Wide steps get a gap; ninety days of bars have none to spare.
                var gap = n <= 12 ? 2 : (n <= 31 ? 1 : 0)
                var w = Math.max(1, (width - gap * (n - 1)) / n)
                for (var i = 0; i < n; i++) {
                  var p = points[i]
                  // A floor of one pixel: a step that spent a little must never
                  // draw as a step that spent nothing.
                  var h = p.fraction > 0 ? Math.max(1, p.fraction * height) : 0
                  // The step in progress carries the dim tone *and* the
                  // reduced alpha below; taking both drew it as a grey ghost
                  // that read as chrome rather than as this month so far. The
                  // alpha is the signal, the fill stays the series colour.
                  ctx.fillStyle = String(p.tone) === "critical"
                    ? root.urgent : root.foreground
                  ctx.globalAlpha = p.partial ? 0.45 : 1.0
                  ctx.fillRect(i * (w + gap), height - h, w, h)
                }
              }
            }

            Item {
              width: parent.width
              visible: historyChart.visible
              height: firstStep.implicitHeight

              Text {
                textFormat: Text.PlainText
                id: firstStep
                anchors.left: parent.left
                text: root.historySeries && root.historySeries.points.length > 0
                  ? root.historySeries.points[0].full_label : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
              }
              Text {
                textFormat: Text.PlainText
                anchors.right: parent.right
                text: root.historySeries && root.historySeries.points.length > 0
                  ? root.historySeries.points[root.historySeries.points.length - 1].full_label
                  : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
              }
            }

            Text {
              textFormat: Text.PlainText
              width: parent.width
              visible: root.history !== null
              text: root.history
                ? [root.history.covers].concat(root.history.notes || []).join("  ·  ")
                : ""
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }

          // ---------- Settings ----------
          PanelSectionHeader {
            visible: root.settingsOpen
            width: parent.width
            text: "PROVIDERS"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            spacing: Style.space(6)
            visible: root.settingsOpen

            Repeater {
              model: usage.allProviders

              Item {
                required property var modelData
                width: parent.width
                height: providerToggle.implicitHeight

                required property int index

                Text {
                  textFormat: Text.PlainText
                  anchors.left: parent.left
                  anchors.verticalCenter: parent.verticalCenter
                  text: (parent.index < 9 ? (parent.index + 1) + "  " : "") + root.providerLabel(modelData)
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                ToggleSwitch {
                  id: providerToggle
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  checked: usage.enabled.indexOf(String(modelData)) >= 0
                  busy: usage.loading
                  foreground: root.foreground
                  accent: Color.accent
                  onToggled: usage.setProvider(String(modelData), !checked)
                }
              }
            }
          }

          PanelSectionHeader {
            visible: root.settingsOpen
            width: parent.width
            text: "PIN TO BAR"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          ButtonGroup {
            visible: root.settingsOpen
            width: parent.width
            foreground: root.foreground
            accent: Color.accent
            fontFamily: root.fontFamily
            options: {
              var out = [{ value: "highest", label: "Highest usage" }]
              for (var i = 0; i < usage.enabled.length; i++) {
                var id = String(usage.enabled[i])
                out.push({ value: id, label: root.providerLabel(id) })
              }
              return out
            }
            // The config spells "no pin" as an empty primary; the chip row
            // needs a value to select, so it shows as "Highest usage".
            value: root.present(usage.primary) === "" ? "highest" : String(usage.primary).toLowerCase()
            onChanged: function(value) { usage.setPrimary(value) }
          }

          Text {
            textFormat: Text.PlainText
            visible: root.settingsOpen
            width: parent.width
            topPadding: Style.space(4)
            text: "A number toggles a provider, p walks the pin, y sets up fleet sync so tokens and cost add up across your machines, u and s open the provider's usage dashboard and status page. Thresholds, refresh interval, and the click action live in ~/.config/tokengauge/config.toml."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          Text {
            textFormat: Text.PlainText
            visible: root.settingsOpen
            width: parent.width
            wrapMode: Text.WordWrap
            // Both, because they are installed separately: `--update` replaces
            // binaries, and until the plugin directory is reinstalled it keeps
            // driving whatever QML this box already had.
            text: {
              if (usage.version === "") return "TokenGauge"
              if (usage.widgetVersion === "" || usage.widgetVersion === usage.version)
                return "TokenGauge v" + usage.version
              return "widget v" + usage.widgetVersion + ", binary v" + usage.version
                + " — reinstall: tokengauge-waybar --install-frontend omarchy"
            }
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }

          // ---------- Errors ----------
          Repeater {
            model: usage.errors

            Text {
              textFormat: Text.PlainText
              required property var modelData
              width: parent.width
              text: String(modelData.provider || "") + ": " + String(modelData.message || "")
              color: root.urgent
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }
        }
      }
    }
  }
}

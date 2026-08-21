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
    return 0
  }
  readonly property var provider: providers.length > 0 ? providers[providerIndex] : null

  readonly property var limits: limitWindows(provider)
  readonly property var cost: provider && provider.cost ? provider.cost : null
  readonly property var headline: limits.length > 0 ? limits[0] : null
  readonly property bool alarming: !!headline && headline.percent >= 90

  property bool settingsOpen: false

  function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)) }
  function alpha(c, a) { return Qt.rgba(c.r, c.g, c.b, a) }

  // The snapshot spells "no value" as an em dash, which reads as content to
  // every `!== ""` check downstream.
  function present(value) {
    var text = String(value === null || value === undefined ? "" : value).trim()
    return text === "" || text === "-" || text === "—" ? "" : text
  }

  function selectProvider(index) {
    if (providers.length === 0) return
    var wrapped = ((index % providers.length) + providers.length) % providers.length
    selectedProviderId = String(providers[wrapped].provider)
  }

  // ---------------------------------------------------------------- limits

  // One record shape for every window the snapshot reports, so the meters
  // below never branch on which provider produced them.
  function limitWindow(title, used, reset, pace) {
    return {
      title: String(title || ""),
      percent: Math.max(0, Number(used) || 0),
      reset: root.present(reset),
      pace: root.present(pace)
    }
  }

  function limitWindows(row) {
    if (!row) return []
    var labels = Array.isArray(row.window_labels) ? row.window_labels : ["Session", "Weekly", "Tertiary"]
    var out = []

    if (row.session_used !== null && row.session_used !== undefined)
      out.push(limitWindow(labels[0], row.session_used, row.session_reset, row.session_pace))
    if (row.weekly_used !== null && row.weekly_used !== undefined)
      out.push(limitWindow(labels[1], row.weekly_used, row.weekly_reset, row.weekly_pace))
    if (row.tertiary_used !== null && row.tertiary_used !== undefined)
      out.push(limitWindow(labels[2], row.tertiary_used, row.tertiary_reset, null))

    // Anthropic's usage endpoint carries a slot for every limit kind it knows
    // about, including features an account does not have: those come back as
    // an explicit null, which the core turns into a 0% window with no reset so
    // the bar keeps its shape. On a dashboard that is a permanently empty row
    // ("Daily Routines", a model-scoped week never used), so drop the ones
    // carrying neither usage nor a reset. A window an account really has
    // always reports when it rolls over, even while it sits at 0%.
    var extra = Array.isArray(row.extra_windows) ? row.extra_windows : []
    for (var i = 0; i < extra.length; i++) {
      var entry = extra[i] || {}
      var window = limitWindow(entry.title, entry.used, entry.reset, null)
      if (window.percent === 0 && window.reset === "") continue
      out.push(window)
    }
    return out
  }

  // ---------------------------------------------------------- formatting

  function money(value) {
    var n = Number(value)
    if (!isFinite(n)) return "-"
    if (n >= 100) return "$" + Math.round(n)
    return "$" + n.toFixed(2)
  }

  function tokens(value) {
    var n = Number(value)
    if (!isFinite(n) || n === 0) return "0"
    if (n >= 1e9) return (n / 1e9).toFixed(1) + "B"
    if (n >= 1e6) return (n / 1e6).toFixed(1) + "M"
    if (n >= 1e3) return (n / 1e3).toFixed(1) + "K"
    return String(Math.round(n))
  }

  function exactTokens(value) {
    var n = Number(value) || 0
    return Math.round(n).toLocaleString(Qt.locale(), "f", 0)
  }

  // `claude-haiku-4-5-20251001` -> `Haiku 4.5`. Model ids separate version
  // parts with the same dash they use between words, so a plain dash-to-space
  // pass turns every point release into two numbers.
  function modelLabel(id) {
    var parts = String(id || "")
      .replace(/-\d{8}$/, "")
      .replace(/^(claude|anthropic|openai)-/, "")
      .split("-")

    var words = []
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i]
      var previous = words.length > 0 ? words[words.length - 1] : ""
      if (/^\d+$/.test(part) && /\d$/.test(previous)) words[words.length - 1] = previous + "." + part
      else words.push(part)
    }

    return words.map(function(word) {
      if (/^(gpt|glm|zai|ai)$/i.test(word)) return word.toUpperCase()
      return word.charAt(0).toUpperCase() + word.slice(1)
    }).join(" ")
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

  // ------------------------------------------------------------- history

  // ccusage omits days with no spend, so each entry is labelled from its own
  // date rather than by counting back from today - otherwise an idle day
  // silently shifts every label in the chart.
  readonly property string todayDate: Qt.formatDate(new Date(), "yyyy-MM-dd")

  readonly property var history: {
    if (!cost || !Array.isArray(cost.weekly_history)) return []
    var raw = cost.weekly_history
    var out = []
    for (var i = 0; i < raw.length; i++) {
      var entry = raw[i] || {}
      var date = String(entry.date || "")
      var parsed = Date.fromLocaleDateString(Qt.locale(), date, "yyyy-MM-dd")
      out.push({
        date: date,
        label: date === root.todayDate ? "Today" : Qt.formatDate(parsed, "ddd"),
        usd: Number(entry.usd) || 0,
        tokens: Number(entry.tokens) || 0,
        today: date === root.todayDate
      })
    }
    return out
  }
  readonly property real historyMax: {
    var max = 0
    for (var i = 0; i < history.length; i++) max = Math.max(max, history[i].tokens)
    return max
  }

  function dayTooltip(day) {
    if (!day) return ""
    var parsed = Date.fromLocaleDateString(Qt.locale(), day.date, "yyyy-MM-dd")
    return Qt.formatDate(parsed, "dddd d MMMM")
      + "\n" + root.exactTokens(day.tokens) + " tokens"
      + "\n$" + (Number(day.usd) || 0).toFixed(2)
  }

  readonly property var modelRows: {
    if (!cost || !Array.isArray(cost.monthly_models)) return []
    var raw = cost.monthly_models.slice()
    raw.sort(function(a, b) { return (Number(b.tokens) || 0) - (Number(a.tokens) || 0) })
    return raw
  }
  readonly property real modelMax: {
    var max = 0
    for (var i = 0; i < modelRows.length; i++) max = Math.max(max, Number(modelRows[i].tokens) || 0)
    return max
  }

  // The split only reaches the snapshot from ccusage 16+; older caches carry
  // zeroes, and a breakdown that adds up to nothing is worse than none.
  function modelTooltip(row) {
    if (!row) return ""
    var split = (Number(row.input_tokens) || 0) + (Number(row.output_tokens) || 0)
      + (Number(row.cache_creation_tokens) || 0) + (Number(row.cache_read_tokens) || 0)
    var lines = [String(row.model || "")]
    if (split > 0) {
      lines.push("Input   " + root.exactTokens(row.input_tokens))
      lines.push("Output  " + root.exactTokens(row.output_tokens))
      lines.push("Cache write  " + root.exactTokens(row.cache_creation_tokens))
      lines.push("Cache read   " + root.exactTokens(row.cache_read_tokens))
    } else {
      lines.push(root.exactTokens(row.tokens) + " tokens")
    }
    lines.push("$" + (Number(row.usd) || 0).toFixed(2) + " this month")
    return lines.join("\n")
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
        anchors.left: parent.left
        text: meterRow.label
        color: meterRow.emphasized ? root.foreground : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        font.bold: meterRow.emphasized
      }

      Text {
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

    Text {
      text: meterRow.footnote
      visible: text !== ""
      color: root.dim
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
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
      text: tableRow.label
      color: root.foreground
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
      elide: Text.ElideRight
      anchors.left: parent.left
      anchors.leftMargin: Style.space(8)
      anchors.right: rowValue.left
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
    }

    Text {
      id: rowValue
      text: tableRow.value
      color: root.dim
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

  readonly property string barText: {
    if (!root.provider) return "󰊚"
    var glyph = String(root.provider.glyph || "󰊚")
    return root.headline ? glyph + " " + root.headline.percent + "%" : glyph
  }

  // WidgetButton rather than BarIconButton: the latter pins itself to a
  // one-glyph icon slot, which clips the percentage off.
  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.vertical ? "" : root.barText
    labelVisible: !root.vertical
    hasVisualContent: text !== ""
    active: root.alarming
    tooltipText: root.provider ? String(root.provider.plan_label || root.provider.label || "") : "TokenGauge"

    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) usage.refreshNow()
      else if (buttonCode === Qt.MiddleButton) root.selectProvider(root.providerIndex + 1)
      else root.toggle()
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
        else if (t === "s" || t === "S") root.settingsOpen = !root.settingsOpen
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
              PanelActionButton {
                iconText: "󰒓"
                tooltipText: "Settings"
                foreground: root.settingsOpen ? Color.accent : root.dim
                hoverColor: root.foreground
                fontFamily: root.fontFamily
                onClicked: root.settingsOpen = !root.settingsOpen
              }
            }
          }

          Text {
            visible: !root.provider
            width: parent.width
            topPadding: Style.space(24)
            text: usage.lastError !== "" ? usage.lastError
                                         : "No provider usage yet.\nEnable one in the settings below."
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

          // ---------- Limits ----------
          PanelSectionHeader {
            visible: root.limits.length > 0 && !root.settingsOpen
            width: parent.width
            text: "LIMITS"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            spacing: Style.space(10)
            visible: root.limits.length > 0 && !root.settingsOpen

            Repeater {
              model: root.limits

              MeterRow {
                required property var modelData
                label: modelData.title
                value: modelData.percent + "%"
                fraction: modelData.percent / 100
                fill: modelData.percent >= 90 ? root.urgent : root.foreground
                footnote: {
                  var parts = []
                  if (modelData.reset !== "") parts.push("Resets " + modelData.reset)
                  if (modelData.pace !== "") parts.push(modelData.pace)
                  return parts.join("  ·  ")
                }
              }
            }
          }

          // ---------- Cost ----------
          PanelSectionHeader {
            visible: !!root.cost && !root.settingsOpen
            width: parent.width
            text: "COST"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            spacing: Style.space(6)
            visible: !!root.cost && !root.settingsOpen

            Repeater {
              model: {
                if (!root.cost) return []
                var burn = root.cost.burn_rate || null
                var out = [
                  { label: "Today", value: root.money(root.cost.today_usd) + "  ·  " + root.tokens(root.cost.today_tokens) },
                  { label: "This month", value: root.money(root.cost.monthly_usd) + "  ·  " + root.tokens(root.cost.monthly_tokens) }
                ]
                if (burn && Number(burn.cost_per_hour) > 0)
                  out.push({ label: "Burn rate", value: root.money(burn.cost_per_hour) + "/hr" })
                return out
              }

              Item {
                required property var modelData
                width: parent.width
                height: costLabel.implicitHeight

                Text {
                  id: costLabel
                  anchors.left: parent.left
                  text: modelData.label
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                Text {
                  anchors.right: parent.right
                  text: modelData.value
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }
              }
            }
          }

          // ---------- Tokens by day ----------
          PanelSectionHeader {
            visible: root.history.length > 0 && !root.settingsOpen
            width: parent.width
            text: "TOKENS BY DAY"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            spacing: Style.space(8)
            visible: root.history.length > 0 && !root.settingsOpen

            Repeater {
              model: root.history

              MeterRow {
                required property var modelData
                label: modelData.label
                value: root.tokens(modelData.tokens) + "  ·  " + root.money(modelData.usd)
                fraction: root.historyMax > 0 ? modelData.tokens / root.historyMax : 0
                emphasized: modelData.today
                tooltip: root.dayTooltip(modelData)
              }
            }
          }

          // ---------- Tokens by model ----------
          PanelSectionHeader {
            visible: root.modelRows.length > 0 && !root.settingsOpen
            width: parent.width
            // The cost layer is scoped to the calendar month, unlike the
            // built-in agents widget whose model table is all-time. Two panels
            // side by side with the same heading and different numbers is
            // worse than a longer heading.
            text: "TOKENS BY MODEL · THIS MONTH"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Column {
            width: parent.width
            spacing: Style.space(4)
            visible: root.modelRows.length > 0 && !root.settingsOpen

            Repeater {
              model: root.modelRows

              TableRow {
                required property var modelData
                label: root.modelLabel(modelData.model)
                value: root.tokens(modelData.tokens) + "  ·  " + root.money(modelData.usd)
                fraction: root.modelMax > 0 ? (Number(modelData.tokens) || 0) / root.modelMax : 0
                tooltip: root.modelTooltip(modelData)
              }
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

                Text {
                  anchors.left: parent.left
                  anchors.verticalCenter: parent.verticalCenter
                  text: root.providerLabel(modelData)
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
              var out = [{ value: "highest", label: "Highest" }]
              for (var i = 0; i < usage.enabled.length; i++) {
                var id = String(usage.enabled[i])
                out.push({ value: id, label: root.providerLabel(id) })
              }
              return out
            }
            // The config spells "no pin" as an empty primary; the chip row
            // needs a value to select, so it shows as "Highest".
            value: root.present(usage.primary) === "" ? "highest" : String(usage.primary).toLowerCase()
            onChanged: function(value) { usage.setPrimary(value) }
          }

          Text {
            visible: root.settingsOpen
            width: parent.width
            topPadding: Style.space(4)
            text: "Thresholds, refresh interval, and the click action live in ~/.config/tokengauge/config.toml."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          // ---------- Errors ----------
          Repeater {
            model: usage.errors

            Text {
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

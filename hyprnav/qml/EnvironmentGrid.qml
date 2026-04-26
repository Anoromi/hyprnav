import QtQuick
import QtQuick.Window
import QtQuick.Controls
import QtQuick.Layouts
import com.anoromi.hyprnav 1.0

Window {
    id: root
    width: Screen.width
    height: Screen.height
    visible: false
    property bool hasBeenVisible: false
    color: "transparent"
    title: "Hyprnav Environment Grid"
    property color overlayScrim: "#5807080a"
    property color panelBackground: "#17191c"
    property color panelBorder: "#2b3036"
    property color cardBackground: "#1d2126"
    property color cardSelectedBackground: "#262b31"
    property color cardBorder: "#30363d"
    property color cardSelectedBorder: "#8b949e"
    property color cardActiveBorder: "#55606c"
    property color previewBackground: "#111316"
    property color previewFallback: "#1a1d21"
    property color textPrimary: "#e6e9ed"
    property color textSecondary: "#a7afb8"
    property color textMuted: "#7d8791"
    property color badgeBackground: "#2b3138"
    property color badgeText: "#d9dfe5"
    property color scrollbarColor: "#69737d"

    Timer {
        interval: 1000
        repeat: true
        running: Controller.visible
        onTriggered: Controller.refreshSnapshotIfVisible()
    }

    Timer {
        interval: 10
        repeat: true
        running: true
        onTriggered: Controller.pumpSessionCommands()
    }

    Timer {
        id: openRefreshTimer
        interval: 120
        repeat: false
        onTriggered: Controller.refreshSnapshotIfStale()
    }

    Timer {
        id: navigationRefreshTimer
        interval: 220
        repeat: false
        onTriggered: Controller.refreshSnapshotIfVisible()
    }

    Timer {
        id: selectionRevealTimer
        interval: 1
        repeat: false
        onTriggered: {
            if (Controller.visible)
                flick.ensureSelectedVisible()
        }
    }

    Timer {
        id: warmStartTimer
        interval: 50
        repeat: false
        onTriggered: Controller.warmSnapshot()
    }

    FocusScope {
        id: keyHandler
        anchors.fill: parent
        visible: Controller.visible
        enabled: Controller.visible
        focus: Controller.visible

        Keys.onPressed: event => {
            if (event.key === Qt.Key_Right || event.key === Qt.Key_Tab) {
                if ((event.modifiers & Qt.ShiftModifier) && event.key === Qt.Key_Tab)
                    Controller.moveLeft()
                else
                    Controller.moveRight()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Left) {
                Controller.moveLeft()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Up) {
                Controller.moveUp()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Down) {
                Controller.moveDown()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                Controller.activateCurrent()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Escape) {
                Controller.cancel()
                event.accepted = true
            }
        }

        Keys.onReleased: event => {
            if (event.key === Qt.Key_Alt || event.key === Qt.Key_Meta || event.key === Qt.Key_Super_L || event.key === Qt.Key_Super_R) {
                Controller.handleModifierReleased()
                event.accepted = true
            }
        }

        Rectangle {
            anchors.fill: parent
            color: root.overlayScrim
        }

        Rectangle {
            id: dialog
            anchors.centerIn: parent
            property int cardWidth: 210
            property int cardHeight: 216
            property int horizontalSpacing: 1
            property int rowSpacing: 1
            property int rowHeaderHeight: 16
            property int rowHeaderGap: 1
            property int cellWidth: cardWidth + horizontalSpacing
            property int rowBlockHeight: rowHeaderHeight + rowHeaderGap + cardHeight
            property int contentColumnsWidth: Controller.gridColumnCount > 0
                ? cardWidth + Math.max(0, Controller.gridColumnCount - 1) * cellWidth
                : 0
            property int contentRowsHeight: Controller.gridRowCount > 0
                ? Controller.gridRowCount * rowBlockHeight + Math.max(0, Controller.gridRowCount - 1) * rowSpacing
                : 0
            property real trackpadScrollMultiplier: 1.8
            property real wheelScrollStep: rowBlockHeight + rowSpacing

            width: Math.min(root.width - 8, Math.max(192, contentColumnsWidth + 2))
            height: Math.min(root.height - 8, Math.max(192, contentRowsHeight + 2))
            radius: 0
            color: root.panelBackground
            border.width: 1
            border.color: root.panelBorder

            Flickable {
                id: flick
                anchors.fill: parent
                anchors.margins: 0
                contentWidth: Math.max(width, dialog.contentColumnsWidth)
                contentHeight: Math.max(height, dialog.contentRowsHeight)
                flickableDirection: Flickable.HorizontalAndVerticalFlick
                interactive: false
                boundsBehavior: Flickable.StopAtBounds
                clip: true

                ScrollBar.vertical: ScrollBar {
                    id: verticalBar
                    policy: flick.contentHeight > flick.height ? ScrollBar.AsNeeded : ScrollBar.AlwaysOff

                    contentItem: Rectangle {
                        implicitWidth: 6
                        implicitHeight: 48
                        radius: 3
                        color: root.scrollbarColor
                        opacity: verticalBar.active || verticalBar.hovered || flick.movingVertically ? 0.85 : 0.35
                    }

                    background: Item {
                        implicitWidth: 6
                    }
                }

                function clamp(value, minValue, maxValue) {
                    return Math.max(minValue, Math.min(maxValue, value))
                }

                function axisDelta(pixelDelta, angleDelta, fallbackStep) {
                    if (pixelDelta !== 0)
                        return pixelDelta * dialog.trackpadScrollMultiplier

                    if (angleDelta === 0)
                        return 0

                    return Math.sign(angleDelta) * fallbackStep
                }

                function scrollBy(rawDx, rawDy, shiftPressed) {
                    const dx = shiftPressed && rawDx === 0 ? rawDy : rawDx
                    const dy = shiftPressed && rawDx === 0 ? 0 : rawDy

                    contentX = clamp(
                        contentX - dx,
                        0,
                        Math.max(0, contentWidth - width)
                    )
                    contentY = clamp(
                        contentY - dy,
                        0,
                        Math.max(0, contentHeight - height)
                    )
                }

                function selectedDelegate() {
                    for (let i = 0; i < contentRoot.children.length; ++i) {
                        const child = contentRoot.children[i]
                        if (child && child.workspaceDelegate && child.workspaceSelected)
                            return child
                    }

                    return null
                }

                function ensureSelectedVisible() {
                    const selected = selectedDelegate()
                    if (!selected)
                        return

                    const left = selected.x
                    const right = left + selected.width
                    const top = selected.y + selected.cardTop
                    const bottom = top + dialog.cardHeight
                    const maxX = Math.max(0, contentWidth - width)
                    const maxY = Math.max(0, contentHeight - height)
                    let nextX = contentX
                    let nextY = contentY

                    if (left < nextX)
                        nextX = left
                    else if (right > nextX + width)
                        nextX = right - width

                    if (top < nextY)
                        nextY = top
                    else if (bottom > nextY + height)
                        nextY = bottom - height

                    contentX = clamp(nextX, 0, maxX)
                    contentY = clamp(nextY, 0, maxY)
                }

                MouseArea {
                    anchors.fill: parent
                    acceptedButtons: Qt.NoButton
                    propagateComposedEvents: true

                    onWheel: event => {
                        const rawDx = flick.axisDelta(event.pixelDelta.x, event.angleDelta.x, dialog.wheelScrollStep)
                        const rawDy = flick.axisDelta(event.pixelDelta.y, event.angleDelta.y, dialog.wheelScrollStep)
                        flick.scrollBy(rawDx, rawDy, (event.modifiers & Qt.ShiftModifier) !== 0)
                        event.accepted = true
                    }
                }

                Item {
                    id: contentRoot
                    width: parent.contentWidth
                    height: parent.contentHeight

                    Repeater {
                        model: Controller

                        delegate: Item {
                            required property int index
                            required property int rowIndex
                            required property int columnIndex
                            required property int slotIndex
                            required property int physicalWorkspaceId
                            required property string workspaceName
                            required property bool workspaceActive
                            required property url workspacePreview
                            required property string environmentDisplayId
                            required property string environmentTitle
                            required property bool environmentLocked
                            required property bool showEnvironmentLabel
                            property bool workspaceDelegate: true
                            readonly property bool workspaceSelected: Controller.currentIndex === index
                            property int cardTop: dialog.rowHeaderHeight + dialog.rowHeaderGap

                            x: columnIndex * dialog.cellWidth
                            y: rowIndex * (dialog.rowBlockHeight + dialog.rowSpacing)
                            width: dialog.cardWidth
                            height: dialog.rowBlockHeight

                            Label {
                                visible: showEnvironmentLabel
                                x: 0
                                y: 0
                                width: contentRoot.width
                                text: environmentTitle.length > 0 ? environmentTitle : environmentDisplayId
                                color: environmentLocked ? root.textPrimary : root.textSecondary
                                font.pixelSize: 12
                                font.family: "IBM Plex Sans"
                                font.weight: Font.Medium
                                elide: Text.ElideRight
                            }

                            Rectangle {
                                y: cardTop
                                width: parent.width
                                height: dialog.cardHeight
                                radius: 0
                                clip: true
                                color: workspaceSelected
                                    ? root.cardSelectedBackground
                                    : root.cardBackground
                                border.width: 1
                                border.color: workspaceSelected
                                    ? root.cardSelectedBorder
                                    : (workspaceActive ? root.cardActiveBorder : root.cardBorder)
                                scale: 1.0
                                opacity: 1.0

                                ColumnLayout {
                                    anchors.fill: parent
                                    anchors.margins: 0
                                    spacing: 1

                                    Rectangle {
                                        id: previewFrame
                                        Layout.fillWidth: true
                                        Layout.fillHeight: true
                                        Layout.minimumHeight: 0
                                        Layout.preferredHeight: 0
                                        radius: 0
                                        color: root.previewBackground
                                        clip: true
                                        property url observedPreview: workspacePreview
                                        property url displaySource: ""
                                        property url pendingSource: ""

                                        function clearPreviewState() {
                                            displaySource = ""
                                            pendingSource = ""
                                            loadingImage.source = ""
                                        }

                                        function syncPreviewSource() {
                                            const nextSource = workspacePreview.toString()
                                            const currentDisplay = displaySource.toString()
                                            const currentPending = pendingSource.toString()

                                            if (nextSource.length === 0) {
                                                clearPreviewState()
                                                return
                                            }

                                            if (nextSource === currentDisplay || nextSource === currentPending)
                                                return

                                            pendingSource = workspacePreview
                                            loadingImage.source = pendingSource
                                        }

                                        Image {
                                            id: displayImage
                                            anchors.fill: parent
                                            source: previewFrame.displaySource
                                            fillMode: Image.PreserveAspectCrop
                                            visible: previewFrame.displaySource.toString().length > 0
                                        }

                                        Image {
                                            id: loadingImage
                                            anchors.fill: parent
                                            visible: false
                                            asynchronous: true
                                            fillMode: Image.PreserveAspectCrop

                                            onStatusChanged: {
                                                if (status === Image.Ready && source.toString() === previewFrame.pendingSource.toString()) {
                                                    previewFrame.displaySource = previewFrame.pendingSource
                                                    previewFrame.pendingSource = ""
                                                    source = ""
                                                } else if (status === Image.Error && source.toString() === previewFrame.pendingSource.toString()) {
                                                    previewFrame.pendingSource = ""
                                                    source = ""
                                                }
                                            }
                                        }

                                        Rectangle {
                                            anchors.fill: parent
                                            visible: previewFrame.displaySource.toString().length === 0
                                            color: root.previewFallback
                                        }

                                        onObservedPreviewChanged: syncPreviewSource()

                                        Component.onCompleted: syncPreviewSource()

                                        Rectangle {
                                            anchors.left: parent.left
                                            anchors.leftMargin: 1
                                            anchors.top: parent.top
                                            anchors.topMargin: 1
                                            radius: 0
                                            color: root.badgeBackground
                                            implicitHeight: 18
                                            implicitWidth: slotBadgeLabel.implicitWidth + 6

                                            Label {
                                                id: slotBadgeLabel
                                                anchors.centerIn: parent
                                                text: slotIndex.toString()
                                                color: root.badgeText
                                                font.pixelSize: 10
                                                font.family: "IBM Plex Sans"
                                                font.weight: Font.Medium
                                            }
                                        }
                                    }

                                    Label {
                                        Layout.fillWidth: true
                                        Layout.preferredHeight: 24
                                        Layout.maximumHeight: 24
                                        text: workspaceName
                                        color: workspaceSelected ? root.textPrimary : root.textSecondary
                                        font.pixelSize: 13
                                        font.family: "IBM Plex Sans"
                                        font.weight: Font.Medium
                                        elide: Text.ElideRight
                                        maximumLineCount: 1
                                        wrapMode: Text.NoWrap
                                        clip: true
                                    }
                                }
                            }

                            MouseArea {
                                anchors.fill: parent
                                onClicked: Controller.activateWorkspaceAt(index)
                            }
                        }
                    }

                    Label {
                        anchors.centerIn: parent
                        visible: !Controller.hasSnapshot && Controller.loading
                        text: "Loading environments"
                        color: root.textMuted
                        font.pixelSize: 15
                        font.family: "IBM Plex Sans"
                    }

                    Label {
                        anchors.centerIn: parent
                        visible: Controller.hasSnapshot && Controller.gridRowCount === 0
                        text: "No mapped environments"
                        color: root.textMuted
                        font.pixelSize: 15
                        font.family: "IBM Plex Sans"
                    }
                }
            }
        }
    }

    Connections {
        target: Controller

        function onVisibleChanged() {
            if (Controller.visible) {
                hasBeenVisible = true
                keyHandler.forceActiveFocus()
                selectionRevealTimer.restart()
            } else if (hasBeenVisible && !Controller.residentMode) {
                Qt.quit()
            }
        }
    }

    Connections {
        target: Controller

        function onCurrentIndexChanged() {
            navigationRefreshTimer.restart()
            selectionRevealTimer.restart()
        }

        function onGridRowCountChanged() {
            selectionRevealTimer.restart()
        }

        function onGridColumnCountChanged() {
            selectionRevealTimer.restart()
        }

        function onOpenGenerationChanged() {
            openRefreshTimer.interval = Controller.hasSnapshot ? 120 : 24
            openRefreshTimer.restart()
            selectionRevealTimer.restart()
        }
    }

    Component.onCompleted: {
        Qt.callLater(() => {
            Controller.initializeIfNeeded()
            if (Controller.residentMode)
                warmStartTimer.start()
        })
    }
}

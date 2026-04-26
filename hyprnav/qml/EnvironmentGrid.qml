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
            color: "#40070a0f"
        }

        Rectangle {
            id: dialog
            anchors.centerIn: parent
            property int cardWidth: 210
            property int cardHeight: 216
            property int horizontalSpacing: 14
            property int rowSpacing: 12
            property int rowHeaderHeight: 20
            property int rowHeaderGap: 6
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

            width: Math.min(root.width - 80, Math.max(192, contentColumnsWidth + 44))
            height: Math.min(root.height - 80, Math.max(192, contentRowsHeight + 44))
            radius: 14
            color: "#e811171d"
            border.width: 1
            border.color: "#32404b"

            Flickable {
                id: flick
                anchors.fill: parent
                anchors.margins: 22
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
                        color: "#c7d2d8"
                        opacity: verticalBar.active || verticalBar.hovered || flick.movingVertically ? 0.95 : 0.55
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
                            required property string workspaceSubtitle
                            required property string workspaceAppClass
                            required property int workspaceWindowCount
                            required property bool workspaceActive
                            required property bool workspaceSelected
                            required property url workspacePreview
                            required property string environmentDisplayId
                            required property bool environmentLocked
                            required property bool showEnvironmentLabel
                            property bool workspaceDelegate: true
                            property int cardTop: dialog.rowHeaderHeight + dialog.rowHeaderGap

                            property string cardSummary: {
                                if (workspaceSubtitle.length > 0)
                                    return workspaceSubtitle

                                if (workspaceAppClass.length > 0)
                                    return workspaceAppClass

                                return "WS " + slotIndex
                            }

                            x: columnIndex * dialog.cellWidth
                            y: rowIndex * (dialog.rowBlockHeight + dialog.rowSpacing)
                            width: dialog.cardWidth
                            height: dialog.rowBlockHeight

                            Label {
                                visible: showEnvironmentLabel
                                x: 0
                                y: 0
                                width: contentRoot.width
                                text: environmentDisplayId
                                color: environmentLocked ? "#efe6d4" : "#c7d2d8"
                                font.pixelSize: 14
                                font.family: "IBM Plex Sans"
                                font.weight: Font.Medium
                                elide: Text.ElideRight
                            }

                            Rectangle {
                                y: cardTop
                                width: parent.width
                                height: dialog.cardHeight
                                radius: 12
                                clip: true
                                color: workspaceSelected ? "#efe6d4" : "#111920"
                                border.width: workspaceActive ? 2 : 1
                                border.color: workspaceSelected ? "#111920" : (workspaceActive ? "#caa45d" : "#27323c")
                                scale: workspaceSelected ? 1.0 : 0.96
                                opacity: workspaceSelected ? 1.0 : 0.9

                                ColumnLayout {
                                    anchors.fill: parent
                                    anchors.margins: 12
                                    spacing: 10

                                    Rectangle {
                                        Layout.fillWidth: true
                                        Layout.preferredHeight: 132
                                        radius: 10
                                        color: workspaceSelected ? "#d9ded6" : "#0a1015"
                                        clip: true

                                        Image {
                                            id: previewImage
                                            anchors.fill: parent
                                            source: workspacePreview
                                            fillMode: Image.PreserveAspectCrop
                                            asynchronous: true
                                            visible: source.toString().length > 0
                                        }

                                        Rectangle {
                                            anchors.fill: parent
                                            visible: !previewImage.visible
                                            color: workspaceSelected ? "#d2d8d2" : "#141d24"
                                        }

                                        Label {
                                            anchors.horizontalCenter: parent.horizontalCenter
                                            anchors.bottom: parent.bottom
                                            anchors.bottomMargin: 10
                                            text: "WS " + slotIndex
                                            color: workspaceSelected ? "#111920" : "#ecf2ef"
                                            font.pixelSize: 22
                                            font.family: "IBM Plex Sans"
                                            font.weight: Font.DemiBold
                                        }
                                    }

                                    Label {
                                        Layout.fillWidth: true
                                        Layout.preferredHeight: 38
                                        Layout.maximumHeight: 38
                                        text: cardSummary
                                        color: workspaceSelected ? "#1d2a33" : "#c7d2d8"
                                        font.pixelSize: 14
                                        font.family: "IBM Plex Sans"
                                        font.weight: Font.Medium
                                        elide: Text.ElideRight
                                        maximumLineCount: 2
                                        wrapMode: Text.Wrap
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
                        color: "#9fb0b8"
                        font.pixelSize: 15
                        font.family: "IBM Plex Sans"
                    }

                    Label {
                        anchors.centerIn: parent
                        visible: Controller.hasSnapshot && Controller.gridRowCount === 0
                        text: "No mapped environments"
                        color: "#9fb0b8"
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
                Qt.callLater(() => {
                    keyHandler.forceActiveFocus()
                    flick.ensureSelectedVisible()
                })
            } else if (hasBeenVisible && !Controller.residentMode) {
                Qt.quit()
            }
        }
    }

    Connections {
        target: Controller

        function onCurrentIndexChanged() {
            Qt.callLater(() => flick.ensureSelectedVisible())
        }

        function onGridRowCountChanged() {
            Qt.callLater(() => flick.ensureSelectedVisible())
        }

        function onGridColumnCountChanged() {
            Qt.callLater(() => flick.ensureSelectedVisible())
        }

        function onOpenGenerationChanged() {
            openRefreshTimer.interval = Controller.hasSnapshot ? 120 : 24
            openRefreshTimer.restart()
            Qt.callLater(() => flick.ensureSelectedVisible())
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

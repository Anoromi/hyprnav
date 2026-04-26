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
    title: "Hyprnav"

    property int maxGridColumns: 6
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
        running: root.visible
        onTriggered: Controller.refreshSnapshotIfVisible()
    }

    Timer {
        id: navigationRefreshTimer
        interval: 220
        repeat: false
        onTriggered: Controller.refreshSnapshotIfVisible()
    }

    Timer {
        interval: 40
        repeat: true
        running: root.visible
        onTriggered: Controller.pumpSessionCommands()
    }

    FocusScope {
        id: keyHandler
        anchors.fill: parent
        focus: true

        Keys.onPressed: event => {
            if (event.key === Qt.Key_Tab && (event.modifiers & (Qt.AltModifier | Qt.MetaModifier))) {
                if (event.modifiers & Qt.ShiftModifier)
                    Controller.selectPrevious()
                else
                    Controller.selectNext()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Right || event.key === Qt.Key_Down) {
                Controller.selectNext()
                event.accepted = true
                return
            }

            if (event.key === Qt.Key_Left || event.key === Qt.Key_Up) {
                Controller.selectPrevious()
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
            property int verticalSpacing: 1
            property int cellWidth: cardWidth + horizontalSpacing
            property int cellHeight: cardHeight + verticalSpacing
            property int workspaceCount: Math.max(0, workspaceGrid.count)
            property int columnCount: Math.max(1, Math.min(workspaceCount, root.maxGridColumns))
            property int rowCount: workspaceCount > 0 ? Math.ceil(workspaceCount / columnCount) : 1

            width: Math.min(
                root.width - 8,
                columnCount * cellWidth + 2
            )
            height: Math.max(
                184,
                rowCount * cellHeight + 2
            )
            radius: 0
            color: root.panelBackground
            border.width: 1
            border.color: root.panelBorder

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 0
                spacing: 0

                GridView {
                    id: workspaceGrid
                    Layout.alignment: Qt.AlignHCenter | Qt.AlignVCenter
                    Layout.preferredWidth: dialog.width - 2
                    Layout.preferredHeight: dialog.height - 2
                    Layout.maximumWidth: Layout.preferredWidth
                    Layout.maximumHeight: Layout.preferredHeight
                    model: Controller
                    cellWidth: dialog.cellWidth
                    cellHeight: dialog.cellHeight
                    flow: GridView.FlowLeftToRight
                    interactive: false
                    clip: true

                    delegate: Item {
                        required property int index
                        required property int workspaceId
                        required property int slotIndex
                        required property string workspaceName
                        required property bool workspaceActive
                        required property url workspacePreview
                        readonly property bool workspaceSelected: Controller.currentIndex === index

                        width: dialog.cardWidth + dialog.horizontalSpacing
                        height: dialog.cardHeight + dialog.verticalSpacing

                        Rectangle {
                            x: 0
                            y: 0
                            width: dialog.cardWidth
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
                                        visible: slotIndex > 0
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

                                    Rectangle {
                                        anchors.right: parent.right
                                        anchors.rightMargin: 1
                                        anchors.top: parent.top
                                        anchors.topMargin: 1
                                        visible: workspaceActive
                                        radius: 0
                                        color: root.badgeBackground
                                        implicitHeight: 18
                                        implicitWidth: currentLabel.implicitWidth + 6

                                        Label {
                                            id: currentLabel
                                            anchors.centerIn: parent
                                            text: "Current"
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
                            hoverEnabled: true
                            onClicked: Controller.activateWorkspaceAt(index)
                        }
                    }
                }

                Label {
                    Layout.alignment: Qt.AlignHCenter
                    visible: workspaceGrid.count === 0
                    text: "No populated workspaces"
                    color: root.textMuted
                    font.pixelSize: 15
                    font.family: "IBM Plex Sans"
                }
            }
        }

    }

    Connections {
        target: Controller

        function onCurrentIndexChanged() {
            navigationRefreshTimer.restart()
        }

        function onVisibleChanged() {
            if (Controller.visible)
                root.show()
            else
                root.hide()
        }
    }

    onVisibleChanged: {
        if (visible) {
            hasBeenVisible = true
            keyHandler.forceActiveFocus()
        } else if (hasBeenVisible) {
            Qt.quit()
        }
    }

    Component.onCompleted: {
        Qt.callLater(() => Controller.initializeIfNeeded())
    }
}

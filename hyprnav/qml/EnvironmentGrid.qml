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
        running: false
        onTriggered: Controller.refreshSnapshotIfVisible()
    }

    FocusScope {
        id: keyHandler
        anchors.fill: parent
        focus: true

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
            property int rowLabelWidth: 184
            property int cardWidth: 210
            property int cardHeight: 216
            property int horizontalSpacing: 14
            property int verticalSpacing: 18
            property int cellWidth: cardWidth + horizontalSpacing
            property int rowHeight: cardHeight + verticalSpacing

            width: Math.min(root.width - 80, rowLabelWidth + Controller.gridColumnCount * cellWidth + 44)
            height: Math.min(root.height - 80, Math.max(192, Controller.gridRowCount * rowHeight + 44))
            radius: 14
            color: "#e811171d"
            border.width: 1
            border.color: "#32404b"

            Flickable {
                anchors.fill: parent
                anchors.margins: 22
                contentWidth: Math.max(width, dialog.rowLabelWidth + Controller.gridColumnCount * dialog.cellWidth)
                contentHeight: Math.max(height, Controller.gridRowCount * dialog.rowHeight)
                clip: true

                Item {
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

                            property string cardSummary: {
                                if (workspaceSubtitle.length > 0)
                                    return workspaceSubtitle

                                if (workspaceAppClass.length > 0)
                                    return workspaceAppClass

                                return "WS " + slotIndex
                            }

                            x: dialog.rowLabelWidth + columnIndex * dialog.cellWidth
                            y: rowIndex * dialog.rowHeight
                            width: dialog.cardWidth
                            height: dialog.cardHeight

                            Label {
                                visible: showEnvironmentLabel
                                x: -dialog.rowLabelWidth + 8
                                y: 58
                                width: dialog.rowLabelWidth - 22
                                text: environmentDisplayId
                                color: environmentLocked ? "#efe6d4" : "#c7d2d8"
                                font.pixelSize: 17
                                font.family: "IBM Plex Sans"
                                font.weight: Font.DemiBold
                                elide: Text.ElideRight
                            }

                            Rectangle {
                                anchors.fill: parent
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
                        visible: Controller.gridRowCount === 0
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

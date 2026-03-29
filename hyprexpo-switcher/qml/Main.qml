import QtQuick
import QtQuick.Window
import QtQuick.Controls
import QtQuick.Layouts
import com.anoromi.hyprexpo.switcher 1.0

Window {
    id: root
    width: Screen.width
    height: Screen.height
    visible: Controller.visible
    color: "transparent"
    title: "Hyprexpo Switcher"
    flags: Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint

    property int maxGridColumns: 6

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

        Timer {
            interval: 40
            running: true
            repeat: true
            onTriggered: Controller.pumpBackendEvents()
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
            property int verticalSpacing: 16
            property int cellWidth: cardWidth + horizontalSpacing
            property int cellHeight: cardHeight + verticalSpacing
            property int workspaceCount: Math.max(0, workspaceGrid.count)
            property int columnCount: Math.max(1, Math.min(workspaceCount, root.maxGridColumns))
            property int rowCount: workspaceCount > 0 ? Math.ceil(workspaceCount / columnCount) : 1

            width: Math.min(
                root.width - 72,
                columnCount * cellWidth + 44
            )
            height: Math.max(
                184,
                rowCount * cellHeight + 44
            )
            radius: 16
            color: "#e811171d"
            border.width: 1
            border.color: "#32404b"

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 22
                spacing: 0

                GridView {
                    id: workspaceGrid
                    Layout.alignment: Qt.AlignHCenter | Qt.AlignVCenter
                    Layout.preferredWidth: dialog.width - 44
                    Layout.preferredHeight: dialog.height - 44
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
                        required property string workspaceName
                        required property string workspaceSubtitle
                        required property string workspaceAppClass
                        required property int workspaceWindowCount
                        required property bool workspaceActive
                        required property bool workspaceSelected
                        required property url workspacePreview

                        property string cardSummary: {
                            if (workspaceSubtitle.length > 0)
                                return workspaceSubtitle

                            if (workspaceAppClass.length > 0)
                                return workspaceAppClass

                            return "WS " + workspaceName
                        }

                        width: dialog.cardWidth + dialog.horizontalSpacing
                        height: dialog.cardHeight + dialog.verticalSpacing

                        Rectangle {
                            anchors.horizontalCenter: parent.horizontalCenter
                            anchors.verticalCenter: parent.verticalCenter
                            width: dialog.cardWidth
                            height: dialog.cardHeight
                            radius: 12
                            clip: true
                            color: workspaceSelected ? "#efe6d4" : "#111920"
                            border.width: workspaceActive ? 2 : 1
                            border.color: workspaceSelected ? "#111920" : (workspaceActive ? "#caa45d" : "#27323c")
                            scale: workspaceSelected ? 1.0 : 0.96
                            opacity: workspaceSelected ? 1.0 : 0.9

                            Behavior on scale {
                                NumberAnimation {
                                    duration: 110
                                    easing.type: Easing.OutCubic
                                }
                            }

                            Behavior on opacity {
                                NumberAnimation {
                                    duration: 110
                                    easing.type: Easing.OutCubic
                                }
                            }

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

                                    Rectangle {
                                        anchors.left: parent.left
                                        anchors.leftMargin: 10
                                        anchors.top: parent.top
                                        anchors.topMargin: 10
                                        visible: workspaceActive
                                        radius: 8
                                        color: workspaceSelected ? "#111920" : "#caa45d"
                                        implicitHeight: 24
                                        implicitWidth: currentLabel.implicitWidth + 14

                                        Label {
                                            id: currentLabel
                                            anchors.centerIn: parent
                                            text: "Current"
                                            color: workspaceSelected ? "#efe6d4" : "#111920"
                                            font.pixelSize: 11
                                            font.family: "IBM Plex Sans"
                                            font.weight: Font.Medium
                                        }
                                    }

                                    Label {
                                        anchors.horizontalCenter: parent.horizontalCenter
                                        anchors.bottom: parent.bottom
                                        anchors.bottomMargin: 10
                                        text: "WS " + workspaceName
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
                            hoverEnabled: true
                            onClicked: Controller.activateWorkspaceAt(index)
                        }
                    }
                }

                Label {
                    Layout.alignment: Qt.AlignHCenter
                    visible: workspaceGrid.count === 0
                    text: "No populated workspaces"
                    color: "#9fb0b8"
                    font.pixelSize: 15
                    font.family: "IBM Plex Sans"
                }
            }
        }

    }

    onVisibleChanged: {
        if (visible)
            keyHandler.forceActiveFocus()
    }
}

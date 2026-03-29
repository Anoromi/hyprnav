#include "AppController.hpp"

#include "workspace_utils.hpp"

#include "../../hyprexpo/common.hpp"

#include <QCoreApplication>
#include <QDir>
#include <QEvent>
#include <QFileInfo>
#include <QGuiApplication>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QProcess>
#include <QProcessEnvironment>
#include <QQuickWindow>
#include <QScreen>

#include <algorithm>
#include <filesystem>

#ifdef WAYLAND_LAYER_SHELL
#include <LayerShellQt/window.h>
#endif

namespace {

bool isSwitcherModifier(Qt::KeyboardModifiers modifiers) {
    return modifiers.testFlag(Qt::AltModifier) || modifiers.testFlag(Qt::MetaModifier);
}

bool isModifierReleaseKey(int key) {
    return key == Qt::Key_Alt || key == Qt::Key_Meta || key == Qt::Key_Super_L || key == Qt::Key_Super_R;
}

int parseLeadingWorkspaceID(const QByteArray& payload) {
    const auto first = payload.split(',').value(0).trimmed();
    bool       ok    = false;
    const auto id    = first.toInt(&ok);
    return ok && id > 0 ? id : -1;
}

int parseFocusedMonitorWorkspaceID(const QByteArray& payload) {
    const auto parts = payload.split(',');
    if (parts.size() < 2)
        return -1;

    bool       ok = false;
    const auto id = parts[1].trimmed().toInt(&ok);
    return ok && id > 0 ? id : -1;
}

QVector<int> toQVector(const std::vector<int>& values) {
    QVector<int> out;
    out.reserve(static_cast<int>(values.size()));

    for (const auto value : values) {
        out.push_back(value);
    }

    return out;
}

}

AppController::AppController(QObject* parent) : QObject(parent), m_workspaces(this) {
    m_stateRefreshTimer.setSingleShot(true);
    connect(&m_stateRefreshTimer, &QTimer::timeout, this, &AppController::refreshWorkspaceState);

    m_reconnectTimer.setSingleShot(true);
    connect(&m_reconnectTimer, &QTimer::timeout, this, &AppController::reconnectBackgroundSockets);

    connect(&m_controlServer, &QLocalServer::newConnection, this, &AppController::handleControlConnection);

    connect(&m_previewSocket, &QLocalSocket::readyRead, this, &AppController::handlePreviewMessages);
    connect(&m_previewSocket, &QLocalSocket::connected, this, [this]() {
        bootstrapCachedPreviews();
        backfillMissingPreviews();
    });
    connect(&m_previewSocket, &QLocalSocket::disconnected, this, [this]() { scheduleReconnect(); });

    connect(&m_hyprEventSocket, &QLocalSocket::readyRead, this, &AppController::handleHyprEventMessages);
    connect(&m_hyprEventSocket, &QLocalSocket::connected, this, [this]() { scheduleStateRefresh(1); });
    connect(&m_hyprEventSocket, &QLocalSocket::disconnected, this, [this]() {
        m_instanceSignature.clear();
        scheduleReconnect();
    });
}

WorkspaceModel* AppController::workspaces() {
    return &m_workspaces;
}

bool AppController::initialize() {
    refreshRuntimePaths(true);
    if (!startControlServer())
        return false;

    refreshWorkspaceState();
    connectPreviewSocket();
    connectHyprEventSocket();
    return true;
}

void AppController::configureWindow(QQuickWindow* window) {
    if (!window)
        return;

    m_window = window;
    m_window->installEventFilter(this);
    m_window->setColor(Qt::transparent);
    m_window->setVisibility(QWindow::Hidden);

    if (auto* screen = QGuiApplication::primaryScreen())
        m_window->setGeometry(screen->geometry());

#ifdef WAYLAND_LAYER_SHELL
    if (QGuiApplication::platformName() == "wayland") {
        namespace Shell = LayerShellQt;
        if (auto* layerWindow = Shell::Window::get(m_window)) {
            layerWindow->setAnchors(Shell::Window::AnchorNone);
            layerWindow->setLayer(Shell::Window::LayerTop);
            layerWindow->setKeyboardInteractivity(Shell::Window::KeyboardInteractivityExclusive);
            layerWindow->setScreenConfiguration(Shell::Window::ScreenConfiguration::ScreenFromCompositor);
            layerWindow->setScope(QStringLiteral("hyprexpo-switcher"));
            layerWindow->setMargins({});
            layerWindow->setExclusiveZone(-1);
            return;
        }

        qWarning("hyprexpo-switcher: LayerShellQt was compiled in but no layer-shell window could be created; falling back to a normal window");
    }
#endif

    m_window->setFlags(Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint);
}

bool AppController::eventFilter(QObject* watched, QEvent* event) {
    Q_UNUSED(watched)

    if (!m_visible)
        return QObject::eventFilter(watched, event);

    if (event->type() == QEvent::KeyPress) {
        auto* const keyEvent = static_cast<QKeyEvent*>(event);

        if (keyEvent->key() == Qt::Key_Tab && isSwitcherModifier(keyEvent->modifiers())) {
            if (keyEvent->modifiers() & Qt::ShiftModifier)
                selectPrevious();
            else
                selectNext();
            event->accept();
            return true;
        }

        if (keyEvent->key() == Qt::Key_Right || keyEvent->key() == Qt::Key_Down) {
            selectNext();
            event->accept();
            return true;
        }

        if (keyEvent->key() == Qt::Key_Left || keyEvent->key() == Qt::Key_Up) {
            selectPrevious();
            event->accept();
            return true;
        }

        if (keyEvent->key() == Qt::Key_Return || keyEvent->key() == Qt::Key_Enter) {
            activateCurrent();
            event->accept();
            return true;
        }

        if (keyEvent->key() == Qt::Key_Escape) {
            cancel();
            event->accept();
            return true;
        }
    } else if (event->type() == QEvent::KeyRelease) {
        auto* const keyEvent = static_cast<QKeyEvent*>(event);
        if (isModifierReleaseKey(keyEvent->key())) {
            handleModifierReleased();
            event->accept();
            return true;
        }
    }

    return QObject::eventFilter(watched, event);
}

QString AppController::runtimeDir() const {
    const auto envValue = qEnvironmentVariable("XDG_RUNTIME_DIR");
    return envValue.isEmpty() ? QStringLiteral("/run/user/%1").arg(getuid()) : envValue;
}

void AppController::refreshRuntimePaths(bool forceRebindServer) {
    const auto runtimeBytes   = runtimeDir().toUtf8();
    const auto currentBytes   = m_instanceSignature.toUtf8();
    const auto envBytes       = qgetenv("HYPRLAND_INSTANCE_SIGNATURE");
    const auto hint           = !envBytes.isEmpty() ? envBytes.constData() : currentBytes.constData();
    const auto discoveredHIS  = QString::fromStdString(hyprexpo::discoverHyprlandInstanceSignature(runtimeBytes.constData(), hint));
    const auto resolvedHIS    = discoveredHIS.isEmpty() ? m_instanceSignature : discoveredHIS;
    const auto previewPath    = QString::fromStdString(hyprexpo::socketPath(runtimeBytes.constData(), resolvedHIS.toUtf8().constData()).string());
    const auto switcherPath   = QString::fromStdString(hyprexpo::switcherSocketPath(runtimeBytes.constData(), resolvedHIS.toUtf8().constData()).string());
    const auto eventSocket    = QString::fromStdString(hyprexpo::hyprlandEventSocketPath(runtimeBytes.constData(), resolvedHIS.toUtf8().constData()).string());

    const auto oldSwitcherPath = m_switcherSocketPath;

    m_instanceSignature = resolvedHIS;
    m_runtimePath       = runtimeDir();
    m_previewSocketPath = previewPath;
    m_switcherSocketPath = switcherPath;
    m_hyprEventSocketPath = eventSocket;

    if ((forceRebindServer || oldSwitcherPath != m_switcherSocketPath) && !m_switcherSocketPath.isEmpty()) {
        if (!oldSwitcherPath.isEmpty()) {
            m_controlServer.close();
            QLocalServer::removeServer(oldSwitcherPath);
        }

        if (m_controlServer.isListening())
            m_controlServer.close();

        startControlServer();
    }
}

bool AppController::startControlServer() {
    if (m_switcherSocketPath.isEmpty())
        return false;

    const auto socketInfo = QFileInfo(m_switcherSocketPath);
    QDir().mkpath(socketInfo.path());
    QLocalServer::removeServer(m_switcherSocketPath);

    if (m_controlServer.isListening())
        m_controlServer.close();

    return m_controlServer.listen(m_switcherSocketPath);
}

void AppController::handleControlConnection() {
    while (auto* socket = m_controlServer.nextPendingConnection()) {
        m_controlBuffers.insert(socket, {});
        connect(socket, &QLocalSocket::readyRead, this, [this, socket]() {
            auto& buffer = m_controlBuffers[socket];
            buffer += socket->readAll();

            qsizetype newline = -1;
            while ((newline = buffer.indexOf('\n')) >= 0) {
                auto line = buffer.left(newline);
                buffer.remove(0, newline + 1);

                if (!line.isEmpty() && line.endsWith('\r'))
                    line.chop(1);

                handleControlLine(socket, line);
            }
        });
        connect(socket, &QLocalSocket::disconnected, this, [this, socket]() {
            m_controlBuffers.remove(socket);
            socket->deleteLater();
        });
    }
}

void AppController::handleControlLine(QLocalSocket* socket, const QByteArray& line) {
    std::string error;
    const auto  command = hyprexpo::parseSwitcherCommand(std::string_view(line.constData(), static_cast<size_t>(line.size())), error);
    if (!command.has_value()) {
        socket->write(QByteArrayLiteral("ERROR "));
        socket->write(QByteArray::fromStdString(error));
        socket->write(QByteArrayLiteral("\n"));
        socket->flush();
        socket->disconnectFromServer();
        return;
    }

    switch (command->command) {
        case hyprexpo::eSwitcherCommand::SHOW_FORWARD: showSwitcher(false); break;
        case hyprexpo::eSwitcherCommand::SHOW_REVERSE: showSwitcher(true); break;
        case hyprexpo::eSwitcherCommand::HIDE: hideSwitcher(); break;
        case hyprexpo::eSwitcherCommand::PING: break;
    }

    socket->write(QByteArrayLiteral("OK\n"));
    socket->flush();
    socket->disconnectFromServer();
}

QByteArray AppController::runHyprctlJSON(const QStringList& args) {
    refreshRuntimePaths();

    QProcess process;
    auto     environment = QProcessEnvironment::systemEnvironment();
    if (!m_instanceSignature.isEmpty())
        environment.insert(QStringLiteral("HYPRLAND_INSTANCE_SIGNATURE"), m_instanceSignature);
    process.setProcessEnvironment(environment);
    process.start(QStringLiteral("hyprctl"), args);
    if (!process.waitForFinished(2000))
        return {};

    if (process.exitStatus() != QProcess::NormalExit || process.exitCode() != 0)
        return {};

    return process.readAllStandardOutput();
}

void AppController::refreshWorkspaceState() {
    const auto monitors   = runHyprctlJSON({QStringLiteral("-j"), QStringLiteral("monitors")});
    const auto workspaces = runHyprctlJSON({QStringLiteral("-j"), QStringLiteral("workspaces")});
    const auto clients    = runHyprctlJSON({QStringLiteral("-j"), QStringLiteral("clients")});

    applyWorkspaceState(buildWorkspaceDescriptors(monitors, workspaces, clients));
}

void AppController::applyWorkspaceState(QVector<SWorkspaceDescriptor> items) {
    const auto selectedWorkspaceID = m_workspaces.currentWorkspaceID();
    int        newActiveWorkspaceID = -1;

    for (const auto& item : items) {
        if (item.active) {
            newActiveWorkspaceID = item.id;
            noteWorkspaceActivated(item.id);
            break;
        }
    }

    sortWorkspacesForSwitcher(items, std::vector<int>(m_mruWorkspaceIDs.begin(), m_mruWorkspaceIDs.end()));

    m_workspaces.setWorkspaces(items);

    if (m_visible) {
        const auto selectedIndex = m_workspaces.indexOfWorkspace(selectedWorkspaceID);
        m_workspaces.setCurrentIndex(selectedIndex >= 0 ? selectedIndex : initialSelectionIndex(items, false));
    } else
        m_workspaces.setCurrentIndex(-1);

    if (newActiveWorkspaceID > 0)
        m_activeWorkspaceID = newActiveWorkspaceID;

    bootstrapCachedPreviews();
    backfillMissingPreviews();
}

void AppController::scheduleStateRefresh(int delayMs) {
    if (!m_stateRefreshTimer.isActive() || delayMs < m_stateRefreshTimer.remainingTime())
        m_stateRefreshTimer.start(std::max(delayMs, 1));
}

void AppController::connectPreviewSocket() {
    refreshRuntimePaths();

    if (m_previewSocketPath.isEmpty() || m_previewSocket.state() != QLocalSocket::UnconnectedState)
        return;

    if (!QFileInfo::exists(m_previewSocketPath)) {
        scheduleReconnect();
        return;
    }

    m_previewSocket.connectToServer(m_previewSocketPath);
}

void AppController::connectHyprEventSocket() {
    refreshRuntimePaths();

    if (m_hyprEventSocketPath.isEmpty() || m_hyprEventSocket.state() != QLocalSocket::UnconnectedState)
        return;

    if (!QFileInfo::exists(m_hyprEventSocketPath)) {
        scheduleReconnect();
        return;
    }

    m_hyprEventSocket.connectToServer(m_hyprEventSocketPath);
}

void AppController::reconnectBackgroundSockets() {
    refreshRuntimePaths();

    if (m_previewSocket.state() == QLocalSocket::ConnectedState && m_previewSocket.serverName() != m_previewSocketPath) {
        m_previewSocket.abort();
    }

    if (m_hyprEventSocket.state() == QLocalSocket::ConnectedState && m_hyprEventSocket.serverName() != m_hyprEventSocketPath) {
        m_hyprEventSocket.abort();
    }

    connectPreviewSocket();
    connectHyprEventSocket();
}

void AppController::scheduleReconnect(int delayMs) {
    if (!m_reconnectTimer.isActive() || delayMs < m_reconnectTimer.remainingTime())
        m_reconnectTimer.start(std::max(delayMs, 1));
}

void AppController::requestPreviewRefreshAsync(const QVector<int>& workspaceIDs) {
    if (workspaceIDs.isEmpty())
        return;

    QStringList ids;
    ids.reserve(workspaceIDs.size());
    for (const auto workspaceID : workspaceIDs) {
        ids << QString::number(workspaceID);
    }

    auto* process      = new QProcess(this);
    auto  environment  = QProcessEnvironment::systemEnvironment();
    if (!m_instanceSignature.isEmpty())
        environment.insert(QStringLiteral("HYPRLAND_INSTANCE_SIGNATURE"), m_instanceSignature);
    process->setProcessEnvironment(environment);
    connect(process, &QProcess::finished, process, &QObject::deleteLater);
    process->start(QStringLiteral("hyprctl"), {QStringLiteral("dispatch"), QStringLiteral("hyprexpo:preview"), ids.join(QLatin1Char(' '))});
}

void AppController::handlePreviewMessages() {
    while (m_previewSocket.canReadLine()) {
        const auto payload = m_previewSocket.readLine().trimmed();
        if (payload.isEmpty())
            continue;

        const auto doc = QJsonDocument::fromJson(payload);
        if (!doc.isObject())
            continue;

        const auto object = doc.object();
        const auto event  = object.value(QStringLiteral("event")).toString();

        if (event == QStringLiteral("hello")) {
            bootstrapCachedPreviews();
            continue;
        }

        if (event != QStringLiteral("preview"))
            continue;

        m_workspaces.updatePreview(object.value(QStringLiteral("workspaceId")).toInt(), object.value(QStringLiteral("path")).toString(),
                                   static_cast<quint64>(object.value(QStringLiteral("generation")).toDouble()));
    }
}

void AppController::handleHyprEventMessages() {
    m_hyprEventBuffer += m_hyprEventSocket.readAll();

    qsizetype newline = -1;
    while ((newline = m_hyprEventBuffer.indexOf('\n')) >= 0) {
        auto line = m_hyprEventBuffer.left(newline);
        m_hyprEventBuffer.remove(0, newline + 1);

        if (!line.isEmpty() && line.endsWith('\r'))
            line.chop(1);

        handleHyprEventLine(line);
    }
}

void AppController::bootstrapCachedPreviews() {
    const auto runtimeBytes = runtimeDir().toUtf8();
    const auto hisBytes     = m_instanceSignature.toUtf8();

    for (int row = 0; row < m_workspaces.rowCount(); ++row) {
        const auto workspaceID = m_workspaces.index(row, 0).data(WorkspaceModel::IdRole).toInt();
        if (workspaceID <= 0 || !m_workspaces.previewPathForWorkspace(workspaceID).isEmpty())
            continue;

        const auto previewPath =
            QString::fromStdString(hyprexpo::previewPath(runtimeBytes.constData(), hisBytes.constData(), workspaceID).string());
        if (QFileInfo::exists(previewPath))
            m_workspaces.bootstrapPreview(workspaceID, previewPath);
    }
}

void AppController::backfillMissingPreviews() {
    QVector<int> workspaceIDs;

    for (int row = 0; row < m_workspaces.rowCount(); ++row) {
        const auto workspaceID = m_workspaces.index(row, 0).data(WorkspaceModel::IdRole).toInt();
        if (workspaceID <= 0 || m_backfilledWorkspaceIDs.contains(workspaceID))
            continue;

        if (!m_workspaces.previewPathForWorkspace(workspaceID).isEmpty())
            continue;

        m_backfilledWorkspaceIDs.insert(workspaceID);
        workspaceIDs.push_back(workspaceID);
    }

    if (!workspaceIDs.isEmpty())
        requestPreviewRefreshAsync(workspaceIDs);
}

void AppController::handleActiveWorkspaceChanged(int workspaceID) {
    if (workspaceID <= 0)
        return;

    if (m_activeWorkspaceID > 0 && m_activeWorkspaceID != workspaceID)
        requestPreviewRefreshAsync({m_activeWorkspaceID});

    m_activeWorkspaceID = workspaceID;
    noteWorkspaceActivated(workspaceID);
}

void AppController::handleHyprEventLine(const QByteArray& line) {
    const auto separator = line.indexOf(">>");
    if (separator < 0)
        return;

    const auto event   = line.left(separator);
    const auto payload = line.mid(separator + 2);

    if (event == "workspacev2" || event == "workspace") {
        handleActiveWorkspaceChanged(parseLeadingWorkspaceID(payload));
        scheduleStateRefresh(20);
        return;
    }

    if (event == "focusedmonv2" || event == "focusedmon") {
        handleActiveWorkspaceChanged(parseFocusedMonitorWorkspaceID(payload));
        scheduleStateRefresh(20);
        return;
    }

    if (event == "activewindowv2" || event == "openwindow" || event == "closewindow" || event == "movewindowv2" || event == "createworkspacev2" ||
        event == "destroyworkspacev2" || event == "renameworkspace") {
        scheduleStateRefresh(40);
    }
}

void AppController::showSwitcher(bool reverse) {
    reconnectBackgroundSockets();

    if (!m_visible) {
        updateSelectionForShow(reverse);
        updateWindowVisibility(true);
        return;
    }

    if (reverse)
        selectPrevious();
    else
        selectNext();
}

void AppController::hideSwitcher() {
    updateWindowVisibility(false);
    m_workspaces.setCurrentIndex(-1);
}

void AppController::updateSelectionForShow(bool reverse) {
    if (m_workspaces.rowCount() <= 0) {
        m_workspaces.setCurrentIndex(-1);
        return;
    }

    if (reverse) {
        m_workspaces.setCurrentIndex(m_workspaces.rowCount() - 1);
        return;
    }

    for (int row = 0; row < m_workspaces.rowCount(); ++row) {
        if (!m_workspaces.index(row, 0).data(WorkspaceModel::ActiveRole).toBool()) {
            m_workspaces.setCurrentIndex(row);
            return;
        }
    }

    m_workspaces.setCurrentIndex(0);
}

void AppController::updateWindowVisibility(bool visible) {
    m_visible = visible;

    if (!m_window)
        return;

    if (visible) {
        m_window->show();
        m_window->requestActivate();
        m_window->raise();
    } else
        m_window->hide();
}

void AppController::selectNext() {
    m_workspaces.selectNext();
}

void AppController::selectPrevious() {
    m_workspaces.selectPrevious();
}

void AppController::activateWorkspace(int workspaceID) {
    hideSwitcher();

    if (workspaceID <= 0)
        return;

    noteWorkspaceActivated(workspaceID);

    QProcess process;
    auto     environment = QProcessEnvironment::systemEnvironment();
    if (!m_instanceSignature.isEmpty())
        environment.insert(QStringLiteral("HYPRLAND_INSTANCE_SIGNATURE"), m_instanceSignature);
    process.setProcessEnvironment(environment);
    process.start(QStringLiteral("hyprctl"), {QStringLiteral("dispatch"), QStringLiteral("workspace"), QString::number(workspaceID)});
    process.waitForFinished(1000);
}

void AppController::activateCurrent() {
    activateWorkspace(m_workspaces.currentWorkspaceID());
}

void AppController::activateWorkspaceAt(int index) {
    m_workspaces.setCurrentIndex(index);
    activateCurrent();
}

void AppController::cancel() {
    hideSwitcher();
}

void AppController::handleModifierReleased() {
    if (m_visible)
        activateCurrent();
}

void AppController::noteWorkspaceActivated(int workspaceID) {
    if (workspaceID <= 0)
        return;

    const auto existingIndex = std::find(m_mruWorkspaceIDs.begin(), m_mruWorkspaceIDs.end(), workspaceID);
    if (existingIndex != m_mruWorkspaceIDs.end())
        m_mruWorkspaceIDs.erase(existingIndex);

    m_mruWorkspaceIDs.push_front(workspaceID);
}

QVector<int> AppController::visibleWorkspaceIDs() const {
    QVector<int> ids;
    ids.reserve(m_workspaces.rowCount());

    for (int row = 0; row < m_workspaces.rowCount(); ++row) {
        ids.push_back(m_workspaces.index(row, 0).data(WorkspaceModel::IdRole).toInt());
    }

    return ids;
}

int AppController::activeWorkspaceID() const {
    for (int row = 0; row < m_workspaces.rowCount(); ++row) {
        const auto modelIndex = m_workspaces.index(row, 0);
        if (modelIndex.data(WorkspaceModel::ActiveRole).toBool())
            return modelIndex.data(WorkspaceModel::IdRole).toInt();
    }

    return -1;
}

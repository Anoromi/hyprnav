#include "AppController.hpp"

#include "../../hyprexpo/common.hpp"

#include <QCoreApplication>
#include <QDir>
#include <QFileInfo>
#include <QGuiApplication>
#include <QLocalSocket>
#include <QProcess>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QQuickWindow>
#include <QThread>

#include <filesystem>
#include <unistd.h>

namespace {

QString runtimeDir() {
    const auto envValue = qEnvironmentVariable("XDG_RUNTIME_DIR");
    return envValue.isEmpty() ? QStringLiteral("/run/user/%1").arg(getuid()) : envValue;
}

QString discoverHyprlandInstanceSignature() {
    const auto runtimeBytes = runtimeDir().toUtf8();
    const auto envBytes     = qgetenv("HYPRLAND_INSTANCE_SIGNATURE");
    return QString::fromStdString(hyprexpo::discoverHyprlandInstanceSignature(runtimeBytes.constData(), envBytes.constData()));
}

QString currentSwitcherSocketPath() {
    const auto runtimeBytes = runtimeDir().toUtf8();
    const auto his          = discoverHyprlandInstanceSignature().toUtf8();
    return QString::fromStdString(hyprexpo::switcherSocketPath(runtimeBytes.constData(), his.constData()).string());
}

QStringList fallbackSwitcherSocketPaths() {
    QStringList paths;
    const auto  root = QDir(runtimeDir() + QStringLiteral("/hx"));

    for (const auto& entry : root.entryInfoList(QDir::Dirs | QDir::NoDotAndDotDot, QDir::Time)) {
        const auto path = entry.filePath() + QStringLiteral("/switcher.sock");
        if (QFileInfo::exists(path))
            paths << path;
    }

    return paths;
}

bool sendCommandToSocket(const QString& socketPath, QByteArray command, bool waitForResponse) {
    if (socketPath.isEmpty())
        return false;

    QLocalSocket socket;
    socket.connectToServer(socketPath);
    if (!socket.waitForConnected(250))
        return false;

    socket.write(std::move(command));
    if (!socket.waitForBytesWritten(250))
        return false;

    if (!waitForResponse)
        return true;

    if (!socket.waitForReadyRead(500))
        return false;

    const auto response = socket.readAll();
    return !response.startsWith("ERROR");
}

bool sendCommandWithFallbacks(QByteArray command, bool waitForResponse) {
    const auto preferredPath = currentSwitcherSocketPath();
    if (sendCommandToSocket(preferredPath, command, waitForResponse))
        return true;

    const auto fallbacks = fallbackSwitcherSocketPaths();
    for (const auto& path : fallbacks) {
        if (path == preferredPath)
            continue;

        if (sendCommandToSocket(path, command, waitForResponse))
            return true;
    }

    return false;
}

int runTrigger(bool reverse) {
    const QByteArray command = reverse ? QByteArrayLiteral("SHOW REVERSE\n") : QByteArrayLiteral("SHOW FORWARD\n");
    if (sendCommandWithFallbacks(command, false))
        return 0;

    if (!QProcess::startDetached(QCoreApplication::applicationFilePath(), {QStringLiteral("daemon")}))
        return 1;

    for (int attempt = 0; attempt < 12; ++attempt) {
        QThread::msleep(150);
        if (sendCommandWithFallbacks(command, false))
            return 0;
    }

    return 1;
}

bool daemonAlreadyRunning() {
    return sendCommandWithFallbacks(QByteArrayLiteral("PING\n"), true);
}

}

int main(int argc, char** argv) {
    const QStringList arguments(argv, argv + argc);
    const auto        daemonMode = arguments.contains(QStringLiteral("daemon"));
    const auto        triggerMode = arguments.contains(QStringLiteral("trigger"));
    const auto        reverse = arguments.contains(QStringLiteral("--reverse"));

    if (triggerMode) {
        QCoreApplication app(argc, argv);
        return runTrigger(reverse);
    }

    if (daemonAlreadyRunning())
        return 0;

    qputenv("QML_DISABLE_DISK_CACHE", QByteArrayLiteral("1"));

    QGuiApplication app(argc, argv);
    QGuiApplication::setDesktopFileName(QStringLiteral("hyprexpo-switcher"));
    QGuiApplication::setQuitOnLastWindowClosed(false);

    if (!daemonMode && arguments.size() > 1)
        return runTrigger(reverse);

    AppController         controller;
    QQmlApplicationEngine engine;

    engine.rootContext()->setContextProperty(QStringLiteral("Controller"), &controller);
    engine.rootContext()->setContextProperty(QStringLiteral("WorkspaceModel"), controller.workspaces());

    engine.load(QUrl(QStringLiteral("qrc:/hyprexpo-switcher/Main.qml")));
    if (engine.rootObjects().isEmpty())
        return 1;

    if (auto* window = qobject_cast<QQuickWindow*>(engine.rootObjects().constFirst()))
        controller.configureWindow(window);

    if (!controller.initialize())
        return 1;

    return app.exec();
}

#include "layer_shell_bridge.hpp"

#include <LayerShellQt/window.h>

#include <QGuiApplication>
#include <QMetaObject>
#include <QQmlApplicationEngine>
#include <QQuickWindow>
#include <QScreen>

bool hyprexpo_configure_root_window(QQmlApplicationEngine& engine) {
    if (engine.rootObjects().isEmpty())
        return false;

    auto* window = qobject_cast<QQuickWindow*>(engine.rootObjects().constFirst());
    if (!window)
        return false;

    window->setColor(Qt::transparent);
    window->setVisibility(QWindow::Hidden);

    if (auto* screen = QGuiApplication::primaryScreen())
        window->setGeometry(screen->geometry());

    if (QGuiApplication::platformName() == QStringLiteral("wayland")) {
        if (auto* layerWindow = LayerShellQt::Window::get(window)) {
            layerWindow->setAnchors(LayerShellQt::Window::AnchorNone);
            layerWindow->setLayer(LayerShellQt::Window::LayerTop);
            layerWindow->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
            layerWindow->setScope(QStringLiteral("hyprexpo-switcher"));
            layerWindow->setWantsToBeOnActiveScreen(true);
            layerWindow->setMargins({});
            layerWindow->setExclusiveZone(-1);
            return true;
        }
    }

    window->setFlags(Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint);
    return true;
}

void hyprexpo_set_quit_on_last_window_closed(QGuiApplication& app, bool quitOnLastWindowClosed) {
    app.setQuitOnLastWindowClosed(quitOnLastWindowClosed);
}

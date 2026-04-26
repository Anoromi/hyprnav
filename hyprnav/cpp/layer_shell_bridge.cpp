#include "layer_shell_bridge.hpp"

#include <LayerShellQt/window.h>

#include <QGuiApplication>
#include <QMetaObject>
#include <QPointer>
#include <QQmlApplicationEngine>
#include <QQuickWindow>
#include <QScreen>
#include <QDebug>

namespace {
QPointer<QQuickWindow> g_rootWindow;
QPointer<LayerShellQt::Window> g_layerWindow;
}

bool hyprexpo_configure_root_window(QQmlApplicationEngine& engine) {
    if (engine.rootObjects().isEmpty())
        return false;

    auto* window = qobject_cast<QQuickWindow*>(engine.rootObjects().constFirst());
    if (!window)
        return false;

    g_rootWindow = window;

    window->setColor(Qt::transparent);

    if (auto* screen = QGuiApplication::primaryScreen())
        window->setGeometry(screen->geometry());

    if (QGuiApplication::platformName() == QStringLiteral("wayland")) {
        if (auto* layerWindow = LayerShellQt::Window::get(window)) {
            g_layerWindow = layerWindow;
            layerWindow->setAnchors(LayerShellQt::Window::AnchorNone);
            layerWindow->setLayer(LayerShellQt::Window::LayerTop);
            layerWindow->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
            layerWindow->setScope(QStringLiteral("hyprnav"));
            layerWindow->setWantsToBeOnActiveScreen(true);
            layerWindow->setMargins({});
            layerWindow->setExclusiveZone(-1);
            return true;
        }
    }

    window->setFlags(Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint);
    return true;
}

bool hyprexpo_load_qml_from_module(QQmlApplicationEngine& engine, const QString& uri, const QString& typeName) {
    engine.loadFromModule(uri, typeName);
    if (engine.rootObjects().isEmpty()) {
        qWarning() << "hyprnav failed to load QML root from module" << uri << typeName;
        return false;
    }
    return true;
}

void hyprexpo_set_quit_on_last_window_closed(QGuiApplication& app, bool quitOnLastWindowClosed) {
    app.setQuitOnLastWindowClosed(quitOnLastWindowClosed);
}

void hyprexpo_map_root_window_resident() {
    if (!g_rootWindow)
        return;

    if (g_layerWindow)
        g_layerWindow->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityNone);
    g_rootWindow->setFlag(Qt::WindowTransparentForInput, true);
    g_rootWindow->show();
}

void hyprexpo_set_root_window_interactive(bool interactive) {
    if (!g_rootWindow)
        return;

    if (g_layerWindow) {
        g_layerWindow->setKeyboardInteractivity(
            interactive ? LayerShellQt::Window::KeyboardInteractivityExclusive
                        : LayerShellQt::Window::KeyboardInteractivityNone
        );
    }

    g_rootWindow->setFlag(Qt::WindowTransparentForInput, !interactive);
    if (interactive)
        g_rootWindow->requestActivate();
}

void hyprexpo_set_root_window_visible(bool visible) {
    if (!g_rootWindow)
        return;

    if (visible) {
        if (g_layerWindow) {
            g_layerWindow->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
        }
        g_rootWindow->setFlag(Qt::WindowTransparentForInput, false);
        g_rootWindow->show();
        g_rootWindow->requestActivate();
    } else {
        g_rootWindow->hide();
    }
}

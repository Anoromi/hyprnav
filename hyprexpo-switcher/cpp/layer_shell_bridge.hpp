#pragma once

#include <QtCore/QString>

class QGuiApplication;
class QQmlApplicationEngine;

bool hyprexpo_configure_root_window(QQmlApplicationEngine& engine);
void hyprexpo_set_quit_on_last_window_closed(QGuiApplication& app, bool quitOnLastWindowClosed);

#pragma once

#include <QtCore/QString>

class QGuiApplication;
class QQmlApplicationEngine;

bool hyprexpo_configure_root_window(QQmlApplicationEngine& engine);
bool hyprexpo_load_qml_from_module(QQmlApplicationEngine& engine, const QString& uri, const QString& typeName);
void hyprexpo_set_quit_on_last_window_closed(QGuiApplication& app, bool quitOnLastWindowClosed);
void hyprexpo_set_root_window_visible(bool visible);

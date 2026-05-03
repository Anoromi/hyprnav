#pragma once

#include <QtCore/QString>

class QGuiApplication;
class QQmlApplicationEngine;

bool hyprnav_configure_root_window(QQmlApplicationEngine& engine);
bool hyprnav_load_qml_from_module(QQmlApplicationEngine& engine, const QString& uri, const QString& typeName);
bool hyprnav_activation_modifier_held();
void hyprnav_set_quit_on_last_window_closed(QGuiApplication& app, bool quitOnLastWindowClosed);
void hyprnav_map_root_window_resident();
void hyprnav_set_root_window_interactive(bool interactive);
void hyprnav_set_root_window_visible(bool visible);

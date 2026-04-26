#pragma once

#include <cstdint>

#include "cxx-qt-lib/core/qlist/qlist_i32.h"

class QAbstractListModel;

void hyprexpo_emit_rows_changed(
    QAbstractListModel& model,
    std::int32_t firstRow,
    std::int32_t lastRow,
    const QList_i32& roles);

#include "model_bridge.hpp"

#include <QtCore/QAbstractListModel>

void hyprexpo_emit_rows_changed(
    QAbstractListModel& model,
    std::int32_t firstRow,
    std::int32_t lastRow,
    const QList_i32& roles)
{
    const QModelIndex topLeft = model.index(firstRow, 0);
    const QModelIndex bottomRight = model.index(lastRow, 0);
    Q_EMIT model.dataChanged(topLeft, bottomRight, roles);
}

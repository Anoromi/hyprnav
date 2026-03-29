#pragma once

#include <QByteArray>
#include <QString>
#include <QVector>

#include <limits>
#include <vector>

struct SWorkspaceDescriptor {
    int     id        = 0;
    QString name;
    QString subtitle;
    QString appClass;
    int     windowCount      = 0;
    int     focusHistoryRank = std::numeric_limits<int>::max();
    bool    active = false;
};

QVector<SWorkspaceDescriptor> buildWorkspaceDescriptors(const QByteArray& monitorsJSON, const QByteArray& workspacesJSON, const QByteArray& clientsJSON);
void                          sortWorkspacesForSwitcher(QVector<SWorkspaceDescriptor>& workspaces, const std::vector<int>& mruWorkspaceIDs);
int                           initialSelectionIndex(const QVector<SWorkspaceDescriptor>& workspaces, bool reverse);

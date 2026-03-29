#include "workspace_utils.hpp"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

#include <algorithm>
#include <limits>
#include <unordered_map>

namespace {
    struct SWorkspaceClientInfo {
        QString title;
        QString appClass;
        int     windowCount      = 0;
        int     focusHistoryRank = std::numeric_limits<int>::max();
    };

    int mruRankForWorkspace(const std::vector<int>& mruWorkspaceIDs, int workspaceID) {
        for (size_t i = 0; i < mruWorkspaceIDs.size(); ++i) {
            if (mruWorkspaceIDs[i] == workspaceID)
                return static_cast<int>(i);
        }

        return std::numeric_limits<int>::max();
    }
}

QVector<SWorkspaceDescriptor> buildWorkspaceDescriptors(const QByteArray& monitorsJSON, const QByteArray& workspacesJSON, const QByteArray& clientsJSON) {
    const auto monitorDoc   = QJsonDocument::fromJson(monitorsJSON);
    const auto workspaceDoc = QJsonDocument::fromJson(workspacesJSON);
    const auto clientsDoc   = QJsonDocument::fromJson(clientsJSON);

    int activeWorkspaceID = -1;
    for (const auto monitorValue : monitorDoc.array()) {
        const auto monitorObject = monitorValue.toObject();
        if (!monitorObject.value(QStringLiteral("focused")).toBool())
            continue;

        activeWorkspaceID = monitorObject.value(QStringLiteral("activeWorkspace")).toObject().value(QStringLiteral("id")).toInt(-1);
        break;
    }

    std::unordered_map<int, SWorkspaceClientInfo> clientInfoByWorkspace;
    for (const auto clientValue : clientsDoc.array()) {
        const auto clientObject = clientValue.toObject();
        if (!clientObject.value(QStringLiteral("mapped")).toBool())
            continue;

        const auto workspaceID = clientObject.value(QStringLiteral("workspace")).toObject().value(QStringLiteral("id")).toInt(0);
        if (workspaceID <= 0)
            continue;

        auto& workspaceInfo = clientInfoByWorkspace[workspaceID];
        workspaceInfo.windowCount++;

        const auto focusHistoryRank = clientObject.value(QStringLiteral("focusHistoryID")).toInt(std::numeric_limits<int>::max());
        if (focusHistoryRank >= workspaceInfo.focusHistoryRank)
            continue;

        workspaceInfo.focusHistoryRank = focusHistoryRank;
        workspaceInfo.title            = clientObject.value(QStringLiteral("title")).toString();
        workspaceInfo.appClass         = clientObject.value(QStringLiteral("class")).toString();
    }

    QVector<SWorkspaceDescriptor> workspaces;
    for (const auto workspaceValue : workspaceDoc.array()) {
        const auto workspaceObject = workspaceValue.toObject();
        const auto workspaceID     = workspaceObject.value(QStringLiteral("id")).toInt(0);
        if (workspaceID <= 0)
            continue;

        const auto windowCount = workspaceObject.value(QStringLiteral("windows")).toInt(0);
        if (windowCount <= 0)
            continue;

        const auto name              = workspaceObject.value(QStringLiteral("name")).toString(QString::number(workspaceID));
        const auto fallbackTitle     = workspaceObject.value(QStringLiteral("lastwindowtitle")).toString();
        const auto clientInfoIt      = clientInfoByWorkspace.find(workspaceID);
        const auto clientInfoPresent = clientInfoIt != clientInfoByWorkspace.end();
        const auto appClass          = clientInfoPresent ? clientInfoIt->second.appClass : QString{};
        const auto subtitle          = clientInfoPresent && !clientInfoIt->second.title.isEmpty()    ? clientInfoIt->second.title :
                                       !appClass.isEmpty()                                            ? appClass :
                                       !fallbackTitle.isEmpty()                                       ? fallbackTitle :
                                                                                                       QStringLiteral("No recent window");
        const auto focusHistoryRank  = clientInfoPresent ? clientInfoIt->second.focusHistoryRank : std::numeric_limits<int>::max();
        const auto derivedCount      = clientInfoPresent ? clientInfoIt->second.windowCount : windowCount;

        workspaces.push_back(SWorkspaceDescriptor{
            .id               = workspaceID,
            .name             = name,
            .subtitle         = subtitle,
            .appClass         = appClass,
            .windowCount      = derivedCount,
            .focusHistoryRank = focusHistoryRank,
            .active           = workspaceID == activeWorkspaceID,
        });
    }

    std::sort(workspaces.begin(), workspaces.end(), [](const auto& left, const auto& right) {
        if (left.active != right.active)
            return left.active;

        if (left.focusHistoryRank != right.focusHistoryRank)
            return left.focusHistoryRank < right.focusHistoryRank;

        return left.id < right.id;
    });

    return workspaces;
}

void sortWorkspacesForSwitcher(QVector<SWorkspaceDescriptor>& workspaces, const std::vector<int>& mruWorkspaceIDs) {
    std::sort(workspaces.begin(), workspaces.end(), [&](const auto& left, const auto& right) {
        if (left.active != right.active)
            return left.active;

        const auto leftMRURank  = mruRankForWorkspace(mruWorkspaceIDs, left.id);
        const auto rightMRURank = mruRankForWorkspace(mruWorkspaceIDs, right.id);

        if (leftMRURank != rightMRURank)
            return leftMRURank < rightMRURank;

        if (left.focusHistoryRank != right.focusHistoryRank)
            return left.focusHistoryRank < right.focusHistoryRank;

        return left.id < right.id;
    });
}

int initialSelectionIndex(const QVector<SWorkspaceDescriptor>& workspaces, bool reverse) {
    if (workspaces.isEmpty())
        return -1;

    if (workspaces.size() == 1)
        return 0;

    if (reverse)
        return workspaces.size() - 1;

    for (int i = 0; i < workspaces.size(); ++i) {
        if (!workspaces[i].active)
            return i;
    }

    return 0;
}

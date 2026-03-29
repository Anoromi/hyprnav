#include "WorkspaceModel.hpp"

#include <QUrl>
#include <QUrlQuery>

#include <unordered_map>

WorkspaceModel::WorkspaceModel(QObject* parent) : QAbstractListModel(parent) {
}

int WorkspaceModel::rowCount(const QModelIndex& parent) const {
    if (parent.isValid())
        return 0;

    return m_items.size();
}

QVariant WorkspaceModel::data(const QModelIndex& index, int role) const {
    if (!index.isValid() || index.row() < 0 || index.row() >= m_items.size())
        return {};

    const auto& item = m_items[index.row()];
    switch (role) {
        case IdRole: return item.id;
        case NameRole: return item.name;
        case SubtitleRole: return item.subtitle;
        case AppClassRole: return item.appClass;
        case WindowCountRole: return item.windowCount;
        case ActiveRole: return item.active;
        case SelectedRole: return item.selected;
        case PreviewPathRole:
            if (item.previewPath.isEmpty())
                return {};

            {
                auto     url = QUrl::fromLocalFile(item.previewPath);
                QUrlQuery query;
                query.addQueryItem(QStringLiteral("g"), QString::number(item.generation));
                url.setQuery(query);
                return url;
            }
        case GenerationRole: return item.generation;
        default: return {};
    }
}

QHash<int, QByteArray> WorkspaceModel::roleNames() const {
    return {
        {IdRole, "workspaceId"},
        {NameRole, "workspaceName"},
        {SubtitleRole, "workspaceSubtitle"},
        {AppClassRole, "workspaceAppClass"},
        {WindowCountRole, "workspaceWindowCount"},
        {ActiveRole, "workspaceActive"},
        {SelectedRole, "workspaceSelected"},
        {PreviewPathRole, "workspacePreview"},
        {GenerationRole, "workspaceGeneration"},
    };
}

void WorkspaceModel::setWorkspaces(const QVector<SWorkspaceDescriptor>& workspaces) {
    std::unordered_map<int, SWorkspaceItem> previousByID;
    previousByID.reserve(static_cast<size_t>(m_items.size()));
    for (const auto& item : m_items) {
        previousByID.emplace(item.id, item);
    }

    beginResetModel();
    m_items.clear();
    m_items.reserve(workspaces.size());

    for (const auto& workspace : workspaces) {
        auto previous = previousByID.find(workspace.id);

        m_items.push_back(SWorkspaceItem{
            .id          = workspace.id,
            .name        = workspace.name,
            .subtitle    = workspace.subtitle,
            .appClass    = workspace.appClass,
            .windowCount = workspace.windowCount,
            .active      = workspace.active,
            .previewPath = previous != previousByID.end() ? previous->second.previewPath : QString{},
            .generation  = previous != previousByID.end() ? previous->second.generation : 0,
        });
    }

    m_currentIndex = -1;
    endResetModel();
    emit currentIndexChanged();
}

void WorkspaceModel::setSelectedFlag(int index, bool selected) {
    if (index < 0 || index >= m_items.size())
        return;

    m_items[index].selected = selected;
    emit dataChanged(this->index(index), this->index(index), {SelectedRole});
}

void WorkspaceModel::setCurrentIndex(int index) {
    if (index == m_currentIndex)
        return;

    if (m_currentIndex >= 0)
        setSelectedFlag(m_currentIndex, false);

    m_currentIndex = index >= 0 && index < m_items.size() ? index : -1;

    if (m_currentIndex >= 0)
        setSelectedFlag(m_currentIndex, true);

    emit currentIndexChanged();
}

int WorkspaceModel::currentIndex() const {
    return m_currentIndex;
}

int WorkspaceModel::currentWorkspaceID() const {
    if (m_currentIndex < 0 || m_currentIndex >= m_items.size())
        return -1;

    return m_items[m_currentIndex].id;
}

int WorkspaceModel::indexOfWorkspace(int workspaceID) const {
    for (int i = 0; i < m_items.size(); ++i) {
        if (m_items[i].id == workspaceID)
            return i;
    }

    return -1;
}

QString WorkspaceModel::previewPathForWorkspace(int workspaceID) const {
    const auto itemIndex = indexOfWorkspace(workspaceID);
    if (itemIndex < 0)
        return {};

    return m_items[itemIndex].previewPath;
}

void WorkspaceModel::selectNext() {
    if (m_items.isEmpty())
        return;

    if (m_currentIndex < 0)
        setCurrentIndex(0);
    else
        setCurrentIndex((m_currentIndex + 1) % m_items.size());
}

void WorkspaceModel::selectPrevious() {
    if (m_items.isEmpty())
        return;

    if (m_currentIndex < 0)
        setCurrentIndex(0);
    else
        setCurrentIndex((m_currentIndex + m_items.size() - 1) % m_items.size());
}

void WorkspaceModel::bootstrapPreview(int workspaceID, const QString& previewPath) {
    for (int i = 0; i < m_items.size(); ++i) {
        if (m_items[i].id != workspaceID || !m_items[i].previewPath.isEmpty())
            continue;

        m_items[i].previewPath = previewPath;
        emit dataChanged(index(i), index(i), {PreviewPathRole});
        return;
    }
}

void WorkspaceModel::updatePreview(int workspaceID, const QString& previewPath, quint64 generation) {
    for (int i = 0; i < m_items.size(); ++i) {
        if (m_items[i].id != workspaceID)
            continue;

        m_items[i].previewPath = previewPath;
        m_items[i].generation  = generation;
        emit dataChanged(index(i), index(i), {PreviewPathRole, GenerationRole});
        return;
    }
}

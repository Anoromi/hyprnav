#pragma once

#include "workspace_utils.hpp"

#include <QAbstractListModel>
#include <QVector>

struct SWorkspaceItem {
    int     id = 0;
    QString name;
    QString subtitle;
    QString appClass;
    int     windowCount = 0;
    bool    active = false;
    bool    selected = false;
    QString previewPath;
    quint64 generation = 0;
};

class WorkspaceModel : public QAbstractListModel {
    Q_OBJECT
    Q_PROPERTY(int currentIndex READ currentIndex NOTIFY currentIndexChanged)

  public:
    enum ERoles : int {
        IdRole = Qt::UserRole + 1,
        NameRole,
        SubtitleRole,
        AppClassRole,
        WindowCountRole,
        ActiveRole,
        SelectedRole,
        PreviewPathRole,
        GenerationRole,
    };

    explicit WorkspaceModel(QObject* parent = nullptr);

    int                    rowCount(const QModelIndex& parent = {}) const override;
    QVariant               data(const QModelIndex& index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    void                   setWorkspaces(const QVector<SWorkspaceDescriptor>& workspaces);
    Q_INVOKABLE void       setCurrentIndex(int index);
    int                    currentIndex() const;
    int                    currentWorkspaceID() const;
    int                    indexOfWorkspace(int workspaceID) const;
    QString                previewPathForWorkspace(int workspaceID) const;

    Q_INVOKABLE void       selectNext();
    Q_INVOKABLE void       selectPrevious();
    void                   bootstrapPreview(int workspaceID, const QString& previewPath);
    void                   updatePreview(int workspaceID, const QString& previewPath, quint64 generation);

  signals:
    void currentIndexChanged();

  private:
    void                 setSelectedFlag(int index, bool selected);

    QVector<SWorkspaceItem> m_items;
    int                     m_currentIndex = -1;
};

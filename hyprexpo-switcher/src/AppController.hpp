#pragma once

#include "WorkspaceModel.hpp"

#include <QObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QSet>
#include <QTimer>

class QQuickWindow;

class AppController : public QObject {
    Q_OBJECT
    Q_PROPERTY(WorkspaceModel* workspaces READ workspaces CONSTANT)

  public:
    explicit AppController(QObject* parent = nullptr);

    WorkspaceModel* workspaces();

    bool            initialize();
    void            configureWindow(QQuickWindow* window);

    Q_INVOKABLE void selectNext();
    Q_INVOKABLE void selectPrevious();
    Q_INVOKABLE void activateCurrent();
    Q_INVOKABLE void activateWorkspaceAt(int index);
    Q_INVOKABLE void cancel();
    Q_INVOKABLE void handleModifierReleased();

  private:
    bool             eventFilter(QObject* watched, QEvent* event) override;

    void             refreshRuntimePaths(bool forceRebindServer = false);
    bool             startControlServer();
    void             handleControlConnection();
    void             handleControlLine(QLocalSocket* socket, const QByteArray& line);

    QByteArray       runHyprctlJSON(const QStringList& args);
    void             refreshWorkspaceState();
    void             applyWorkspaceState(QVector<SWorkspaceDescriptor> items);
    void             scheduleStateRefresh(int delayMs = 50);

    void             connectPreviewSocket();
    void             connectHyprEventSocket();
    void             reconnectBackgroundSockets();
    void             scheduleReconnect(int delayMs = 350);
    void             requestPreviewRefreshAsync(const QVector<int>& workspaceIDs);
    void             handlePreviewMessages();
    void             handleHyprEventMessages();
    void             handleHyprEventLine(const QByteArray& line);
    void             bootstrapCachedPreviews();
    void             backfillMissingPreviews();
    void             handleActiveWorkspaceChanged(int workspaceID);

    void             showSwitcher(bool reverse);
    void             hideSwitcher();
    void             updateSelectionForShow(bool reverse);
    void             updateWindowVisibility(bool visible);
    void             activateWorkspace(int workspaceID);
    void             noteWorkspaceActivated(int workspaceID);

    QVector<int>     visibleWorkspaceIDs() const;
    int              activeWorkspaceID() const;
    QString          runtimeDir() const;

    WorkspaceModel                    m_workspaces;
    QQuickWindow*                     m_window = nullptr;
    QLocalServer                      m_controlServer;
    QHash<QLocalSocket*, QByteArray>  m_controlBuffers;
    QLocalSocket                      m_previewSocket;
    QLocalSocket                      m_hyprEventSocket;
    QByteArray                        m_previewBuffer;
    QByteArray                        m_hyprEventBuffer;
    QTimer                            m_stateRefreshTimer;
    QTimer                            m_reconnectTimer;
    QVector<int>                      m_mruWorkspaceIDs;
    QString                           m_instanceSignature;
    QString                           m_previewSocketPath;
    QString                           m_switcherSocketPath;
    QString                           m_hyprEventSocketPath;
    QString                           m_runtimePath;
    bool                              m_visible = false;
    int                               m_activeWorkspaceID = -1;
    QSet<int>                         m_backfilledWorkspaceIDs;
};

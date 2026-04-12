#pragma once

#include "common.hpp"

#include <hyprland/src/helpers/signal/Signal.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopTimer.hpp>
#include <hyprland/src/desktop/DesktopTypes.hpp>

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <string>
#include <sys/types.h>
#include <unordered_map>
#include <unordered_set>

class CSpawnManager {
  public:
    CSpawnManager();
    ~CSpawnManager();

  private:
    enum class ESpawnFocusPolicy {
        Follow,
        Preserve,
    };

    struct SClientState {
        int         fd = -1;
        std::string readBuffer;
    };

    struct SSpawnOperation {
        std::string                    operationID;
        int                            workspaceID          = -1;
        int                            targetMonitorID      = -1;
        ESpawnFocusPolicy              focusPolicy          = ESpawnFocusPolicy::Follow;
        int                            originMonitorID      = -1;
        int                            originWorkspaceID    = -1;
        std::optional<uintptr_t>       originWindowAddress;
        pid_t                          rootPID              = -1;
        uint64_t                       createdAtMs          = 0;
        std::unordered_set<uintptr_t> movedWindowAddresses;
        std::unordered_set<uintptr_t> restoredWindowAddresses;
    };

    void createSocket();
    void refreshRuntimePaths();
    void acceptClients();
    void readClients();
    void disconnectClient(int fd);
    void handleClientLine(int fd, const std::string& line);
    void wakeTimer(std::chrono::milliseconds timeout = std::chrono::milliseconds{1});
    void onTimer(SP<CEventLoopTimer> self);
    void registerEventListeners();
    void handleWindow(PHLWINDOW window);
    bool restoreOriginalFocus(const SSpawnOperation& operation, PHLWINDOW spawnedWindow) const;
    bool matchesOperation(const SSpawnOperation& operation, pid_t windowPID) const;
    bool isDescendantProcess(pid_t pid, pid_t ancestorPID) const;
    pid_t readParentPID(pid_t pid) const;
    PHLWORKSPACE ensureWorkspace(const SSpawnOperation& operation) const;
    PHLWINDOW findWindowByAddress(uintptr_t address) const;

    bool sendLine(int fd, const std::string& payload);
    bool sendOK(int fd);
    bool sendError(int fd, std::string_view message);

    int                   m_serverFD   = -1;
    bool                  m_destroying = false;
    std::filesystem::path m_runtimeDir;
    std::filesystem::path m_socketPath;
    SP<CEventLoopTimer>   m_timer;
    CHyprSignalListener   m_openEarlyListener;
    CHyprSignalListener   m_openListener;

    std::unordered_map<int, SClientState>         m_clients;
    std::unordered_map<std::string, SSpawnOperation> m_operations;
};

inline std::unique_ptr<CSpawnManager> g_pSpawnManager;

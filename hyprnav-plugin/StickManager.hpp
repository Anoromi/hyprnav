#pragma once

#include "common.hpp"

#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopTimer.hpp>
#include <hyprland/src/desktop/DesktopTypes.hpp>
#include <hyprland/src/helpers/signal/Signal.hpp>

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <set>
#include <string>
#include <sys/types.h>
#include <unordered_map>
#include <unordered_set>

// Hard sticking: every window that belongs to a spawned process tree stays on
// the tree's workspace, for as long as the tree or any of its windows lives.
//
// Socket protocol (one JSON object per line, one reply line):
//   {"op":"ping"}                                  -> {"ok":true,"result":{"instance":"..","sticks":n}}
//   {"op":"stick", "stick_id":"..", "workspace_id":N, "root_pid":P,
//     "target_monitor_id":M, "focus_policy":"follow|preserve",
//     "origin_monitor_id":M, "origin_workspace_id":W, "origin_window_address":"0x.."}
//   {"op":"unstick","stick_id":".."}               forget the root, windows stay
//   {"op":"move","stick_id":"..","workspace_id":N}  retarget and move its windows
//   {"op":"sync"}                                  -> {"ok":true,"result":{"instance":"..","active":[..],"dropped":[..]}}
//   {"op":"list"}                                  -> per-stick detail
// "watch"/"unwatch" are accepted as aliases of "stick"/"unstick".
class CStickManager {
  public:
    CStickManager();
    ~CStickManager();

  private:
    enum class EFocusPolicy {
        Follow,
        Preserve,
    };

    struct SClientState {
        int         fd = -1;
        std::string readBuffer;
    };

    struct SStickRoot {
        std::string              stickID;
        int                      workspaceID      = -1;
        int                      targetMonitorID  = -1;
        pid_t                    rootPID          = -1;
        EFocusPolicy             focusPolicy      = EFocusPolicy::Preserve;
        bool                     followConsumed   = false;
        int                      originMonitorID  = -1;
        int                      originWorkspaceID = -1;
        std::optional<uintptr_t> originWindowAddress;
        uint64_t                 createdAtMs      = 0;
        std::set<pid_t>          knownPIDs;       // rootPID and every descendant seen
        std::set<std::string>    classes;         // app classes seen in this tree
        std::set<uintptr_t>      windows;         // attributed windows still alive
    };

    // socket
    void createSocket();
    void refreshRuntimePaths();
    void acceptClients();
    void readClients();
    void disconnectClient(int fd);
    void handleClientLine(int fd, const std::string& line);
    bool sendLine(int fd, const std::string& payload);
    bool sendOK(int fd);
    bool sendResult(int fd, const std::string& resultJSON);
    bool sendError(int fd, std::string_view message);

    // lifecycle
    void wakeTimer(std::chrono::milliseconds timeout = std::chrono::milliseconds{1});
    void onTimer(SP<CEventLoopTimer> self);
    void registerEventListeners();
    void reapRoots();

    // attribution and placement
    void        onWindowOpenEarly(PHLWINDOW window);
    void        onWindowOpen(PHLWINDOW window);
    void        onWindowClose(PHLWINDOW window);
    SStickRoot* attribute(PHLWINDOW window, bool allowHeuristics);
    SStickRoot* rootForWindow(uintptr_t address);
    bool        pidInTree(SStickRoot& root, pid_t pid);
    bool        isDescendantProcess(pid_t pid, pid_t ancestorPID, std::set<pid_t>& seenChain) const;
    pid_t       readParentPID(pid_t pid) const;
    static bool pidAlive(pid_t pid);
    void        adopt(SStickRoot& root, PHLWINDOW window);
    void        place(SStickRoot& root, PHLWINDOW window, bool early);
    void        attributeExistingWindows(SStickRoot& root);
    PHLWORKSPACE ensureWorkspace(const SStickRoot& root) const;
    PHLWINDOW   findWindowByAddress(uintptr_t address) const;
    bool        restoreOriginalFocus(const SStickRoot& root, PHLWINDOW spawnedWindow) const;
    std::string describe(const SStickRoot& root) const;

    int                   m_serverFD   = -1;
    bool                  m_destroying = false;
    std::string           m_instanceID;
    std::filesystem::path m_runtimeDir;
    std::filesystem::path m_socketPath;
    SP<CEventLoopTimer>   m_timer;
    CHyprSignalListener   m_openEarlyListener;
    CHyprSignalListener   m_openListener;
    CHyprSignalListener   m_closeListener;
    uint64_t              m_lastReapMs = 0;

    std::unordered_map<int, SClientState>          m_clients;
    std::unordered_map<std::string, SStickRoot>    m_roots;
    std::unordered_map<uintptr_t, std::string>     m_windowToStick;
    std::vector<std::string>                       m_dropped; // reported on next sync
};

inline std::unique_ptr<CStickManager> g_pStickManager;

#pragma once

#include "common.hpp"

#include <hyprland/src/desktop/DesktopTypes.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopTimer.hpp>
#include <hyprland/src/render/Framebuffer.hpp>

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <vector>

class CPreviewManager {
  public:
    CPreviewManager();
    ~CPreviewManager();

    void onWorkspaceDamaged(int workspaceID);
    void requestRefresh(const std::vector<int>& workspaceIDs);

    bool active() const;

  private:
    struct SClientState {
        int         fd = -1;
        std::string readBuffer;
    };

    void                     createSocket();
    void                     refreshRuntimePaths();
    void                     acceptClients();
    void                     readClients();
    void                     disconnectClient(int fd);
    void                     handleClientLine(int fd, const std::string& line);
    void                     refreshPending();
    void                     queueRefresh(const std::vector<int>& workspaceIDs);
    bool                     sendLine(int fd, const std::string& payload);
    void                     broadcastPreviewEvent(int workspaceID, const std::filesystem::path& path, int width, int height, uint64_t generation);
    void                     wakeTimer(std::chrono::milliseconds timeout = std::chrono::milliseconds{1});
    void                     onTimer(SP<CEventLoopTimer> self);

    bool                     renderWorkspacePreview(PHLMONITOR monitor, int workspaceID, std::filesystem::path& outPath, hyprexpo::SPreviewSize& outSize);
    bool                     writeFramebufferJPEG(CFramebuffer& fb, const hyprexpo::SPreviewSize& size, const std::filesystem::path& path);

    int                      m_serverFD      = -1;
    int                      m_previewHeight = 480;
    std::filesystem::path    m_runtimeDir;
    std::filesystem::path    m_socketPath;
    bool                     m_destroying = false;
    SP<CEventLoopTimer>      m_timer;

    std::unordered_map<int, SClientState> m_clients;
    std::unordered_set<int>               m_pendingRefreshIDs;
    std::unordered_map<int, uint64_t>     m_generations;
};

inline std::unique_ptr<CPreviewManager> g_pPreviewManager;

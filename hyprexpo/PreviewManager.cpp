#include "PreviewManager.hpp"
#include "globals.hpp"

#include <any>
#define private public
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/helpers/time/Time.hpp>
#include <hyprland/src/managers/animation/DesktopAnimationManager.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopManager.hpp>
#undef private

#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <format>
#include <jpeglib.h>
#include <setjmp.h>
#include <span>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static constexpr size_t MAX_CLIENT_LINE = 4096;

namespace {
struct SJpegErrorManager {
    jpeg_error_mgr pub;
    jmp_buf        setjmpBuffer;
};

extern "C" void onJPEGError(j_common_ptr cinfo) {
    auto* errorManager = reinterpret_cast<SJpegErrorManager*>(cinfo->err);
    longjmp(errorManager->setjmpBuffer, 1);
}
}

static const char* previewInstanceSignature() {
    if (g_pCompositor && !g_pCompositor->m_instanceSignature.empty())
        return g_pCompositor->m_instanceSignature.c_str();

    return std::getenv("HYPRLAND_INSTANCE_SIGNATURE");
}

CPreviewManager::CPreviewManager() {
    if (PHANDLE) {
        static auto* const* PPREVIEWHEIGHT = (Hyprlang::INT* const*)HyprlandAPI::getConfigValue(PHANDLE, "plugin:hyprexpo:preview_height")->getDataStaticPtr();
        m_previewHeight                    = std::max(64, static_cast<int>(**PPREVIEWHEIGHT));
    }

    refreshRuntimePaths();

    if (g_pEventLoopManager) {
        m_timer = makeShared<CEventLoopTimer>(std::optional<Time::steady_dur>{std::chrono::milliseconds{250}},
                                              [this](SP<CEventLoopTimer> self, void*) { onTimer(self); }, nullptr);
        g_pEventLoopManager->addTimer(m_timer);
        wakeTimer();
    }
}

CPreviewManager::~CPreviewManager() {
    m_destroying = true;

    if (m_timer)
        m_timer->cancel();

    if (m_timer && g_pEventLoopManager)
        g_pEventLoopManager->removeTimer(m_timer);

    m_timer.reset();

    for (const auto& [fd, _] : m_clients) {
        close(fd);
    }

    m_clients.clear();

    if (m_serverFD >= 0)
        close(m_serverFD);

    if (!m_socketPath.empty())
        std::filesystem::remove(m_socketPath);
}

bool CPreviewManager::active() const {
    return m_serverFD >= 0;
}

void CPreviewManager::wakeTimer(std::chrono::milliseconds timeout) {
    if (!m_destroying && m_timer)
        m_timer->updateTimeout(timeout);
}

void CPreviewManager::createSocket() {
    std::error_code ec;
    std::filesystem::create_directories(m_runtimeDir, ec);
    std::filesystem::remove(m_socketPath, ec);

    m_serverFD = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (m_serverFD < 0) {
        Log::logger->log(Log::ERR, "[hyprexpo] failed to create preview socket");
        return;
    }

    sockaddr_un addr = {};
    addr.sun_family  = AF_UNIX;

    const auto socketString = m_socketPath.string();
    if (socketString.size() >= sizeof(addr.sun_path)) {
        Log::logger->log(Log::ERR, "[hyprexpo] preview socket path too long");
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }

    std::memcpy(addr.sun_path, socketString.c_str(), socketString.size() + 1);

    if (bind(m_serverFD, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to bind preview socket: {}", std::strerror(errno)));
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }

    if (listen(m_serverFD, 8) < 0) {
        Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to listen on preview socket: {}", std::strerror(errno)));
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }
}

void CPreviewManager::refreshRuntimePaths() {
    const auto runtimeDir = hyprexpo::runtimeDirectory(std::getenv("XDG_RUNTIME_DIR"), previewInstanceSignature());
    const auto socketPath = hyprexpo::socketPath(std::getenv("XDG_RUNTIME_DIR"), previewInstanceSignature());

    if (runtimeDir == m_runtimeDir && socketPath == m_socketPath && active())
        return;

    if (m_serverFD >= 0) {
        close(m_serverFD);
        m_serverFD = -1;
    }

    if (!m_socketPath.empty()) {
        std::error_code ec;
        std::filesystem::remove(m_socketPath, ec);
    }

    m_runtimeDir = runtimeDir;
    m_socketPath = socketPath;
    createSocket();
}

void CPreviewManager::onTimer(SP<CEventLoopTimer> self) {
    if (m_destroying || !self)
        return;

    refreshRuntimePaths();

    if (m_serverFD >= 0) {
        acceptClients();
        readClients();
    }

    refreshPending();

    const auto hasActiveWork = !m_pendingRefreshIDs.empty() || !m_clients.empty();
    self->updateTimeout(hasActiveWork ? std::chrono::milliseconds{50} : std::chrono::milliseconds{250});
}

void CPreviewManager::onWorkspaceDamaged(int workspaceID) {
    (void)workspaceID;
}

void CPreviewManager::requestRefresh(const std::vector<int>& workspaceIDs) {
    refreshRuntimePaths();
    queueRefresh(workspaceIDs);
    if (m_timer)
        wakeTimer();
    else
        refreshPending();
}

void CPreviewManager::acceptClients() {
    while (true) {
        const auto clientFD = accept4(m_serverFD, nullptr, nullptr, SOCK_NONBLOCK);
        if (clientFD < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK)
                break;

            Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to accept preview client: {}", std::strerror(errno)));
            break;
        }

        m_clients.emplace(clientFD, SClientState{
                                     .fd = clientFD,
                                 });

        if (!sendLine(clientFD, hyprexpo::formatHelloEvent(m_socketPath, m_previewHeight)))
            disconnectClient(clientFD);
    }
}

void CPreviewManager::readClients() {
    std::vector<int> disconnected;

    for (auto& [fd, client] : m_clients) {
        char buffer[1024];

        while (true) {
            const auto bytes = recv(fd, buffer, sizeof(buffer), 0);
            if (bytes == 0) {
                disconnected.push_back(fd);
                break;
            }

            if (bytes < 0) {
                if (errno == EAGAIN || errno == EWOULDBLOCK)
                    break;

                disconnected.push_back(fd);
                break;
            }

            client.readBuffer.append(buffer, bytes);

            if (client.readBuffer.size() > MAX_CLIENT_LINE) {
                sendLine(fd, hyprexpo::formatErrorEvent("command too long"));
                disconnected.push_back(fd);
                break;
            }

            size_t newline = std::string::npos;
            while ((newline = client.readBuffer.find('\n')) != std::string::npos) {
                auto line = client.readBuffer.substr(0, newline);
                client.readBuffer.erase(0, newline + 1);

                if (!line.empty() && line.back() == '\r')
                    line.pop_back();

                handleClientLine(fd, line);
            }
        }
    }

    for (const auto fd : disconnected) {
        disconnectClient(fd);
    }
}

void CPreviewManager::disconnectClient(int fd) {
    const auto it = m_clients.find(fd);
    if (it == m_clients.end())
        return;

    close(fd);
    m_clients.erase(it);
}

void CPreviewManager::handleClientLine(int fd, const std::string& line) {
    std::string error;
    const auto  command = hyprexpo::parseClientCommand(line, error);
    if (!command.has_value()) {
        sendLine(fd, hyprexpo::formatErrorEvent(error));
        return;
    }

    switch (command->command) {
        case hyprexpo::eClientCommand::HELLO: sendLine(fd, hyprexpo::formatHelloEvent(m_socketPath, m_previewHeight)); break;
        case hyprexpo::eClientCommand::WATCH: {
            queueRefresh(command->workspaceIDs);
            wakeTimer();
            break;
        }
        case hyprexpo::eClientCommand::REFRESH:
            queueRefresh(command->workspaceIDs);
            wakeTimer();
            break;
        case hyprexpo::eClientCommand::CLEAR: break;
        case hyprexpo::eClientCommand::PING: sendLine(fd, "{\"event\":\"pong\"}\n"); break;
    }
}

void CPreviewManager::queueRefresh(const std::vector<int>& workspaceIDs) {
    for (const auto id : workspaceIDs) {
        if (id > 0)
            m_pendingRefreshIDs.insert(id);
    }
}

bool CPreviewManager::sendLine(int fd, const std::string& payload) {
    if (fd < 0)
        return false;

    ssize_t written = send(fd, payload.c_str(), payload.size(), MSG_NOSIGNAL);
    if (written < 0)
        return errno == EAGAIN || errno == EWOULDBLOCK;

    return true;
}

void CPreviewManager::refreshPending() {
    if (m_pendingRefreshIDs.empty())
        return;

    const auto monitor = Desktop::focusState()->monitor();
    if (!monitor)
        return;

    const auto refreshIDs = m_pendingRefreshIDs;
    m_pendingRefreshIDs.clear();

    for (const auto workspaceID : refreshIDs) {
        std::filesystem::path preview;
        hyprexpo::SPreviewSize size;
        if (!renderWorkspacePreview(monitor, workspaceID, preview, size)) {
            broadcastPreviewEvent(workspaceID, {}, 0, 0, 0);
            continue;
        }

        const auto generation = ++m_generations[workspaceID];
        broadcastPreviewEvent(workspaceID, preview, size.width, size.height, generation);
    }
}

void CPreviewManager::broadcastPreviewEvent(int workspaceID, const std::filesystem::path& path, int width, int height, uint64_t generation) {
    const auto payload = hyprexpo::formatPreviewEvent(workspaceID, path, width, height, generation);
    std::vector<int> disconnected;

    for (const auto& [fd, _] : m_clients) {
        if (!sendLine(fd, payload))
            disconnected.push_back(fd);
    }

    for (const auto fd : disconnected) {
        disconnectClient(fd);
    }
}

bool CPreviewManager::renderWorkspacePreview(PHLMONITOR monitor, int workspaceID, std::filesystem::path& outPath, hyprexpo::SPreviewSize& outSize) {
    if (!monitor)
        return false;

    outSize = hyprexpo::computePreviewSize(monitor->m_pixelSize.x, monitor->m_pixelSize.y, m_previewHeight);
    if (outSize.width <= 0 || outSize.height <= 0)
        return false;

    CFramebuffer  framebuffer;
    PHLWORKSPACE  previousWorkspace = monitor->m_activeWorkspace;
    PHLWORKSPACE  previousSpecial   = monitor->m_activeSpecialWorkspace;
    const auto    targetWorkspace   = g_pCompositor->getWorkspaceByID(workspaceID);

    g_pHyprRenderer->makeEGLCurrent();

    if (!framebuffer.alloc(outSize.width, outSize.height, monitor->m_output->state->state().drmFormat))
        return false;

    if (previousSpecial)
        monitor->m_activeSpecialWorkspace.reset();

    if (previousWorkspace)
        previousWorkspace->m_visible = false;

    CRegion fakeDamage{0, 0, INT16_MAX, INT16_MAX};
    g_pHyprRenderer->m_bBlockSurfaceFeedback = true;
    if (!g_pHyprRenderer->beginRender(monitor, fakeDamage, RENDER_MODE_FULL_FAKE, nullptr, &framebuffer)) {
        g_pHyprRenderer->m_bBlockSurfaceFeedback = false;

        if (previousSpecial)
            monitor->m_activeSpecialWorkspace = previousSpecial;

        monitor->m_activeWorkspace = previousWorkspace;
        if (previousWorkspace)
            previousWorkspace->m_visible = true;

        return false;
    }
    g_pHyprOpenGL->clear(CHyprColor{0, 0, 0, 1.0});

    CBox renderBox{0, 0, static_cast<double>(outSize.width), static_cast<double>(outSize.height)};

    if (targetWorkspace) {
        monitor->m_activeWorkspace = targetWorkspace;
        targetWorkspace->m_visible = true;
        g_pDesktopAnimationManager->startAnimation(targetWorkspace, CDesktopAnimationManager::ANIMATION_TYPE_IN, true, true);
        g_pHyprRenderer->renderWorkspace(monitor, targetWorkspace, Time::steadyNow(), renderBox);
        targetWorkspace->m_visible = false;
        g_pDesktopAnimationManager->startAnimation(targetWorkspace, CDesktopAnimationManager::ANIMATION_TYPE_OUT, false, true);
    } else
        g_pHyprRenderer->renderWorkspace(monitor, nullptr, Time::steadyNow(), renderBox);

    g_pHyprOpenGL->m_renderData.blockScreenShader = true;
    g_pHyprRenderer->endRender();
    g_pHyprRenderer->m_bBlockSurfaceFeedback = false;

    if (previousSpecial)
        monitor->m_activeSpecialWorkspace = previousSpecial;

    monitor->m_activeWorkspace = previousWorkspace;
    if (previousWorkspace)
        previousWorkspace->m_visible = true;

    outPath = hyprexpo::previewPath(std::getenv("XDG_RUNTIME_DIR"), previewInstanceSignature(), workspaceID);
    return writeFramebufferJPEG(framebuffer, outSize, outPath);
}

bool CPreviewManager::writeFramebufferJPEG(CFramebuffer& fb, const hyprexpo::SPreviewSize& size, const std::filesystem::path& path) {
    std::error_code ec;
    std::filesystem::create_directories(path.parent_path(), ec);
    const auto tempPath = path.string() + ".tmp";

    std::vector<uint8_t> rgba(static_cast<size_t>(size.width) * static_cast<size_t>(size.height) * 4);
    std::vector<uint8_t> rgb(static_cast<size_t>(size.width) * static_cast<size_t>(size.height) * 3);

    GLint previousFB = 0;
    glGetIntegerv(GL_FRAMEBUFFER_BINDING, &previousFB);
    glBindFramebuffer(GL_FRAMEBUFFER, fb.getFBID());
    glReadPixels(0, 0, size.width, size.height, GL_RGBA, GL_UNSIGNED_BYTE, rgba.data());
    glBindFramebuffer(GL_FRAMEBUFFER, previousFB);

    for (int y = 0; y < size.height; ++y) {
        const auto srcY = size.height - 1 - y;

        for (int x = 0; x < size.width; ++x) {
            const auto srcIdx = static_cast<size_t>(srcY * size.width + x) * 4;
            const auto dstIdx = static_cast<size_t>(y * size.width + x) * 3;
            const auto a      = rgba[srcIdx + 3];

            rgb[dstIdx + 0] = static_cast<uint8_t>((rgba[srcIdx + 0] * a) / 255);
            rgb[dstIdx + 1] = static_cast<uint8_t>((rgba[srcIdx + 1] * a) / 255);
            rgb[dstIdx + 2] = static_cast<uint8_t>((rgba[srcIdx + 2] * a) / 255);
        }
    }

    auto* file = std::fopen(tempPath.c_str(), "wb");
    if (!file)
        return false;

    jpeg_compress_struct compressor = {};
    SJpegErrorManager    errorManager;
    compressor.err = jpeg_std_error(&errorManager.pub);
    errorManager.pub.error_exit = onJPEGError;

    if (setjmp(errorManager.setjmpBuffer) != 0) {
        jpeg_destroy_compress(&compressor);
        std::fclose(file);
        std::filesystem::remove(tempPath, ec);
        return false;
    }

    jpeg_create_compress(&compressor);
    jpeg_stdio_dest(&compressor, file);
    compressor.image_width      = size.width;
    compressor.image_height     = size.height;
    compressor.input_components = 3;
    compressor.in_color_space   = JCS_RGB;
    jpeg_set_defaults(&compressor);
    jpeg_set_quality(&compressor, 82, TRUE);
    jpeg_start_compress(&compressor, TRUE);

    while (compressor.next_scanline < static_cast<JDIMENSION>(size.height)) {
        JSAMPROW rowPointer = reinterpret_cast<JSAMPROW>(rgb.data() + static_cast<size_t>(compressor.next_scanline) * size.width * 3);
        jpeg_write_scanlines(&compressor, &rowPointer, 1);
    }

    jpeg_finish_compress(&compressor);
    jpeg_destroy_compress(&compressor);
    std::fclose(file);

    std::filesystem::rename(tempPath, path, ec);
    if (ec) {
        std::filesystem::remove(path, ec);
        ec.clear();
        std::filesystem::rename(tempPath, path, ec);
        if (ec) {
            std::filesystem::remove(tempPath, ec);
            return false;
        }
    }

    return true;
}

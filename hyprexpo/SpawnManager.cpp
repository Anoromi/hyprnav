#include "SpawnManager.hpp"
#include "globals.hpp"

#define private public
#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/helpers/Monitor.hpp>
#include <hyprland/src/helpers/time/Time.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopManager.hpp>
#undef private

#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <format>
#include <fstream>
#include <optional>
#include <regex>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static constexpr size_t MAX_CLIENT_LINE = 4096;

namespace {
static const char* spawnInstanceSignature() {
    if (g_pCompositor && !g_pCompositor->m_instanceSignature.empty())
        return g_pCompositor->m_instanceSignature.c_str();

    return std::getenv("HYPRLAND_INSTANCE_SIGNATURE");
}

std::optional<std::string> jsonStringField(const std::string& line, const char* key) {
    const std::regex pattern(std::format("\"{}\"\\s*:\\s*\"([^\"]*)\"", key));
    std::smatch      match;
    if (!std::regex_search(line, match, pattern) || match.size() < 2)
        return std::nullopt;

    return match[1].str();
}

std::optional<int> jsonIntField(const std::string& line, const char* key) {
    const std::regex pattern(std::format("\"{}\"\\s*:\\s*(-?\\d+)", key));
    std::smatch      match;
    if (!std::regex_search(line, match, pattern) || match.size() < 2)
        return std::nullopt;

    try {
        return std::stoi(match[1].str());
    } catch (...) {
        return std::nullopt;
    }
}

std::optional<uintptr_t> jsonAddressField(const std::string& line, const char* key) {
    const auto value = jsonStringField(line, key);
    if (!value.has_value() || value->empty())
        return std::nullopt;

    try {
        return static_cast<uintptr_t>(std::stoull(*value, nullptr, 0));
    } catch (...) {
        return std::nullopt;
    }
}

uint64_t nowMs() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}
}

CSpawnManager::CSpawnManager() {
    refreshRuntimePaths();
    registerEventListeners();

    if (g_pEventLoopManager) {
        m_timer = makeShared<CEventLoopTimer>(std::optional<Time::steady_dur>{std::chrono::milliseconds{250}},
                                              [this](SP<CEventLoopTimer> self, void*) { onTimer(self); }, nullptr);
        g_pEventLoopManager->addTimer(m_timer);
        wakeTimer();
    }
}

CSpawnManager::~CSpawnManager() {
    m_destroying = true;

    if (m_timer)
        m_timer->cancel();

    if (m_timer && g_pEventLoopManager)
        g_pEventLoopManager->removeTimer(m_timer);

    m_timer.reset();
    m_openEarlyListener.reset();
    m_openListener.reset();

    for (const auto& [fd, _] : m_clients)
        close(fd);
    m_clients.clear();

    if (m_serverFD >= 0)
        close(m_serverFD);

    if (!m_socketPath.empty())
        std::filesystem::remove(m_socketPath);
}

void CSpawnManager::wakeTimer(std::chrono::milliseconds timeout) {
    if (!m_destroying && m_timer)
        m_timer->updateTimeout(timeout);
}

void CSpawnManager::registerEventListeners() {
    if (!Event::bus())
        return;

    m_openEarlyListener = Event::bus()->m_events.window.openEarly.listen([this](PHLWINDOW window) {
        handleWindow(window);
    });
    m_openListener = Event::bus()->m_events.window.open.listen([this](PHLWINDOW window) {
        handleWindow(window);
    });
}

void CSpawnManager::createSocket() {
    std::error_code ec;
    std::filesystem::create_directories(m_runtimeDir, ec);
    std::filesystem::remove(m_socketPath, ec);

    m_serverFD = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (m_serverFD < 0) {
        Log::logger->log(Log::ERR, "[hyprexpo] failed to create spawn socket");
        return;
    }

    sockaddr_un addr = {};
    addr.sun_family  = AF_UNIX;

    const auto socketString = m_socketPath.string();
    if (socketString.size() >= sizeof(addr.sun_path)) {
        Log::logger->log(Log::ERR, "[hyprexpo] spawn socket path too long");
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }

    std::memcpy(addr.sun_path, socketString.c_str(), socketString.size() + 1);

    if (bind(m_serverFD, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to bind spawn socket: {}", std::strerror(errno)));
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }

    if (listen(m_serverFD, 8) < 0) {
        Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to listen on spawn socket: {}", std::strerror(errno)));
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }
}

void CSpawnManager::refreshRuntimePaths() {
    const auto runtimeDir = hyprexpo::runtimeDirectory(std::getenv("XDG_RUNTIME_DIR"), spawnInstanceSignature());
    const auto socketPath = hyprexpo::spawnSocketPath(std::getenv("XDG_RUNTIME_DIR"), spawnInstanceSignature());

    if (runtimeDir == m_runtimeDir && socketPath == m_socketPath && m_serverFD >= 0)
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

void CSpawnManager::onTimer(SP<CEventLoopTimer> self) {
    if (m_destroying || !self)
        return;

    refreshRuntimePaths();

    if (m_serverFD >= 0) {
        acceptClients();
        readClients();
    }

    const auto hasActiveWork = !m_clients.empty() || !m_operations.empty();
    self->updateTimeout(hasActiveWork ? std::chrono::milliseconds{50} : std::chrono::milliseconds{250});
}

void CSpawnManager::acceptClients() {
    while (true) {
        const auto clientFD = accept4(m_serverFD, nullptr, nullptr, SOCK_NONBLOCK);
        if (clientFD < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK)
                break;

            Log::logger->log(Log::ERR, std::format("[hyprexpo] failed to accept spawn client: {}", std::strerror(errno)));
            break;
        }

        m_clients.emplace(clientFD, SClientState{
                                         .fd = clientFD,
                                     });
    }
}

void CSpawnManager::readClients() {
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
                sendError(fd, "command too long");
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

    for (const auto fd : disconnected)
        disconnectClient(fd);
}

void CSpawnManager::disconnectClient(int fd) {
    const auto it = m_clients.find(fd);
    if (it == m_clients.end())
        return;

    close(fd);
    m_clients.erase(it);
}

void CSpawnManager::handleClientLine(int fd, const std::string& line) {
    const auto op = jsonStringField(line, "op");
    if (!op.has_value()) {
        sendError(fd, "missing op");
        return;
    }

    if (*op == "ping") {
        sendOK(fd);
        return;
    }

    if (*op == "unwatch") {
        const auto operationID = jsonStringField(line, "operation_id");
        if (!operationID.has_value() || operationID->empty()) {
            sendError(fd, "operation_id is required");
            return;
        }

        m_operations.erase(*operationID);
        sendOK(fd);
        return;
    }

    if (*op == "watch") {
        const auto operationID     = jsonStringField(line, "operation_id");
        const auto workspaceID     = jsonIntField(line, "workspace_id");
        const auto rootPID         = jsonIntField(line, "root_pid");
        const auto targetMonitorID = jsonIntField(line, "target_monitor_id");
        const auto focusPolicy     = jsonStringField(line, "focus_policy");
        const auto originMonitorID = jsonIntField(line, "origin_monitor_id");
        const auto originWorkspaceID = jsonIntField(line, "origin_workspace_id");
        const auto originWindowAddress = jsonAddressField(line, "origin_window_address");

        if (!operationID.has_value() || operationID->empty()) {
            sendError(fd, "operation_id is required");
            return;
        }
        if (!workspaceID.has_value() || *workspaceID <= 0) {
            sendError(fd, "workspace_id must be positive");
            return;
        }
        if (!rootPID.has_value() || *rootPID <= 0) {
            sendError(fd, "root_pid must be positive");
            return;
        }
        if (!focusPolicy.has_value() || (*focusPolicy != "follow" && *focusPolicy != "preserve")) {
            sendError(fd, "focus_policy must be follow or preserve");
            return;
        }
        if (*focusPolicy == "preserve") {
            if (!originMonitorID.has_value() || *originMonitorID < 0) {
                sendError(fd, "origin_monitor_id is required for preserve");
                return;
            }
            if (!originWorkspaceID.has_value() || *originWorkspaceID <= 0) {
                sendError(fd, "origin_workspace_id is required for preserve");
                return;
            }
        }

        auto& operation           = m_operations[*operationID];
        operation.operationID     = *operationID;
        operation.workspaceID     = *workspaceID;
        operation.rootPID         = static_cast<pid_t>(*rootPID);
        operation.targetMonitorID = targetMonitorID.value_or(-1);
        operation.focusPolicy     = *focusPolicy == "preserve" ? ESpawnFocusPolicy::Preserve : ESpawnFocusPolicy::Follow;
        operation.originMonitorID = originMonitorID.value_or(-1);
        operation.originWorkspaceID = originWorkspaceID.value_or(-1);
        operation.originWindowAddress = originWindowAddress;
        operation.createdAtMs     = nowMs();
        operation.movedWindowAddresses.clear();
        operation.restoredWindowAddresses.clear();
        sendOK(fd);
        return;
    }

    sendError(fd, std::format("unknown op: {}", *op));
}

bool CSpawnManager::sendLine(int fd, const std::string& payload) {
    if (fd < 0)
        return false;

    const ssize_t written = send(fd, payload.c_str(), payload.size(), MSG_NOSIGNAL);
    if (written < 0)
        return errno == EAGAIN || errno == EWOULDBLOCK;

    return true;
}

bool CSpawnManager::sendOK(int fd) {
    return sendLine(fd, "{\"ok\":true,\"result\":{}}\n");
}

bool CSpawnManager::sendError(int fd, std::string_view message) {
    return sendLine(fd, std::format("{{\"ok\":false,\"error\":{{\"message\":\"{}\"}}}}\n", hyprexpo::escapeJSON(message)));
}

void CSpawnManager::handleWindow(PHLWINDOW window) {
    if (!window || m_operations.empty())
        return;

    const auto windowPID = window->getPID();
    if (windowPID <= 0)
        return;

    std::vector<SSpawnOperation*> ordered;
    ordered.reserve(m_operations.size());
    for (auto& [_, operation] : m_operations)
        ordered.push_back(&operation);

    std::sort(ordered.begin(), ordered.end(), [](const auto* left, const auto* right) {
        return left->createdAtMs > right->createdAtMs;
    });

    const auto windowAddress = reinterpret_cast<uintptr_t>(window.get());
    for (auto* operation : ordered) {
        if (!operation)
            continue;

        if (operation->movedWindowAddresses.contains(windowAddress))
            return;

        if (!matchesOperation(*operation, windowPID))
            continue;

        const auto workspace = ensureWorkspace(*operation);
        if (!workspace)
            return;

        if (operation->focusPolicy == ESpawnFocusPolicy::Preserve) {
            window->m_noInitialFocus = true;
            window->m_suppressedEvents |= Desktop::View::SUPPRESS_ACTIVATE;
            window->m_suppressedEvents |= Desktop::View::SUPPRESS_ACTIVATE_FOCUSONLY;
        }

        g_pCompositor->moveWindowToWorkspaceSafe(window, workspace);
        operation->movedWindowAddresses.insert(windowAddress);
        if (operation->focusPolicy == ESpawnFocusPolicy::Preserve &&
            !operation->restoredWindowAddresses.contains(windowAddress)) {
            restoreOriginalFocus(*operation, window);
            operation->restoredWindowAddresses.insert(windowAddress);
        }
        return;
    }
}

bool CSpawnManager::matchesOperation(const SSpawnOperation& operation, pid_t windowPID) const {
    if (operation.rootPID <= 0 || windowPID <= 0)
        return false;

    if (windowPID == operation.rootPID)
        return true;

    return isDescendantProcess(windowPID, operation.rootPID);
}

bool CSpawnManager::isDescendantProcess(pid_t pid, pid_t ancestorPID) const {
    std::unordered_set<pid_t> seen;
    pid_t current = pid;

    while (current > 1 && !seen.contains(current)) {
        seen.insert(current);
        const auto parent = readParentPID(current);
        if (parent <= 0)
            return false;
        if (parent == ancestorPID)
            return true;
        current = parent;
    }

    return false;
}

pid_t CSpawnManager::readParentPID(pid_t pid) const {
    std::ifstream stream(std::format("/proc/{}/status", pid));
    if (!stream.is_open())
        return -1;

    std::string line;
    while (std::getline(stream, line)) {
        if (!line.starts_with("PPid:"))
            continue;

        try {
            return static_cast<pid_t>(std::stoi(line.substr(5)));
        } catch (...) {
            return -1;
        }
    }

    return -1;
}

PHLWORKSPACE CSpawnManager::ensureWorkspace(const SSpawnOperation& operation) const {
    auto workspace = g_pCompositor->getWorkspaceByID(operation.workspaceID);
    if (workspace)
        return workspace;

    auto monitor = operation.targetMonitorID >= 0 ? g_pCompositor->getMonitorFromID(operation.targetMonitorID) : nullptr;
    if (!monitor)
        monitor = Desktop::focusState()->monitor();
    if (!monitor)
        return nullptr;

    return g_pCompositor->createNewWorkspace(operation.workspaceID, monitor->m_id, "", true);
}

PHLWINDOW CSpawnManager::findWindowByAddress(uintptr_t address) const {
    if (!g_pCompositor || address == 0)
        return nullptr;

    for (const auto& window : g_pCompositor->m_windows) {
        if (window && reinterpret_cast<uintptr_t>(window.get()) == address)
            return window;
    }

    return nullptr;
}

bool CSpawnManager::restoreOriginalFocus(const SSpawnOperation& operation, PHLWINDOW spawnedWindow) const {
    if (!g_pCompositor)
        return false;

    const auto originMonitor =
        operation.originMonitorID >= 0 ? g_pCompositor->getMonitorFromID(operation.originMonitorID) : nullptr;
    const auto originWorkspace =
        operation.originWorkspaceID > 0 ? g_pCompositor->getWorkspaceByID(operation.originWorkspaceID) : nullptr;
    if (originMonitor && originWorkspace)
        originMonitor->changeWorkspace(originWorkspace, false, true, true);

    const auto originWindow = operation.originWindowAddress.has_value() ? findWindowByAddress(*operation.originWindowAddress) : nullptr;
    if (originWindow && originWindow != spawnedWindow) {
        Desktop::focusState()->fullWindowFocus(originWindow, Desktop::FOCUS_REASON_DESKTOP_STATE_CHANGE);
        return true;
    }

    if (originMonitor) {
        Desktop::focusState()->rawMonitorFocus(originMonitor);
        return true;
    }

    return false;
}

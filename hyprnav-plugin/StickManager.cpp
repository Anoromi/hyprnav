#include "StickManager.hpp"
#include "globals.hpp"

#define private public
#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/desktop/state/GlobalWindowController.hpp>
#include <hyprland/src/desktop/state/WindowState.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/state/MonitorState.hpp>
#include <hyprland/src/state/WorkspaceState.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/output/Monitor.hpp>
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
#include <random>
#include <regex>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static constexpr size_t   MAX_CLIENT_LINE = 4096;
static constexpr uint64_t REAP_INTERVAL_MS = 2000;

namespace {
const char* instanceSignature() {
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
    } catch (...) { return std::nullopt; }
}

std::optional<uintptr_t> jsonAddressField(const std::string& line, const char* key) {
    const auto value = jsonStringField(line, key);
    if (!value.has_value() || value->empty())
        return std::nullopt;
    try {
        return static_cast<uintptr_t>(std::stoull(*value, nullptr, 0));
    } catch (...) { return std::nullopt; }
}

uint64_t nowMs() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::system_clock::now().time_since_epoch()).count();
}

std::string randomInstanceID() {
    std::random_device rd;
    return std::format("{:08x}{:08x}", rd(), rd());
}

std::string joinQuoted(const auto& items) {
    std::string out;
    for (const auto& item : items) {
        if (!out.empty())
            out += ",";
        out += "\"" + hyprnav_plugin::escapeJSON(std::format("{}", item)) + "\"";
    }
    return out;
}
}

CStickManager::CStickManager() : m_instanceID(randomInstanceID()) {
    refreshRuntimePaths();
    registerEventListeners();

    if (g_pEventLoopManager) {
        m_timer = makeShared<CEventLoopTimer>(std::optional<Time::steady_dur>{std::chrono::milliseconds{250}},
                                              [this](SP<CEventLoopTimer> self, void*) { onTimer(self); }, nullptr);
        g_pEventLoopManager->addTimer(m_timer);
        wakeTimer();
    }
    Log::logger->log(Log::INFO, std::format("[hyprnav-plugin] stick manager up, instance {}", m_instanceID));
}

CStickManager::~CStickManager() {
    m_destroying = true;
    if (m_timer)
        m_timer->cancel();
    if (m_timer && g_pEventLoopManager)
        g_pEventLoopManager->removeTimer(m_timer);
    m_timer.reset();
    m_openEarlyListener.reset();
    m_openListener.reset();
    m_closeListener.reset();
    for (const auto& [fd, _] : m_clients)
        close(fd);
    m_clients.clear();
    if (m_serverFD >= 0)
        close(m_serverFD);
    if (!m_socketPath.empty())
        std::filesystem::remove(m_socketPath);
}

void CStickManager::wakeTimer(std::chrono::milliseconds timeout) {
    if (!m_destroying && m_timer)
        m_timer->updateTimeout(timeout);
}

void CStickManager::registerEventListeners() {
    m_openEarlyListener = Event::bus()->m_events.window.openEarly.listen([this](PHLWINDOW window) { onWindowOpenEarly(window); });
    m_openListener      = Event::bus()->m_events.window.open.listen([this](PHLWINDOW window) { onWindowOpen(window); });
    m_closeListener     = Event::bus()->m_events.window.close.listen([this](PHLWINDOW window) { onWindowClose(window); });
}

// ---------------------------------------------------------------- socket

void CStickManager::createSocket() {
    std::error_code ec;
    std::filesystem::create_directories(m_runtimeDir, ec);
    std::filesystem::remove(m_socketPath, ec);

    m_serverFD = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (m_serverFD < 0) {
        Log::logger->log(Log::ERR, "[hyprnav-plugin] failed to create spawn socket");
        return;
    }
    sockaddr_un addr = {};
    addr.sun_family  = AF_UNIX;
    const auto socketString = m_socketPath.string();
    if (socketString.size() >= sizeof(addr.sun_path)) {
        Log::logger->log(Log::ERR, "[hyprnav-plugin] spawn socket path too long");
        close(m_serverFD);
        m_serverFD = -1;
        return;
    }
    std::memcpy(addr.sun_path, socketString.c_str(), socketString.size() + 1);
    if (bind(m_serverFD, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0 || listen(m_serverFD, 8) < 0) {
        Log::logger->log(Log::ERR, std::format("[hyprnav-plugin] spawn socket bind/listen failed: {}", std::strerror(errno)));
        close(m_serverFD);
        m_serverFD = -1;
    }
}

void CStickManager::refreshRuntimePaths() {
    const auto runtimeDir = hyprnav_plugin::runtimeDirectory(std::getenv("XDG_RUNTIME_DIR"), instanceSignature());
    const auto socketPath = hyprnav_plugin::spawnSocketPath(std::getenv("XDG_RUNTIME_DIR"), instanceSignature());
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

void CStickManager::onTimer(SP<CEventLoopTimer> self) {
    if (m_destroying || !self)
        return;
    refreshRuntimePaths();
    if (m_serverFD >= 0) {
        acceptClients();
        readClients();
    }
    const auto now = nowMs();
    if (now - m_lastReapMs >= REAP_INTERVAL_MS) {
        m_lastReapMs = now;
        reapRoots();
    }
    const auto busy = !m_clients.empty();
    self->updateTimeout(busy ? std::chrono::milliseconds{50} : std::chrono::milliseconds{250});
}

void CStickManager::acceptClients() {
    while (true) {
        const auto clientFD = accept4(m_serverFD, nullptr, nullptr, SOCK_NONBLOCK);
        if (clientFD < 0) {
            if (errno != EAGAIN && errno != EWOULDBLOCK)
                Log::logger->log(Log::ERR, std::format("[hyprnav-plugin] accept failed: {}", std::strerror(errno)));
            break;
        }
        m_clients.emplace(clientFD, SClientState{.fd = clientFD});
    }
}

void CStickManager::readClients() {
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
                if (errno != EAGAIN && errno != EWOULDBLOCK)
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

void CStickManager::disconnectClient(int fd) {
    const auto it = m_clients.find(fd);
    if (it == m_clients.end())
        return;
    close(fd);
    m_clients.erase(it);
}

bool CStickManager::sendLine(int fd, const std::string& payload) {
    if (fd < 0)
        return false;
    const ssize_t written = send(fd, payload.c_str(), payload.size(), MSG_NOSIGNAL);
    if (written < 0)
        return errno == EAGAIN || errno == EWOULDBLOCK;
    return true;
}

bool CStickManager::sendOK(int fd) {
    return sendLine(fd, "{\"ok\":true,\"result\":{}}\n");
}

bool CStickManager::sendResult(int fd, const std::string& resultJSON) {
    return sendLine(fd, std::format("{{\"ok\":true,\"result\":{}}}\n", resultJSON));
}

bool CStickManager::sendError(int fd, std::string_view message) {
    return sendLine(fd, std::format("{{\"ok\":false,\"error\":{{\"message\":\"{}\"}}}}\n", hyprnav_plugin::escapeJSON(message)));
}

std::string CStickManager::describe(const SStickRoot& root) const {
    std::vector<int> windowWorkspaces;
    for (const auto address : root.windows) {
        const auto window = findWindowByAddress(address);
        if (window && window->m_workspace)
            windowWorkspaces.push_back(window->workspaceID());
    }
    std::string pids;
    for (const auto pid : root.knownPIDs) {
        if (!pids.empty())
            pids += ",";
        pids += std::format("{}", pid);
    }
    std::string wss;
    for (const auto id : windowWorkspaces) {
        if (!wss.empty())
            wss += ",";
        wss += std::format("{}", id);
    }
    return std::format("{{\"stick_id\":\"{}\",\"workspace_id\":{},\"root_pid\":{},\"root_alive\":{},\"pids\":[{}],\"classes\":[{}],\"windows\":{},\"window_workspaces\":[{}]}}",
                       hyprnav_plugin::escapeJSON(root.stickID), root.workspaceID, root.rootPID, pidAlive(root.rootPID) ? "true" : "false", pids,
                       joinQuoted(root.classes), root.windows.size(), wss);
}

void CStickManager::handleClientLine(int fd, const std::string& line) {
    const auto op = jsonStringField(line, "op");
    if (!op.has_value()) {
        sendError(fd, "missing op");
        return;
    }

    if (*op == "ping") {
        sendResult(fd, std::format("{{\"instance\":\"{}\",\"sticks\":{}}}", m_instanceID, m_roots.size()));
        return;
    }

    if (*op == "sync") {
        std::vector<std::string> active;
        for (const auto& [id, _] : m_roots)
            active.push_back(id);
        const auto dropped = m_dropped;
        m_dropped.clear();
        sendResult(fd, std::format("{{\"instance\":\"{}\",\"active\":[{}],\"dropped\":[{}]}}", m_instanceID, joinQuoted(active), joinQuoted(dropped)));
        return;
    }

    if (*op == "list") {
        std::string items;
        for (const auto& [_, root] : m_roots) {
            if (!items.empty())
                items += ",";
            items += describe(root);
        }
        sendResult(fd, std::format("{{\"instance\":\"{}\",\"sticks\":[{}]}}", m_instanceID, items));
        return;
    }

    if (*op == "unstick" || *op == "unwatch") {
        const auto id = jsonStringField(line, "stick_id").value_or(jsonStringField(line, "operation_id").value_or(""));
        if (id.empty()) {
            sendError(fd, "stick_id is required");
            return;
        }
        const auto it = m_roots.find(id);
        if (it != m_roots.end()) {
            for (const auto address : it->second.windows)
                m_windowToStick.erase(address);
            m_roots.erase(it);
        }
        sendOK(fd);
        return;
    }

    if (*op == "move") {
        const auto id          = jsonStringField(line, "stick_id").value_or("");
        const auto workspaceID = jsonIntField(line, "workspace_id");
        const auto it          = m_roots.find(id);
        if (it == m_roots.end()) {
            sendError(fd, "unknown stick");
            return;
        }
        if (!workspaceID.has_value() || *workspaceID <= 0) {
            sendError(fd, "workspace_id must be positive");
            return;
        }
        it->second.workspaceID = *workspaceID;
        for (const auto address : std::vector<uintptr_t>(it->second.windows.begin(), it->second.windows.end())) {
            const auto window = findWindowByAddress(address);
            if (window)
                place(it->second, window, false);
        }
        sendOK(fd);
        return;
    }

    if (*op == "stick" || *op == "watch") {
        const auto id              = jsonStringField(line, "stick_id").value_or(jsonStringField(line, "operation_id").value_or(""));
        const auto workspaceID     = jsonIntField(line, "workspace_id");
        const auto rootPID         = jsonIntField(line, "root_pid");
        const auto targetMonitorID = jsonIntField(line, "target_monitor_id");
        const auto focusPolicy     = jsonStringField(line, "focus_policy").value_or("preserve");

        if (id.empty()) {
            sendError(fd, "stick_id is required");
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
        if (focusPolicy != "follow" && focusPolicy != "preserve") {
            sendError(fd, "focus_policy must be follow or preserve");
            return;
        }

        const bool existed        = m_roots.contains(id);
        auto&      root           = m_roots[id];
        root.stickID              = id;
        root.workspaceID          = *workspaceID;
        root.rootPID              = static_cast<pid_t>(*rootPID);
        root.targetMonitorID      = targetMonitorID.value_or(-1);
        root.focusPolicy          = focusPolicy == "follow" ? EFocusPolicy::Follow : EFocusPolicy::Preserve;
        root.originMonitorID      = jsonIntField(line, "origin_monitor_id").value_or(-1);
        root.originWorkspaceID    = jsonIntField(line, "origin_workspace_id").value_or(-1);
        root.originWindowAddress  = jsonAddressField(line, "origin_window_address");
        if (!existed) {
            root.createdAtMs = nowMs();
            root.followConsumed = jsonIntField(line, "replay").value_or(0) != 0; // replayed roots never steal focus
        }
        root.knownPIDs.insert(root.rootPID);

        // Windows that already exist (replay after a plugin reload, or a
        // tree that mapped before the daemon told us) are attributed now.
        attributeExistingWindows(root);
        sendOK(fd);
        return;
    }

    sendError(fd, std::format("unknown op: {}", *op));
}

// ---------------------------------------------------------------- roots

void CStickManager::reapRoots() {
    std::vector<std::string> dead;
    for (auto& [id, root] : m_roots) {
        // Forget windows that no longer exist.
        for (auto it = root.windows.begin(); it != root.windows.end();) {
            if (!findWindowByAddress(*it)) {
                m_windowToStick.erase(*it);
                it = root.windows.erase(it);
            } else
                ++it;
        }
        if (!root.windows.empty())
            continue;
        bool anyAlive = false;
        for (const auto pid : root.knownPIDs) {
            if (pidAlive(pid)) {
                anyAlive = true;
                break;
            }
        }
        if (!anyAlive)
            dead.push_back(id);
    }
    for (const auto& id : dead) {
        Log::logger->log(Log::INFO, std::format("[hyprnav-plugin] stick {} released: tree and windows gone", id));
        m_roots.erase(id);
        m_dropped.push_back(id);
    }
}

bool CStickManager::pidAlive(pid_t pid) {
    if (pid <= 0)
        return false;
    // A zombie still has /proc/<pid>; treat state Z as dead.
    std::ifstream stream(std::format("/proc/{}/stat", pid));
    if (!stream.is_open())
        return false;
    std::string content;
    std::getline(stream, content);
    const auto close = content.rfind(')');
    if (close == std::string::npos || close + 2 >= content.size())
        return true;
    return content[close + 2] != 'Z' && content[close + 2] != 'X';
}

pid_t CStickManager::readParentPID(pid_t pid) const {
    std::ifstream stream(std::format("/proc/{}/status", pid));
    if (!stream.is_open())
        return -1;
    std::string line;
    while (std::getline(stream, line)) {
        if (!line.starts_with("PPid:"))
            continue;
        try {
            return static_cast<pid_t>(std::stoi(line.substr(5)));
        } catch (...) { return -1; }
    }
    return -1;
}

bool CStickManager::isDescendantProcess(pid_t pid, pid_t ancestorPID, std::set<pid_t>& seenChain) const {
    pid_t current = pid;
    while (current > 1 && !seenChain.contains(current)) {
        seenChain.insert(current);
        const auto parent = readParentPID(current);
        if (parent <= 0)
            return false;
        if (parent == ancestorPID)
            return true;
        current = parent;
    }
    return false;
}

bool CStickManager::pidInTree(SStickRoot& root, pid_t pid) {
    if (pid <= 0)
        return false;
    if (root.knownPIDs.contains(pid))
        return true;
    // Walk up from pid; if we hit any known PID of this tree, the whole
    // chain belongs to it. Remember the chain so later lookups are cheap and
    // so the tree survives the root exiting.
    std::set<pid_t> chain;
    pid_t           current = pid;
    while (current > 1 && !chain.contains(current)) {
        chain.insert(current);
        const auto parent = readParentPID(current);
        if (parent <= 0)
            return false;
        if (root.knownPIDs.contains(parent)) {
            root.knownPIDs.insert(chain.begin(), chain.end());
            return true;
        }
        current = parent;
    }
    return false;
}

CStickManager::SStickRoot* CStickManager::rootForWindow(uintptr_t address) {
    const auto it = m_windowToStick.find(address);
    if (it == m_windowToStick.end())
        return nullptr;
    const auto rootIt = m_roots.find(it->second);
    if (rootIt == m_roots.end()) {
        m_windowToStick.erase(it);
        return nullptr;
    }
    return &rootIt->second;
}

// Attribution order:
//  1. PID is the root or a descendant.
//  2. xdg or X11 parent toplevel is a stuck window.
//  3. Same PID as a stuck window.
//  4. Class known to exactly one root, and no window of that class exists
//     outside any stick.
CStickManager::SStickRoot* CStickManager::attribute(PHLWINDOW window, bool allowHeuristics) {
    if (!window || m_roots.empty())
        return nullptr;

    const auto address = reinterpret_cast<uintptr_t>(window.get());
    if (auto* known = rootForWindow(address))
        return known;

    const auto pid = window->getPID();

    // 1. process tree
    for (auto& [_, root] : m_roots)
        if (pidInTree(root, pid))
            return &root;

    // 2. parent toplevel
    PHLWINDOW parent = window->m_isX11 ? window->x11Parent() : window->parent();
    if (parent) {
        if (auto* root = rootForWindow(reinterpret_cast<uintptr_t>(parent.get())))
            return root;
    }

    if (!allowHeuristics)
        return nullptr;

    // 3. same PID as an attributed window
    if (pid > 0) {
        for (auto& [_, root] : m_roots) {
            for (const auto stuckAddress : root.windows) {
                const auto stuck = findWindowByAddress(stuckAddress);
                if (stuck && stuck->getPID() == pid)
                    return &root;
            }
        }
    }

    // 4. class heuristic
    const auto cls = window->m_initialClass.empty() ? window->m_class : window->m_initialClass;
    if (cls.empty())
        return nullptr;
    SStickRoot* candidate = nullptr;
    for (auto& [_, root] : m_roots) {
        if (!root.classes.contains(cls))
            continue;
        if (candidate)
            return nullptr; // ambiguous
        candidate = &root;
    }
    if (!candidate || candidate->windows.empty())
        return nullptr;
    for (const auto& other : Desktop::windowState()->windows()) {
        if (!other || other == window || !other->m_isMapped)
            continue;
        const auto otherClass = other->m_initialClass.empty() ? other->m_class : other->m_initialClass;
        if (otherClass != cls)
            continue;
        if (!m_windowToStick.contains(reinterpret_cast<uintptr_t>(other.get())))
            return nullptr; // class also lives outside sticks
    }
    return candidate;
}

void CStickManager::adopt(SStickRoot& root, PHLWINDOW window) {
    const auto address = reinterpret_cast<uintptr_t>(window.get());
    root.windows.insert(address);
    m_windowToStick[address] = root.stickID;
    const auto pid = window->getPID();
    if (pid > 0)
        root.knownPIDs.insert(pid);
    const auto cls = window->m_initialClass.empty() ? window->m_class : window->m_initialClass;
    if (!cls.empty())
        root.classes.insert(cls);
}

void CStickManager::place(SStickRoot& root, PHLWINDOW window, bool early) {
    const auto workspace = ensureWorkspace(root);
    if (!workspace)
        return;

    const bool follow = root.focusPolicy == EFocusPolicy::Follow && !root.followConsumed && window->getPID() == root.rootPID;

    if (!follow) {
        window->m_noInitialFocus = true;
        window->m_suppressedEvents |= Desktop::View::SUPPRESS_ACTIVATE;
        window->m_suppressedEvents |= Desktop::View::SUPPRESS_ACTIVATE_FOCUSONLY;
        window->m_isUrgent = false;
    }

    if (early) {
        // Before the window is mapped only the target matters; the layout
        // reads m_workspace when it maps.
        if (window->m_workspace != workspace) {
            window->m_workspace = workspace;
            window->m_monitor   = workspace->m_monitor;
        }
        return;
    }

    if (window->m_workspace != workspace)
        Desktop::globalWindowController()->moveWindowToWorkspace(window, workspace);

    if (follow) {
        root.followConsumed = true;
        Desktop::focusState()->fullWindowFocus(window, Desktop::FOCUS_REASON_NEW_WINDOW);
        return;
    }

    // Silent: whatever focus the map handler gave the new window goes back.
    const auto focused = Desktop::focusState()->window();
    if (focused == window)
        restoreOriginalFocus(root, window);
}

void CStickManager::attributeExistingWindows(SStickRoot& root) {
    for (const auto& window : Desktop::windowState()->windows()) {
        if (!window || !window->m_isMapped)
            continue;
        const auto address = reinterpret_cast<uintptr_t>(window.get());
        if (m_windowToStick.contains(address))
            continue;
        if (!pidInTree(root, window->getPID()))
            continue;
        adopt(root, window);
        // Existing windows of a replayed stick are moved but never focused.
        root.followConsumed = true;
        place(root, window, false);
    }
}

void CStickManager::onWindowOpenEarly(PHLWINDOW window) {
    if (!window || m_roots.empty())
        return;
    auto* root = attribute(window, true);
    if (!root)
        return;
    adopt(*root, window);
    place(*root, window, true);
}

void CStickManager::onWindowOpen(PHLWINDOW window) {
    if (!window || m_roots.empty())
        return;
    auto* root = attribute(window, true);
    if (!root)
        return;
    adopt(*root, window);
    place(*root, window, false);
}

void CStickManager::onWindowClose(PHLWINDOW window) {
    if (!window)
        return;
    const auto address = reinterpret_cast<uintptr_t>(window.get());
    const auto it      = m_windowToStick.find(address);
    if (it == m_windowToStick.end())
        return;
    if (const auto rootIt = m_roots.find(it->second); rootIt != m_roots.end())
        rootIt->second.windows.erase(address);
    m_windowToStick.erase(it);
}

// ---------------------------------------------------------------- helpers

PHLWORKSPACE CStickManager::ensureWorkspace(const SStickRoot& root) const {
    auto workspace = State::workspaceState()->query().id(root.workspaceID).run();
    if (workspace)
        return workspace;
    auto monitor = root.targetMonitorID >= 0 ? State::monitorState()->query().id(root.targetMonitorID).run() : nullptr;
    if (!monitor)
        monitor = Desktop::focusState()->monitor();
    if (!monitor)
        return nullptr;
    return State::workspaceState()->create(root.workspaceID, monitor->m_id, "", true);
}

PHLWINDOW CStickManager::findWindowByAddress(uintptr_t address) const {
    if (!g_pCompositor || address == 0)
        return nullptr;
    for (const auto& window : Desktop::windowState()->windows())
        if (window && reinterpret_cast<uintptr_t>(window.get()) == address)
            return window;
    return nullptr;
}

bool CStickManager::restoreOriginalFocus(const SStickRoot& root, PHLWINDOW spawnedWindow) const {
    if (!g_pCompositor)
        return false;
    const auto originMonitor   = root.originMonitorID >= 0 ? State::monitorState()->query().id(root.originMonitorID).run() : nullptr;
    const auto originWorkspace = root.originWorkspaceID > 0 ? State::workspaceState()->query().id(root.originWorkspaceID).run() : nullptr;
    if (originMonitor && originWorkspace && originMonitor->m_activeWorkspace != originWorkspace)
        originMonitor->changeWorkspace(originWorkspace, false, true, true);

    const auto originWindow = root.originWindowAddress.has_value() ? findWindowByAddress(*root.originWindowAddress) : nullptr;
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

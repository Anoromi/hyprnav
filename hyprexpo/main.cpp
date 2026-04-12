#define WLR_USE_UNSTABLE

#include <unistd.h>

#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/config/ConfigManager.hpp>
#include <hyprland/src/desktop/DesktopTypes.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/managers/input/trackpad/GestureTypes.hpp>
#include <hyprland/src/managers/input/trackpad/TrackpadGestures.hpp>
#include <hyprutils/string/ConstVarList.hpp>
using namespace Hyprutils::String;

#include "globals.hpp"
#include "overview.hpp"
#include "ExpoGesture.hpp"
#include "PreviewManager.hpp"
#include "SpawnManager.hpp"

// Methods
inline CFunctionHook* g_pRenderWorkspaceHook = nullptr;
inline CFunctionHook* g_pAddDamageHookA      = nullptr;
inline CFunctionHook* g_pAddDamageHookB      = nullptr;
typedef void (*origRenderWorkspace)(void*, PHLMONITOR, PHLWORKSPACE, timespec*, const CBox&);
typedef void (*origAddDamageA)(void*, const CBox&);
typedef void (*origAddDamageB)(void*, const pixman_region32_t*);

static bool g_unloading = false;

static void cleanupHooks() {
    if (g_pRenderWorkspaceHook) {
        g_pRenderWorkspaceHook->unhook();
        g_pRenderWorkspaceHook = nullptr;
    }

    if (g_pAddDamageHookA) {
        g_pAddDamageHookA->unhook();
        g_pAddDamageHookA = nullptr;
    }

    if (g_pAddDamageHookB) {
        g_pAddDamageHookB->unhook();
        g_pAddDamageHookB = nullptr;
    }
}

static std::string pluginClientHash() {
    static const auto stripPatch = [](const char* ver) -> std::string {
        std::string_view v = ver;
        if (!v.contains('.'))
            return std::string{v};

        return std::string{v.substr(0, v.find_last_of('.'))};
    };

    static const std::string ver = (std::string{GIT_COMMIT_HASH} + "_aq_" + stripPatch(AQUAMARINE_VERSION) + "_hu_" + stripPatch(HYPRUTILS_VERSION) +
                                    "_hg_" + stripPatch(HYPRGRAPHICS_VERSION) + "_hc_" + stripPatch(HYPRCURSOR_VERSION) + "_hlg_" + stripPatch(HYPRLANG_VERSION));
    return ver;
}

// Do NOT change this function.
APICALL EXPORT std::string PLUGIN_API_VERSION() {
    return HYPRLAND_API_VERSION;
}

static bool       renderingOverview = false;

const std::string KEYWORD_EXPO_GESTURE = "hyprexpo-gesture";

//
static void hkRenderWorkspace(void* thisptr, PHLMONITOR pMonitor, PHLWORKSPACE pWorkspace, timespec* now, const CBox& geometry) {
    if (!g_pOverview || renderingOverview || g_pOverview->blockOverviewRendering || g_pOverview->pMonitor != pMonitor)
        ((origRenderWorkspace)(g_pRenderWorkspaceHook->m_original))(thisptr, pMonitor, pWorkspace, now, geometry);
    else
        g_pOverview->render();
}

static void hkAddDamageA(void* thisptr, const CBox& box) {
    const auto PMONITOR = (CMonitor*)thisptr;

    if (g_pPreviewManager && PMONITOR->m_self == Desktop::focusState()->monitor())
        g_pPreviewManager->onWorkspaceDamaged(PMONITOR->activeWorkspaceID());

    if (!g_pOverview || g_pOverview->pMonitor != PMONITOR->m_self || g_pOverview->blockDamageReporting) {
        ((origAddDamageA)g_pAddDamageHookA->m_original)(thisptr, box);
        return;
    }

    g_pOverview->onDamageReported();
}

static void hkAddDamageB(void* thisptr, const pixman_region32_t* rg) {
    const auto PMONITOR = (CMonitor*)thisptr;

    if (g_pPreviewManager && PMONITOR->m_self == Desktop::focusState()->monitor())
        g_pPreviewManager->onWorkspaceDamaged(PMONITOR->activeWorkspaceID());

    if (!g_pOverview || g_pOverview->pMonitor != PMONITOR->m_self || g_pOverview->blockDamageReporting) {
        ((origAddDamageB)g_pAddDamageHookB->m_original)(thisptr, rg);
        return;
    }

    g_pOverview->onDamageReported();
}

static SDispatchResult onExpoDispatcher(std::string arg) {

    if (g_pOverview && g_pOverview->m_isSwiping)
        return {.success = false, .error = "already swiping"};

    if (arg == "select") {
        if (g_pOverview) {
            g_pOverview->selectHoveredWorkspace();
            g_pOverview->close();
        }
        return {};
    }
    if (arg == "toggle") {
        if (g_pOverview)
            g_pOverview->close();
        else {
            renderingOverview = true;
            g_pOverview       = std::make_unique<COverview>(Desktop::focusState()->monitor()->m_activeWorkspace);
            renderingOverview = false;
        }
        return {};
    }

    if (arg == "off" || arg == "close" || arg == "disable") {
        if (g_pOverview)
            g_pOverview->close();
        return {};
    }

    if (g_pOverview)
        return {};

    renderingOverview = true;
    g_pOverview       = std::make_unique<COverview>(Desktop::focusState()->monitor()->m_activeWorkspace);
    renderingOverview = false;
    return {};
}

static SDispatchResult onPreviewDispatcher(std::string arg) {
    if (!g_pPreviewManager)
        return {.success = false, .error = "preview manager unavailable"};

    CConstVarList ids(arg);
    std::vector<int> workspaceIDs;
    workspaceIDs.reserve(ids.size());

    for (size_t i = 0; i < ids.size(); ++i) {
        try {
            const auto id = std::stoi(std::string{ids[i]});
            if (id > 0)
                workspaceIDs.push_back(id);
        } catch (...) {
            return {.success = false, .error = std::format("invalid workspace id: {}", ids[i])};
        }
    }

    if (workspaceIDs.empty())
        return {.success = false, .error = "workspace ids required"};

    g_pPreviewManager->requestRefresh(workspaceIDs);
    return {};
}

static void failNotif(const std::string& reason) {
    HyprlandAPI::addNotification(PHANDLE, "[hyprexpo] Failure in initialization: " + reason, CHyprColor{1.0, 0.2, 0.2, 1.0}, 5000);
}

static Hyprlang::CParseResult expoGestureKeyword(const char* LHS, const char* RHS) {
    Hyprlang::CParseResult result;

    if (g_unloading)
        return result;

    CConstVarList             data(RHS);

    size_t                    fingerCount = 0;
    eTrackpadGestureDirection direction   = TRACKPAD_GESTURE_DIR_NONE;

    try {
        fingerCount = std::stoul(std::string{data[0]});
    } catch (...) {
        result.setError(std::format("Invalid value {} for finger count", data[0]).c_str());
        return result;
    }

    if (fingerCount <= 1 || fingerCount >= 10) {
        result.setError(std::format("Invalid value {} for finger count", data[0]).c_str());
        return result;
    }

    direction = g_pTrackpadGestures->dirForString(data[1]);

    if (direction == TRACKPAD_GESTURE_DIR_NONE) {
        result.setError(std::format("Invalid direction: {}", data[1]).c_str());
        return result;
    }

    int      startDataIdx   = 2;
    uint32_t modMask        = 0;
    float    deltaScale     = 1.F;
    bool     disableInhibit = false;

    for (const auto arg : std::string(LHS).substr(KEYWORD_EXPO_GESTURE.size())) {
        switch (arg) {
            case 'p': disableInhibit = true; break;
            default: result.setError("hyprexpo-gesture: invalid flag"); return result;
        }
    }

    while (true) {

        if (data[startDataIdx].starts_with("mod:")) {
            modMask = g_pKeybindManager->stringToModMask(std::string{data[startDataIdx].substr(4)});
            startDataIdx++;
            continue;
        } else if (data[startDataIdx].starts_with("scale:")) {
            try {
                deltaScale = std::clamp(std::stof(std::string{data[startDataIdx].substr(6)}), 0.1F, 10.F);
                startDataIdx++;
                continue;
            } catch (...) {
                result.setError(std::format("Invalid delta scale: {}", std::string{data[startDataIdx].substr(6)}).c_str());
                return result;
            }
        }

        break;
    }

    std::expected<void, std::string> resultFromGesture;

    if (data[startDataIdx] == "expo")
        resultFromGesture = g_pTrackpadGestures->addGesture(makeUnique<CExpoGesture>(), fingerCount, direction, modMask, deltaScale, disableInhibit);
    else if (data[startDataIdx] == "unset")
        resultFromGesture = g_pTrackpadGestures->removeGesture(fingerCount, direction, modMask, deltaScale, disableInhibit);
    else {
        result.setError(std::format("Invalid gesture: {}", data[startDataIdx]).c_str());
        return result;
    }

    if (!resultFromGesture) {
        result.setError(resultFromGesture.error().c_str());
        return result;
    }

    return result;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    PHANDLE = handle;

    const std::string HASH        = __hyprland_api_get_hash();
    const std::string CLIENT_HASH = pluginClientHash();

    if (HASH != CLIENT_HASH) {
        failNotif(std::format("Version mismatch host={} client={}", HASH, CLIENT_HASH));
        throw std::runtime_error(std::format("[he] Version mismatch host={} client={}", HASH, CLIENT_HASH));
    }

    auto FNS = HyprlandAPI::findFunctionsByName(PHANDLE, "renderWorkspace");
    if (FNS.empty()) {
        failNotif("no fns for hook renderWorkspace");
        throw std::runtime_error("[he] No fns for hook renderWorkspace");
    }

    g_pRenderWorkspaceHook = HyprlandAPI::createFunctionHook(PHANDLE, FNS[0].address, (void*)hkRenderWorkspace);

    FNS = HyprlandAPI::findFunctionsByName(PHANDLE, "addDamageEPK15pixman_region32");
    if (FNS.empty()) {
        failNotif("no fns for hook addDamageEPK15pixman_region32");
        throw std::runtime_error("[he] No fns for hook addDamageEPK15pixman_region32");
    }

    g_pAddDamageHookB = HyprlandAPI::createFunctionHook(PHANDLE, FNS[0].address, (void*)hkAddDamageB);

    FNS = HyprlandAPI::findFunctionsByName(PHANDLE, "_ZN8CMonitor9addDamageERKN9Hyprutils4Math4CBoxE");
    if (FNS.empty()) {
        failNotif("no fns for hook _ZN8CMonitor9addDamageERKN9Hyprutils4Math4CBoxE");
        throw std::runtime_error("[he] No fns for hook _ZN8CMonitor9addDamageERKN9Hyprutils4Math4CBoxE");
    }

    g_pAddDamageHookA = HyprlandAPI::createFunctionHook(PHANDLE, FNS[0].address, (void*)hkAddDamageA);

    bool success = g_pRenderWorkspaceHook->hook();
    success      = success && g_pAddDamageHookA->hook();
    success      = success && g_pAddDamageHookB->hook();

    if (!success) {
        failNotif("Failed initializing hooks");
        throw std::runtime_error("[he] Failed initializing hooks");
    }

    HyprlandAPI::addDispatcherV2(PHANDLE, "hyprexpo:expo", ::onExpoDispatcher);
    HyprlandAPI::addDispatcherV2(PHANDLE, "hyprexpo:preview", ::onPreviewDispatcher);

    HyprlandAPI::addConfigKeyword(PHANDLE, KEYWORD_EXPO_GESTURE, ::expoGestureKeyword, {true});

    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:columns", Hyprlang::INT{3});
    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:gap_size", Hyprlang::INT{5});
    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:bg_col", Hyprlang::INT{0xFF111111});
    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:workspace_method", Hyprlang::STRING{"center current"});
    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:skip_empty", Hyprlang::INT{0});

    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:gesture_distance", Hyprlang::INT{200});
    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprexpo:preview_height", Hyprlang::INT{480});

    HyprlandAPI::reloadConfig();

    g_pPreviewManager = std::make_unique<CPreviewManager>();
    g_pSpawnManager   = std::make_unique<CSpawnManager>();

    return {"hyprexpo", "A plugin for an overview", "Vaxry", "1.0"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    g_unloading       = true;
    renderingOverview = false;

    g_pHyprRenderer->m_renderPass.removeAllOfType("COverviewPassElement");
    g_pSpawnManager.reset();
    g_pPreviewManager.reset();
    g_pOverview.reset();
    cleanupHooks();

    g_pConfigManager->reload(); // we need to reload now to clear all the gestures
}

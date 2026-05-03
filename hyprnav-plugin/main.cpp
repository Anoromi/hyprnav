#define WLR_USE_UNSTABLE

#include <unistd.h>

#include <format>
#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/desktop/DesktopTypes.hpp>
#include <hyprland/src/helpers/Monitor.hpp>
#include <hyprutils/string/ConstVarList.hpp>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>
using namespace Hyprutils::String;

#include "globals.hpp"
#include "PreviewManager.hpp"
#include "SpawnManager.hpp"

// Methods
inline CFunctionHook* g_pAddDamageHookA = nullptr;
inline CFunctionHook* g_pAddDamageHookB = nullptr;
typedef void (*origAddDamageA)(void*, const CBox&);
typedef void (*origAddDamageB)(void*, const pixman_region32_t*);

static void cleanupHooks() {
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

static void hkAddDamageA(void* thisptr, const CBox& box) {
    const auto PMONITOR = (CMonitor*)thisptr;

    if (g_pPreviewManager && PMONITOR->m_self == Desktop::focusState()->monitor())
        g_pPreviewManager->onWorkspaceDamaged(PMONITOR->activeWorkspaceID());

    ((origAddDamageA)g_pAddDamageHookA->m_original)(thisptr, box);
}

static void hkAddDamageB(void* thisptr, const pixman_region32_t* rg) {
    const auto PMONITOR = (CMonitor*)thisptr;

    if (g_pPreviewManager && PMONITOR->m_self == Desktop::focusState()->monitor())
        g_pPreviewManager->onWorkspaceDamaged(PMONITOR->activeWorkspaceID());

    ((origAddDamageB)g_pAddDamageHookB->m_original)(thisptr, rg);
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
    HyprlandAPI::addNotification(PHANDLE, "[hyprnav-plugin] Failure in initialization: " + reason, CHyprColor{1.0, 0.2, 0.2, 1.0}, 5000);
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    PHANDLE = handle;

    const std::string HASH        = __hyprland_api_get_hash();
    const std::string CLIENT_HASH = pluginClientHash();

    if (HASH != CLIENT_HASH) {
        failNotif(std::format("Version mismatch host={} client={}", HASH, CLIENT_HASH));
        throw std::runtime_error(std::format("[he] Version mismatch host={} client={}", HASH, CLIENT_HASH));
    }

    auto FNS = HyprlandAPI::findFunctionsByName(PHANDLE, "addDamageEPK15pixman_region32");
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

    bool success = g_pAddDamageHookA->hook();
    success      = success && g_pAddDamageHookB->hook();

    if (!success) {
        failNotif("Failed initializing hooks");
        throw std::runtime_error("[he] Failed initializing hooks");
    }

    HyprlandAPI::addDispatcherV2(PHANDLE, "hyprnav:preview", ::onPreviewDispatcher);

    HyprlandAPI::addConfigValue(PHANDLE, "plugin:hyprnav-plugin:preview_height", Hyprlang::INT{480});

    g_pPreviewManager = std::make_unique<CPreviewManager>();
    g_pSpawnManager   = std::make_unique<CSpawnManager>();

    return {"hyprnav-plugin", "Preview and spawn integration for hyprnav", "Andrii Zahorulko", "0.1"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    g_pSpawnManager.reset();
    g_pPreviewManager.reset();
    cleanupHooks();
}

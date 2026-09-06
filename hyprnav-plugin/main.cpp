#define WLR_USE_UNSTABLE

#include <format>
#include <hyprland/src/Compositor.hpp>
#include <stdexcept>
#include <string>
#include <string_view>

#include "globals.hpp"
#include "SpawnManager.hpp"

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

    g_pSpawnManager = std::make_unique<CSpawnManager>();

    return {"hyprnav-plugin", "Spawn placement integration for hyprnav", "Andrii Zahorulko", "0.1"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    g_pSpawnManager.reset();
}

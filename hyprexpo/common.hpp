#pragma once

#include <cstdint>
#include <filesystem>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace hyprexpo {

enum class eClientCommand : uint8_t {
    HELLO,
    WATCH,
    REFRESH,
    CLEAR,
    PING,
};

enum class eSwitcherCommand : uint8_t {
    SHOW_FORWARD,
    SHOW_REVERSE,
    HIDE,
    PING,
};

struct SPreviewSize {
    int width  = 0;
    int height = 0;
};

struct SClientCommand {
    eClientCommand   command = eClientCommand::PING;
    std::vector<int> workspaceIDs;
};

struct SSwitcherCommand {
    eSwitcherCommand command = eSwitcherCommand::PING;
};

std::optional<SClientCommand> parseClientCommand(std::string_view line, std::string& error);
std::optional<SSwitcherCommand> parseSwitcherCommand(std::string_view line, std::string& error);
std::vector<int>              dedupeWorkspaceIDs(const std::vector<int>& ids);

SPreviewSize                  computePreviewSize(int sourceWidth, int sourceHeight, int targetHeight = 480);

std::string                   discoverHyprlandInstanceSignature(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         runtimeDirectory(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         socketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         switcherSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         hyprlandSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         hyprlandEventSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         previewPath(const char* runtimeDir, const char* hyprlandInstanceSignature, int workspaceID);

std::string                   escapeJSON(std::string_view value);
std::string                   formatHelloEvent(const std::filesystem::path& socketPath, int previewHeight);
std::string                   formatPreviewEvent(int workspaceID, const std::filesystem::path& imagePath, int width, int height, uint64_t generation);
std::string                   formatErrorEvent(std::string_view message);

}

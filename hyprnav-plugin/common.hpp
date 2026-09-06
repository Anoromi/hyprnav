#pragma once

#include <cstdint>
#include <filesystem>
#include <string>
#include <string_view>

namespace hyprnav_plugin {

std::string                   discoverHyprlandInstanceSignature(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         runtimeDirectory(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         spawnSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         hyprlandSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);
std::filesystem::path         hyprlandEventSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature);

std::string                   escapeJSON(std::string_view value);

}

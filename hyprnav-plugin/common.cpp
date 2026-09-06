#include "common.hpp"

#include <filesystem>
#include <format>
#include <sstream>

namespace hyprnav_plugin {

static std::string shortInstanceToken(const char* hyprlandInstanceSignature) {
    constexpr uint64_t FNV_OFFSET = 14695981039346656037ull;
    constexpr uint64_t FNV_PRIME  = 1099511628211ull;

    const std::string_view signature =
        hyprlandInstanceSignature && *hyprlandInstanceSignature ? std::string_view{hyprlandInstanceSignature} : std::string_view{"default"};

    uint64_t hash = FNV_OFFSET;
    for (const auto c : signature) {
        hash ^= static_cast<unsigned char>(c);
        hash *= FNV_PRIME;
    }

    std::ostringstream out;
    out << std::format("{:016x}", hash);
    return out.str();
}

std::string discoverHyprlandInstanceSignature(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    const auto baseRuntime = runtimeDir && *runtimeDir ? runtimeDir : "/tmp";
    const auto hyprDir     = std::filesystem::path(baseRuntime) / "hypr";

    auto hasCommandSocket = [&](std::string_view signature) {
        if (signature.empty())
            return false;

        return std::filesystem::exists(hyprDir / signature / ".socket.sock");
    };

    if (hyprlandInstanceSignature && *hyprlandInstanceSignature && hasCommandSocket(hyprlandInstanceSignature))
        return hyprlandInstanceSignature;

    std::error_code ec;
    if (!std::filesystem::exists(hyprDir, ec))
        return {};

    std::filesystem::directory_entry bestEntry;
    std::filesystem::file_time_type  bestTime{};
    bool                             found = false;

    for (const auto& entry : std::filesystem::directory_iterator(hyprDir, ec)) {
        if (ec)
            break;

        if (!entry.is_directory(ec) || ec)
            continue;

        if (!hasCommandSocket(entry.path().filename().string()))
            continue;

        const auto writeTime = entry.last_write_time(ec);
        if (ec)
            continue;

        if (!found || writeTime > bestTime) {
            bestEntry = entry;
            bestTime  = writeTime;
            found     = true;
        }
    }

    return found ? bestEntry.path().filename().string() : std::string{};
}

std::filesystem::path runtimeDirectory(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    const auto base = runtimeDir && *runtimeDir ? runtimeDir : "/tmp";
    return std::filesystem::path(base) / "hx" / shortInstanceToken(hyprlandInstanceSignature);
}

std::filesystem::path spawnSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    return runtimeDirectory(runtimeDir, hyprlandInstanceSignature) / "spawn.sock";
}

std::filesystem::path hyprlandSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    const auto signature = hyprlandInstanceSignature && *hyprlandInstanceSignature ? hyprlandInstanceSignature : "default";
    const auto base      = runtimeDir && *runtimeDir ? runtimeDir : "/tmp";
    return std::filesystem::path(base) / "hypr" / signature / ".socket.sock";
}

std::filesystem::path hyprlandEventSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    const auto signature = hyprlandInstanceSignature && *hyprlandInstanceSignature ? hyprlandInstanceSignature : "default";
    const auto base      = runtimeDir && *runtimeDir ? runtimeDir : "/tmp";
    return std::filesystem::path(base) / "hypr" / signature / ".socket2.sock";
}

std::string escapeJSON(std::string_view value) {
    std::string out;
    out.reserve(value.size() + 8);

    for (const auto c : value) {
        switch (c) {
            case '\\': out += "\\\\"; break;
            case '"': out += "\\\""; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default: out.push_back(c); break;
        }
    }

    return out;
}

}

#include "common.hpp"

#include <algorithm>
#include <charconv>
#include <filesystem>
#include <format>
#include <sstream>

namespace hyprexpo {

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

static std::vector<std::string_view> splitWords(std::string_view line) {
    std::vector<std::string_view> out;

    while (!line.empty()) {
        const auto first = line.find_first_not_of(" \t\r\n");
        if (first == std::string_view::npos)
            break;

        line.remove_prefix(first);

        const auto end = line.find_first_of(" \t\r\n");
        if (end == std::string_view::npos) {
            out.emplace_back(line);
            break;
        }

        out.emplace_back(line.substr(0, end));
        line.remove_prefix(end);
    }

    return out;
}

std::optional<SClientCommand> parseClientCommand(std::string_view line, std::string& error) {
    error.clear();

    const auto parts = splitWords(line);
    if (parts.empty()) {
        error = "empty command";
        return std::nullopt;
    }

    SClientCommand cmd;

    if (parts[0] == "HELLO")
        cmd.command = eClientCommand::HELLO;
    else if (parts[0] == "WATCH")
        cmd.command = eClientCommand::WATCH;
    else if (parts[0] == "REFRESH")
        cmd.command = eClientCommand::REFRESH;
    else if (parts[0] == "CLEAR")
        cmd.command = eClientCommand::CLEAR;
    else if (parts[0] == "PING")
        cmd.command = eClientCommand::PING;
    else {
        error = std::format("unknown command: {}", parts[0]);
        return std::nullopt;
    }

    if (cmd.command == eClientCommand::HELLO || cmd.command == eClientCommand::CLEAR || cmd.command == eClientCommand::PING) {
        if (parts.size() != 1) {
            error = "unexpected arguments";
            return std::nullopt;
        }

        return cmd;
    }

    if (parts.size() < 2) {
        error = "workspace ids required";
        return std::nullopt;
    }

    for (size_t i = 1; i < parts.size(); ++i) {
        int        id     = 0;
        const auto begin  = parts[i].data();
        const auto end    = parts[i].data() + parts[i].size();
        const auto parsed = std::from_chars(begin, end, id);
        if (parsed.ec != std::errc{} || parsed.ptr != end || id <= 0) {
            error = std::format("invalid workspace id: {}", parts[i]);
            return std::nullopt;
        }

        cmd.workspaceIDs.push_back(id);
    }

    cmd.workspaceIDs = dedupeWorkspaceIDs(cmd.workspaceIDs);
    return cmd;
}

std::optional<SSwitcherCommand> parseSwitcherCommand(std::string_view line, std::string& error) {
    error.clear();

    const auto parts = splitWords(line);
    if (parts.empty()) {
        error = "empty command";
        return std::nullopt;
    }

    SSwitcherCommand command;

    if (parts[0] == "SHOW") {
        if (parts.size() != 2) {
            error = "show direction required";
            return std::nullopt;
        }

        if (parts[1] == "FORWARD")
            command.command = eSwitcherCommand::SHOW_FORWARD;
        else if (parts[1] == "REVERSE")
            command.command = eSwitcherCommand::SHOW_REVERSE;
        else {
            error = std::format("unknown show direction: {}", parts[1]);
            return std::nullopt;
        }

        return command;
    }

    if (parts.size() != 1) {
        error = "unexpected arguments";
        return std::nullopt;
    }

    if (parts[0] == "HIDE")
        command.command = eSwitcherCommand::HIDE;
    else if (parts[0] == "PING")
        command.command = eSwitcherCommand::PING;
    else {
        error = std::format("unknown command: {}", parts[0]);
        return std::nullopt;
    }

    return command;
}

std::vector<int> dedupeWorkspaceIDs(const std::vector<int>& ids) {
    std::vector<int> out;
    out.reserve(ids.size());

    for (const auto id : ids) {
        if (std::find(out.begin(), out.end(), id) == out.end())
            out.push_back(id);
    }

    return out;
}

SPreviewSize computePreviewSize(int sourceWidth, int sourceHeight, int targetHeight) {
    if (sourceWidth <= 0 || sourceHeight <= 0 || targetHeight <= 0)
        return {};

    if (sourceWidth >= sourceHeight) {
        const auto scale = static_cast<double>(targetHeight) / static_cast<double>(sourceHeight);
        return {
            .width  = std::max(1, static_cast<int>(sourceWidth * scale)),
            .height = targetHeight,
        };
    }

    const auto scale = static_cast<double>(targetHeight) / static_cast<double>(sourceWidth);
    return {
        .width  = targetHeight,
        .height = std::max(1, static_cast<int>(sourceHeight * scale)),
    };
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

std::filesystem::path socketPath(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    return runtimeDirectory(runtimeDir, hyprlandInstanceSignature) / "preview.sock";
}

std::filesystem::path switcherSocketPath(const char* runtimeDir, const char* hyprlandInstanceSignature) {
    return runtimeDirectory(runtimeDir, hyprlandInstanceSignature) / "switcher.sock";
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

std::filesystem::path previewPath(const char* runtimeDir, const char* hyprlandInstanceSignature, int workspaceID) {
    return runtimeDirectory(runtimeDir, hyprlandInstanceSignature) / std::format("workspace-{}.jpg", workspaceID);
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

std::string formatHelloEvent(const std::filesystem::path& socketPathValue, int previewHeight) {
    return std::format("{{\"event\":\"hello\",\"socket\":\"{}\",\"previewHeight\":{}}}\n", escapeJSON(socketPathValue.string()), previewHeight);
}

std::string formatPreviewEvent(int workspaceID, const std::filesystem::path& imagePath, int width, int height, uint64_t generation) {
    return std::format("{{\"event\":\"preview\",\"workspaceId\":{},\"path\":\"{}\",\"width\":{},\"height\":{},\"generation\":{}}}\n", workspaceID,
                       escapeJSON(imagePath.string()), width, height, generation);
}

std::string formatErrorEvent(std::string_view message) {
    return std::format("{{\"event\":\"error\",\"message\":\"{}\"}}\n", escapeJSON(message));
}

}

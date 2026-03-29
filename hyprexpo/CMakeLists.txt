cmake_minimum_required(VERSION 3.27)

project(hyprexpo
    DESCRIPTION "hyprexpo plugin for Hyprland"
    VERSION 0.1
)

set(CMAKE_CXX_STANDARD 23)

add_library(hyprexpo SHARED
    main.cpp
    overview.cpp
    ExpoGesture.cpp
    OverviewPassElement.cpp
    PreviewManager.cpp
    common.cpp
)

find_package(PkgConfig REQUIRED)

file(GLOB HYPREXPO_NIX_JPEG_HINTS
    "/nix/store/*-libjpeg-turbo-*-dev"
    "/nix/store/*-libjpeg-turbo-*"
)

find_path(HYPREXPO_JPEG_INCLUDE_DIR
    NAMES jpeglib.h
    HINTS ${HYPREXPO_NIX_JPEG_HINTS}
    PATH_SUFFIXES include
)

find_library(HYPREXPO_JPEG_LIBRARY
    NAMES jpeg
    HINTS ${HYPREXPO_NIX_JPEG_HINTS}
    PATH_SUFFIXES lib
)

if (HYPREXPO_JPEG_INCLUDE_DIR AND HYPREXPO_JPEG_LIBRARY)
    add_library(HyprexpoJPEG UNKNOWN IMPORTED)
    set_target_properties(HyprexpoJPEG PROPERTIES
        IMPORTED_LOCATION "${HYPREXPO_JPEG_LIBRARY}"
        INTERFACE_INCLUDE_DIRECTORIES "${HYPREXPO_JPEG_INCLUDE_DIR}"
    )
    set(HYPREXPO_JPEG_TARGET HyprexpoJPEG)
else()
    find_package(JPEG REQUIRED)
    set(HYPREXPO_JPEG_TARGET JPEG::JPEG)
endif()

pkg_check_modules(deps REQUIRED IMPORTED_TARGET
    hyprland
    libdrm
    libinput
    libudev
    pangocairo
    pixman-1
    wayland-server
    xkbcommon
)
target_link_libraries(hyprexpo PRIVATE rt PkgConfig::deps ${HYPREXPO_JPEG_TARGET})

install(TARGETS hyprexpo)

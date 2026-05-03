cmake_minimum_required(VERSION 3.27)

project(hyprnav-plugin
    DESCRIPTION "Preview and spawn integration for hyprnav"
    VERSION 0.1
)

set(CMAKE_CXX_STANDARD 23)

add_library(hyprnav-plugin SHARED
    main.cpp
    PreviewManager.cpp
    SpawnManager.cpp
    common.cpp
)

find_package(PkgConfig REQUIRED)

file(GLOB HYPRNAV_PLUGIN_NIX_JPEG_HINTS
    "/nix/store/*-libjpeg-turbo-*-dev"
    "/nix/store/*-libjpeg-turbo-*"
)

find_path(HYPRNAV_PLUGIN_JPEG_INCLUDE_DIR
    NAMES jpeglib.h
    HINTS ${HYPRNAV_PLUGIN_NIX_JPEG_HINTS}
    PATH_SUFFIXES include
)

find_library(HYPRNAV_PLUGIN_JPEG_LIBRARY
    NAMES jpeg
    HINTS ${HYPRNAV_PLUGIN_NIX_JPEG_HINTS}
    PATH_SUFFIXES lib
)

if (HYPRNAV_PLUGIN_JPEG_INCLUDE_DIR AND HYPRNAV_PLUGIN_JPEG_LIBRARY)
    add_library(HyprnavPluginJPEG UNKNOWN IMPORTED)
    set_target_properties(HyprnavPluginJPEG PROPERTIES
        IMPORTED_LOCATION "${HYPRNAV_PLUGIN_JPEG_LIBRARY}"
        INTERFACE_INCLUDE_DIRECTORIES "${HYPRNAV_PLUGIN_JPEG_INCLUDE_DIR}"
    )
    set(HYPRNAV_PLUGIN_JPEG_TARGET HyprnavPluginJPEG)
else()
    find_package(JPEG REQUIRED)
    set(HYPRNAV_PLUGIN_JPEG_TARGET JPEG::JPEG)
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
target_link_libraries(hyprnav-plugin PRIVATE rt PkgConfig::deps ${HYPRNAV_PLUGIN_JPEG_TARGET})

install(TARGETS hyprnav-plugin)

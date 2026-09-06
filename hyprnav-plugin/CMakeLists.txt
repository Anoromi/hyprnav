cmake_minimum_required(VERSION 3.27)

project(hyprnav-plugin
    DESCRIPTION "Spawn placement integration for hyprnav"
    VERSION 0.1
)

set(CMAKE_CXX_STANDARD 23)

add_library(hyprnav-plugin SHARED
    main.cpp
    SpawnManager.cpp
    common.cpp
)

find_package(PkgConfig REQUIRED)

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

target_link_libraries(hyprnav-plugin PRIVATE rt PkgConfig::deps)

install(TARGETS hyprnav-plugin)

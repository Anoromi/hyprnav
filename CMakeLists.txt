cmake_minimum_required(VERSION 3.27)
project(hyprnav
    DESCRIPTION "Local hyprnav tooling for Hyprland"
    LANGUAGES CXX
)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

include(CTest)

add_subdirectory(hyprnav-plugin)

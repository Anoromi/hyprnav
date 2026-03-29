cmake_minimum_required(VERSION 3.27)

project(hyprexpo-switcher LANGUAGES CXX)

set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)
set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 REQUIRED COMPONENTS Core Gui Qml Quick QuickControls2 Network Test)
find_package(LayerShellQt REQUIRED CONFIG)

get_filename_component(HYPREXPO_QTBASE_PREFIX "${Qt6Core_DIR}/../../.." ABSOLUTE)
get_filename_component(HYPREXPO_QTDECLARATIVE_PREFIX "${Qt6Qml_DIR}/../../.." ABSOLUTE)
set(HYPREXPO_SWITCHER_QT_ROOT_INCLUDES
    "${HYPREXPO_QTBASE_PREFIX}/include"
    "${HYPREXPO_QTDECLARATIVE_PREFIX}/include"
)

add_executable(hyprexpo-switcher
    src/main.cpp
    src/AppController.cpp
    src/WorkspaceModel.cpp
    src/workspace_utils.cpp
    ../hyprexpo/common.cpp
    qml/qml.qrc
)

target_include_directories(hyprexpo-switcher PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/src
    ${CMAKE_CURRENT_SOURCE_DIR}/../hyprexpo
    ${HYPREXPO_SWITCHER_QT_ROOT_INCLUDES}
)

target_link_libraries(hyprexpo-switcher PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Qml
    Qt6::Quick
    Qt6::QuickControls2
    Qt6::Network
)

foreach(include_root IN LISTS HYPREXPO_SWITCHER_QT_ROOT_INCLUDES)
    target_compile_options(hyprexpo-switcher PRIVATE "-I${include_root}")
endforeach()

target_compile_definitions(hyprexpo-switcher PRIVATE WAYLAND_LAYER_SHELL)
target_link_libraries(hyprexpo-switcher PRIVATE LayerShellQt::Interface)

add_executable(hyprexpo-switcher-tests
    tests/test_switcher.cpp
    src/WorkspaceModel.cpp
    src/workspace_utils.cpp
    ../hyprexpo/common.cpp
)

target_include_directories(hyprexpo-switcher-tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/src
    ${CMAKE_CURRENT_SOURCE_DIR}/../hyprexpo
    ${HYPREXPO_SWITCHER_QT_ROOT_INCLUDES}
)

target_link_libraries(hyprexpo-switcher-tests PRIVATE
    Qt6::Core
    Qt6::Test
)

foreach(include_root IN LISTS HYPREXPO_SWITCHER_QT_ROOT_INCLUDES)
    target_compile_options(hyprexpo-switcher-tests PRIVATE "-I${include_root}")
endforeach()

include(CTest)
add_test(NAME hyprexpo-switcher-tests COMMAND hyprexpo-switcher-tests)

install(TARGETS hyprexpo-switcher)

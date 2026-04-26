use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("com.anoromi.hyprnav")
            .qml_file("qml/Main.qml")
            .qml_file("qml/EnvironmentGrid.qml"),
    )
    .qt_module("Gui")
    .qt_module("Qml")
    .qt_module("Quick")
    .qt_module("QuickControls2")
    .qt_module("Network")
    .file("src/controller.rs")
    .cpp_file("cpp/layer_shell_bridge.cpp")
    .cpp_file("cpp/model_bridge.cpp")
    .build();

    println!("cargo:rustc-link-lib=LayerShellQtInterface");
}

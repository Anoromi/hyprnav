use crate::runtime_paths::{
    ensure_parent_dir, fallback_switcher_socket_paths, preview_path, resolve_runtime_paths,
    RuntimePaths,
};
use crate::workspace_utils::{
    build_workspace_descriptors, initial_selection_index, sort_workspaces_for_switcher, WorkspaceDescriptor,
};
use cxx_qt::CxxQtType;
use cxx_qt_lib::{QByteArray, QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QUrl, QVariant};
use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tracing::warn;

const ROLE_ID: i32 = 0x0101;
const ROLE_NAME: i32 = 0x0102;
const ROLE_SUBTITLE: i32 = 0x0103;
const ROLE_APP_CLASS: i32 = 0x0104;
const ROLE_WINDOW_COUNT: i32 = 0x0105;
const ROLE_ACTIVE: i32 = 0x0106;
const ROLE_SELECTED: i32 = 0x0107;
const ROLE_PREVIEW: i32 = 0x0108;
const ROLE_GENERATION: i32 = 0x0109;

#[derive(Clone, Debug, Default)]
struct WorkspaceItem {
    id: i32,
    name: String,
    subtitle: String,
    app_class: String,
    window_count: i32,
    active: bool,
    preview_path: String,
    generation: u64,
}

#[derive(Debug)]
enum BackendEvent {
    Show { reverse: bool },
    Hide,
    PreviewSocketConnected,
    PreviewUpdated { workspace_id: i32, path: String, generation: u64 },
    RefreshState,
    ActiveWorkspaceChanged(i32),
}

#[derive(Debug, Deserialize)]
struct PreviewMessage {
    #[serde(default)]
    event: String,
    #[serde(default, rename = "workspaceId")]
    workspace_id: i32,
    #[serde(default)]
    path: String,
    #[serde(default)]
    generation: u64,
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;

        include!("cxx-qt-lib/qhash_i32_QByteArray.h");
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;

        include!("cxx-qt-lib/qmodelindex.h");
        type QModelIndex = cxx_qt_lib::QModelIndex;

        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qurl.h");
        type QUrl = cxx_qt_lib::QUrl;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("cxx-qt-lib/qguiapplication.h");
        type QGuiApplication = cxx_qt_lib::QGuiApplication;

        include!("cxx-qt-lib/qqmlapplicationengine.h");
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;

        include!("hyprexpo-switcher/cpp/layer_shell_bridge.hpp");
        fn hyprexpo_configure_root_window(engine: Pin<&mut QQmlApplicationEngine>) -> bool;
        fn hyprexpo_set_quit_on_last_window_closed(app: Pin<&mut QGuiApplication>, quit_on_last_window_closed: bool);
    }

    unsafe extern "C++Qt" {
        include!(<QtCore/QAbstractListModel>);
        #[qobject]
        type QAbstractListModel;
    }

    extern "RustQt" {
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_singleton]
        #[qproperty(bool, visible)]
        #[qproperty(i32, current_index, cxx_name = "currentIndex")]
        type Controller = super::ControllerRust;

        #[qinvokable]
        #[cxx_name = "selectNext"]
        fn select_next(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "selectPrevious"]
        fn select_previous(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "activateCurrent"]
        fn activate_current(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "activateWorkspaceAt"]
        fn activate_workspace_at(self: Pin<&mut Controller>, index: i32);

        #[qinvokable]
        fn cancel(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "handleModifierReleased"]
        fn handle_modifier_released(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "pumpBackendEvents"]
        fn pump_backend_events(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &Controller, parent: &QModelIndex) -> i32;

        #[qinvokable]
        #[cxx_override]
        fn data(self: &Controller, index: &QModelIndex, role: i32) -> QVariant;

        #[qinvokable]
        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &Controller) -> QHash_i32_QByteArray;
    }

    unsafe extern "RustQt" {
        #[inherit]
        #[cxx_name = "beginResetModel"]
        unsafe fn begin_reset_model(self: Pin<&mut Controller>);

        #[inherit]
        #[cxx_name = "endResetModel"]
        unsafe fn end_reset_model(self: Pin<&mut Controller>);
    }

    impl cxx_qt::Initialize for Controller {}
}

pub struct ControllerRust {
    visible: bool,
    current_index: i32,
    items: Vec<WorkspaceItem>,
    active_workspace_id: i32,
    mru_workspace_ids: VecDeque<i32>,
    backfilled_workspace_ids: HashSet<i32>,
    receiver: Option<Receiver<BackendEvent>>,
    backend_started: bool,
    current_paths: Option<RuntimePaths>,
}

impl Default for ControllerRust {
    fn default() -> Self {
        Self {
            visible: false,
            current_index: -1,
            items: Vec::new(),
            active_workspace_id: -1,
            mru_workspace_ids: VecDeque::new(),
            backfilled_workspace_ids: HashSet::new(),
            receiver: None,
            backend_started: false,
            current_paths: None,
        }
    }
}

impl qobject::Controller {
    fn start_backend(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().backend_started {
            return;
        }

        let (sender, receiver) = unbounded();
        {
            let mut rust = self.as_mut().rust_mut();
            rust.backend_started = true;
            rust.receiver = Some(receiver);
        }

        spawn_control_socket_worker(sender.clone());
        spawn_preview_socket_worker(sender.clone());
        spawn_hypr_event_worker(sender);
        self.as_mut().refresh_workspace_state();
    }

    pub fn row_count(&self, parent: &QModelIndex) -> i32 {
        if parent.is_valid() {
            0
        } else {
            self.rust().items.len() as i32
        }
    }

    pub fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let Some(item) = self.rust().items.get(index.row() as usize) else {
            return QVariant::default();
        };

        match role {
            ROLE_ID => QVariant::from(&item.id),
            ROLE_NAME => QVariant::from(&QString::from(item.name.as_str())),
            ROLE_SUBTITLE => QVariant::from(&QString::from(item.subtitle.as_str())),
            ROLE_APP_CLASS => QVariant::from(&QString::from(item.app_class.as_str())),
            ROLE_WINDOW_COUNT => QVariant::from(&item.window_count),
            ROLE_ACTIVE => QVariant::from(&item.active),
            ROLE_SELECTED => QVariant::from(&(index.row() == self.rust().current_index)),
            ROLE_PREVIEW => {
                if item.preview_path.is_empty() {
                    QVariant::default()
                } else {
                    let mut url = QUrl::from_local_file(&QString::from(item.preview_path.as_str()));
                    url.set_query(&QString::from(format!("g={}", item.generation).as_str()));
                    QVariant::from(&url)
                }
            }
            ROLE_GENERATION => QVariant::from(&(item.generation as u64)),
            _ => QVariant::default(),
        }
    }

    pub fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
        roles.insert(ROLE_ID, QByteArray::from("workspaceId".as_bytes()));
        roles.insert(ROLE_NAME, QByteArray::from("workspaceName".as_bytes()));
        roles.insert(ROLE_SUBTITLE, QByteArray::from("workspaceSubtitle".as_bytes()));
        roles.insert(ROLE_APP_CLASS, QByteArray::from("workspaceAppClass".as_bytes()));
        roles.insert(ROLE_WINDOW_COUNT, QByteArray::from("workspaceWindowCount".as_bytes()));
        roles.insert(ROLE_ACTIVE, QByteArray::from("workspaceActive".as_bytes()));
        roles.insert(ROLE_SELECTED, QByteArray::from("workspaceSelected".as_bytes()));
        roles.insert(ROLE_PREVIEW, QByteArray::from("workspacePreview".as_bytes()));
        roles.insert(ROLE_GENERATION, QByteArray::from("workspaceGeneration".as_bytes()));
        roles
    }

    pub fn select_next(mut self: Pin<&mut Self>) {
        let count = self.as_ref().rust().items.len() as i32;
        if count <= 0 {
            return;
        }

        let next_index = if self.as_ref().rust().current_index < 0 {
            0
        } else {
            (self.as_ref().rust().current_index + 1) % count
        };

        self.as_mut().set_selection(next_index);
    }

    pub fn select_previous(mut self: Pin<&mut Self>) {
        let count = self.as_ref().rust().items.len() as i32;
        if count <= 0 {
            return;
        }

        let next_index = if self.as_ref().rust().current_index < 0 {
            0
        } else {
            (self.as_ref().rust().current_index + count - 1) % count
        };

        self.as_mut().set_selection(next_index);
    }

    pub fn activate_current(mut self: Pin<&mut Self>) {
        let workspace_id = self.as_ref().current_workspace_id();
        self.as_mut().activate_workspace(workspace_id);
    }

    pub fn activate_workspace_at(mut self: Pin<&mut Self>, index: i32) {
        self.as_mut().set_selection(index);
        self.as_mut().activate_current();
    }

    pub fn cancel(mut self: Pin<&mut Self>) {
        self.as_mut().hide_switcher();
    }

    pub fn handle_modifier_released(mut self: Pin<&mut Self>) {
        if *self.visible() {
            self.as_mut().activate_current();
        }
    }

    pub fn pump_backend_events(mut self: Pin<&mut Self>) {
        let Some(receiver) = self.as_ref().rust().receiver.clone() else {
            return;
        };

        while let Ok(event) = receiver.try_recv() {
            match event {
                BackendEvent::Show { reverse } => self.as_mut().show_switcher(reverse),
                BackendEvent::Hide => self.as_mut().hide_switcher(),
                BackendEvent::PreviewSocketConnected => {
                    self.as_mut().bootstrap_cached_previews();
                    self.as_mut().backfill_missing_previews();
                }
                BackendEvent::PreviewUpdated {
                    workspace_id,
                    path,
                    generation,
                } => self.as_mut().update_preview(workspace_id, &path, generation),
                BackendEvent::RefreshState => self.as_mut().refresh_workspace_state(),
                BackendEvent::ActiveWorkspaceChanged(workspace_id) => self.as_mut().handle_active_workspace_changed(workspace_id),
            }
        }
    }

    fn show_switcher(mut self: Pin<&mut Self>, reverse: bool) {
        if !*self.visible() {
            self.as_mut().update_selection_for_show(reverse);
            self.as_mut().set_visible(true);
            return;
        }

        if reverse {
            self.as_mut().select_previous();
        } else {
            self.as_mut().select_next();
        }
    }

    fn hide_switcher(mut self: Pin<&mut Self>) {
        self.as_mut().set_visible(false);
        self.as_mut().set_selection(-1);
    }

    fn update_selection_for_show(mut self: Pin<&mut Self>, reverse: bool) {
        let items = self.as_ref().rust().items.iter().map(item_to_descriptor).collect::<Vec<_>>();
        let index = initial_selection_index(&items, reverse);
        self.as_mut().set_selection(index);
    }

    fn set_selection(mut self: Pin<&mut Self>, index: i32) {
        let count = self.as_ref().rust().items.len() as i32;
        let normalized = if index >= 0 && index < count { index } else { -1 };
        if *self.current_index() == normalized {
            return;
        }

        self.as_mut().set_current_index(normalized);
        self.as_mut().reset_model();
    }

    fn current_workspace_id(&self) -> i32 {
        self.rust()
            .items
            .get(self.rust().current_index.max(0) as usize)
            .map(|item| item.id)
            .unwrap_or(-1)
    }

    fn visible_workspace_ids(&self) -> Vec<i32> {
        self.rust().items.iter().map(|item| item.id).collect()
    }

    fn reset_model(mut self: Pin<&mut Self>) {
        unsafe {
            self.as_mut().begin_reset_model();
            self.as_mut().end_reset_model();
        }
    }

    fn refresh_workspace_state(mut self: Pin<&mut Self>) {
        let paths = resolve_runtime_paths();
        let monitors = run_hyprctl_json(&paths, &["-j", "monitors"]).unwrap_or_default();
        let workspaces = run_hyprctl_json(&paths, &["-j", "workspaces"]).unwrap_or_default();
        let clients = run_hyprctl_json(&paths, &["-j", "clients"]).unwrap_or_default();
        let mut descriptors = build_workspace_descriptors(&monitors, &workspaces, &clients);

        {
            let mut rust = self.as_mut().rust_mut();
            rust.current_paths = Some(paths.clone());
            if let Some(active) = descriptors.iter().find(|descriptor| descriptor.active) {
                rust.active_workspace_id = active.id;
                unsafe {
                    note_workspace_activated(rust.as_mut().get_unchecked_mut(), active.id);
                }
            }
            sort_workspaces_for_switcher(
                &mut descriptors,
                rust.mru_workspace_ids.iter().copied().collect::<Vec<_>>().as_slice(),
            );
        }

        self.as_mut().replace_items(descriptors);
        self.as_mut().bootstrap_cached_previews();
        self.as_mut().backfill_missing_previews();
    }

    fn replace_items(mut self: Pin<&mut Self>, workspaces: Vec<WorkspaceDescriptor>) {
        let selected_workspace_id = self.as_ref().current_workspace_id();
        let visible = *self.visible();
        let current_items = self
            .as_ref()
            .rust()
            .items
            .iter()
            .map(|item| (item.id, item.clone()))
            .collect::<HashMap<_, _>>();

        let mut next_items = workspaces
            .iter()
            .map(|workspace| WorkspaceItem {
                id: workspace.id,
                name: workspace.name.clone(),
                subtitle: workspace.subtitle.clone(),
                app_class: workspace.app_class.clone(),
                window_count: workspace.window_count,
                active: workspace.active,
                preview_path: current_items
                    .get(&workspace.id)
                    .map(|item| item.preview_path.clone())
                    .unwrap_or_default(),
                generation: current_items.get(&workspace.id).map(|item| item.generation).unwrap_or(0),
            })
            .collect::<Vec<_>>();

        {
            let mut rust = self.as_mut().rust_mut();
            rust.items.clear();
            rust.items.append(&mut next_items);
        }

        let new_index = if visible {
            self.as_ref()
                .rust()
                .items
                .iter()
                .position(|item| item.id == selected_workspace_id)
                .map(|index| index as i32)
                .unwrap_or_else(|| initial_selection_index(workspaces.as_slice(), false))
        } else {
            -1
        };

        self.as_mut().set_current_index(new_index);
        self.as_mut().reset_model();
    }

    fn bootstrap_cached_previews(mut self: Pin<&mut Self>) {
        let Some(paths) = self.as_ref().rust().current_paths.clone() else {
            return;
        };

        let mut changed = false;
        for item in &mut self.as_mut().rust_mut().items {
            if !item.preview_path.is_empty() {
                continue;
            }

            let path = preview_path(&paths.runtime_root, &paths.instance_signature, item.id);
            if path.is_file() {
                item.preview_path = path.to_string_lossy().into_owned();
                changed = true;
            }
        }

        if changed {
            self.as_mut().reset_model();
        }
    }

    fn backfill_missing_previews(mut self: Pin<&mut Self>) {
        let workspace_ids = {
            let controller = self.as_ref();
            let rust = controller.rust();
            rust.items
                .iter()
                .filter(|item| {
                    item.id > 0
                        && !rust.backfilled_workspace_ids.contains(&item.id)
                        && item.preview_path.is_empty()
                })
                .map(|item| item.id)
                .collect::<Vec<_>>()
        };

        if !workspace_ids.is_empty() {
            let mut rust = self.as_mut().rust_mut();
            for workspace_id in &workspace_ids {
                rust.backfilled_workspace_ids.insert(*workspace_id);
            }
        }

        if !workspace_ids.is_empty() {
            request_preview_refresh_async(workspace_ids);
        }
    }

    fn update_preview(mut self: Pin<&mut Self>, workspace_id: i32, preview_path: &str, generation: u64) {
        let mut changed = false;
        for item in &mut self.as_mut().rust_mut().items {
            if item.id != workspace_id {
                continue;
            }

            item.preview_path = preview_path.to_owned();
            item.generation = generation;
            changed = true;
            break;
        }

        if changed {
            self.as_mut().reset_model();
        }
    }

    fn handle_active_workspace_changed(mut self: Pin<&mut Self>, workspace_id: i32) {
        if workspace_id <= 0 {
            return;
        }

        {
            let mut rust = self.as_mut().rust_mut();
            if rust.active_workspace_id > 0 && rust.active_workspace_id != workspace_id {
                request_preview_refresh_async(vec![rust.active_workspace_id]);
            }

            rust.active_workspace_id = workspace_id;
            unsafe {
                note_workspace_activated(rust.as_mut().get_unchecked_mut(), workspace_id);
            }
        }
    }

    fn activate_workspace(mut self: Pin<&mut Self>, workspace_id: i32) {
        self.as_mut().hide_switcher();

        if workspace_id <= 0 {
            return;
        }

        {
            let mut rust = self.as_mut().rust_mut();
            unsafe {
                note_workspace_activated(rust.as_mut().get_unchecked_mut(), workspace_id);
            }
        }

        let paths = self.as_ref().rust().current_paths.clone().unwrap_or_else(resolve_runtime_paths);
        run_hyprctl_command(&paths, &["dispatch", "workspace", &workspace_id.to_string()]);
    }
}

impl cxx_qt::Initialize for qobject::Controller {
    fn initialize(mut self: Pin<&mut Self>) {
        self.as_mut().start_backend();
    }
}

fn item_to_descriptor(item: &WorkspaceItem) -> WorkspaceDescriptor {
    WorkspaceDescriptor {
        id: item.id,
        name: item.name.clone(),
        subtitle: item.subtitle.clone(),
        app_class: item.app_class.clone(),
        window_count: item.window_count,
        focus_history_rank: i32::MAX,
        active: item.active,
    }
}

fn note_workspace_activated(rust: &mut ControllerRust, workspace_id: i32) {
    if workspace_id <= 0 {
        return;
    }

    if let Some(index) = rust.mru_workspace_ids.iter().position(|candidate| *candidate == workspace_id) {
        rust.mru_workspace_ids.remove(index);
    }

    rust.mru_workspace_ids.push_front(workspace_id);
}

fn run_hyprctl_json(paths: &RuntimePaths, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("hyprctl");
    command.args(args);
    if !paths.instance_signature.is_empty() {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", &paths.instance_signature);
    }

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }

    Some(output.stdout)
}

fn run_hyprctl_command(paths: &RuntimePaths, args: &[&str]) {
    let mut command = Command::new("hyprctl");
    command.args(args);
    if !paths.instance_signature.is_empty() {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", &paths.instance_signature);
    }

    if let Err(error) = command.status() {
        warn!("failed to run hyprctl {:?}: {error}", args);
    }
}

fn request_preview_refresh_async(workspace_ids: Vec<i32>) {
    if workspace_ids.is_empty() {
        return;
    }

    let ids = workspace_ids
        .into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    thread::spawn(move || {
        let paths = resolve_runtime_paths();
        run_hyprctl_command(&paths, &["dispatch", "hyprexpo:preview", &ids]);
    });
}

fn spawn_control_socket_worker(sender: Sender<BackendEvent>) {
    thread::spawn(move || {
        let runtime_root = crate::runtime_paths::runtime_root();
        let mut current_socket_path = PathBuf::new();
        let mut listener: Option<UnixListener> = None;

        loop {
            let paths = resolve_runtime_paths();
            if paths.switcher_socket_path != current_socket_path {
                current_socket_path = paths.switcher_socket_path.clone();
                listener = bind_switcher_listener(&current_socket_path).ok();
            }

            let Some(listener_ref) = listener.as_ref() else {
                thread::sleep(Duration::from_millis(250));
                continue;
            };

            match listener_ref.accept() {
                Ok((stream, _)) => handle_control_stream(stream, &sender),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(80)),
                Err(error) => {
                    warn!("control socket accept failed: {error}");
                    listener = None;
                    let mut paths = fallback_switcher_socket_paths(&runtime_root);
                    if let Some(path) = paths.drain(..1).next() {
                        current_socket_path = path;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
    });
}

fn bind_switcher_listener(path: &Path) -> anyhow::Result<UnixListener> {
    ensure_parent_dir(path)?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }

    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn handle_control_stream(stream: UnixStream, sender: &Sender<BackendEvent>) {
    let mut reader = match stream.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(error) => {
            warn!("failed to clone control stream: {error}");
            return;
        }
    };
    let mut writer = stream;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                let trimmed = line.trim();
                let response = match trimmed {
                    "SHOW FORWARD" => {
                        let _ = sender.send(BackendEvent::Show { reverse: false });
                        "OK\n"
                    }
                    "SHOW REVERSE" => {
                        let _ = sender.send(BackendEvent::Show { reverse: true });
                        "OK\n"
                    }
                    "HIDE" => {
                        let _ = sender.send(BackendEvent::Hide);
                        "OK\n"
                    }
                    "PING" => "OK\n",
                    _ => "ERROR invalid command\n",
                };

                let _ = writer.write_all(response.as_bytes());
                let _ = writer.flush();
            }
            Err(error) => {
                warn!("failed to read control command: {error}");
                return;
            }
        }
    }
}

fn spawn_preview_socket_worker(sender: Sender<BackendEvent>) {
    thread::spawn(move || loop {
        let paths = resolve_runtime_paths();
        let stream = match UnixStream::connect(&paths.preview_socket_path) {
            Ok(stream) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(350));
                continue;
            }
        };

        let _ = sender.send(BackendEvent::PreviewSocketConnected);
        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(message) = serde_json::from_str::<PreviewMessage>(line.trim()) else {
                        continue;
                    };

                    if message.event == "hello" {
                        let _ = sender.send(BackendEvent::PreviewSocketConnected);
                        continue;
                    }

                    if message.event == "preview" {
                        let _ = sender.send(BackendEvent::PreviewUpdated {
                            workspace_id: message.workspace_id,
                            path: message.path,
                            generation: message.generation,
                        });
                    }
                }
                Err(_) => break,
            }
        }

        thread::sleep(Duration::from_millis(250));
    });
}

fn spawn_hypr_event_worker(sender: Sender<BackendEvent>) {
    thread::spawn(move || loop {
        let paths = resolve_runtime_paths();
        let stream = match UnixStream::connect(&paths.hypr_event_socket_path) {
            Ok(stream) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(350));
                continue;
            }
        };

        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => handle_hypr_event_line(line.trim(), &sender),
                Err(_) => break,
            }
        }

        thread::sleep(Duration::from_millis(250));
    });
}

fn handle_hypr_event_line(line: &str, sender: &Sender<BackendEvent>) {
    let Some((event, payload)) = line.split_once(">>") else {
        return;
    };

    match event {
        "workspacev2" | "workspace" => {
            if let Some(workspace_id) = parse_leading_workspace_id(payload) {
                let _ = sender.send(BackendEvent::ActiveWorkspaceChanged(workspace_id));
            }
            let _ = sender.send(BackendEvent::RefreshState);
        }
        "focusedmonv2" | "focusedmon" => {
            if let Some(workspace_id) = parse_focused_monitor_workspace_id(payload) {
                let _ = sender.send(BackendEvent::ActiveWorkspaceChanged(workspace_id));
            }
            let _ = sender.send(BackendEvent::RefreshState);
        }
        "activewindowv2" | "openwindow" | "closewindow" | "movewindowv2" | "createworkspacev2" | "destroyworkspacev2"
        | "renameworkspace" => {
            let _ = sender.send(BackendEvent::RefreshState);
        }
        _ => {}
    }
}

fn parse_leading_workspace_id(payload: &str) -> Option<i32> {
    payload
        .split(',')
        .next()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| *value > 0)
}

fn parse_focused_monitor_workspace_id(payload: &str) -> Option<i32> {
    payload
        .split(',')
        .nth(1)
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| *value > 0)
}

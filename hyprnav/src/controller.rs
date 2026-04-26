use crate::protocol::{
    send_request_with_fallbacks, GridCellSnapshot, GridSnapshot, Request, SwitcherSnapshot,
    WorkspaceCardSnapshot,
};
use crate::runtime_paths::resolve_runtime_paths;
use crate::ui_session::{
    drain_grid_session_commands, drain_switcher_session_commands, GridUiCommand, UiSessionCommand,
};
use cxx_qt::casting::Upcast;
use cxx_qt::CxxQtType;
use cxx_qt_lib::{
    QByteArray, QHash, QHashPair_i32_QByteArray, QList, QModelIndex, QString, QUrl, QVariant,
};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

const ROLE_ID: i32 = 0x0101;
const ROLE_NAME: i32 = 0x0102;
const ROLE_SUBTITLE: i32 = 0x0103;
const ROLE_APP_CLASS: i32 = 0x0104;
const ROLE_WINDOW_COUNT: i32 = 0x0105;
const ROLE_ACTIVE: i32 = 0x0106;
const ROLE_PREVIEW: i32 = 0x0108;
const ROLE_GENERATION: i32 = 0x0109;
const ROLE_ENVIRONMENT_ID: i32 = 0x010a;
const ROLE_ENVIRONMENT_DISPLAY_ID: i32 = 0x010b;
const ROLE_SLOT_INDEX: i32 = 0x010c;
const ROLE_PHYSICAL_WORKSPACE_ID: i32 = 0x010d;
const ROLE_ENVIRONMENT_LOCKED: i32 = 0x010e;
const ROLE_SHOW_ENVIRONMENT_LABEL: i32 = 0x010f;
const ROLE_ROW_INDEX: i32 = 0x0110;
const ROLE_COLUMN_INDEX: i32 = 0x0111;
const OPEN_REFRESH_STALE_THRESHOLD_MS: u64 = 1_500;
const NAVIGATION_REFRESH_GRACE_MS: u64 = 250;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct UiItem {
    workspace_id: i32,
    workspace_name: String,
    subtitle: String,
    app_class: String,
    window_count: i32,
    active: bool,
    preview_path: String,
    generation: u64,
    environment_id: String,
    environment_display_id: String,
    slot_index: i32,
    physical_workspace_id: i32,
    environment_locked: bool,
    show_environment_label: bool,
    row_index: i32,
    column_index: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UiMode {
    #[default]
    Switcher,
    Grid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GridSelectionKey {
    environment_id: String,
    slot_index: i32,
    physical_workspace_id: i32,
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

        include!("cxx-qt-lib/core/qlist/qlist_i32.h");
        type QList_i32 = cxx_qt_lib::QList<i32>;

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

        include!("hyprnav/cpp/layer_shell_bridge.hpp");
        fn hyprexpo_configure_root_window(engine: Pin<&mut QQmlApplicationEngine>) -> bool;
        fn hyprexpo_load_qml_from_module(
            engine: Pin<&mut QQmlApplicationEngine>,
            uri: &QString,
            type_name: &QString,
        ) -> bool;
        fn hyprexpo_map_root_window_resident();
        fn hyprexpo_set_quit_on_last_window_closed(
            app: Pin<&mut QGuiApplication>,
            quit_on_last_window_closed: bool,
        );
        fn hyprexpo_set_root_window_interactive(interactive: bool);
        fn hyprexpo_set_root_window_visible(visible: bool);
    }

    unsafe extern "C++Qt" {
        include!(<QtCore/QAbstractListModel>);
        #[qobject]
        type QAbstractListModel;
    }

    unsafe extern "C++" {
        include!("hyprnav/cpp/model_bridge.hpp");
        fn hyprexpo_emit_rows_changed(
            model: Pin<&mut QAbstractListModel>,
            first_row: i32,
            last_row: i32,
            roles: &QList_i32,
        );
    }

    extern "RustQt" {
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_singleton]
        #[qproperty(bool, visible)]
        #[qproperty(i32, current_index, cxx_name = "currentIndex")]
        #[qproperty(bool, grid_mode, cxx_name = "gridMode")]
        #[qproperty(i32, grid_row_count, cxx_name = "gridRowCount")]
        #[qproperty(i32, grid_column_count, cxx_name = "gridColumnCount")]
        #[qproperty(bool, loading)]
        #[qproperty(bool, has_snapshot, cxx_name = "hasSnapshot")]
        #[qproperty(bool, resident_mode, cxx_name = "residentMode")]
        #[qproperty(i32, open_generation, cxx_name = "openGeneration")]
        type Controller = super::ControllerRust;

        #[qinvokable]
        #[cxx_name = "initializeIfNeeded"]
        fn initialize_if_needed(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "selectNext"]
        fn select_next(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "selectPrevious"]
        fn select_previous(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "moveLeft"]
        fn move_left(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "moveRight"]
        fn move_right(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "moveUp"]
        fn move_up(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "moveDown"]
        fn move_down(self: Pin<&mut Controller>);

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
        #[cxx_name = "refreshSnapshotIfVisible"]
        fn refresh_snapshot_if_visible(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "refreshSnapshotIfStale"]
        fn refresh_snapshot_if_stale(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "pumpSessionCommands"]
        fn pump_session_commands(self: Pin<&mut Controller>);

        #[qinvokable]
        #[cxx_name = "warmSnapshot"]
        fn warm_snapshot(self: Pin<&mut Controller>);

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
}

#[derive(Default)]
pub struct ControllerRust {
    visible: bool,
    current_index: i32,
    grid_mode: bool,
    grid_row_count: i32,
    grid_column_count: i32,
    loading: bool,
    has_snapshot: bool,
    resident_mode: bool,
    open_generation: i32,
    initialized: bool,
    last_snapshot_ms: u64,
    last_navigation_ms: u64,
    items: Vec<UiItem>,
    mode: UiMode,
    reverse: bool,
    row_to_indices: Vec<Vec<usize>>,
}

impl qobject::Controller {
    pub fn initialize_if_needed(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().initialized {
            return;
        }

        {
            let mut rust = self.as_mut().rust_mut();
            rust.initialized = true;
            rust.mode =
                if std::env::var("HYPREXPO_SWITCHER_UI_MODE").ok().as_deref() == Some("grid") {
                    UiMode::Grid
                } else {
                    UiMode::Switcher
                };
            rust.reverse = std::env::var("HYPREXPO_SWITCHER_UI_REVERSE")
                .ok()
                .as_deref()
                == Some("1");
            rust.grid_mode = rust.mode == UiMode::Grid;
            rust.resident_mode = std::env::var("HYPREXPO_SWITCHER_UI_RESIDENT")
                .ok()
                .as_deref()
                == Some("1");
        }
        let resident_mode = self.as_ref().rust().resident_mode;
        self.as_mut().set_resident_mode(resident_mode);

        if uses_resident_grid_window(self.as_ref().rust().mode, resident_mode) {
            qobject::hyprexpo_map_root_window_resident();
            qobject::hyprexpo_set_root_window_interactive(false);
        } else {
            if let Err(error) = self.as_mut().load_snapshot_and_show() {
                warn!("failed to load UI snapshot: {error}");
                self.as_mut().hide_overlay();
            }
        }
    }

    pub fn warm_snapshot(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode != UiMode::Grid
            || !self.as_ref().rust().resident_mode
            || *self.visible()
            || *self.has_snapshot()
        {
            return;
        }

        if let Err(error) = self.as_mut().refresh_snapshot(false) {
            warn!("failed to warm grid snapshot: {error}");
            self.as_mut().set_loading(false);
        }
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
            ROLE_ID => QVariant::from(&item.workspace_id),
            ROLE_NAME => QVariant::from(&QString::from(item.workspace_name.as_str())),
            ROLE_SUBTITLE => QVariant::from(&QString::from(item.subtitle.as_str())),
            ROLE_APP_CLASS => QVariant::from(&QString::from(item.app_class.as_str())),
            ROLE_WINDOW_COUNT => QVariant::from(&item.window_count),
            ROLE_ACTIVE => QVariant::from(&item.active),
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
            ROLE_ENVIRONMENT_ID => QVariant::from(&QString::from(item.environment_id.as_str())),
            ROLE_ENVIRONMENT_DISPLAY_ID => {
                QVariant::from(&QString::from(item.environment_display_id.as_str()))
            }
            ROLE_SLOT_INDEX => QVariant::from(&item.slot_index),
            ROLE_PHYSICAL_WORKSPACE_ID => QVariant::from(&item.physical_workspace_id),
            ROLE_ENVIRONMENT_LOCKED => QVariant::from(&item.environment_locked),
            ROLE_SHOW_ENVIRONMENT_LABEL => QVariant::from(&item.show_environment_label),
            ROLE_ROW_INDEX => QVariant::from(&item.row_index),
            ROLE_COLUMN_INDEX => QVariant::from(&item.column_index),
            _ => QVariant::default(),
        }
    }

    pub fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
        roles.insert(ROLE_ID, QByteArray::from("workspaceId".as_bytes()));
        roles.insert(ROLE_NAME, QByteArray::from("workspaceName".as_bytes()));
        roles.insert(
            ROLE_SUBTITLE,
            QByteArray::from("workspaceSubtitle".as_bytes()),
        );
        roles.insert(
            ROLE_APP_CLASS,
            QByteArray::from("workspaceAppClass".as_bytes()),
        );
        roles.insert(
            ROLE_WINDOW_COUNT,
            QByteArray::from("workspaceWindowCount".as_bytes()),
        );
        roles.insert(ROLE_ACTIVE, QByteArray::from("workspaceActive".as_bytes()));
        roles.insert(
            ROLE_PREVIEW,
            QByteArray::from("workspacePreview".as_bytes()),
        );
        roles.insert(
            ROLE_GENERATION,
            QByteArray::from("workspaceGeneration".as_bytes()),
        );
        roles.insert(
            ROLE_ENVIRONMENT_ID,
            QByteArray::from("environmentId".as_bytes()),
        );
        roles.insert(
            ROLE_ENVIRONMENT_DISPLAY_ID,
            QByteArray::from("environmentDisplayId".as_bytes()),
        );
        roles.insert(ROLE_SLOT_INDEX, QByteArray::from("slotIndex".as_bytes()));
        roles.insert(
            ROLE_PHYSICAL_WORKSPACE_ID,
            QByteArray::from("physicalWorkspaceId".as_bytes()),
        );
        roles.insert(
            ROLE_ENVIRONMENT_LOCKED,
            QByteArray::from("environmentLocked".as_bytes()),
        );
        roles.insert(
            ROLE_SHOW_ENVIRONMENT_LABEL,
            QByteArray::from("showEnvironmentLabel".as_bytes()),
        );
        roles.insert(ROLE_ROW_INDEX, QByteArray::from("rowIndex".as_bytes()));
        roles.insert(
            ROLE_COLUMN_INDEX,
            QByteArray::from("columnIndex".as_bytes()),
        );
        roles
    }

    pub fn select_next(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode == UiMode::Grid {
            self.as_mut().move_right();
            return;
        }

        self.as_mut().move_linear(1);
    }

    pub fn select_previous(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode == UiMode::Grid {
            self.as_mut().move_left();
            return;
        }

        self.as_mut().move_linear(-1);
    }

    pub fn move_left(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode != UiMode::Grid {
            self.as_mut().move_linear(-1);
            return;
        }

        let Some((row_index, column_index)) = self.as_ref().current_grid_position() else {
            return;
        };
        let next_index = {
            let binding = self.as_ref();
            let row = &binding.rust().row_to_indices[row_index as usize];
            let next_column = if column_index <= 0 {
                row.len().saturating_sub(1) as i32
            } else {
                column_index - 1
            };
            row[next_column as usize] as i32
        };
        self.as_mut().set_selection(next_index);
    }

    pub fn move_right(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode != UiMode::Grid {
            self.as_mut().move_linear(1);
            return;
        }

        let Some((row_index, column_index)) = self.as_ref().current_grid_position() else {
            return;
        };
        let next_index = {
            let binding = self.as_ref();
            let row = &binding.rust().row_to_indices[row_index as usize];
            if row.is_empty() {
                return;
            }
            let next_column = (column_index as usize + 1) % row.len();
            row[next_column] as i32
        };
        self.as_mut().set_selection(next_index);
    }

    pub fn move_up(mut self: Pin<&mut Self>) {
        self.as_mut().move_vertical(-1);
    }

    pub fn move_down(mut self: Pin<&mut Self>) {
        self.as_mut().move_vertical(1);
    }

    pub fn activate_current(mut self: Pin<&mut Self>) {
        match self.as_ref().rust().mode {
            UiMode::Grid => {
                let Some((environment_id, slot_index)) = ({
                    let binding = self.as_ref();
                    let rust = binding.rust();
                    rust.items
                        .get(rust.current_index.max(0) as usize)
                        .map(|item| (item.environment_id.clone(), item.slot_index))
                }) else {
                    return;
                };
                if slot_index <= 0 || environment_id.is_empty() {
                    return;
                }

                if let Err(error) =
                    self.as_ref()
                        .send_request::<serde_json::Value>(Request::WorkspaceGoto {
                            env: Some(environment_id.clone()),
                            slot: slot_index,
                        })
                {
                    warn!(
                        "failed to activate environment {} slot {}: {error}",
                        environment_id, slot_index
                    );
                    return;
                }
            }
            UiMode::Switcher => {
                let workspace_id = self.as_ref().current_physical_workspace_id();
                if workspace_id <= 0 {
                    return;
                }

                if let Err(error) = self.as_ref().send_request::<serde_json::Value>(
                    Request::WorkspaceGotoPhysical { workspace_id },
                ) {
                    warn!("failed to activate workspace {workspace_id}: {error}");
                    return;
                }
            }
        }

        self.as_mut().hide_overlay();
    }

    pub fn activate_workspace_at(mut self: Pin<&mut Self>, index: i32) {
        self.as_mut().set_selection(index);
        self.as_mut().activate_current();
    }

    pub fn cancel(mut self: Pin<&mut Self>) {
        self.as_mut().hide_overlay();
    }

    pub fn handle_modifier_released(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().mode == UiMode::Switcher && *self.visible() {
            self.as_mut().activate_current();
        }
    }

    pub fn refresh_snapshot_if_visible(mut self: Pin<&mut Self>) {
        if !*self.visible() || self.as_ref().rust().navigation_grace_active() {
            return;
        }

        if let Err(error) = self.as_mut().refresh_snapshot(true) {
            warn!("failed to refresh UI snapshot: {error}");
        }
    }

    pub fn refresh_snapshot_if_stale(mut self: Pin<&mut Self>) {
        if !*self.visible() || self.as_ref().rust().navigation_grace_active() {
            return;
        }

        let should_refresh = {
            let binding = self.as_ref();
            let rust = binding.rust();
            !rust.has_snapshot
                || now_ms_qt().saturating_sub(rust.last_snapshot_ms)
                    >= OPEN_REFRESH_STALE_THRESHOLD_MS
        };

        if !should_refresh {
            return;
        }

        if let Err(error) = self.as_mut().refresh_snapshot(true) {
            warn!("failed to refresh stale UI snapshot: {error}");
        }
    }

    pub fn pump_session_commands(mut self: Pin<&mut Self>) {
        match self.as_ref().rust().mode {
            UiMode::Switcher => {
                if !*self.visible() {
                    return;
                }

                for command in drain_switcher_session_commands() {
                    match command {
                        UiSessionCommand::StepForward => self.as_mut().select_next(),
                        UiSessionCommand::StepReverse => self.as_mut().select_previous(),
                        UiSessionCommand::Activate => {
                            self.as_mut().activate_current();
                            break;
                        }
                        UiSessionCommand::Cancel => {
                            self.as_mut().cancel();
                            break;
                        }
                    }
                }
            }
            UiMode::Grid => {
                if !self.as_ref().rust().resident_mode {
                    return;
                }

                for command in drain_grid_session_commands() {
                    match command {
                        GridUiCommand::Open => {
                            self.as_mut().show_overlay();
                            let next_generation = self.as_ref().rust().open_generation + 1;
                            self.as_mut().set_open_generation(next_generation);
                        }
                        GridUiCommand::Close => self.as_mut().hide_overlay(),
                        GridUiCommand::Refresh => {
                            if let Err(error) = self.as_mut().refresh_snapshot(true) {
                                warn!("failed to refresh grid snapshot: {error}");
                            }
                        }
                    }
                }
            }
        }
    }

    fn load_snapshot_and_show(mut self: Pin<&mut Self>) -> anyhow::Result<()> {
        self.as_mut().refresh_snapshot(false)?;
        self.as_mut().show_overlay();
        Ok(())
    }

    fn refresh_snapshot(mut self: Pin<&mut Self>, preserve_selection: bool) -> anyhow::Result<()> {
        self.as_mut().set_loading(true);
        let result = match self.as_ref().rust().mode {
            UiMode::Switcher => {
                let preferred_workspace_id = preserve_selection
                    .then(|| self.as_ref().current_switcher_selection_workspace_id())
                    .flatten();
                let snapshot = self.as_ref().send_request::<SwitcherSnapshot>(
                    Request::UiSnapshotSwitcher {
                        reverse: self.as_ref().rust().reverse,
                    },
                )?;
                self.as_mut()
                    .apply_switcher_snapshot(snapshot, preferred_workspace_id);
                Ok(())
            }
            UiMode::Grid => {
                let preferred_selection = preserve_selection
                    .then(|| self.as_ref().current_grid_selection_key())
                    .flatten();
                let cwd = std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned());
                let snapshot = self
                    .as_ref()
                    .send_request::<GridSnapshot>(Request::UiSnapshotGrid { cwd })?;
                self.as_mut()
                    .apply_grid_snapshot(snapshot, preferred_selection);
                Ok(())
            }
        };

        self.as_mut().set_loading(false);
        if result.is_ok() {
            self.as_mut().rust_mut().last_snapshot_ms = now_ms_qt();
            self.as_mut().set_has_snapshot(true);
        }

        result
    }

    fn show_overlay(mut self: Pin<&mut Self>) {
        self.as_mut().set_visible(true);
        if uses_resident_grid_window(
            self.as_ref().rust().mode,
            self.as_ref().rust().resident_mode,
        ) {
            qobject::hyprexpo_set_root_window_interactive(true);
        } else {
            qobject::hyprexpo_set_root_window_visible(true);
        }
    }

    fn hide_overlay(mut self: Pin<&mut Self>) {
        self.as_mut().set_visible(false);
        if uses_resident_grid_window(
            self.as_ref().rust().mode,
            self.as_ref().rust().resident_mode,
        ) {
            qobject::hyprexpo_set_root_window_interactive(false);
        } else {
            qobject::hyprexpo_set_root_window_visible(false);
        }
    }

    fn apply_switcher_snapshot(
        mut self: Pin<&mut Self>,
        snapshot: SwitcherSnapshot,
        preferred_workspace_id: Option<i32>,
    ) {
        let items = snapshot
            .items
            .into_iter()
            .map(item_from_switcher_snapshot)
            .collect::<Vec<_>>();
        let current_index = preferred_workspace_id
            .and_then(|workspace_id| {
                items
                    .iter()
                    .position(|item| item.workspace_id == workspace_id)
                    .map(|index| index as i32)
            })
            .unwrap_or_else(|| normalize_index(snapshot.initial_index, items.len()));

        let unchanged_snapshot = {
            let binding = self.as_ref();
            let rust = binding.rust();
            snapshot_matches_switcher_layout(&rust.items, &items)
                && !*binding.grid_mode()
                && *binding.grid_row_count() == 0
                && *binding.grid_column_count() == 0
        };

        if unchanged_snapshot {
            self.as_mut().set_current_index(current_index);
            self.as_mut().set_grid_mode(false);
            self.as_mut().set_grid_row_count(0);
            self.as_mut().set_grid_column_count(0);
            return;
        }

        let changed_ranges = {
            let binding = self.as_ref();
            let rust = binding.rust();
            if switcher_snapshot_is_structural_match(&rust.items, &items) {
                Some(changed_row_ranges(&rust.items, &items))
            } else {
                None
            }
        };

        {
            let mut rust = self.as_mut().rust_mut();
            rust.items = items;
            rust.row_to_indices.clear();
        }

        self.as_mut().set_current_index(current_index);
        self.as_mut().set_grid_mode(false);
        self.as_mut().set_grid_row_count(0);
        self.as_mut().set_grid_column_count(0);
        if let Some(changed_ranges) = changed_ranges {
            self.as_mut().emit_rows_changed(changed_ranges);
        } else {
            self.as_mut().reset_model();
        }
    }

    fn apply_grid_snapshot(
        mut self: Pin<&mut Self>,
        snapshot: GridSnapshot,
        preferred_selection: Option<GridSelectionKey>,
    ) {
        let items = snapshot
            .items
            .into_iter()
            .map(item_from_grid_snapshot)
            .collect::<Vec<_>>();

        let current_index = preferred_selection
            .and_then(|selection| {
                items
                    .iter()
                    .position(|item| {
                        item.environment_id == selection.environment_id
                            && item.slot_index == selection.slot_index
                            && item.physical_workspace_id == selection.physical_workspace_id
                    })
                    .map(|index| index as i32)
            })
            .unwrap_or_else(|| normalize_index(snapshot.initial_index, items.len()));
        let row_to_indices = build_row_indices(&items);

        let unchanged_snapshot = {
            let binding = self.as_ref();
            let rust = binding.rust();
            snapshot_matches_grid_layout(&rust.items, &items)
                && rust.row_to_indices == row_to_indices
                && *binding.grid_mode()
                && *binding.grid_row_count() == snapshot.row_count
                && *binding.grid_column_count() == snapshot.max_column_count
        };

        if unchanged_snapshot {
            // Avoid rebuilding delegates on periodic refreshes when the snapshot payload
            // is unchanged. Selection is derived from currentIndex in QML now.
            self.as_mut().set_current_index(current_index);
            self.as_mut().set_grid_mode(true);
            self.as_mut().set_grid_row_count(snapshot.row_count);
            self.as_mut()
                .set_grid_column_count(snapshot.max_column_count);
            return;
        }

        let changed_ranges = {
            let binding = self.as_ref();
            let rust = binding.rust();
            if grid_snapshot_is_structural_match(&rust.items, &items)
                && rust.row_to_indices == row_to_indices
                && *binding.grid_mode()
                && *binding.grid_row_count() == snapshot.row_count
                && *binding.grid_column_count() == snapshot.max_column_count
            {
                Some(changed_row_ranges(&rust.items, &items))
            } else {
                None
            }
        };

        {
            let mut rust = self.as_mut().rust_mut();
            rust.items = items;
            rust.row_to_indices = row_to_indices;
        }

        self.as_mut().set_current_index(current_index);
        self.as_mut().set_grid_mode(true);
        self.as_mut().set_grid_row_count(snapshot.row_count);
        self.as_mut()
            .set_grid_column_count(snapshot.max_column_count);
        if let Some(changed_ranges) = changed_ranges {
            self.as_mut().emit_rows_changed(changed_ranges);
        } else {
            self.as_mut().reset_model();
        }
    }

    fn move_linear(mut self: Pin<&mut Self>, delta: i32) {
        let count = self.as_ref().rust().items.len() as i32;
        if count <= 0 {
            return;
        }

        let current_index = self.as_ref().rust().current_index;
        let next = if current_index < 0 {
            0
        } else {
            (current_index + delta).rem_euclid(count)
        };
        self.as_mut().set_selection(next);
    }

    fn move_vertical(mut self: Pin<&mut Self>, delta: i32) {
        if self.as_ref().rust().mode != UiMode::Grid {
            self.as_mut().move_linear(delta);
            return;
        }

        let Some((row_index, _)) = self.as_ref().current_grid_position() else {
            return;
        };
        let current_slot_index = {
            let binding = self.as_ref();
            let Some(current_item) = binding
                .rust()
                .items
                .get(binding.rust().current_index.max(0) as usize)
            else {
                return;
            };
            current_item.slot_index
        };

        let next_row = row_index + delta;
        if next_row < 0 || next_row >= self.as_ref().rust().row_to_indices.len() as i32 {
            return;
        }

        let next_index = {
            let binding = self.as_ref();
            let row = &binding.rust().row_to_indices[next_row as usize];
            let mut candidate = row
                .iter()
                .copied()
                .filter(|index| {
                    binding
                        .rust()
                        .items
                        .get(*index)
                        .map(|item| item.slot_index <= current_slot_index)
                        .unwrap_or(false)
                })
                .max_by_key(|index| {
                    binding
                        .rust()
                        .items
                        .get(*index)
                        .map(|item| item.slot_index)
                        .unwrap_or_default()
                });

            if candidate.is_none() {
                candidate = row.first().copied();
            }

            candidate.map(|value| value as i32)
        };

        if let Some(next_index) = next_index {
            self.as_mut().set_selection(next_index);
        }
    }

    fn current_grid_position(&self) -> Option<(i32, i32)> {
        let item = self
            .rust()
            .items
            .get(self.rust().current_index.max(0) as usize)?;
        Some((item.row_index, item.column_index))
    }

    fn current_physical_workspace_id(&self) -> i32 {
        self.rust()
            .items
            .get(self.rust().current_index.max(0) as usize)
            .map(|item| {
                if self.rust().mode == UiMode::Grid {
                    item.physical_workspace_id
                } else {
                    item.workspace_id
                }
            })
            .unwrap_or(-1)
    }

    fn current_switcher_selection_workspace_id(&self) -> Option<i32> {
        if self.rust().mode != UiMode::Switcher {
            return None;
        }

        self.rust()
            .items
            .get(self.rust().current_index.max(0) as usize)
            .map(|item| item.workspace_id)
    }

    fn current_grid_selection_key(&self) -> Option<GridSelectionKey> {
        if self.rust().mode != UiMode::Grid {
            return None;
        }

        self.rust()
            .items
            .get(self.rust().current_index.max(0) as usize)
            .map(|item| GridSelectionKey {
                environment_id: item.environment_id.clone(),
                slot_index: item.slot_index,
                physical_workspace_id: item.physical_workspace_id,
            })
    }

    fn set_selection(mut self: Pin<&mut Self>, index: i32) {
        let normalized = normalize_index(index, self.as_ref().rust().items.len());
        if *self.current_index() == normalized {
            return;
        }

        self.as_mut().rust_mut().last_navigation_ms = now_ms_qt();
        self.as_mut().set_current_index(normalized);
    }

    fn send_request<T: serde::de::DeserializeOwned>(&self, request: Request) -> anyhow::Result<T> {
        let paths = resolve_runtime_paths();
        send_request_with_fallbacks(&paths.server_socket_path, &request)
    }

    fn reset_model(mut self: Pin<&mut Self>) {
        unsafe {
            self.as_mut().begin_reset_model();
            self.as_mut().end_reset_model();
        }
    }

    fn emit_rows_changed(mut self: Pin<&mut Self>, changed_ranges: Vec<(i32, i32)>) {
        if changed_ranges.is_empty() {
            return;
        }

        let roles = volatile_roles();
        let mut model: Pin<&mut qobject::QAbstractListModel> = self.as_mut().upcast_pin();
        for (first_row, last_row) in changed_ranges {
            qobject::hyprexpo_emit_rows_changed(model.as_mut(), first_row, last_row, &roles);
        }
    }
}

impl ControllerRust {
    fn navigation_grace_active(&self) -> bool {
        self.last_navigation_ms != 0
            && now_ms_qt().saturating_sub(self.last_navigation_ms) < NAVIGATION_REFRESH_GRACE_MS
    }
}

fn item_from_switcher_snapshot(item: WorkspaceCardSnapshot) -> UiItem {
    UiItem {
        workspace_id: item.workspace_id,
        workspace_name: item.workspace_name,
        subtitle: item.subtitle,
        app_class: item.app_class,
        window_count: item.window_count,
        active: item.active,
        preview_path: item.preview_path,
        generation: item.generation,
        ..UiItem::default()
    }
}

fn item_from_grid_snapshot(item: GridCellSnapshot) -> UiItem {
    UiItem {
        workspace_id: item.physical_workspace_id,
        workspace_name: item.workspace_name,
        subtitle: item.subtitle,
        app_class: item.app_class,
        window_count: item.window_count,
        active: item.active,
        preview_path: item.preview_path,
        generation: item.generation,
        environment_id: item.environment_id,
        environment_display_id: item.environment_display_id,
        slot_index: item.slot_index,
        physical_workspace_id: item.physical_workspace_id,
        environment_locked: item.environment_locked,
        show_environment_label: item.show_environment_label,
        row_index: item.row_index,
        column_index: item.column_index,
    }
}

fn build_row_indices(items: &[UiItem]) -> Vec<Vec<usize>> {
    let mut rows = BTreeMap::<i32, Vec<usize>>::new();
    for (index, item) in items.iter().enumerate() {
        rows.entry(item.row_index).or_default().push(index);
    }
    rows.into_values().collect()
}

fn switcher_snapshot_is_structural_match(current: &[UiItem], next: &[UiItem]) -> bool {
    current.len() == next.len()
        && current
            .iter()
            .zip(next.iter())
            .all(|(left, right)| left.workspace_id == right.workspace_id)
}

fn grid_snapshot_is_structural_match(current: &[UiItem], next: &[UiItem]) -> bool {
    current.len() == next.len()
        && current
            .iter()
            .zip(next.iter())
            .all(|(left, right)| grid_item_structural_eq(left, right))
}

fn grid_item_structural_eq(left: &UiItem, right: &UiItem) -> bool {
    left.workspace_id == right.workspace_id
        && left.environment_id == right.environment_id
        && left.environment_display_id == right.environment_display_id
        && left.slot_index == right.slot_index
        && left.physical_workspace_id == right.physical_workspace_id
        && left.environment_locked == right.environment_locked
        && left.show_environment_label == right.show_environment_label
        && left.row_index == right.row_index
        && left.column_index == right.column_index
}

fn volatile_item_eq(left: &UiItem, right: &UiItem) -> bool {
    left.workspace_name == right.workspace_name
        && left.subtitle == right.subtitle
        && left.app_class == right.app_class
        && left.window_count == right.window_count
        && left.active == right.active
        && left.preview_path == right.preview_path
        && left.generation == right.generation
}

fn snapshot_matches_switcher_layout(current: &[UiItem], next: &[UiItem]) -> bool {
    switcher_snapshot_is_structural_match(current, next)
        && current
            .iter()
            .zip(next.iter())
            .all(|(left, right)| volatile_item_eq(left, right))
}

fn snapshot_matches_grid_layout(current: &[UiItem], next: &[UiItem]) -> bool {
    grid_snapshot_is_structural_match(current, next)
        && current
            .iter()
            .zip(next.iter())
            .all(|(left, right)| volatile_item_eq(left, right))
}

fn changed_row_ranges(current: &[UiItem], next: &[UiItem]) -> Vec<(i32, i32)> {
    let changed_rows = current
        .iter()
        .zip(next.iter())
        .enumerate()
        .filter_map(|(index, (left, right))| (!volatile_item_eq(left, right)).then_some(index))
        .collect::<Vec<_>>();
    coalesce_row_ranges(&changed_rows)
}

fn coalesce_row_ranges(changed_rows: &[usize]) -> Vec<(i32, i32)> {
    let Some(&first_row) = changed_rows.first() else {
        return Vec::new();
    };

    let mut ranges = Vec::new();
    let mut range_start = first_row as i32;
    let mut range_end = range_start;

    for &row in &changed_rows[1..] {
        let row = row as i32;
        if row == range_end + 1 {
            range_end = row;
            continue;
        }

        ranges.push((range_start, range_end));
        range_start = row;
        range_end = row;
    }

    ranges.push((range_start, range_end));
    ranges
}

fn volatile_roles() -> QList<i32> {
    [
        ROLE_NAME,
        ROLE_SUBTITLE,
        ROLE_APP_CLASS,
        ROLE_WINDOW_COUNT,
        ROLE_ACTIVE,
        ROLE_PREVIEW,
        ROLE_GENERATION,
    ]
    .into_iter()
    .collect()
}

fn uses_resident_grid_window(mode: UiMode, resident_mode: bool) -> bool {
    mode == UiMode::Grid && resident_mode
}

fn normalize_index(index: i32, item_count: usize) -> i32 {
    if item_count == 0 {
        -1
    } else if index < 0 {
        0
    } else if index >= item_count as i32 {
        item_count as i32 - 1
    } else {
        index
    }
}

fn now_ms_qt() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl cxx_qt::Initialize for qobject::Controller {
    fn initialize(self: Pin<&mut Self>) {}
}

#[cfg(test)]
mod tests {
    use super::{
        changed_row_ranges, coalesce_row_ranges, grid_item_structural_eq, item_from_grid_snapshot,
        switcher_snapshot_is_structural_match, uses_resident_grid_window, volatile_item_eq,
        GridCellSnapshot, UiItem, UiMode,
    };

    #[test]
    fn uses_resident_window_only_for_resident_grid() {
        assert!(uses_resident_grid_window(UiMode::Grid, true));
        assert!(!uses_resident_grid_window(UiMode::Grid, false));
        assert!(!uses_resident_grid_window(UiMode::Switcher, true));
        assert!(!uses_resident_grid_window(UiMode::Switcher, false));
    }

    #[test]
    fn switcher_structure_ignores_volatile_changes() {
        let current = vec![UiItem {
            workspace_id: 4,
            workspace_name: "one".into(),
            generation: 1,
            ..UiItem::default()
        }];
        let next = vec![UiItem {
            workspace_id: 4,
            workspace_name: "two".into(),
            generation: 2,
            active: true,
            ..UiItem::default()
        }];

        assert!(switcher_snapshot_is_structural_match(&current, &next));
        assert!(!volatile_item_eq(&current[0], &next[0]));
    }

    #[test]
    fn grid_structure_rejects_layout_changes() {
        let left = item_from_grid_snapshot(GridCellSnapshot {
            workspace_name: "1".into(),
            subtitle: String::new(),
            app_class: String::new(),
            window_count: 1,
            active: false,
            preview_path: String::new(),
            generation: 1,
            environment_id: "env".into(),
            environment_display_id: "Env".into(),
            slot_index: 1,
            physical_workspace_id: 1,
            environment_locked: false,
            show_environment_label: true,
            row_index: 0,
            column_index: 0,
        });
        let right = UiItem {
            row_index: 1,
            ..left.clone()
        };

        assert!(!grid_item_structural_eq(&left, &right));
    }

    #[test]
    fn changed_rows_only_track_volatile_differences() {
        let current = vec![
            UiItem {
                workspace_id: 1,
                workspace_name: "one".into(),
                generation: 1,
                ..UiItem::default()
            },
            UiItem {
                workspace_id: 2,
                workspace_name: "two".into(),
                generation: 1,
                ..UiItem::default()
            },
            UiItem {
                workspace_id: 3,
                workspace_name: "three".into(),
                generation: 1,
                ..UiItem::default()
            },
        ];
        let next = vec![
            current[0].clone(),
            UiItem {
                workspace_name: "second".into(),
                generation: 2,
                ..current[1].clone()
            },
            UiItem {
                preview_path: "/tmp/preview.png".into(),
                ..current[2].clone()
            },
        ];

        assert_eq!(changed_row_ranges(&current, &next), vec![(1, 2)]);
    }

    #[test]
    fn coalesces_adjacent_changed_rows() {
        assert_eq!(
            coalesce_row_ranges(&[0, 1, 3, 4, 7]),
            vec![(0, 1), (3, 4), (7, 7)]
        );
    }
}

use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceDescriptor {
    pub id: i32,
    pub name: String,
    pub subtitle: String,
    pub app_class: String,
    pub window_count: i32,
    pub focus_history_rank: i32,
    pub active: bool,
}

#[derive(Debug, Deserialize)]
struct MonitorInfo {
    #[serde(default)]
    focused: bool,
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: WorkspaceRef,
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceRef {
    #[serde(default)]
    id: i32,
}

#[derive(Debug, Deserialize)]
struct WorkspaceInfo {
    #[serde(default)]
    id: i32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    windows: i32,
    #[serde(default, rename = "lastwindowtitle")]
    last_window_title: String,
}

#[derive(Debug, Deserialize)]
struct ClientInfo {
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    workspace: WorkspaceRef,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "class")]
    app_class: String,
    #[serde(default = "max_i32", rename = "focusHistoryID")]
    focus_history_id: i32,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceClientInfo {
    title: String,
    app_class: String,
    window_count: i32,
    focus_history_rank: i32,
}

pub fn build_workspace_descriptors(
    monitors_json: &[u8],
    workspaces_json: &[u8],
    clients_json: &[u8],
) -> Vec<WorkspaceDescriptor> {
    let monitors = serde_json::from_slice::<Vec<MonitorInfo>>(monitors_json).unwrap_or_default();
    let workspaces =
        serde_json::from_slice::<Vec<WorkspaceInfo>>(workspaces_json).unwrap_or_default();
    let clients = serde_json::from_slice::<Vec<ClientInfo>>(clients_json).unwrap_or_default();

    let active_workspace_id = monitors
        .iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.active_workspace.id)
        .unwrap_or(-1);

    let mut client_info_by_workspace = std::collections::HashMap::<i32, WorkspaceClientInfo>::new();
    for client in clients
        .into_iter()
        .filter(|client| client.mapped && client.workspace.id > 0)
    {
        let entry = client_info_by_workspace
            .entry(client.workspace.id)
            .or_insert_with(|| WorkspaceClientInfo {
                focus_history_rank: i32::MAX,
                ..WorkspaceClientInfo::default()
            });

        entry.window_count += 1;
        if client.focus_history_id < entry.focus_history_rank {
            entry.focus_history_rank = client.focus_history_id;
            entry.title = client.title;
            entry.app_class = client.app_class;
        }
    }

    let mut descriptors = workspaces
        .into_iter()
        .filter(|workspace| workspace.id > 0 && workspace.windows > 0)
        .map(|workspace| {
            let client_info = client_info_by_workspace.get(&workspace.id);
            let app_class = client_info
                .map(|item| item.app_class.clone())
                .unwrap_or_default();
            let subtitle = client_info
                .filter(|item| !item.title.is_empty())
                .map(|item| item.title.clone())
                .or_else(|| (!app_class.is_empty()).then_some(app_class.clone()))
                .or_else(|| {
                    (!workspace.last_window_title.is_empty())
                        .then_some(workspace.last_window_title.clone())
                })
                .unwrap_or_else(|| "No recent window".to_owned());

            WorkspaceDescriptor {
                id: workspace.id,
                name: if workspace.name.is_empty() {
                    workspace.id.to_string()
                } else {
                    workspace.name
                },
                subtitle,
                app_class,
                window_count: client_info
                    .map(|item| item.window_count)
                    .unwrap_or(workspace.windows),
                focus_history_rank: client_info
                    .map(|item| item.focus_history_rank)
                    .unwrap_or(i32::MAX),
                active: workspace.id == active_workspace_id,
            }
        })
        .collect::<Vec<_>>();

    descriptors.sort_by(|left, right| {
        left.active
            .cmp(&right.active)
            .reverse()
            .then_with(|| left.focus_history_rank.cmp(&right.focus_history_rank))
            .then_with(|| left.id.cmp(&right.id))
    });

    descriptors
}

pub fn sort_workspaces_for_switcher(
    workspaces: &mut [WorkspaceDescriptor],
    mru_workspace_ids: &[i32],
) {
    workspaces.sort_by(|left, right| {
        left.active
            .cmp(&right.active)
            .reverse()
            .then_with(|| {
                mru_rank(mru_workspace_ids, left.id).cmp(&mru_rank(mru_workspace_ids, right.id))
            })
            .then_with(|| left.focus_history_rank.cmp(&right.focus_history_rank))
            .then_with(|| left.id.cmp(&right.id))
    });
}

pub fn initial_selection_index(workspaces: &[WorkspaceDescriptor], reverse: bool) -> i32 {
    if workspaces.is_empty() {
        return -1;
    }

    if workspaces.len() == 1 {
        return 0;
    }

    if reverse {
        return workspaces.len() as i32 - 1;
    }

    workspaces
        .iter()
        .position(|workspace| !workspace.active)
        .map(|index| index as i32)
        .unwrap_or(0)
}

fn mru_rank(mru_workspace_ids: &[i32], workspace_id: i32) -> usize {
    mru_workspace_ids
        .iter()
        .position(|candidate| *candidate == workspace_id)
        .unwrap_or(usize::MAX)
}

const fn max_i32() -> i32 {
    i32::MAX
}

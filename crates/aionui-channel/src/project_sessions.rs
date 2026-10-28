//! Project selection for channel sessions. Callback data carries IDs, never paths.
use std::collections::HashMap;

use aionui_db::{IProjectStore, ProjectRow, Role};

use super::{ActionExecutor, ActionResponse};
use crate::error::ChannelError;
use crate::types::{ActionBehavior, ActionButton, UnifiedAction};

const PAGE_SIZE: usize = 8;

fn response(text: String, buttons: Vec<Vec<ActionButton>>) -> ActionResponse {
    ActionResponse {
        text: Some(text),
        parse_mode: None,
        buttons: Some(buttons),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn button(label: String, action: &str, key: &str, value: String) -> ActionButton {
    ActionButton {
        label,
        action: action.into(),
        params: Some(HashMap::from([(key.into(), value)])),
    }
}

/// Re-resolve the owner's current standard-project workspace on every selection.
pub(crate) async fn workspace(
    store: &dyn IProjectStore,
    owner: &str,
    id: &str,
) -> Result<Option<(ProjectRow, String)>, ChannelError> {
    let Some(project) = store.get_project(owner, id).await?.filter(|p| p.kind == "standard") else {
        return Ok(None);
    };
    let entries = store.list_entries(owner, id).await?;
    let Some((_, folder)) = entries.into_iter().find(|(e, _)| e.role == Role::Workspace.as_str()) else {
        return Ok(None);
    };
    let Some(path) = url::Url::parse(&folder.resource_uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
    else {
        return Ok(None);
    };
    if !path.is_dir() {
        return Ok(None);
    }
    Ok(Some((project, path.to_string_lossy().into_owned())))
}

impl ActionExecutor {
    pub(super) async fn project_picker(
        &self,
        owner: &str,
        action: &UnifiedAction,
    ) -> Result<ActionResponse, ChannelError> {
        let store = self
            .project_store
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Project selection unavailable".into()))?;
        let projects = store.list_standard_projects(owner).await?;
        let page = action
            .params
            .as_ref()
            .and_then(|p| p.get("page"))
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0);
        let pages = projects.len().div_ceil(PAGE_SIZE).max(1);
        let page = page.min(pages - 1);
        let mut buttons = vec![vec![button(
            "Temporary chat (no project)".into(),
            "session.go",
            "p",
            "default".into(),
        )]];
        for project in projects.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
            buttons.push(vec![button(
                project.name.clone(),
                "session.go",
                "p",
                project.project_id.clone(),
            )]);
        }
        let mut navigation = Vec::new();
        if page > 0 {
            navigation.push(button(
                "Previous projects".into(),
                "session.new",
                "page",
                (page - 1).to_string(),
            ));
        }
        if page + 1 < pages {
            navigation.push(button(
                "More projects".into(),
                "session.new",
                "page",
                (page + 1).to_string(),
            ));
        }
        if !navigation.is_empty() {
            buttons.push(navigation);
        }
        Ok(response(
            format!(
                "Choose a project for the new session (page {} of {}).\nYour current session stays unchanged until you choose. Project sessions use that project's workspace, rules and memory.",
                page + 1,
                pages
            ),
            buttons,
        ))
    }

    pub(super) async fn start_project_session(
        &self,
        owner: &str,
        action: &UnifiedAction,
        user: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let selected = action.params.as_ref().and_then(|p| p.get("p"));
        let (name, selected_workspace) = match selected.map(String::as_str) {
            Some("default") => ("Temporary chat".to_owned(), None),
            Some(id) => {
                let store = self
                    .project_store
                    .as_ref()
                    .ok_or_else(|| ChannelError::InvalidConfig("Project selection unavailable".into()))?;
                let Some((project, path)) = workspace(store.as_ref(), owner, id).await? else {
                    return Ok(response("This project is unavailable. Your current session was not changed. Use /new to choose an available project.".into(), Vec::new()));
                };
                (project.name, Some(path))
            }
            None => return self.project_picker(owner, action).await,
        };
        let config = self.settings.get_agent_config(owner, action.context.platform).await?;
        let session = self
            .session_mgr
            .reset_session(
                owner,
                user,
                &action.context.chat_id,
                &config.agent_type,
                selected_workspace.as_deref(),
            )
            .await?;
        tracing::info!(session_id = %session.id, project_selected = selected_workspace.is_some(), "channel session scope selected");
        Ok(response(
            format!(
                "New session created.\nProject: {name}\nSession: {}\nSend a message to start.",
                &session.id[..8]
            ),
            vec![vec![button("New Session".into(), "session.new", "page", "0".into())]],
        ))
    }
}

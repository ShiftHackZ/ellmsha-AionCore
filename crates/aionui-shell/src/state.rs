use std::sync::Arc;

use aionui_system::ClientPrefService;

use crate::shell::ShellService;
use crate::stt::SttService;

#[derive(Clone)]
pub struct ShellRouterState {
    pub background_process_service: Arc<crate::BackgroundProcessService>,
    pub shell_service: Arc<ShellService>,
    pub stt_service: Arc<SttService>,
    pub client_pref_service: ClientPrefService,
}

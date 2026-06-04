use crate::backend::AppState;
use dioxus::prelude::*;

#[derive(Clone, Copy)]
pub struct AppContext {
    pub state: &'static AppState,
}

pub fn use_app_state() -> AppState {
    use_context::<AppContext>().state.clone()
}

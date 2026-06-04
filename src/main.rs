mod backend;
mod ui;

use ui::App;

fn main() {
    env_logger::init();

    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("smarrst");
    let state = backend::AppState::new(&data_dir).expect("failed to init app state");

    // Leak the state to get a 'static reference so we can hand it to a fn() -> Element.
    let state: &'static backend::AppState = Box::leak(Box::new(state));

    dioxus::LaunchBuilder::desktop()
        .with_context_provider(move || Box::new(ui::context::AppContext { state }))
        .launch(App);
}

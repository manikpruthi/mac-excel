use super::Dialog;

/// Help ▸ About Mac Excel: the app's name, version and author.
pub(super) fn about(ui: &mut egui::Ui, _d: &mut Dialog) {
    ui.heading("Mac Excel");
    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
    ui.label("From Manik Pruthi");
}

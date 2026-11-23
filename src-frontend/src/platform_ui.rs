//! Native dialogs (`rfd`) and OS integration used by the screens.

use std::path::{Path, PathBuf};

/// Native folder picker (blocks the UI thread while open, like any modal OS dialog). `None` = cancelled.
pub fn pick_folder(title: &str, start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(start) = start.filter(|p| p.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_folder()
}

/// "Reveal in Finder / Explorer" label for this OS.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(windows) {
    "Show in Explorer"
} else {
    "Open containing folder"
};

/// Whether files can be dragged out of the window into other apps on this OS.
pub const CAN_DRAG_FILES_OUT: bool = cfg!(any(windows, target_os = "macos"));

/// Dragging files out of the window into another app (an editor, a chat, Explorer / Finder), which opens or copies
/// them there. A widget asks with [`FileDrag::request`] while drawing; the app starts the OS drag after the frame,
/// where it has the window handle.
#[derive(Default)]
pub struct FileDrag {
    pending: Vec<PathBuf>,
    /// The OS drag swallowed the mouse release: egui still thinks the button is down, so the next frame's input
    /// gets a release.
    release_pointer: bool,
}

impl FileDrag {
    /// Starts dragging `paths` (absolute) out of the window once this frame ends. Missing files are left out.
    pub fn request(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if CAN_DRAG_FILES_OUT {
            self.pending = paths.into_iter().filter(|p| p.exists()).collect();
        }
    }

    /// Runs the requested drag, if any. Windows: blocks until the files are dropped or the drag is cancelled.
    #[cfg(any(windows, target_os = "macos"))]
    pub fn start(&mut self, window: &eframe::Frame) -> Result<(), String> {
        let paths = std::mem::take(&mut self.pending);
        if paths.is_empty() {
            return Ok(());
        }
        self.release_pointer = true;
        let icon = drag::Image::Raw(include_bytes!("../assets/32x32.png").to_vec());
        drag::start_drag(window, drag::DragItem::Files(paths), icon, |_, _| {}, drag::Options::default()).map_err(|e| e.to_string())
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn start(&mut self, _window: &eframe::Frame) -> Result<(), String> {
        self.pending.clear();
        Ok(())
    }

    /// Adds the mouse release the OS drag swallowed (see `release_pointer`).
    pub fn fix_input(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if !std::mem::take(&mut self.release_pointer) {
            return;
        }
        let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
        input.events.insert(
            0,
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: ctx.input(|i| i.modifiers),
            },
        );
    }
}

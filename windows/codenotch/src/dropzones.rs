//! The four places the notch can land, shown while it is carried from the move handle.
//!
//! A transparent, click-through window over the monitor the notch is on, drawing the notch's own
//! outline at each edge — the shape differs per edge, and showing the real silhouette is what makes
//! the choice legible before it is made. The page is `ui/dropzones.html`; this side only says where
//! the window goes and which zone the pointer is nearest.

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "dropzones";
/// The last state pushed, for a page that finished loading after it was sent.
static CURRENT: Mutex<Option<Zones>> = Mutex::new(None);

/// What the page needs to draw itself: its own size, the notch's shape and the target. One shape for
/// all four zones, turned on its side for the flat edges, as the Mac does — the notch really is a
/// different size lying flat, but four outlines of four different sizes read as four different
/// things rather than as one notch offered four places.
#[derive(Clone, serde::Serialize)]
pub struct Zones {
    pub w: f64,
    pub h: f64,
    /// How deep the notch sits against its edge and how far it runs along it, in the page's CSS px
    pub depth: f64,
    pub length: f64,
    pub target: String,
    /// Where the target zone's centre sits along its edge, 0 to 1 of the work area: it follows the
    /// pointer, so the notch can be set anywhere along an edge and not only in its middle
    pub along: f64,
}

/// Built on the spot rather than kept hidden for the life of the app, which is what the settings
/// window stopped doing: a second WebView that is used for a second at a time is not worth a process.
pub fn show(app: &AppHandle, screen: &crate::Screen, zones: &Zones) {
    *CURRENT.lock().unwrap() = Some(zones.clone());
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.emit_to(LABEL, "zones", zones);
        let _ = w.show();
        return;
    }
    // The work area, not the monitor: the zones are welded to this window's own edges, so sizing it
    // to where the notch may actually land is what keeps a zone's promise true on the taskbar's edge.
    let (ax, ay, aw, ah) = screen.area();
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("dropzones.html".into()))
        .title("Codenotch drop zones")
        .position(ax as f64 / screen.scale, ay as f64 / screen.scale)
        .inner_size(aw as f64 / screen.scale, ah as f64 / screen.scale)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .resizable(false);
    match builder.build() {
        Ok(w) => {
            // The builder's figures are logical, and Windows converts them with whichever monitor it
            // decides the window belongs to — which is how a taskbar anywhere but the bottom left the
            // overlay short of the work area's corner and hanging off the far edge by the same amount.
            // The work area is physical and absolute, so it is pinned again here, as `place_notch` does.
            let _ = w.set_position(tauri::PhysicalPosition::new(ax, ay));
            let _ = w.set_size(tauri::PhysicalSize::new(aw.max(1) as u32, ah.max(1) as u32));
            let _ = w.set_ignore_cursor_events(true);
            // The page asks for the zones itself once it is listening; this covers the other order
            let _ = w.emit_to(LABEL, "zones", zones);
            // The notch is the thing being carried, so it belongs over the places it can go
            if let Some(notch) = app.get_webview_window("notch") {
                let _ = notch.set_always_on_top(true);
            }
        }
        Err(e) => crate::applog(&format!("drop zones: {e}")),
    }
}

/// The pointer carried the notch onto another screen: the overlay follows it there. Moved and resized
/// in place, in physical pixels as `show` pins it, rather than destroyed and rebuilt mid-carry.
pub fn move_to(app: &AppHandle, screen: &crate::Screen, zones: &Zones) {
    let Some(w) = app.get_webview_window(LABEL) else {
        show(app, screen, zones);
        return;
    };
    *CURRENT.lock().unwrap() = Some(zones.clone());
    let (ax, ay, aw, ah) = screen.area();
    let _ = w.set_position(tauri::PhysicalPosition::new(ax, ay));
    let _ = w.set_size(tauri::PhysicalSize::new(aw.max(1) as u32, ah.max(1) as u32));
    let _ = w.emit_to(LABEL, "zones", zones);
}

pub fn retarget(app: &AppHandle, zones: &Zones) {
    *CURRENT.lock().unwrap() = Some(zones.clone());
    let _ = app.emit_to(LABEL, "zones", zones);
}

pub fn hide(app: &AppHandle) {
    *CURRENT.lock().unwrap() = None;
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.destroy();
    }
}

/// What the page asks for when it loads, in case it started listening after the first push.
#[tauri::command]
pub fn get_zones() -> Option<Zones> {
    CURRENT.lock().unwrap().clone()
}

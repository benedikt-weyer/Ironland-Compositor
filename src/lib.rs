#![warn(rust_2018_idioms)]
#![allow(clippy::collapsible_match)]
// If no backend is enabled, a large portion of the codebase is unused.
// So silence this useless warning for the CI.
#![cfg_attr(
    not(any(feature = "winit", feature = "x11", feature = "udev")),
    allow(dead_code, unused_imports)
)]

// The config schema/file-I/O itself lives in the `ironland-config` crate
// (a separate, smithay-free workspace member so `ironlandctl` and the
// settings GUI can share it without a heavy build) - re-exported here under
// its old name so every existing `crate::config::Foo` reference keeps
// working unchanged.
pub use ironland_config as config;
pub mod backend;
pub mod input;
#[cfg(feature = "libei")]
pub mod libei;
pub mod protocols;
pub mod render;
pub mod shell;
pub mod state;
pub mod ui;

// Flat re-exports keep the pre-grouping `crate::foo` paths working.
#[cfg(feature = "udev")]
pub use backend::{session, udev};
#[cfg(feature = "winit")]
pub use backend::winit;
#[cfg(feature = "x11")]
pub use backend::x11;
pub use input::{focus, input_handler, keybindings};
pub use protocols::{
    cinema_mode, clipboard, display_filter, ext_workspace, focus_grab, foreign_toplevel, frame_capture, ironland_protocols,
    screencopy, shortcuts, vm_launch, workspace_windows,
};
#[cfg(any(feature = "udev", feature = "xwayland"))]
pub use render::cursor;
pub use render::{border, cinema, drawing, font, key_tracker, perf_overlay, rounded_corners, tint, vm_badge, wallpaper};
pub use ui::{launcher, lock, overview, permission_prompt, vm_overview};

pub use state::{AnvilState, ClientState};

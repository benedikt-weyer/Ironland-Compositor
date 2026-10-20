//! Wayland protocol server sides: the custom `ironland-*` extensions and the
//! standard protocols bridged to the shell.

pub mod clipboard;
pub mod display_filter;
pub mod ext_workspace;
pub mod focus_grab;
pub mod foreign_toplevel;
pub mod frame_capture;
pub mod ironland_protocols;
pub mod screencopy;
pub mod shortcuts;
pub mod vm_launch;
pub mod workspace_windows;

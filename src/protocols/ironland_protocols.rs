//! Server-side generated bindings for ironland-compositor's own small
//! Wayland protocol extensions (XML under `protocols/`): named shortcuts,
//! single-surface focus grabs, per-window workspace membership, an
//! in-memory clipboard history, a screen-wide color filter, and launching
//! applications inside VMs (see `crate::shortcuts`, `crate::focus_grab`,
//! `crate::display_filter`, `crate::workspace_windows`, `crate::clipboard`
//! and `crate::vm_launch`).
//!
//! These aren't in the `wayland-protocols`/`wayland-protocols-wlr` crates
//! (nothing standard covers either need, and Hyprland's own equivalents -
//! `hyprland-global-shortcuts-v1`, `hyprland-focus-grab-v1` - carry more
//! than this compositor needs: per-client app_id namespacing and
//! description/trigger-description strings for the former, a whole
//! multi-surface whitelist/commit protocol for the latter), so this module
//! generates its own bindings at compile time via `wayland-scanner`,
//! following the pattern `wayland-protocols`/`wayland-protocols-wlr` use
//! internally for every protocol they ship.
//!
//! The shell talks to these through its own Qt WaylandClient-based QML
//! plugin types, not through Quickshell's built-in `Quickshell.Hyprland`
//! module (which only speaks Hyprland's actual protocols).
//!
//! `wayland-server`/`wayland-backend` are direct dependencies pinned to the
//! exact versions `smithay` resolves to (see `Cargo.toml`), so the types
//! generated here are the same crate instances as
//! `smithay::reexports::wayland_server`'s - not a second, incompatible
//! copy - and can be used directly in this crate's `Dispatch2`/
//! `GlobalDispatch2` impls alongside smithay's own protocol modules.

#![allow(dead_code, non_camel_case_types, unused_imports, missing_docs, clippy::all)]

pub mod shortcuts {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-shortcuts-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-shortcuts-v1.xml");
}

pub mod focus_grab {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-focus-grab-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-focus-grab-v1.xml");
}

pub mod workspace_windows {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-workspace-windows-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-workspace-windows-v1.xml");
}

pub mod permission_prompt {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-permission-prompt-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-permission-prompt-v1.xml");
}

pub mod frame_capture {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-frame-capture-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-frame-capture-v1.xml");
}

pub mod clipboard_history {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-clipboard-history-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-clipboard-history-v1.xml");
}

pub mod vm_launch {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-vm-launch-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-vm-launch-v1.xml");
}

pub mod display_filter {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-display-filter-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("./protocols/ironland-display-filter-v1.xml");
}

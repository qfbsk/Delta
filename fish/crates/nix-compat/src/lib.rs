//! A compatibility layer for the parts of the `nix` crate that fish uses.
//!
//! On Unix this is simply a re-export of the real `nix` crate. On Windows the POSIX interfaces are
//! implemented over the Win32 API (CRT descriptors, console modes, process handles and job
//! objects). This is not an emulation environment: it is the port's operating-system boundary.

#[cfg(unix)]
pub use nix_upstream::*;

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::*;

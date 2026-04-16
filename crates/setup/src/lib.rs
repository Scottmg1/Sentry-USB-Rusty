//! SentryUSB setup orchestrator.
//!
//! Replaces the entire rc.local + setup-sentryusb shell script chain with
//! native Rust.  Each phase is a module that performs one logical step of the
//! setup process, reporting progress via a callback so the web UI can stream
//! live updates over WebSocket.

pub mod env;
pub mod partition;
pub mod disk_images;
pub mod system;
pub mod archive;
pub mod network;
pub mod readonly;
pub mod scripts;
pub mod runner;

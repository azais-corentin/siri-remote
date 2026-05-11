//! Linux-only BlueZ-over-D-Bus helpers: pairing agent, ObjectManager walk,
//! Device1/Adapter1 method shims.

#![cfg(target_os = "linux")]

pub mod agent;
pub mod device;

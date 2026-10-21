//! **A pool of warm Android devices and the app instances on them** -- what the omnidroid app shows
//! and its MCP server drives.
//!
//! * [`plan`]: where a launch goes and which devices idle out -- pure rules, each one a test.
//! * [`device`]: what is done to one device through its control channel (displays, users, the
//!   app's install, start and stop).
//! * [`pool`]: the devices and instances, the work on threads of their own (an app's restart on
//!   its window among them), and the ticker that follows boots, closed windows, ended apps and idle
//!   devices.
//! * [`config`]: the settings, kept in `<app-data>/../app.conf`.
//! * [`extras`]: what a launch does beyond the install and the start (a plugin's cookie, link).
pub mod config;
pub mod crash;
pub mod device;
pub mod extras;
pub mod plan;
pub mod pool;

pub use config::Config;
pub use extras::{Extras, Remake};
pub use plan::DeviceId;
pub use pool::{DeviceInfo, DeviceState, InstanceId, InstanceInfo, InstanceState, Pool, RESTART_DELAY};

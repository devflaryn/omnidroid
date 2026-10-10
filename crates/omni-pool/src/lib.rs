//! **A pool of warm Android devices and the app instances on them** -- what the omnidroid app shows
//! and its MCP server drives.
//!
//! * [`plan`]: where a launch goes and which devices idle out -- pure rules, each one a test.
//! * [`device`]: what is done to one device through its control channel (displays, users, the
//!   app's install, start and stop).
//! * [`pool`]: the devices and instances, the work on threads of their own, and the ticker that
//!   follows boots, closed windows, ended apps and idle devices.
//! * [`config`]: the settings, kept in `<app-data>/../app.conf`.
pub mod config;
pub mod device;
pub mod plan;
pub mod pool;

pub use config::Config;
pub use plan::DeviceId;
pub use pool::{DeviceInfo, DeviceState, InstanceId, InstanceInfo, InstanceState, Pool};

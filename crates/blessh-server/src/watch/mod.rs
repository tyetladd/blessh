//! The adapter watchdog: seeing a Bluetooth controller that stopped
//! answering and rebinding its driver so it answers again.
//!
//! Linux only; the crate root includes the module for Linux builds.

// The policy is the watchdog's decision core. The command that drives it
// is added to this module separately, so only the tests use it for now.
#[allow(dead_code)]
mod policy;

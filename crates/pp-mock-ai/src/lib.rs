//! Only available in development builds: a local Responses service and embedded debug scenarios.

#[cfg(debug_assertions)]
mod mock_ai;

#[cfg(debug_assertions)]
pub use mock_ai::{
    MockEndpoint, MockServer, MockTest, MockTestEvent, MockTestReport, MockTestSnapshot,
    current_endpoint, install_endpoint, run_test,
};

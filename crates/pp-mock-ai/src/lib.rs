//! 仅开发构建提供的本地 Responses 服务与内嵌调试场景。

#[cfg(debug_assertions)]
mod mock_ai;

#[cfg(debug_assertions)]
pub use mock_ai::{
    MockEndpoint, MockServer, MockTest, MockTestEvent, MockTestReport, MockTestSnapshot,
    current_endpoint, install_endpoint, run_test,
};

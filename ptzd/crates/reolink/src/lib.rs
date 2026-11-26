//! Reolink side of ptzd (plan §9 D1, D4, Credentials). The Reolink HTTP
//! API is used for configuration and credential checks only, never to move motors.
#![forbid(unsafe_code)]

pub mod api;
pub mod auth;
pub mod cfg;
pub mod http;
pub mod mock;
pub mod sha256;
pub mod state;

pub use api::{ApiClient, ApiError, ApiSession, LoginOk};
pub use auth::{AuthConfig, ReolinkAuth};
pub use http::{HttpTransport, Transport, TransportError};
pub use state::{RecheckHandle, Reolink, ReolinkConfig};

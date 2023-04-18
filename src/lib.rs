//! ## Freature flags
#![doc = document_features::document_features!()]

mod interface;
/// Describes possible actions to/from `vlink` bridges
pub use interface::Action;

#[cfg(feature = "logic")]
mod bridge;
#[cfg(feature = "logic")]
/// - only available with the feature **logic** *(enabled by default)*
pub use bridge::TcpBridge;

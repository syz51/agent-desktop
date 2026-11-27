#![forbid(unsafe_code)]

pub mod annotate;
pub mod audit;
pub mod backend;
pub mod clock;
pub mod engine;
pub mod frame_ring;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod policy;
pub mod tree;

pub use annotate::draw_annotations_rgba;
pub use audit::AuditLog;
pub use backend::{AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo};
pub use clock::{Clock, SystemClock};
pub use engine::DesktopEngine;
pub use frame_ring::{FrameKey, FrameRing, dhash_distance, dhash_from_pixels, has_visual_change};
pub use policy::PolicyStore;

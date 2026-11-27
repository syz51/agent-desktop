#![forbid(unsafe_code)]

pub mod annotate;
pub mod audit;
pub mod backend;
pub mod clock;
pub mod engine;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod policy;
pub mod tree;

pub use annotate::draw_annotations_rgba;
pub use audit::AuditLog;
pub use backend::{AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo};
pub use clock::{Clock, SystemClock};
pub use engine::DesktopEngine;
pub use policy::PolicyStore;

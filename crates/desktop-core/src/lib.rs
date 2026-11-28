#![forbid(unsafe_code)]

pub mod annotate;
pub mod audit;
pub mod backend;
pub mod clock;
pub mod engine;
pub mod frame_ring;
pub mod kill_switch;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod policy;
pub mod tree;

pub use annotate::{draw_annotations_bgra, draw_annotations_rgba};
pub use audit::AuditLog;
pub use backend::{
    AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo, Timestamp, VisualSample,
};
pub use clock::{Clock, SystemClock};
pub use engine::{DesktopEngine, EXPLICIT_STOP_COOLDOWN_SECS};
pub use frame_ring::{
    FrameKey, FrameRing, has_visual_change, max_visual_delta, visual_signature_from_pixels,
};
pub use kill_switch::{DOUBLE_ESC_WINDOW, EscKillSwitch};
pub use policy::PolicyStore;

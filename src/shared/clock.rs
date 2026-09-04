//! When a frame reached the screen.

/// A `CLOCK_MONOTONIC` instant, in the shape `wp_presentation_feedback` wants.
#[derive(Debug, Clone, Copy)]
pub struct PresentedAt {
    /// Whole seconds part
    pub tv_sec: i64,
    /// Nanosecond part
    pub tv_nsec: i64,
}

impl PresentedAt {
    /// Read the clock now. Called by a backend at the moment it presents, so
    /// the time a client is told is the backend's, not one measured a channel
    /// hop later in the compositor.
    pub fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` only writes through the pointer it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
        Self {
            tv_sec: ts.tv_sec,
            tv_nsec: ts.tv_nsec,
        }
    }
}

//! The headset guard (spec 10.3).
//!
//! The user may be in VR on this PC, so no runtime code may run by accident. Opening a runtime's library
//! already runs its code, so the loader asks this guard first, and refuses unless one of two things is
//! true:
//! - `PE_XR_REAL=1` is set: a deliberate real-runtime run, which needs the coordinator's go each time;
//! - the application's configuration turns VR on. It is off by default.
//!
//! `#[ignore]` on real-runtime tests is a second guard only: `cargo test -- --ignored` runs them anyway.

/// The environment variable that permits a real-runtime run.
pub const REAL_ENV: &str = "PE_XR_REAL";

/// Why the guard let a runtime be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permit {
    /// `PE_XR_REAL=1`.
    RealRunEnv,
    /// The application's configuration turned VR on.
    ConfiguredOn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub reason: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The guard's inputs, read once. Tests build it directly, so they never depend on the process's
/// environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guard {
    /// The value of `PE_XR_REAL`, if set.
    pub real_env: Option<String>,
    /// Whether the application's configuration turns VR on (default off).
    pub configured_on: bool,
}

impl Guard {
    /// A guard that refuses everything: what every test that is not a real-runtime run uses.
    pub const CLOSED: Guard = Guard { real_env: None, configured_on: false };

    /// The guard from this process's environment and the application's VR setting.
    pub fn from_env(configured_on: bool) -> Guard {
        Guard { real_env: std::env::var(REAL_ENV).ok(), configured_on }
    }

    pub fn check(&self) -> Result<Permit, Refused> {
        if self.real_env.as_deref() == Some("1") {
            return Ok(Permit::RealRunEnv);
        }
        if self.configured_on {
            return Ok(Permit::ConfiguredOn);
        }
        Err(Refused {
            reason: format!(
                "VR is off: the configuration does not turn it on and {REAL_ENV} is not 1, so no OpenXR runtime is opened"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_by_default() {
        assert!(Guard::CLOSED.check().is_err());
        assert!(Guard { real_env: Some("0".into()), configured_on: false }.check().is_err());
        assert!(Guard { real_env: Some("yes".into()), configured_on: false }.check().is_err());
    }

    #[test]
    fn opens_only_on_env_one_or_configuration() {
        assert_eq!(Guard { real_env: Some("1".into()), configured_on: false }.check(), Ok(Permit::RealRunEnv));
        assert_eq!(Guard { real_env: None, configured_on: true }.check(), Ok(Permit::ConfiguredOn));
    }
}

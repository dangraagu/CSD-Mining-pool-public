//! Optional NVIDIA GPU telemetry via NVML (feature = "nvml").
//!
//! This is the **secondary** build's GPU telemetry source: when compiled with
//! `--features nvml` AND run on a host whose NVIDIA driver exposes the NVIDIA
//! Management Library, it reads the mining GPU's **core temperature** and
//! **power draw**, feeds them into the `/1/summary` telemetry + the HiveOS
//! `temp[]`, and drives the temperature-limit safety throttle (see
//! [`crate::thermal`]).
//!
//! ## Degrade-to-disabled contract (the important part)
//!
//! Telemetry is *best-effort* and NEVER required to mine:
//!   - Built WITHOUT the `nvml` feature ⇒ [`GpuTelemetry::disabled`] is the only
//!     constructor; every read returns `None`. Zero deps, zero runtime cost.
//!   - Built WITH the feature but NVML can't load (non-NVIDIA host, missing
//!     `nvml.dll` / `libnvidia-ml.so`, OpenCL-on-AMD, the CPU backend, a
//!     permissions issue) ⇒ [`GpuTelemetry::init`] logs a one-line notice and
//!     returns a DISABLED handle; the miner runs exactly as it would without the
//!     feature.
//!   - Built with the feature, NVML loads, device found ⇒ live reads.
//!
//! Because the public surface ([`GpuTelemetry`] + [`TelemetrySample`]) is
//! identical in both builds, the rest of the codebase calls it with no `#[cfg]`
//! sprinkling — the feature gate lives entirely here.
//!
//! ## No new endpoints, no PoW impact
//!
//! NVML is read-only telemetry (plus the optional, operator-gated power-limit
//! SET); it opens NO sockets and touches NO hashing/PoW path. `nvml-wrapper`
//! dlopen's the driver's own library at runtime (via `libloading`) — it links no
//! NVIDIA library at build time, so this module compiles on any host.

/// A point-in-time telemetry sample. Both fields are `Option` because either
/// metric may be unsupported on a given card / driver even when NVML is up, and
/// both are `None` whenever telemetry is disabled. Pure data — always available
/// regardless of the `nvml` feature, so [`crate::stats`] is testable either way.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TelemetrySample {
    /// GPU core temperature in whole degrees Celsius, if readable.
    pub temp_c: Option<f64>,
    /// GPU board power draw in Watts, if readable.
    pub power_w: Option<f64>,
}

impl TelemetrySample {
    /// An empty sample (both metrics unavailable). Same as `default()`, but
    /// named so call sites read clearly.
    pub const fn none() -> Self {
        TelemetrySample {
            temp_c: None,
            power_w: None,
        }
    }
}

// ===========================================================================
//  Feature ON: real NVML-backed implementation.
// ===========================================================================
#[cfg(feature = "nvml")]
mod imp {
    use super::TelemetrySample;
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    use nvml_wrapper::Nvml;

    /// Live NVML telemetry for ONE GPU (the mining device). Owns the NVML handle
    /// for the process lifetime; `device_index` is re-resolved on each read so a
    /// transient device error never permanently wedges telemetry. When NVML could
    /// not initialise this is `None` (disabled) and all reads return empty.
    pub struct GpuTelemetry {
        nvml: Option<Nvml>,
        device_index: u32,
    }

    impl GpuTelemetry {
        /// A disabled handle (all reads return `None`). Used when the operator
        /// hasn't opted into telemetry, or to mirror the no-feature build.
        pub fn disabled() -> Self {
            GpuTelemetry {
                nvml: None,
                device_index: 0,
            }
        }

        /// Try to bring up NVML for `device_index`. On ANY failure this logs a
        /// single concise notice and returns a DISABLED handle — it never errors
        /// out of the caller, so a non-NVIDIA host (or missing NVML) just runs
        /// without telemetry. Optionally enforces a board power limit (Watts) via
        /// NVML when `power_limit_w` is `Some` (best-effort; needs privileges).
        pub fn init(device_index: usize, power_limit_w: Option<f64>) -> Self {
            let nvml = match Nvml::init() {
                Ok(n) => n,
                Err(e) => {
                    tracing::info!(
                        "nvml: telemetry unavailable ({e}); continuing without GPU temp/power (this is harmless on non-NVIDIA hosts or when NVML isn't installed)"
                    );
                    return Self::disabled();
                }
            };
            let idx = device_index as u32;
            // Probe the device once so we fail to "disabled" cleanly if the index
            // is wrong, rather than logging an error on every later read. `mut`
            // because the optional power-limit SET needs `&mut Device`.
            match nvml.device_by_index(idx) {
                Ok(mut dev) => {
                    let name = dev.name().unwrap_or_else(|_| "<unknown>".to_string());
                    tracing::info!("nvml: telemetry enabled for device {idx} ({name})");
                    // Optional power-limit enforcement (best-effort).
                    if let Some(w) = power_limit_w {
                        Self::apply_power_limit(&mut dev, w);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "nvml: device {idx} not found ({e}); telemetry disabled (the miner runs normally)"
                    );
                    return Self::disabled();
                }
            }
            GpuTelemetry {
                nvml: Some(nvml),
                device_index: idx,
            }
        }

        /// Best-effort set of the GPU's enforced power-management limit (Watts).
        /// NVML takes milliwatts and clamps to the card's allowed constraints; we
        /// clamp ourselves first so an out-of-range request becomes the nearest
        /// valid value instead of an error. Never panics; logs the outcome.
        fn apply_power_limit(dev: &mut nvml_wrapper::Device<'_>, watts: f64) {
            let req_mw = (watts * 1000.0).round().max(0.0) as u32;
            let mw = match dev.power_management_limit_constraints() {
                Ok(c) => req_mw.clamp(c.min_limit, c.max_limit),
                Err(_) => req_mw, // no constraints readable — pass the request through
            };
            match dev.set_power_management_limit(mw) {
                Ok(()) => tracing::info!(
                    "nvml: set GPU power limit to {} W ({} mW)",
                    mw / 1000,
                    mw
                ),
                Err(e) => tracing::warn!(
                    "nvml: could not set power limit to {watts} W ({e}); needs elevated privileges — continuing at the card's current limit"
                ),
            }
        }

        /// True iff live NVML telemetry is active (feature on AND init succeeded).
        pub fn is_enabled(&self) -> bool {
            self.nvml.is_some()
        }

        /// Read a fresh [`TelemetrySample`]. Any per-metric failure degrades that
        /// metric to `None`; a disabled handle returns an empty sample. NEVER
        /// panics — every NVML error becomes a `None`.
        pub fn sample(&self) -> TelemetrySample {
            let nvml = match &self.nvml {
                Some(n) => n,
                None => return TelemetrySample::none(),
            };
            let dev = match nvml.device_by_index(self.device_index) {
                Ok(d) => d,
                Err(_) => return TelemetrySample::none(),
            };
            let temp_c = dev
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|c| c as f64);
            // NVML power_usage is milliwatts → Watts.
            let power_w = dev.power_usage().ok().map(|mw| mw as f64 / 1000.0);
            TelemetrySample { temp_c, power_w }
        }
    }
}

// ===========================================================================
//  Feature OFF: zero-dependency stub with the IDENTICAL public surface.
// ===========================================================================
#[cfg(not(feature = "nvml"))]
mod imp {
    use super::TelemetrySample;

    /// Stub telemetry (the `nvml` feature is off): always disabled, every read
    /// returns `None`. Mirrors the real type's API so callers need no `#[cfg]`.
    pub struct GpuTelemetry;

    impl GpuTelemetry {
        /// A disabled handle.
        pub fn disabled() -> Self {
            GpuTelemetry
        }
        /// Without the feature, `init` is exactly `disabled()` (args ignored).
        pub fn init(_device_index: usize, _power_limit_w: Option<f64>) -> Self {
            GpuTelemetry
        }
        /// Never enabled in the stub.
        pub fn is_enabled(&self) -> bool {
            false
        }
        /// Always an empty sample.
        pub fn sample(&self) -> TelemetrySample {
            TelemetrySample::none()
        }
    }
}

pub use imp::GpuTelemetry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_sample_none_is_empty() {
        let s = TelemetrySample::none();
        assert_eq!(s.temp_c, None);
        assert_eq!(s.power_w, None);
        assert_eq!(s, TelemetrySample::default());
    }

    #[test]
    fn disabled_handle_reads_none_in_either_build() {
        // This holds regardless of the `nvml` feature: a disabled handle is inert.
        let t = GpuTelemetry::disabled();
        assert!(!t.is_enabled());
        assert_eq!(t.sample(), TelemetrySample::none());
    }

    #[cfg(not(feature = "nvml"))]
    #[test]
    fn stub_init_is_always_disabled() {
        // Without the feature, even init() yields a disabled, all-None handle.
        let t = GpuTelemetry::init(0, Some(200.0));
        assert!(!t.is_enabled());
    }

    #[cfg(feature = "nvml")]
    #[test]
    fn featured_init_never_panics_and_reads_safely() {
        // With the feature on, init must NOT panic on a host with no NVIDIA GPU /
        // no NVML — it degrades to a disabled handle. (On a real NVIDIA host in
        // CI this may instead be enabled; either way the reads must be safe.)
        let t = GpuTelemetry::init(0, None);
        let _ = t.is_enabled(); // either is acceptable depending on the host
        let s = t.sample(); // must never panic
        // If disabled, the sample is empty; if enabled, the values are plausible.
        if !t.is_enabled() {
            assert_eq!(s, TelemetrySample::none());
        } else {
            if let Some(c) = s.temp_c {
                assert!((0.0..=150.0).contains(&c), "temp {c} out of sane range");
            }
            if let Some(w) = s.power_w {
                assert!((0.0..=2000.0).contains(&w), "power {w} out of sane range");
            }
        }
    }
}

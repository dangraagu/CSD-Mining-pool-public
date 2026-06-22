//! Native Windows Service support (OPTIONAL — secondary build only).
//!
//! This module is split into two layers:
//!
//!   1. **Pure, always-compiled mode parsing** ([`ServiceMode`] +
//!      [`parse_service_mode`]). It maps the three CLI flags
//!      (`--install-service` / `--uninstall-service` / `--run-as-service`) to a
//!      single mode, rejecting conflicting combinations. No Windows API, no I/O —
//!      so it is unit-tested with a fake clock's spirit (just inputs → output),
//!      exactly like `stratum::watchdog`'s pure decision functions.
//!
//!   2. **The Windows SCM glue** (register/unregister/run under the Service
//!      Control Manager with auto-restart + stop/shutdown handling). That half
//!      needs the live SCM and the `windows-service` crate, so it is gated behind
//!      `#[cfg(all(windows, feature = "winsvc"))]`. On any other target — or in
//!      the default (lean fleet) build with the feature off — the entry points
//!      degrade to a clear "not available on this build/platform" error and do
//!      nothing else (a no-op, never a panic).
//!
//! The PoW/hash path is untouched: running as a service simply calls the same
//! mining entry point the foreground CLI does.

use anyhow::{bail, Result};

/// The canonical Windows service name the miner registers itself under. Used for
/// install, uninstall, and the service's own dispatcher entry. Kept short and
/// unambiguous (no spaces) so `sc.exe query csd-pool-miner` works too.
pub const SERVICE_NAME: &str = "csd-pool-miner";

/// Human-readable display name shown in `services.msc`.
pub const SERVICE_DISPLAY_NAME: &str = "CSD Pool Miner";

/// Which Windows-service action the operator requested, if any. Derived purely
/// from the three mutually-exclusive CLI flags by [`parse_service_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMode {
    /// `--install-service`: register this binary with the SCM (auto-start),
    /// passing the current mining flags through to the service's command line.
    Install,
    /// `--uninstall-service`: stop (if running) and delete the SCM registration.
    Uninstall,
    /// `--run-as-service`: hand control to the SCM dispatcher. This is the mode
    /// the SCM itself invokes; an operator does not normally type it.
    Run,
}

/// Map the three service flags to at most one [`ServiceMode`]. Pure: no Windows
/// API, no I/O — the unit-test target.
///
/// - Exactly one flag set ⇒ that mode.
/// - No flag set ⇒ `Ok(None)` (normal foreground mining; the caller proceeds).
/// - More than one set ⇒ an error (the actions are mutually exclusive; silently
///   picking one would be a footgun, e.g. install+uninstall in the same line).
pub fn parse_service_mode(
    install: bool,
    uninstall: bool,
    run: bool,
) -> Result<Option<ServiceMode>> {
    match (install, uninstall, run) {
        (false, false, false) => Ok(None),
        (true, false, false) => Ok(Some(ServiceMode::Install)),
        (false, true, false) => Ok(Some(ServiceMode::Uninstall)),
        (false, false, true) => Ok(Some(ServiceMode::Run)),
        _ => bail!(
            "the --install-service / --uninstall-service / --run-as-service flags are \
             mutually exclusive; pass at most one"
        ),
    }
}

/// The mining body the service runs. It is handed the shared stop flag that the
/// SCM control handler flips on Stop/Shutdown, and should mine until that flag is
/// set, then return. `main.rs` supplies a closure that runs the existing mining
/// path; this module owns only the service lifecycle around it.
pub type RunMiner = Box<dyn FnOnce(std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<()> + Send>;

/// Execute a resolved [`ServiceMode`].
///
/// * [`ServiceMode::Install`] / [`ServiceMode::Uninstall`] talk to the SCM and
///   return (they do not mine).
/// * [`ServiceMode::Run`] hands `run_miner` to the SCM dispatcher and blocks
///   until the service is stopped.
///
/// On a build without the `winsvc` feature, or on a non-Windows target, every
/// mode returns a clear "not available" error and does nothing else — a no-op,
/// never a panic. The caller (`main`) only reaches this when a service flag was
/// passed, so the error is the right outcome there.
#[cfg(all(windows, feature = "winsvc"))]
pub fn run_service_action(mode: ServiceMode, run_miner: RunMiner) -> Result<()> {
    match mode {
        ServiceMode::Install => imp::install(),
        ServiceMode::Uninstall => imp::uninstall(),
        ServiceMode::Run => imp::run(run_miner),
    }
}

/// Stub for builds without Windows-service support: report clearly and do
/// nothing (no SCM, no panic). `run_miner` is dropped unused.
#[cfg(not(all(windows, feature = "winsvc")))]
pub fn run_service_action(mode: ServiceMode, run_miner: RunMiner) -> Result<()> {
    let _ = run_miner; // never invoked on this build
    let what = match mode {
        ServiceMode::Install => "--install-service",
        ServiceMode::Uninstall => "--uninstall-service",
        ServiceMode::Run => "--run-as-service",
    };
    bail!(
        "{what} is unavailable in this build: native Windows Service support requires \
         a Windows target built with the `winsvc` feature \
         (`cargo build --release --features winsvc`)"
    )
}

/// The live Windows SCM implementation. Compiled only when targeting Windows
/// with the `winsvc` feature on, so `windows-service` (a Windows-only crate) is
/// referenced nowhere else and the default/cross builds never see it.
#[cfg(all(windows, feature = "winsvc"))]
mod imp {
    use super::{RunMiner, SERVICE_DISPLAY_NAME, SERVICE_NAME};
    use anyhow::{anyhow, Context, Result};
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    /// This service runs in its own process (one miner = one process), so the
    /// type is OWN_PROCESS throughout (install + status must agree).
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    /// The miner body + the stop flag, stashed here so the `'static` dispatcher
    /// entry generated by `define_windows_service!` can reach them. Set once by
    /// [`run`] just before `service_dispatcher::start` blocks.
    static RUN_CTX: OnceLock<Mutex<Option<RunMiner>>> = OnceLock::new();
    static STOP_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

    windows_service::define_windows_service!(ffi_service_main, service_main);

    /// Forward CLI arguments to embed in the installed service's command line:
    /// everything the operator typed EXCEPT the program name and the
    /// `--install-service` flag itself, with `--run-as-service` appended. So
    /// `csd-pool-miner --address X --backend cuda --install-service` installs a
    /// service that launches `csd-pool-miner --address X --backend cuda
    /// --run-as-service`. Pure (args in → args out) for easy reasoning.
    fn service_launch_args(argv: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
        let mut out: Vec<OsString> = argv
            .into_iter()
            .skip(1) // program name
            .filter(|a| a != "--install-service")
            .collect();
        out.push(OsString::from("--run-as-service"));
        out
    }

    /// `--install-service`: register this exe with the SCM as an auto-start
    /// service that re-launches itself in `--run-as-service` mode, and configure
    /// it to auto-restart on crash.
    pub fn install() -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("opening the Service Control Manager (run as Administrator)")?;

        let exe = std::env::current_exe().context("resolving this executable's path")?;
        let launch_args = service_launch_args(std::env::args_os());

        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe,
            launch_arguments: launch_args,
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };

        let service = manager
            .create_service(
                &info,
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
            )
            .context("creating the service (is it already installed?)")?;

        service
            .set_description(OsString::from(
                "Compute Substrate (CSD) standalone pool miner.",
            ))
            .ok(); // cosmetic; never fail the install over the description

        // Auto-restart on crash: restart after a short backoff, three escalating
        // tries, and reset the failure counter after a day of health. This is the
        // SCM analogue of the systemd unit's `Restart=always` + `RestartSec`.
        let failure_actions = ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(15),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
            ]),
        };
        service
            .update_failure_actions(failure_actions)
            .context("setting auto-restart (failure) actions")?;
        // Also restart on a *clean* non-zero exit (e.g. the GPU-stall watchdog's
        // exit code 17), not only on a crash — mirrors the systemd unit's
        // RestartForceExitStatus so a hung-GPU exit comes back up.
        service
            .set_failure_actions_on_non_crash_failures(true)
            .ok();

        println!(
            "installed Windows service '{SERVICE_NAME}' ({SERVICE_DISPLAY_NAME}); \
             start it with:  sc start {SERVICE_NAME}   (or reboot — it is auto-start)"
        );
        Ok(())
    }

    /// `--uninstall-service`: stop the service if it is running, then delete its
    /// SCM registration. Idempotent-ish: a stop error on an already-stopped
    /// service is ignored; only a delete failure is fatal.
    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("opening the Service Control Manager (run as Administrator)")?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
            )
            .with_context(|| format!("opening service '{SERVICE_NAME}' (is it installed?)"))?;

        // Best-effort stop; ignore the error if it is already stopped.
        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                let _ = service.stop();
            }
        }
        service
            .delete()
            .with_context(|| format!("deleting service '{SERVICE_NAME}'"))?;
        println!("uninstalled Windows service '{SERVICE_NAME}'");
        Ok(())
    }

    /// `--run-as-service`: stash the miner body + a fresh stop flag, then hand
    /// control to the SCM dispatcher (blocks until the service stops). The
    /// dispatcher invokes `ffi_service_main` → [`service_main`] on a background
    /// thread.
    pub fn run(run_miner: RunMiner) -> Result<()> {
        RUN_CTX
            .set(Mutex::new(Some(run_miner)))
            .map_err(|_| anyhow!("run-as-service entered twice"))?;
        STOP_FLAG
            .set(Arc::new(AtomicBool::new(false)))
            .map_err(|_| anyhow!("run-as-service entered twice"))?;
        // Blocks, on this thread, until the service is told to stop. Maps the
        // crate's error into anyhow so `main` can print it.
        windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| anyhow!("service dispatcher failed (are we running under the SCM? \
                use `sc start {SERVICE_NAME}`, not a bare --run-as-service): {e}"))?;
        Ok(())
    }

    /// Higher-level service entry the macro delegates to. Any error here is
    /// logged; we cannot return it to the SCM beyond the status we report.
    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_under_scm() {
            tracing::error!("windows service: {e:#}");
        }
    }

    /// Register the control handler, report Running, drive the miner, and report
    /// Stopped. The control handler flips the shared stop flag on Stop/Shutdown,
    /// so the miner's own cooperative shutdown (the same one Ctrl-C uses) winds
    /// the loop down cleanly.
    fn run_under_scm() -> Result<()> {
        let stop = STOP_FLAG
            .get()
            .cloned()
            .ok_or_else(|| anyhow!("stop flag was not initialised before dispatch"))?;

        let handler_stop = stop.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                // Stop and Shutdown both ask us to wind down cooperatively.
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    handler_stop.store(true, Ordering::SeqCst);
                    ServiceControlHandlerResult::NoError
                }
                // Required no-op.
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("registering the service control handler")?;

        // We accept STOP and SHUTDOWN events while running.
        let running = ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle
            .set_service_status(running)
            .context("reporting Running to the SCM")?;

        // Take the miner body and run it on this (dispatcher) thread until the
        // stop flag is set by the control handler.
        let miner = RUN_CTX
            .get()
            .and_then(|m| m.lock().ok().and_then(|mut g| g.take()))
            .ok_or_else(|| anyhow!("miner body was not initialised before dispatch"))?;
        let result = miner(stop);

        // Report Stopped regardless of how the miner exited; surface a non-zero
        // exit code to the SCM when the miner returned an error so its restart
        // policy can act.
        let exit_code = if result.is_ok() {
            ServiceExitCode::Win32(0)
        } else {
            // ERROR_PROCESS_ABORTED-ish generic failure; the exact value only
            // needs to be non-zero for the SCM to treat it as a failure.
            ServiceExitCode::ServiceSpecific(1)
        };
        let stopped = ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle
            .set_service_status(stopped)
            .context("reporting Stopped to the SCM")?;

        result
    }

    #[cfg(test)]
    mod imp_tests {
        use super::*;

        #[test]
        fn launch_args_drop_progname_and_install_flag_and_add_run() {
            let argv = [
                OsString::from("csd-pool-miner.exe"),
                OsString::from("--address"),
                OsString::from("deadbeef"),
                OsString::from("--backend"),
                OsString::from("cuda"),
                OsString::from("--install-service"),
            ];
            let got = service_launch_args(argv);
            let want: Vec<OsString> = ["--address", "deadbeef", "--backend", "cuda", "--run-as-service"]
                .iter()
                .map(OsString::from)
                .collect();
            assert_eq!(got, want);
        }

        #[test]
        fn launch_args_always_end_with_run_as_service() {
            let argv = [OsString::from("csd-pool-miner.exe")];
            let got = service_launch_args(argv);
            assert_eq!(got, vec![OsString::from("--run-as-service")]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flags_is_none() {
        // Nothing requested ⇒ normal foreground mining (caller proceeds).
        assert_eq!(parse_service_mode(false, false, false).unwrap(), None);
    }

    #[test]
    fn each_single_flag_maps_to_its_mode() {
        assert_eq!(
            parse_service_mode(true, false, false).unwrap(),
            Some(ServiceMode::Install)
        );
        assert_eq!(
            parse_service_mode(false, true, false).unwrap(),
            Some(ServiceMode::Uninstall)
        );
        assert_eq!(
            parse_service_mode(false, false, true).unwrap(),
            Some(ServiceMode::Run)
        );
    }

    #[test]
    fn conflicting_flags_are_rejected() {
        // Every multi-flag combination is an error — the actions are mutually
        // exclusive and silently picking one would be a footgun.
        assert!(parse_service_mode(true, true, false).is_err());
        assert!(parse_service_mode(true, false, true).is_err());
        assert!(parse_service_mode(false, true, true).is_err());
        assert!(parse_service_mode(true, true, true).is_err());
    }
}

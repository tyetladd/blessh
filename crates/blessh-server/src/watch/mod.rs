//! The adapter watchdog: seeing a Bluetooth controller that stopped
//! answering and rebinding its driver so it answers again.
//!
//! Linux only; the crate root includes the module for Linux builds.

use std::{
    collections::HashSet,
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use blessh_ble::hci::{self, State};
use blessh_core::logging;
use clap::Parser;
use log::{error, info, warn};

mod policy;
mod reset;

use policy::{Action, Limits, Policy, Probe};

/// What the loop asks of an adapter: whether its controller answers, and
/// how to reset it.
trait Controller {
    /// The adapter's name, for the log.
    fn name(&self) -> &str;

    /// Asks the controller whether it is alive.
    fn probe(&mut self) -> Probe;

    /// Resets the controller: `Ok(Some(description))` after a reset,
    /// `Ok(None)` when there is nothing to reset.
    fn reset(&mut self) -> Result<Option<String>, String>;
}

/// The watchdog's options.
#[derive(Parser, Debug)]
#[command(
    version,
    about = "Probes the Bluetooth controller and resets it when it stops answering"
)]
struct Args {
    /// The adapter to watch, for example hci0 (default: the first one).
    #[arg(long)]
    adapter: Option<String>,
    /// Seconds between probes.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Seconds to wait for a probe's answer.
    #[arg(long, default_value_t = 2)]
    timeout: u64,
    /// The sysfs root (a test hook).
    #[arg(long, default_value = "/sys", hide = true)]
    sysfs: PathBuf,
    /// A shell command that resets the controller instead of rebinding its
    /// driver (a test hook).
    #[arg(long, hide = true)]
    reset_command: Option<String>,
}

/// The real controller: HCI probes and a sysfs rebind, or the reset command.
struct Real {
    name: String,
    dev: u16,
    timeout: Duration,
    sysfs: PathBuf,
    reset_command: Option<String>,
    /// Errors already logged, so a probe that keeps failing logs once per
    /// kind of error.
    logged: HashSet<io::ErrorKind>,
}

impl Real {
    /// Logs a probe error unless one of its kind was logged before.
    fn log_probe_error(&mut self, e: &io::Error) {
        if self.logged.insert(e.kind()) {
            warn!("{}: cannot probe the controller: {e}", self.name);
        }
    }

    /// Waits for the kernel to report the adapter up again after a reset.
    fn wait_up(&self) {
        for _ in 0..60 {
            if matches!(hci::state(self.dev), Ok(State::Up)) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

impl Controller for Real {
    fn name(&self) -> &str {
        &self.name
    }

    fn probe(&mut self) -> Probe {
        match hci::state(self.dev) {
            Ok(State::Up) => {}
            Ok(State::Down) => return Probe::Down,
            Ok(State::Absent) => return Probe::Absent,
            Err(e) => {
                self.log_probe_error(&e);
                return Probe::Down;
            }
        }
        match hci::read_version(self.dev, self.timeout) {
            Ok(true) => Probe::Answered,
            Ok(false) => Probe::NoAnswer,
            Err(e) => {
                self.log_probe_error(&e);
                Probe::Down
            }
        }
    }

    fn reset(&mut self) -> Result<Option<String>, String> {
        let description = match &self.reset_command {
            Some(command) => {
                let status = Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .status()
                    .map_err(|e| format!("cannot run {command:?}: {e}"))?;
                if !status.success() {
                    return Err(format!("{command:?} failed: {status}"));
                }
                command.clone()
            }
            None => match reset::rebind(&self.sysfs, &self.name, Duration::from_secs(2)) {
                Ok(Some(rebind)) => format!("rebind {} ({})", rebind.driver, rebind.device),
                Ok(None) => return Ok(None),
                Err(e) => return Err(format!("cannot rebind the driver: {e}")),
            },
        };
        // The driver brings the adapter back some time after the bind, and
        // the cooldown starts only once it is back.
        self.wait_up();
        Ok(Some(description))
    }
}

/// `blessh server adapter-watch`: runs until SIGTERM or Ctrl-C.
pub fn main(argv: Vec<String>) -> i32 {
    logging::init("info");
    let args = Args::parse_from(argv);
    let adapter = args.adapter.unwrap_or_else(|| default_adapter(&args.sysfs));
    let Some(dev) = hci::dev_id(&adapter) else {
        error!("adapter name {adapter:?} is not of the form hciN");
        return 2;
    };
    let ctrl = Real {
        name: adapter,
        dev,
        timeout: Duration::from_secs(args.timeout),
        sysfs: args.sysfs,
        reset_command: args.reset_command,
        logged: HashSet::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(serve(ctrl, Duration::from_secs(args.interval))) {
        Ok(()) => 0,
        Err(e) => {
            error!("{e}");
            1
        }
    };
    // A probe in flight must not hold the exit up.
    rt.shutdown_background();
    code
}

/// Runs the loop until SIGTERM or Ctrl-C.
async fn serve(ctrl: Real, interval: Duration) -> Result<(), String> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("cannot listen for SIGTERM: {e}"))?;
    let stop = async move {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    run(ctrl, interval, Policy::new(Limits::default()), stop).await;
    Ok(())
}

/// The adapter to watch when `--adapter` is not given: the lowest-numbered
/// `hciN` under `<sysfs>/class/bluetooth`, or `hci0` when there is none.
fn default_adapter(sysfs: &Path) -> String {
    let mut lowest: Option<(u16, String)> = None;
    if let Ok(entries) = fs::read_dir(sysfs.join("class/bluetooth")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(index) = hci::dev_id(&name) else {
                continue;
            };
            let replace = match &lowest {
                Some((best, _)) => index < *best,
                None => true,
            };
            if replace {
                lowest = Some((index, name));
            }
        }
    }
    match lowest {
        Some((_, name)) => name,
        None => "hci0".into(),
    }
}

/// The `last-reset` file's line: an RFC 3339 UTC time, then what the
/// watchdog did.
fn last_reset_line(secs: libc::time_t, description: &str) -> String {
    // SAFETY: `gmtime_r` gets a valid time and writes to `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::gmtime_r(&secs, &mut tm) };
    let (year, month, day, hour, minute, second) = if ok.is_null() {
        (1970, 1, 1, 0, 0, 0)
    } else {
        (
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
        )
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z {description}\n")
}

/// Writes the time and outcome of a reset attempt for `blessh server
/// status`; failing to write is only logged.
fn write_last_reset(directory: &Path, description: &str) {
    let mut now: libc::time_t = 0;
    // SAFETY: `time` writes to a valid `time_t`.
    unsafe { libc::time(&mut now) };
    let path = directory.join("last-reset");
    if let Err(e) = fs::write(&path, last_reset_line(now, description)) {
        warn!("cannot write {}: {e}", path.display());
    }
}

/// The watchdog loop: probe every `interval`, feed the results to the
/// policy, and act on its decision.
///
/// The controller moves to the blocking pool for each probe and reset, so a
/// slow probe never delays `stop`; on stop the loop returns without waiting
/// for the blocking task.
async fn run<C: Controller + Send + 'static>(
    ctrl: C,
    interval: Duration,
    mut policy: Policy,
    stop: impl Future<Output = ()>,
) {
    let name = ctrl.name().to_owned();
    info!("watching {name}: probing every {}s", interval.as_secs());
    // `blessh server status` reads the last reset from here; outside
    // systemd's RuntimeDirectory there is nowhere to write it.
    let last_reset_dir = std::env::var_os("RUNTIME_DIRECTORY")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let mut ctrl = ctrl;
    tokio::pin!(stop);
    loop {
        // The policy takes std Instants; under a test's paused clock tokio's
        // Instant carries the virtual time, so convert at this boundary.
        let now = tokio::time::Instant::now().into();
        if policy.may_probe(now) {
            // The policy zeroes the miss count when it returns `Reset`, so
            // the count for the log is the previous misses plus this probe.
            let misses = policy.misses() + 1;
            let probe_task = tokio::task::spawn_blocking(move || {
                let mut ctrl = ctrl;
                let probe = ctrl.probe();
                (ctrl, probe)
            });
            let (c, probe) = tokio::select! {
                done = probe_task => done.expect("the probe task panicked"),
                _ = &mut stop => return,
            };
            ctrl = c;
            match policy.on_probe(now, probe) {
                Action::None => {}
                Action::Reset => {
                    let reset_task = tokio::task::spawn_blocking(move || {
                        let mut ctrl = ctrl;
                        let reset = ctrl.reset();
                        (ctrl, reset)
                    });
                    let (c, reset) = tokio::select! {
                        done = reset_task => done.expect("the reset task panicked"),
                        _ = &mut stop => return,
                    };
                    ctrl = c;
                    let now = tokio::time::Instant::now().into();
                    let outcome = match reset {
                        Ok(Some(description)) => {
                            warn!("{name} did not answer {misses} probes; resetting ({description}). If this repeats, check the power supply and the kernel log.");
                            description
                        }
                        Ok(None) => {
                            warn!("{name} did not answer {misses} probes and cannot be reset (no driver to rebind)");
                            "not resettable (no driver to rebind)".into()
                        }
                        Err(e) => {
                            error!("{name}: cannot reset the controller: {e}");
                            format!("reset failed: {e}")
                        }
                    };
                    if let Some(directory) = &last_reset_dir {
                        write_last_reset(directory, &outcome);
                    }
                    // A failed or impossible reset spends the cooldown and
                    // counts towards the limit too, so a controller that
                    // cannot be reset is not retried in a loop.
                    policy.reset_done(now);
                }
                Action::GaveUp => {
                    let limits = policy.limits();
                    error!(
                        "{name} was reset {} times in {} minutes and still does not answer; not resetting it again",
                        limits.max_resets(),
                        limits.window().as_secs() / 60
                    );
                }
                Action::Recovered => info!("{name} answers again"),
            }
        }
        tokio::select! {
            _ = &mut stop => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::time::Instant;

    /// What a `Fake` did, kept after the fake moves into `run`.
    #[derive(Default)]
    struct Record {
        probes: Vec<Instant>,
        resets: usize,
    }

    /// Answers probes from a script, then answers every probe, and records
    /// the virtual time of each probe and the resets.
    struct Fake {
        script: VecDeque<Probe>,
        fail_reset: bool,
        record: Arc<Mutex<Record>>,
    }

    impl Fake {
        fn new(script: impl IntoIterator<Item = Probe>) -> (Self, Arc<Mutex<Record>>) {
            Self::with_reset(script, false)
        }

        /// A fake whose resets always fail.
        fn failing(script: impl IntoIterator<Item = Probe>) -> (Self, Arc<Mutex<Record>>) {
            Self::with_reset(script, true)
        }

        fn with_reset(
            script: impl IntoIterator<Item = Probe>,
            fail_reset: bool,
        ) -> (Self, Arc<Mutex<Record>>) {
            let record = Arc::new(Mutex::new(Record::default()));
            (
                Self {
                    script: script.into_iter().collect(),
                    fail_reset,
                    record: Arc::clone(&record),
                },
                record,
            )
        }
    }

    impl Controller for Fake {
        fn name(&self) -> &str {
            "hci0"
        }

        fn probe(&mut self) -> Probe {
            let probe = self.script.pop_front().unwrap_or(Probe::Answered);
            self.record.lock().unwrap().probes.push(Instant::now());
            probe
        }

        fn reset(&mut self) -> Result<Option<String>, String> {
            self.record.lock().unwrap().resets += 1;
            if self.fail_reset {
                Err("the driver did not bind".into())
            } else {
                Ok(Some("rebind hci_uart_bcm (serial0-0)".into()))
            }
        }
    }

    #[test]
    fn last_reset_record() {
        assert_eq!(
            last_reset_line(0, "rebind hci_uart_bcm (serial0-0)"),
            "1970-01-01T00:00:00Z rebind hci_uart_bcm (serial0-0)\n"
        );
        assert_eq!(
            last_reset_line(1_700_000_000, "x"),
            "2023-11-14T22:13:20Z x\n"
        );
    }

    /// The gaps between consecutive probes, in seconds.
    fn gaps(probes: &[Instant]) -> Vec<u64> {
        probes
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs())
            .collect()
    }

    // The stop durations are off the 5 s probe grid so the stop sleep and a
    // probe sleep never come due at the same instant, when `select!` would
    // pick between them at random.

    #[tokio::test(start_paused = true)]
    async fn resets_after_two_misses() {
        let (fake, record) = Fake::new([Probe::Answered, Probe::NoAnswer, Probe::NoAnswer]);
        run(
            fake,
            Duration::from_secs(5),
            Policy::new(Limits::default()),
            tokio::time::sleep(Duration::from_secs(17)),
        )
        .await;
        let record = record.lock().unwrap();
        assert_eq!(record.resets, 1);
        assert_eq!(record.probes.len(), 3);
        assert_eq!(gaps(&record.probes), vec![5, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn absent_adapter_skips_probe() {
        let (fake, record) = Fake::new([Probe::Absent; 10]);
        run(
            fake,
            Duration::from_secs(5),
            Policy::new(Limits::default()),
            tokio::time::sleep(Duration::from_secs(47)),
        )
        .await;
        let record = record.lock().unwrap();
        assert_eq!(record.resets, 0);
        assert_eq!(record.probes.len(), 10);
        assert_eq!(gaps(&record.probes), vec![5; 9]);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_out_the_cooldown() {
        let (fake, record) = Fake::new([Probe::NoAnswer, Probe::NoAnswer, Probe::Answered]);
        run(
            fake,
            Duration::from_secs(5),
            Policy::new(Limits::default()),
            tokio::time::sleep(Duration::from_secs(38)),
        )
        .await;
        let record = record.lock().unwrap();
        assert_eq!(record.resets, 1);
        assert_eq!(record.probes.len(), 3);
        // The first two probes are one interval apart; the third waits out
        // the 30 s cooldown after the reset.
        assert_eq!(gaps(&record.probes), vec![5, 30]);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_resets_count_toward_the_limit() {
        let (fake, record) = Fake::failing([Probe::NoAnswer; 16]);
        run(
            fake,
            Duration::from_secs(5),
            Policy::new(Limits::default()),
            tokio::time::sleep(Duration::from_secs(117)),
        )
        .await;
        let record = record.lock().unwrap();
        // Three failed attempts, each followed by the 30 s cooldown; the
        // fourth pair of misses gives up instead of resetting again.
        assert_eq!(record.resets, 3);
        assert_eq!(record.probes.len(), 9);
        assert_eq!(gaps(&record.probes), vec![5, 30, 5, 30, 5, 30, 5, 5]);
    }
}

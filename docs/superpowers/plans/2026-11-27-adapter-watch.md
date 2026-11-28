# Adapter Watchdog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A root watchdog that notices a Bluetooth controller that stopped answering and rebinds its driver, and a server daemon that keeps its resumable sessions while the adapter goes away and comes back.

**Architecture:** `blessh server adapter-watch` (new, Linux, root unit) probes the controller with HCI Read Local Version on a raw HCI socket; a pure policy decides when to reset; the reset rebinds the device's driver through sysfs. `blessh server run` splits `serve()` into a process part (sessions, registry, signals) and an adapter part (GATT, advertisement, L2CAP listeners) that is rebuilt when the adapter returns.

**Tech Stack:** Rust (tokio current-thread, bluer 0.17, libc), systemd units, POSIX sh tests (`tests/e2e-vhci.sh` with `btvirt`).

**Spec:** `docs/superpowers/specs/2026-11-27-adapter-watch-design.md`

## Global Constraints

- Linux only; everything new is `#[cfg(target_os = "linux")]`. macOS builds must stay green.
- The daemon's unit (`blessh-server.service`) keeps its hardening unchanged; only the new unit holds privilege, and only `CAP_NET_RAW`.
- Probe: HCI Read Local Version Information, OGF 0x04 OCF 0x0001 (opcode 0x1001), every 5 s, 2 s timeout.
- Dead = two failed probes in a row. No probing while the adapter is down (`HCI_UP` flag clear) or absent.
- After a reset: wait for the adapter to return, then 30 s before probing again. At most 3 resets in 10 minutes; then log an error once and only probe.
- Turn off with `BLESSH_ADAPTER_WATCH=no` in `/etc/default/blessh` (`ExecCondition=`), or `systemctl mask blessh-adapter-watch`.
- Command name: `blessh server adapter-watch` (plan's call: the spec wrote `blessh adapter-watch`; it belongs with `server session`, the other helper).
- Repo rules (`docs/AGENTS.md`): English; commit messages carry no tool or agent attribution; no host names or addresses in code, docs or commits. Before each commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`; tasks touching the daemon also `cargo build && tests/e2e-loopback.sh` and `tests/e2e-vhci.sh` on Linux.
- `protocol.md` does not change.

## Review Focus

1. **The GATT application vanishes before `AdapterRemoved` arrives.** BlueZ drops registrations when the adapter goes, so `rx_control`/`tx_control` may end first; today that returns `Err("GATT application was unregistered")` and the daemon exits. Expected: treated as removal when the adapter is gone. Pinned by Task 5's e2e case (killing `btvirt` takes exactly this path; the case fails if the daemon pid changes).
2. **SIGTERM while waiting for the adapter.** With the adapter gone, the daemon sits in `wait_adapter`; `systemctl stop` must still end it within the usual time and detached sessions must get their shutdown. Pinned in Task 5 (e2e step: stop during the gap).
3. **A probe during heavy traffic answers late but answers.** A slow Command Complete within 2 s is a pass; one miss alone never resets. Pinned in Task 2 (`one_miss_does_not_reset`, `answer_clears_miss`).
4. **The driver link disappears on unbind.** `device/driver` is gone after `unbind`, so `bind` must use the driver directory resolved before unbinding. Pinned in Task 3 (`rebind_uses_driver_resolved_before_unbind`).
5. **Adapter index changes after rebind** (hci0 comes back as hci1 when another adapter took the index). Expected: the watchdog follows `--adapter` by name and reports "absent" rather than resetting a different controller; the daemon with no `--adapter` picks the default again. Pinned in Task 2 (`absent_is_not_dead`) and Task 4 (`absent_adapter_skips_probe`).

---

### Task 1: HCI probe primitive

**Files:**
- Create: `crates/blessh-ble/src/hci.rs` (Linux only; add `#[cfg(target_os = "linux")] pub mod hci;` to `crates/blessh-ble/src/lib.rs`)
- Modify: `crates/blessh-ble/Cargo.toml` (add `libc = { workspace = true }` if absent)

**Interfaces:**
- Produces:
  - `pub fn dev_id(name: &str) -> Option<u16>` — `"hci0"` → `Some(0)`, anything else → `None`.
  - `pub enum State { Up, Down, Absent }` and `pub fn state(dev: u16) -> io::Result<State>` — `HCIGETDEVINFO` ioctl on an unbound `BTPROTO_HCI` socket; `ENODEV` → `Absent`; flag bit 0 (`HCI_UP`) decides Up/Down. Needs no privilege.
  - `pub fn read_version(dev: u16, timeout: Duration) -> io::Result<bool>` — raw socket bound to `dev` on `HCI_CHANNEL_RAW`, `HCI_FILTER` passing only event packets 0x0e (Command Complete); writes `READ_VERSION_CMD`; `poll` until `timeout`; `Ok(true)` when a Command Complete for opcode 0x1001 arrives, `Ok(false)` on timeout. Blocking; callers use `spawn_blocking`. Needs `CAP_NET_RAW`.
  - `pub const READ_VERSION_CMD: [u8; 4] = [0x01, 0x01, 0x10, 0x00];`
  - `pub fn is_version_complete(packet: &[u8]) -> bool`

- [ ] **Step 1: Write the failing tests** (in `hci.rs`, `#[cfg(test)] mod tests`)

```rust
#[test]
fn dev_id_parses_hci_names() {
    assert_eq!(dev_id("hci0"), Some(0));
    assert_eq!(dev_id("hci12"), Some(12));
    assert_eq!(dev_id("hci"), None);
    assert_eq!(dev_id("eth0"), None);
}

#[test]
fn version_complete_matches_only_its_opcode() {
    // type 0x04 event, 0x0e Command Complete, plen, ncmd, opcode 0x1001 LE, status 0
    assert!(is_version_complete(&[0x04, 0x0e, 0x0c, 0x01, 0x01, 0x10, 0x00, 8, 0x12, 4, 8, 0x0f, 0, 9, 0x22]));
    assert!(!is_version_complete(&[0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00])); // Reset's opcode
    assert!(!is_version_complete(&[0x04, 0x0f, 0x04, 0x00, 0x01, 0x01, 0x10]));   // Command Status
    assert!(!is_version_complete(&[0x04, 0x0e]));                                 // short
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p blessh-ble hci::` (on Linux)
Expected: FAIL to compile, `dev_id` / `is_version_complete` not found.

- [ ] **Step 3: Implement the module**

Constants from `<bluetooth/hci.h>`: `BTPROTO_HCI = 1`, `SOL_HCI = 0`, `HCI_FILTER = 2`, `HCI_CHANNEL_RAW = 0`, `HCIGETDEVINFO = 0x800448d3` (`_IOR('H', 211, int)`), `HCI_EVENT_PKT = 4`. `sockaddr_hci { hci_family: u16, hci_dev: u16, hci_channel: u16 }`. `hci_filter { type_mask: u32, event_mask: [u32; 2], opcode: u16 }` with `type_mask = 1 << HCI_EVENT_PKT`, `event_mask[0] = 1 << 0x0e`. `hci_dev_info` layout: `dev_id: u16, name: [u8; 8], bdaddr: [u8; 6], flags: u32, type_: u8, features: [u8; 8], pkt_type: u32, link_policy: u32, link_mode: u32, acl_mtu: u16, acl_pkts: u16, sco_mtu: u16, sco_pkts: u16, stat: [u32; 10]` (`#[repr(C)]`). Close fds with `OwnedFd`. Ignore events that are not ours and keep polling until the deadline.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p blessh-ble hci::`
Expected: PASS. Then on a Linux machine with an adapter, as root, a throwaway check: `read_version(0, 2s)` returns `Ok(true)`; as a normal user it returns `Err` with `EPERM` on write.

- [ ] **Step 5: Commit**

```bash
git add crates/blessh-ble/src/hci.rs crates/blessh-ble/src/lib.rs crates/blessh-ble/Cargo.toml
git commit -m "BLE: HCI read-version probe and adapter state for Linux"
```

---

### Task 2: Watchdog policy

**Files:**
- Create: `crates/blessh-server/src/watch/policy.rs`, `crates/blessh-server/src/watch/mod.rs` (module root; declare `mod watch;` under `#[cfg(target_os = "linux")]` in `lib.rs`)

**Interfaces:**
- Produces:
  - `pub enum Probe { Answered, NoAnswer, Down, Absent }`
  - `pub enum Action { None, Reset, GaveUp, Recovered }` — `GaveUp` returned once when the limit is hit; `Recovered` once when a probe answers after `GaveUp` or after a reset.
  - `pub struct Limits { misses: u32, cooldown: Duration, max_resets: usize, window: Duration }` with `impl Default` = `2, 30 s, 3, 600 s`.
  - `pub struct Policy` with `pub fn new(limits: Limits) -> Self`, `pub fn on_probe(&mut self, now: Instant, probe: Probe) -> Action`, `pub fn may_probe(&self, now: Instant) -> bool` (false during cooldown), `pub fn reset_done(&mut self, now: Instant)` (starts the cooldown; records the reset time).
  - `Instant` is `std::time::Instant` passed in, so tests drive time.

- [ ] **Step 1: Write the failing tests** (`policy.rs`, `mod tests`, with `let t0 = Instant::now();` and offsets)

```rust
#[test] fn one_miss_does_not_reset()      // NoAnswer once → Action::None
#[test] fn two_misses_reset()             // NoAnswer, NoAnswer → Action::Reset
#[test] fn answer_clears_miss()           // NoAnswer, Answered, NoAnswer → None, None, None
#[test] fn down_is_not_dead()             // NoAnswer, Down, NoAnswer → no Reset (Down clears the count)
#[test] fn absent_is_not_dead()           // Absent ×5 → never Reset
#[test] fn cooldown_after_reset()         // reset_done(t0); may_probe(t0+29s)==false; may_probe(t0+30s)==true
#[test] fn fourth_reset_in_window_gives_up() // 3 resets at t0, t0+60s, t0+120s (each two misses, reset_done, cooldown); next two misses at t0+200s → GaveUp; two more misses → None (not GaveUp again)
#[test] fn window_slides()                // resets at t0, t0+60s, t0+120s; two misses at t0+601s → Reset
#[test] fn answer_after_giving_up_recovers() // after GaveUp, Answered → Recovered, then misses count again
```

Each test asserts the exact `Action` sequence named in its comment.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p blessh-server watch::policy`
Expected: FAIL to compile (`Policy` not defined).

- [ ] **Step 3: Implement `Policy`**

Fields: consecutive miss count, `VecDeque<Instant>` of reset times (drop those older than `window`), `cooldown_until: Option<Instant>`, `gave_up: bool`, `after_reset: bool`. `Answered` returns `Recovered` when `gave_up || after_reset`, then clears both.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p blessh-server watch::policy`
Expected: PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/blessh-server/src/watch/ crates/blessh-server/src/lib.rs
git commit -m "Server: adapter watchdog policy (two misses, cooldown, reset limit)"
```

---

### Task 3: Driver rebind through sysfs

**Files:**
- Create: `crates/blessh-server/src/watch/reset.rs`

**Interfaces:**
- Produces:
  - `pub struct Rebind { pub driver: String, pub device: String }` (names for the log, e.g. `hci_uart_bcm`, `serial0-0`)
  - `pub fn find(sysfs: &Path, hci: &str) -> io::Result<Option<(PathBuf, String)>>` — resolves `<sysfs>/class/bluetooth/<hci>/device` (canonicalized) and its `driver` link; returns the driver directory and the device's file name; `Ok(None)` when there is no `driver` link (vhci).
  - `pub fn rebind(sysfs: &Path, hci: &str, pause: Duration) -> io::Result<Option<Rebind>>` — `find`, write the device name to `<driver>/unbind`, sleep `pause`, write it to `<driver>/bind`. `Ok(None)` = not resettable.
  - Production `pause`: 2 s.

- [ ] **Step 1: Write the failing tests**

Build a fake tree under a fresh directory in `std::env::temp_dir()` (name with the process id and a counter):
`devices/platform/serial0/serial0-0/` (a directory), `bus/serial/drivers/hci_uart_bcm/{bind,unbind}` (empty files), symlinks `class/bluetooth/hci0/device -> ../../../devices/platform/serial0/serial0-0` and `devices/platform/serial0/serial0-0/driver -> ../../../../bus/serial/drivers/hci_uart_bcm`.

```rust
#[test] fn finds_driver_and_device()            // find → (…/hci_uart_bcm, "serial0-0")
#[test] fn rebind_writes_unbind_then_bind()     // rebind(…, 0s) → Some(Rebind{driver:"hci_uart_bcm", device:"serial0-0"}); both files contain "serial0-0"
#[test] fn rebind_uses_driver_resolved_before_unbind() // call `find`, delete the `driver` symlink (as unbind does), then `rebind_at(driver_dir, "serial0-0", 0s)`; `bind` still receives "serial0-0"
#[test] fn no_driver_is_not_resettable()        // tree without the `driver` link → Ok(None)
#[test] fn missing_adapter_is_an_error()        // hci7 not in the tree → Err(NotFound)
```

(`rebind` is `find` + `rebind_at(driver_dir: &Path, device: &str, pause: Duration) -> io::Result<()>`; the third test calls `rebind_at` directly.)

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p blessh-server watch::reset`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `find`, `rebind_at`, `rebind`**

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p blessh-server watch::reset`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/blessh-server/src/watch/reset.rs crates/blessh-server/src/watch/mod.rs
git commit -m "Server: rebind a Bluetooth adapter's driver through sysfs"
```

---

### Task 4: `blessh server adapter-watch`

**Files:**
- Modify: `crates/blessh-server/src/watch/mod.rs` (command, loop)
- Modify: `crates/blessh-server/src/lib.rs` (`USAGE` gets no line — it is a service helper like `session`; `is_command` gains `"adapter-watch"`; `main` dispatches `"adapter-watch" => watch::main(argv())` on Linux)
- Modify: `crates/blessh-client/src/main.rs:78` (add `"adapter-watch"` next to `"run" | "session"`)

**Interfaces:**
- Consumes: Task 1 `hci::{dev_id, state, read_version, State}`; Task 2 `Policy`, `Probe`, `Action`, `Limits`; Task 3 `reset::rebind`.
- Produces:
  - CLI (`clap`): `--adapter <hci>` (default: BlueZ's default adapter, resolved as the lowest index in `/sys/class/bluetooth` named `hciN`), `--interval <secs>` (5), `--timeout <secs>` (2), hidden `--sysfs <path>` (`/sys`), hidden `--reset-command <sh>` (test hook: run with `sh -c` instead of the sysfs rebind).
  - `pub fn main(argv: Vec<String>) -> i32`
  - `trait Controller { fn probe(&mut self) -> Probe; fn reset(&mut self) -> Result<Option<String>, String>; }` — `Ok(Some(what))` reset with a description, `Ok(None)` not resettable; the real one wraps Tasks 1 and 3 (or the reset command). The real `probe` maps an `Err` from `read_version` (no privilege, socket error) to `Probe::Down` and logs it once per kind of error: an error is never a miss.
  - `async fn run(ctrl: impl Controller, interval: Duration, policy: Policy, stop: impl Future<Output = ()>)` — the loop, generic for tests.

Log lines (exact text; `{a}` adapter, `{d}` description):
- start: `watching {a}: probing every {interval}s`
- reset: `{a} did not answer {misses} probes; resetting ({d}). If this repeats, check the power supply and the kernel log.`
- not resettable: `{a} did not answer {misses} probes and cannot be reset (no driver to rebind)`
- gave up: `{a} was reset {max} times in {window} minutes and still does not answer; not resetting it again`
- recovered: `{a} answers again`

- [ ] **Step 1: Write the failing tests** (tokio `#[tokio::test(start_paused = true)]`, fake `Controller` replaying a script of `Probe`s and counting resets)

```rust
#[tokio::test(start_paused = true)] async fn resets_after_two_misses()     // script [Answered, NoAnswer, NoAnswer, Answered…] → 1 reset
#[tokio::test(start_paused = true)] async fn absent_adapter_skips_probe()  // script [Absent; 10] → 0 resets
#[tokio::test(start_paused = true)] async fn waits_out_the_cooldown()      // after a reset, the next probe happens ≥ 30 s later (record virtual time of each probe)
```

`stop` is a `tokio::time::sleep` for the scripted duration.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p blessh-server watch::tests`
Expected: FAIL to compile (`run` not defined).

- [ ] **Step 3: Implement the loop and `main`**

`probe` runs `read_version` in `spawn_blocking`; it first calls `state()` and returns `Down`/`Absent` without sending. `main` uses a current-thread runtime and stops on SIGTERM / Ctrl-C, like `linux::main`. After a reset, wait (polling `state()` every second, up to 60 s) for the adapter to be `Up` before `reset_done(now)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p blessh-server watch`
Expected: PASS (all watch tests).

- [ ] **Step 5: Check on Linux by hand**

Run as root: `target/debug/blessh server adapter-watch --interval 1`
Expected: `watching hci0: probing every 1s`, then silence. As a normal user: one probe error logged, no reset.

- [ ] **Step 6: Commit**

```bash
git add crates/blessh-server/src/watch/mod.rs crates/blessh-server/src/lib.rs crates/blessh-client/src/main.rs
git commit -m "Server: blessh server adapter-watch probes the controller and resets it"
```

---

### Task 5: The daemon outlives its adapter

**Files:**
- Modify: `crates/blessh-server/src/linux.rs` — `serve()` (`:476-1000` today) is split; `wait_adapter` (`:368`) gains a stop future.
- Modify: `tests/e2e-vhci.sh` (new case at the end)

**Interfaces:**
- Produces (all private to `linux.rs`):
  - `enum AdapterEnd { Removed, Stopped }`
  - `struct Process { session: bluer::Session, sessions: Shared, registry: Registry, shutdown_tx: watch::Sender<bool>, shutdown_rx: watch::Receiver<bool>, sigterm: tokio::signal::unix::Signal }`
  - `async fn serve(args: Args, name: String) -> Result<(), Box<dyn Error>>` — creates `Process`; loops: `wait_adapter` (or stop) → `wait_powered` → `serve_adapter`; on `Removed` logs and loops, on `Stopped` breaks; then sends shutdown and gives detached sessions `SHUTDOWN_GRACE`.
  - `async fn serve_adapter(p: &mut Process, adapter: Adapter, args: &Args, name: &str) -> Result<AdapterEnd, Box<dyn Error>>` — today's body from the `adapter {} ({}), advertising as` log line through the end of the loop and its shutdown block, using `p`'s sessions/registry/shutdown instead of creating them. On every exit path it aborts `accept_tasks`, the active GATT task and `left_watch`, and drops `_app`, `adv` and the listeners.
  - When `rx_control`/`tx_control` end: check `p.session.adapter_names()` for `adapter.name()`; absent → `Ok(AdapterEnd::Removed)`, present → today's error.
  - `SessionEvent::AdapterRemoved` → `Ok(AdapterEnd::Removed)` instead of `Err`.

Log lines (exact):
- removal: `Bluetooth adapter {name} is gone; keeping {n} resumable session(s) while it comes back` (`n` = `registry` size; add `pub fn len(&self) -> usize` to `Registry` in `crates/blessh-core/src/resume.rs` with a one-line unit test `registry_len_counts_sessions`)
- return: the existing `adapter {} ({}), advertising as {:?}` line, which now also runs after a return.

- [ ] **Step 1: Write the failing test**

`crates/blessh-core/src/resume/tests.rs`: `registry_len_counts_sessions` — a new registry has `len() == 0`; after one `serve` registers (reuse the existing in-memory session helper used by `serve_resumes_…` tests) `len() == 1`.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p blessh-core registry_len`
Expected: FAIL to compile (`len` not found).

- [ ] **Step 3: Add `Registry::len`; split `serve` as described**

Keep the L2CAP listeners' bind after `wait_powered`, so the address is read from the adapter that came back (`bluez::l2cap_listen(adapter.address().await?, …)` already happens inside the adapter part).

- [ ] **Step 4: Unit tests, clippy, loopback**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo build && tests/e2e-loopback.sh`
Expected: all pass; loopback `PASS`.

- [ ] **Step 5: Write the e2e case (adapter removed and back)** in `tests/e2e-vhci.sh`

After the existing cases: start `blessh` (Linux client) with `--resume 60` running `for i in $(seq 1 90); do echo tick $i; sleep 1; done` into `$T/ticks.log` in the background; after 5 s, `sudo pkill -x btvirt`, sleep 3, `sudo btvirt -L -l2 &`, wait until `busctl` shows `$SERVER_HCI` powered (≤ 30 s). Pass when: `daemon.log` has `is gone; keeping 1 resumable session(s)` and a second `advertising as`, the daemon pid is unchanged, the daemon log shows `resumed`, and `ticks.log` holds `tick 1` … `tick 90` with no gap. If the Linux client cannot survive its own adapter disappearing, assert only the daemon lines and print `NOTE: client side not checked (client adapter was removed too)`.

Also: stop during the gap — a second short run that kills `btvirt`, then sends SIGTERM to the daemon before restarting `btvirt`; pass when the daemon exits within 5 s with status 0.

- [ ] **Step 6: Run it**

Run (Linux, root setup as in the script header): `cargo build && tests/e2e-vhci.sh`
Expected: `PASS` for all cases.

- [ ] **Step 7: Commit**

```bash
git add crates/blessh-server/src/linux.rs crates/blessh-core/src/resume.rs crates/blessh-core/src/resume/tests.rs tests/e2e-vhci.sh
git commit -m "Server: keep resumable sessions while the adapter goes away and comes back"
```

---

### Task 6: Packaging and `blessh server status`

**Files:**
- Create: `packaging/linux/systemd/blessh-adapter-watch.service`
- Modify: `packaging/linux/systemd/blessh-server.service` (add `Wants=blessh-adapter-watch.service` under `[Unit]`)
- Modify: `crates/blessh-client/Cargo.toml:58-59` (cargo-deb asset for the new unit)
- Modify: `packaging/linux/debian/postinst` (restart the watch unit with the server on upgrade), `packaging/linux/debian/prerm` (stop it)
- Modify: `packaging/linux/install.sh` (install the unit like the others)
- Modify: `packaging/linux/default` (document `BLESSH_ADAPTER_WATCH` and `BLESSH_WATCH_ARGS`)
- Modify: `crates/blessh-server/src/linux_admin.rs` (`status`: a line for the watchdog; warning on adapter mismatch)
- Modify: `tests/e2e-package.sh` (unit installed, `ExecStart` line)
- Note: the COPR spec lives on `packaging/copr-ppa`; when that branch merges, add the unit to its `%files` (a checklist line in that branch's PR, not here).

**Interfaces:**
- Unit content:

```ini
[Unit]
Description=blessh: reset the Bluetooth controller when it stops answering
Documentation=file:///usr/share/doc/blessh/README.md
After=bluetooth.service
PartOf=blessh-server.service

[Service]
Type=simple
EnvironmentFile=-/etc/default/blessh
ExecCondition=/bin/sh -c '[ "$${BLESSH_ADAPTER_WATCH:-yes}" != no ]'
ExecStart=/usr/bin/blessh server adapter-watch $BLESSH_WATCH_ARGS
Restart=on-failure
RestartSec=5
RuntimeDirectory=blessh-adapter-watch
RuntimeDirectoryPreserve=yes
CapabilityBoundingSet=CAP_NET_RAW
NoNewPrivileges=yes
RestrictAddressFamilies=AF_UNIX AF_BLUETOOTH
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
```

(No `[Install]`: the server's `Wants=` starts it, so `blessh server enable` needs no change. `$$` makes the shell, not systemd, expand the variable with its default. `ProtectKernelTunables` is left off on purpose: it makes `/sys` read-only, and the reset writes to it.)

- The watchdog writes `/run/blessh-adapter-watch/last-reset` (one line: RFC 3339 UTC time, then the reset description) after each reset; Task 4's loop gains this write (path from `$RUNTIME_DIRECTORY`, skipped when unset).
- `status` prints `adapter watchdog: on|off (BLESSH_ADAPTER_WATCH=no)|not running`, then `last reset: <line>` when the file exists, and `warning: --adapter differs between BLESSH_ARGS and BLESSH_WATCH_ARGS` when both name one and they differ (parse `/etc/default/blessh` the way `linux_admin.rs` already reads it).

- [ ] **Step 1: Write the failing test** — `linux_admin.rs` unit test `watch_adapter_mismatch` for a pure helper `fn adapter_arg(args: &str) -> Option<&str>`: `"--adapter hci1 --name x"` → `Some("hci1")`, `"--adapter=hci1"` → `Some("hci1")`, `""` → `None`.
- [ ] **Step 2: Run it to verify it fails** — `cargo test -p blessh-server adapter_arg` → FAIL to compile.
- [ ] **Step 3: Implement** the helper, the status lines, the unit, the packaging edits, the `last-reset` write.
- [ ] **Step 4: Run** `cargo test`, then on Linux `tests/e2e-package.sh` (add checks: `/usr/lib/systemd/system/blessh-adapter-watch.service` exists and contains `ExecStart=/usr/bin/blessh server adapter-watch`; `systemd-analyze verify` passes on both units). Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add packaging/linux crates/blessh-client/Cargo.toml crates/blessh-server/src/linux_admin.rs crates/blessh-server/src/watch/mod.rs tests/e2e-package.sh
git commit -m "Packaging: blessh-adapter-watch.service, started with the server"
```

---

### Task 7: End-to-end watchdog test on virtual controllers

**Files:**
- Modify: `tests/e2e-vhci.sh`

- [ ] **Step 1: Write the case**

Start `sudo $BIN/blessh server adapter-watch --adapter $SERVER_HCI --interval 1 --reset-command "kill -CONT $(pgrep -x btvirt); sleep 1" > $T/watch.log 2>&1 &`. Start the resumable `ticks` session as in Task 5. After 5 s, `sudo kill -STOP $(pgrep -x btvirt)`.

The reset command only continues the frozen controller: on a frozen vhci controller `btmgmt power off` blocks past the 40 s window (the kernel's HCI device close waits on the probe that timed out), so the case does not power-cycle the adapter.

Pass when, within 40 s: `watch.log` has `did not answer 2 probes; resetting`, then `answers again`; the daemon pid is unchanged; the session survives without a channel loss (it has no `channel lost` line and its end line says `0 resumes`); `ticks.log` is exactly `tick 1` … `tick 90`. On failure, `kill -CONT` btvirt in the trap so later runs are not left with a frozen controller.

A freeze-only reset keeps the vhci link, so the case asserts session survival; the adapter-removal resume path is Task 5's.

- [ ] **Step 2: Run it**

Run: `cargo build && tests/e2e-vhci.sh`
Expected: `PASS` for every case.

- [ ] **Step 3: Commit**

```bash
git add tests/e2e-vhci.sh
git commit -m "Tests: watchdog resets a virtual controller that stopped answering"
```

---

### Task 8: Docs and the hardware check

**Files:**
- Modify: `docs/linux.md` (new section "Adapter watchdog": what it does, how to turn it off, the log lines from Task 4, `journalctl -u blessh-adapter-watch`)
- Modify: `docs/architecture.md` (components: the watchdog; session model: resumable sessions survive an adapter reset)
- Modify: `docs/hardware.md` (Pi 3 B row: undervoltage stops the controller during long output; watchdog result after the run below)
- Modify: `docs/testing.md` (how to check the watchdog on hardware)

- [ ] **Step 1: Write the docs** (no host names or addresses; describe boards by hardware and OS).
- [ ] **Step 2: Hardware run** — build a package for arm64 from this branch, install it on the Pi 3 B test server, check `blessh server status` shows the watchdog on. Connect from the phone app and from the macOS CLI client (`blessh` from this branch), run `dmesg` repeatedly on the weak supply until the kernel logs `Received unexpected HCI Event 0x00`. Pass: `journalctl -u blessh-adapter-watch` shows the reset and `answers again`, `journalctl -u blessh-server` shows the session `resumed`, and the client's output continues. Record the times in `docs/hardware.md`. If no dip happens in 30 minutes, record `UNTESTED on hardware` and say so.
- [ ] **Step 3: Commit**

```bash
git add docs/linux.md docs/architecture.md docs/hardware.md docs/testing.md
git commit -m "Docs: the adapter watchdog"
```

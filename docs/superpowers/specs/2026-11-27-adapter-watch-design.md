# Adapter watchdog: recover a controller that stopped answering

Status: design, for review.

## Why

On a Raspberry Pi 3 B with a weak power supply, a voltage dip corrupts a
frame on the UART between the kernel and the Bluetooth chip
(`hci0: Received unexpected HCI Event 0x00`, 8 to 20 ms after
`Undervoltage detected!`). From then on the chip answers nothing. Seen twice
in a row on 2026-11-27, each time during large session output (`dmesg`):

* the kernel still lists the LE link as connected and the adapter as
  powered; it notices only when a command is sent
  (`hci0: command tx timeout`, `hcitool cmd 0x04 0x0001` timed out);
* the daemon sits idle with the session's output queued behind it; no
  channel loss is reported, so nothing recovers;
* the client sees the channel stall, drops it and tries to resume, but the
  server is unreachable until someone rebinds the driver by hand
  (`echo serial0-0 > /sys/bus/serial/drivers/hci_uart_bcm/unbind`, then
  `bind`), which brought the chip back both times.

A power supply fixes the cause on that board. The watchdog is for the
general case: any controller (UART or USB) that stops answering leaves a
headless server unreachable until a person intervenes.

## Goals

* Notice a controller that stopped answering within about 10 s, also when
  no client is connected.
* Reset it without a person, as root, while the daemon itself keeps its
  current privileges.
* Transparent for resumable sessions: a session that was running on an L2CAP
  channel resumes on a new channel after the reset, inside the client's
  resume window (120 s by default), with no data lost. Budget: about 10 s
  to detect, 10 to 15 s for the chip to come back, then the client's
  reconnect.

## Non-goals

* Fixing undervoltage or the UART driver.
* The macOS server: CoreBluetooth gives no access to the controller.
* Keeping GATT-stream sessions or sessions that are not resumable alive
  across a reset: they end, as they do on any link loss today.

## Anti-goals

* A watchdog that resets a healthy controller (false positives would cut
  working sessions).
* Reset loops: a controller that does not come back must not be reset
  forever.
* Raising the main daemon's privileges, or loosening its systemd hardening.

## Approaches considered

* **A. Separate root watchdog, and a daemon that outlives its adapter.**
  Picked. Only a small process holds privilege; the daemon keeps its
  sessions through the reset, which is what makes it transparent.
* **B. Same watchdog, daemon unchanged.** Less work, but the daemon exits
  when the adapter is removed (`linux.rs`, `AdapterRemoved`), systemd
  restarts it, and every session is lost. Not transparent.
* **C. The daemon infers a dead controller from stalled sessions** and asks
  a root helper to reset it. Needs no new privileged probe, but cannot tell
  a dead chip from a client that walked away, and cannot notice anything
  while no one is connected, which is exactly when no one can connect.

## Design

### Part 1: `blessh adapter-watch` (new subcommand, Linux)

Runs as root in its own unit. For one adapter (the daemon's `--adapter`,
else the default one, `hci0`):

1. **Probe.** Every 5 s, send HCI Read Local Version Information
   (OGF 0x04, OCF 0x0001) on a raw HCI socket bound to the adapter, with an
   event filter for its Command Complete, and wait up to 2 s. The command
   only reads; `hcitool` sends the same one.
2. **Decide.** Two failed probes in a row mean the controller is dead.
   Skip probing while the adapter is down or absent (bluetoothd or the
   administrator switched it off; the daemon already handles that).
3. **Reset.** Unbind and bind the adapter's device driver through sysfs:
   `/sys/class/bluetooth/<hci>/device` is the device, its `driver` link
   names the driver; write the device name to the driver's `unbind`, wait,
   write it to `bind`. Works the same for `hci_uart_bcm` (`serial0-0`) and
   `btusb` (a USB interface). A device without a driver to rebind (vhci)
   is logged as not resettable.
4. **Limit.** After a reset, wait for the adapter to come back and give it
   30 s before probing again. At most 3 resets in 10 minutes; after that,
   log an error once and only probe (and log when it answers again).
5. **Log** each step at info or warn, in words an administrator can act on
   (for example "hci0 did not answer two probes; rebinding hci_uart_bcm
   (serial0-0)"). The reset message suggests checking the power supply and
   the kernel log; the watchdog itself does not read the kernel log.

Which adapter: `--adapter <hci>` in `BLESSH_WATCH_ARGS`; without it, the
same default the daemon uses (BlueZ's default adapter). An administrator
who sets `--adapter` in `BLESSH_ARGS` sets it in `BLESSH_WATCH_ARGS` too;
`blessh server status` warns when the two differ. (Builder's call: parsing
the daemon's arguments from the watchdog was rejected as fragile.)

"Down" is read with the `HCIGETDEVINFO` ioctl (`HCI_UP` flag), which does
not reach the controller.

### Part 2: the daemon outlives its adapter

`serve()` is split into what lives as long as the process and what lives
as long as one adapter instance:

* **Process lifetime:** the session registry (`Registry`), the session
  table (`Shared`), resumable sessions and their sshd connections,
  shutdown handling.
* **Adapter lifetime:** the bluer `Adapter`, GATT application,
  advertisement, L2CAP listeners and their accept tasks, pending and active
  GATT-stream session state, parked notify socket, disconnect timers.

When the adapter is removed, the adapter part is dropped (its L2CAP
channels fail, and resumable sessions detach as on any channel loss and
keep reading sshd into their replay buffers), the daemon waits for the
adapter to appear and be powered, then sets the adapter part up again. It
no longer exits on `AdapterRemoved`. Sessions that are not resumable end
as today.

The L2CAP listeners bind to the adapter's address. After a rebind the
Broadcom chip briefly reports its default address before the driver sets
the real one, so the address is read again after the adapter is powered,
not cached from before the reset.

### Packaging

* New unit `blessh-adapter-watch.service`: `User=root`,
  `ExecStart=/usr/bin/blessh adapter-watch $BLESSH_WATCH_ARGS`,
  `EnvironmentFile=-/etc/default/blessh`, `PartOf=blessh-server.service`,
  `Restart=on-failure`. Hardened like the server unit as far as the work
  allows: `CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN`,
  `RestrictAddressFamilies=AF_UNIX AF_BLUETOOTH`, the sysfs paths it writes
  made writable while the rest of the file system stays read-only.
* `blessh-server.service` gets `Wants=blessh-adapter-watch.service`, so
  `blessh server enable` / `disable` cover both and nothing new needs
  enabling. To turn the watchdog off: `BLESSH_ADAPTER_WATCH=no` in
  `/etc/default/blessh` (checked by the unit with `ExecCondition=`), or
  `systemctl mask blessh-adapter-watch`.
* `blessh server status` shows whether the watchdog runs and its last
  reset, if any.
* Debian, the COPR spec and `install.sh` install the unit; postinst
  restarts it with the server on upgrade.

### Docs

* `linux.md`: what the watchdog does, how to turn it off, what its log
  lines mean.
* `architecture.md`: the components list and the session model (sessions
  survive an adapter reset).
* `hardware.md`: the Pi 3 B undervoltage finding.
* `protocol.md`: no change. A reset looks to a client like a lost link,
  which the resume section already covers.

## Verification

* Unit tests: the decision logic (two misses, skip while down, cooldown,
  reset limit) driven by a fake probe and a fake clock; the sysfs reset
  against a temporary directory tree laid out like sysfs.
* `tests/e2e-vhci.sh`, new case: start a resumable session with a steady
  output stream, `kill -STOP` the `btvirt` process (both virtual
  controllers stop answering, as the chip did), and check that the
  watchdog reports the dead controller. vhci has no driver to rebind, so
  the test gives the watchdog a reset hook (a test-only option,
  `--reset-command`) that resumes `btvirt` with `kill -CONT` and cycles
  the adapter down and up, which drops its links. Pass: the client resumes,
  the stream has no gap, and the daemon did not restart.
* `tests/e2e-vhci.sh`, second case, for part 2 (down and up does not remove
  the adapter): during a resumable session, kill `btvirt` and start it
  again, so both adapters disappear and come back. Pass: the daemon logs
  the removal, sets up again without restarting, and the client resumes.
  (Builder's call. Risk: the Linux client must also survive its own
  adapter disappearing; if it cannot, this case checks only the server
  side, and says so.)
* Hardware: the Pi 3 B, with the phone app and the macOS CLI client. A
  power dip cannot be produced on demand, so the reset path is exercised
  by stopping the chip's answers another way if one is found, else by
  running long output until a dip occurs on the weak supply.

## Left to the builder

* Module layout inside `blessh-server` and the exact names of options.
* Whether the probe uses `libc` directly or a small HCI socket helper in
  `blessh-ble`.
* The exact sysfs write paths the unit needs (`ReadWritePaths=`).
* Whether part 2 lands first as its own commit (it is useful alone: an
  administrator's manual rebind would then also keep sessions).

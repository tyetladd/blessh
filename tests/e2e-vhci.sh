#!/bin/sh
# End-to-end test over real BlueZ with two virtual controllers (btvirt):
# `blessh server run` serves on one adapter as an unprivileged user, blessh
# connects from the other as an OpenSSH ProxyCommand, over the L2CAP
# channel and over the GATT stream. sshd is started per session by
# systemd-socket-activate and `blessh server session`, mimicking
# blessh-session.socket.
#
# Setup (root): modprobe hci_vhci; btvirt -L -l2. The virtual controllers
# are hci0 and hci1 unless a real adapter holds an index; SERVER_HCI and
# CLIENT_HCI name them.
#
# DAEMON_USER=<user> runs the daemon as that user (through sudo) to test the
# D-Bus policy: install packaging/linux/dbus.conf with _blessh replaced by
# that user into the bus configuration first. The test then also checks
# that the user is denied BlueZ calls the daemon does not need.
set -eu

BIN=$(pwd)/target/debug
SERVER_HCI=${SERVER_HCI:-hci0}
CLIENT_HCI=${CLIENT_HCI:-hci1}
SERVER=$(busctl get-property org.bluez "/org/bluez/$SERVER_HCI" org.bluez.Adapter1 Address | cut -d'"' -f2)
T=/tmp/blessh-vhci
rm -rf "${T:?}"
mkdir -p "$T"

ssh-keygen -q -t ed25519 -N '' -f "$T/hostkey"
ssh-keygen -q -t ed25519 -N '' -f "$T/id"
cp "$T/id.pub" "$T/authorized_keys"
cat > "$T/sshd_config" <<CFG
HostKey $T/hostkey
AuthorizedKeysFile $T/authorized_keys
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PidFile none
LogLevel ERROR
CFG

systemd-socket-activate -l "$T/session.sock" --inetd -a "$BIN/blessh" server session -- /usr/sbin/sshd -i -f "$T/sshd_config" -E "$T/sshd.log" \
    > "$T/socket-activate.log" 2>&1 &
SA=$!
# bluetoothd's discovery drops advertisements without the LE-discoverable
# bit, and btvirt's carry flags 0x04 only, so the daemon advertises as
# discoverable here.
DAEMON_ARGS="--adapter $SERVER_HCI --name vhci-server --session-socket $T/session.sock --host-key $T/hostkey.pub --max-sessions 2 --discoverable"
if [ -n "${DAEMON_USER:-}" ]; then
    cp "$BIN/blessh" "$T/"
    sleep 1
    chmod 666 "$T/session.sock"
    # shellcheck disable=SC2086
    sudo -u "$DAEMON_USER" env RUST_LOG=debug ${DBUS_SYSTEM_BUS_ADDRESS:+DBUS_SYSTEM_BUS_ADDRESS=$DBUS_SYSTEM_BUS_ADDRESS} \
        "$T/blessh" server run $DAEMON_ARGS > "$T/daemon.log" 2>&1 &
else
    # shellcheck disable=SC2086
    RUST_LOG=debug "$BIN/blessh" server run $DAEMON_ARGS > "$T/daemon.log" 2>&1 &
fi
D=$!
trap 'kill $D $SA 2>/dev/null || true' EXIT
sleep 3
grep -q "L2CAP channels on PSM" "$T/daemon.log" || { echo "FAIL: no L2CAP channel"; cat "$T/daemon.log"; exit 1; }

# Successive clients share the one LE link: let the previous blessh finish
# (the last one disconnects on exit) before starting the next.
wait_link_down() {
    for _ in $(seq 40); do
        pgrep -x blessh > /dev/null || return 0
        sleep 0.25
    done
}

SSH="ssh -F /dev/null -i $T/id -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$T/known_hosts"

run() {
    wait_link_down
    # shellcheck disable=SC2086
    timeout 180 $SSH -o HostKeyAlias=vhci-server \
        -o ProxyCommand="env RUST_LOG=info $BIN/blessh proxy ${CONNECT_ARGS:-} --scan-timeout 60 --adapter $CLIENT_HCI $SERVER 2>>$T/connect.log" \
        "$(id -un)@vhci-server" "$@"
}

# Like a phone: the client leaves the link up on exit and the next client
# reuses it.
run_keep() {
    # shellcheck disable=SC2086
    timeout 150 $SSH -o HostKeyAlias=vhci-server \
        -o ProxyCommand="env RUST_LOG=info $BIN/blessh proxy --keep-link --scan-timeout 60 ${CONNECT_ARGS:-} --adapter $CLIENT_HCI $SERVER 2>>$T/connect.log" \
        "$(id -un)@vhci-server" "$@"
}

# Without waiting for other clients: several sessions over one link.
run_now() {
    # shellcheck disable=SC2086
    timeout 150 $SSH -o HostKeyAlias=vhci-server \
        -o ProxyCommand="env RUST_LOG=info $BIN/blessh proxy ${CONNECT_ARGS:-} --scan-timeout 60 --adapter $CLIENT_HCI $SERVER 2>>$T/connect.log" \
        "$(id -un)@vhci-server" "$@"
}

# With btvirt, discovery through bluetoothd can take up to a minute even
# with --discoverable (not seen on real controllers); the proxy scans wait
# for it too.
for _ in 1 2 3 4 5 6; do
    "$BIN/blessh" scan --adapter "$CLIENT_HCI" --timeout 30 2>/dev/null | grep -q "$SERVER" && break
done

head -c 100000 /dev/urandom > "$T/blob"
want=$(sha256sum < "$T/blob" | cut -d' ' -f1)

rounds() {
    for i in 1 2; do
        echo "== $1, round $i"
        out=$(run 'echo hello-over-ble')
        [ "$out" = hello-over-ble ] || { echo "FAIL: echo: $out"; exit 1; }
        got=$(run "cat '$T/blob'" | sha256sum | cut -d' ' -f1)
        [ "$got" = "$want" ] || { echo "FAIL: download checksum"; exit 1; }
        got=$(run 'sha256sum' < "$T/blob" | cut -d' ' -f1)
        [ "$got" = "$want" ] || { echo "FAIL: upload checksum"; exit 1; }
        tty=$(run -tt tty < /dev/null 2>/dev/null | tr -d '\r')
        case $tty in /dev/pts/*) ;; *) echo "FAIL: no pty: $tty"; exit 1 ;; esac
    done
}

rounds "L2CAP channel"
grep -q "l2cap, mtu [0-9]" "$T/connect.log" || { echo "FAIL: client did not report the L2CAP channel"; exit 1; }
grep -q "client connected (l2cap, " "$T/daemon.log" || { echo "FAIL: server did not log an L2CAP session"; exit 1; }
grep -q "transport l2cap" "$T/socket-activate.log" || { echo "FAIL: session helper did not log the transport"; exit 1; }

echo "== ssh through the blessh wrapper"
out=$(wait_link_down; BLESSH_PROXY_ARGS="--adapter $CLIENT_HCI --scan-timeout 60" timeout 180 "$BIN/blessh" -F /dev/null -i "$T/id" -o IdentitiesOnly=yes \
    -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$T/known_hosts" "$(id -un)@$SERVER" 'echo via-wrapper')
[ "$out" = via-wrapper ] || { echo "FAIL: wrapper: $out"; exit 1; }
grep -q "^blessh-$(echo "$SERVER" | tr -d : | tr 'A-F' 'a-f') " "$T/known_hosts" || { echo "FAIL: wrapper did not set the host key alias"; exit 1; }

echo "== picking the server from blessh's list"
# The list's scan runs on the client's adapter through BLESSH_SCANNER.
printf '#!/bin/sh\nexec %s "$@" --adapter %s\n' "$BIN/blessh" "$CLIENT_HCI" > "$T/scanner"
chmod 755 "$T/scanner"
wait_link_down
out=$(BLESSH_SCANNER="$T/scanner" BLESSH_PROXY_ARGS="--adapter $CLIENT_HCI" timeout 150 \
    python3 "$(dirname "$0")/pick-drive.py" vhci-server "$BIN/blessh" -F /dev/null -i "$T/id" -o IdentitiesOnly=yes \
    -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$T/known_hosts" "$(id -un)@" 'echo via-list') \
    || { echo "FAIL: list: $out"; exit 1; }
echo "$out" | grep -q via-list || { echo "FAIL: list: $out"; exit 1; }
grep -q "^blessh-vhci-server " "$T/known_hosts" || { echo "FAIL: list did not connect by name"; exit 1; }
! pgrep -f "blessh scan" > /dev/null || { echo "FAIL: the list's scan still runs"; exit 1; }

echo "== two sessions at once over one link"
wait_link_down
run_now 'sleep 4; echo first' > "$T/first.out" 2>&1 &
FIRST=$!
sleep 2
out=$(run_now 'echo second')
[ "$out" = second ] || { echo "FAIL: second session: $out"; exit 1; }
wait $FIRST
[ "$(cat "$T/first.out")" = first ] || { echo "FAIL: first session: $(cat "$T/first.out")"; exit 1; }

echo "== session limit (--max-sessions 2)"
wait_link_down
run_now 'sleep 6; echo a' > "$T/a.out" 2>&1 &
A=$!
run_now 'sleep 6; echo b' > "$T/b.out" 2>&1 &
B=$!
sleep 3
if run_now 'echo c' > "$T/c.out" 2>&1; then echo "FAIL: third session was admitted"; exit 1; fi
grep -q "refused, 2 sessions in progress" "$T/daemon.log" || { echo "FAIL: no refusal logged"; exit 1; }
wait $A $B
[ "$(cat "$T/a.out")" = a ] && [ "$(cat "$T/b.out")" = b ] || { echo "FAIL: held sessions broke"; exit 1; }

echo "== session ends when the client disappears"
wait_link_down
# Not resumable: the daemon would keep a resumable session (and its sshd)
# for --resume seconds. Kill only the client proxy: the daemon is also
# named blessh since both are one program.
CONNECT_ARGS="--resume 0"
(run -tt 'sleep 4242' < /dev/null > /dev/null 2>&1 &)
for _ in $(seq 180); do
    pgrep -x sleep -a | grep -q 4242 && break
    sleep 0.5
done
pgrep -x sleep -a | grep -q 4242 || { echo "FAIL: remote command did not start"; exit 1; }
pkill -f "blessh proxy" || true
unset CONNECT_ARGS
sleep 5
if pgrep -x sleep -a | grep -q 4242; then echo "FAIL: session survived disconnect"; exit 1; fi

echo "== reconnect over a link the previous client left up"
wait_link_down
[ "$(run_keep 'echo a')" = a ] || { echo "FAIL: first kept-link session"; exit 1; }
sleep 0.5
[ "$(run_keep 'echo b')" = b ] || { echo "FAIL: reconnect over the same link"; exit 1; }
# Reconnect just as the server drops the idle link (10 s after the session):
# that attempt may fail, but must not leave the server stuck.
sleep 9.7
run_keep 'echo c' > /dev/null 2>&1 || echo "   (attempt racing the server's disconnect failed, as allowed)"
sleep 1
for attempt in 1 2 3; do
    out=$(run 'echo d' 2>/dev/null) && break
    sleep 2
done
[ "$out" = d ] || { echo "FAIL: server stuck after a reconnect raced its disconnect"; exit 1; }

CONNECT_ARGS=--gatt rounds "GATT stream"
grep -q "server info: proto=1;name=vhci-server;mtu=[0-9]*;psm=[0-9]" "$T/connect.log" || { echo "FAIL: INFO"; exit 1; }
grep -q "gatt, frame [0-9]" "$T/connect.log" || { echo "FAIL: client did not report the GATT stream"; exit 1; }
grep -q "client connected (gatt, " "$T/daemon.log" || { echo "FAIL: server did not log a GATT session"; exit 1; }
echo "== GATT stream, writes with response"
got=$(CONNECT_ARGS="--gatt --write-with-response" run 'sha256sum' < "$T/blob" | cut -d' ' -f1)
[ "$got" = "$want" ] || { echo "FAIL: upload with write requests"; exit 1; }
grep -q "writes with response" "$T/connect.log" || { echo "FAIL: write mode not used"; exit 1; }

if [ -n "${DAEMON_USER:-}" ]; then
    echo "== D-Bus policy denies other BlueZ calls"
    as_daemon() {
        sudo -u "$DAEMON_USER" env ${DBUS_SYSTEM_BUS_ADDRESS:+DBUS_SYSTEM_BUS_ADDRESS=$DBUS_SYSTEM_BUS_ADDRESS} \
            dbus-send --system --print-reply --dest=org.bluez "$@" 2>&1 || true
    }
    for call in "/org/bluez/$SERVER_HCI org.bluez.Adapter1.StartDiscovery" \
        "/org/bluez/$SERVER_HCI org.freedesktop.DBus.Properties.Set string:org.bluez.Adapter1 string:Pairable variant:boolean:true" \
        "/org/bluez/$SERVER_HCI org.bluez.Adapter1.RemoveDevice objpath:/org/bluez/$SERVER_HCI/dev_00_AA_01_01_00_01"; do
        # shellcheck disable=SC2086
        as_daemon $call | grep -q AccessDenied || { echo "FAIL: allowed: $call"; exit 1; }
    done
fi

echo "== daemon keeps resumable sessions while its adapter goes away and comes back"
removals() { grep -c "is gone; keeping" "$T/daemon.log" 2>/dev/null || true; }
resumable() { grep -c "resumable SSH session started" "$T/daemon.log" 2>/dev/null || true; }
wait_link_down
started=$(resumable)
BLESSH_PROXY_ARGS="--adapter $CLIENT_HCI --resume 60 --scan-timeout 60" timeout 300 "$BIN/blessh" -F /dev/null -i "$T/id" -o IdentitiesOnly=yes \
    -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$T/known_hosts" \
    "$(id -un)@$SERVER" 'for i in $(seq 1 90); do echo tick $i; sleep 1; done' \
    > "$T/ticks.log" 2> "$T/ticks.err" &
TICKS=$!
for _ in $(seq 180); do
    [ "$(resumable)" -gt "$started" ] && break
    sleep 0.5
done
[ "$(resumable)" -gt "$started" ] ||
    { echo "FAIL: the resumable session did not start"; tail -20 "$T/daemon.log" "$T/ticks.err" 2>/dev/null || true; exit 1; }
sleep 5
sudo pkill -x btvirt
sleep 3
sudo btvirt -L -l2 > "$T/btvirt.log" 2>&1 &
POWERED=
for _ in $(seq 60); do
    POWERED=$(busctl get-property org.bluez "/org/bluez/$SERVER_HCI" org.bluez.Adapter1 Powered 2>/dev/null || true)
    [ "$POWERED" = "b true" ] && break
    sleep 0.5
done
[ "$POWERED" = "b true" ] || { echo "FAIL: $SERVER_HCI did not come back powered"; exit 1; }
TICKS_STATUS=0
wait "$TICKS" || TICKS_STATUS=$?
grep -q "is gone; keeping 1 resumable session(s) while it comes back" "$T/daemon.log" ||
    { echo "FAIL: no removal log line"; grep -E "adapter|advertising" "$T/daemon.log" | tail -20; exit 1; }
[ "$(grep -c "advertising as" "$T/daemon.log")" -ge 2 ] ||
    { echo "FAIL: the daemon did not advertise again"; exit 1; }
kill -0 "$D" 2>/dev/null || { echo "FAIL: the daemon restarted"; exit 1; }
expected=$(seq 1 90 | sed 's/^/tick /')
got=$(cat "$T/ticks.log")
if [ "$TICKS_STATUS" = 0 ] && grep -q "resumed (resume" "$T/daemon.log" && [ "$got" = "$expected" ]; then
    echo "   client resumed, the stream had no gap"
else
    echo "NOTE: client side not checked (client adapter was removed too)"
    echo "   client exit status $TICKS_STATUS, $(wc -l < "$T/ticks.log") ticks"
fi

echo "== daemon stops while its adapter is gone"
before=$(removals)
sudo pkill -x btvirt
for _ in $(seq 120); do
    [ "$(removals)" -gt "$before" ] && break
    sleep 0.5
done
[ "$(removals)" -gt "$before" ] || { echo "FAIL: the daemon did not notice the adapter removal"; exit 1; }
# With DAEMON_USER, $D is sudo's pid; signal the daemon itself.
DPID=$D
if [ -n "${DAEMON_USER:-}" ]; then
    DPID=$(pgrep -P "$D" -x blessh) || DPID=$D
fi
kill -TERM "$DPID" 2>/dev/null || { echo "FAIL: the daemon is not running while its adapter is gone"; exit 1; }
( sleep 5; kill -KILL "$DPID" 2>/dev/null ) &
WATCH=$!
STATUS=0
wait "$D" || STATUS=$?
kill "$WATCH" 2>/dev/null || true
[ "$STATUS" = 0 ] ||
    { echo "FAIL: the daemon exited with status $STATUS after SIGTERM while the adapter was gone"; exit 1; }

grep -E "rejecting|no longer connected|did not open|refused" "$T/daemon.log" | sed 's/^/   daemon: /' || true
echo PASS

# dmemcg-booster — systemd-free / runit port

This fork ports Valve's `dmemcg-booster` 0.1.2 from systemd D-Bus notifications to direct cgroup v2 scanning. It has no D-Bus or systemd runtime dependency.

The daemon polls cgroupfs, enables the `dmem` controller where doing so is safe, and writes full-capacity pass-through values to `dmem.low` on internal cgroups. Leaf limits remain untouched so Gamescope or another foreground booster can raise and lower them.

## What changed

- Removed the Rust `dbus` dependency and all `org.freedesktop.systemd1` calls.
- Replaced `UnitNew`/`UnitRemoved` signals with a periodic cgroupfs scan (1 second by default).
- Removed assumptions about `user.slice`, `app.slice`, and `user@.service` names.
- Added system and per-user modes with delegation-boundary handling.
- Added runit services for Void-style and Artix-style layouts.
- Kept `--use-system-bus` as a compatibility alias for `--system`.

## Requirements

1. A unified cgroup v2 hierarchy mounted at `/sys/fs/cgroup`.
2. A kernel and GPU driver exposing the `dmem` controller and `dmem.capacity`.
3. A cgroup manager or launcher that actually places applications in separate cgroups.

Check kernel support:

```sh
grep -w dmem /sys/fs/cgroup/cgroup.controllers
cat /sys/fs/cgroup/dmem.capacity
```

Important: runit supervises processes but does not, by itself, construct a per-application cgroup hierarchy. If every desktop process lives in the same cgroup, there is nothing for a foreground booster to prioritize. A cgroup-aware session/login manager, delegated hierarchy, or game launcher is still required.

## Build and install

```sh
cargo build --release --locked
sudo ./install-runit.sh --enable
sv status dmemcg-booster
```

The installer auto-detects the usual layouts:

- Void: service in `/etc/sv`, enabled through `/var/service`.
- Artix: service in `/etc/runit/sv`, enabled through `/run/runit/service`.

You can override detection:

```sh
sudo SERVICE_DIR=/etc/sv ACTIVE_DIR=/var/service ./install-runit.sh --enable
```

## Per-user service

The system daemon stops at user-owned delegated cgroups. Inside each graphical user session, run the user daemon from a per-user `runsvdir` so it inherits that session's cgroup:

```sh
mkdir -p ~/.config/sv ~/.service
cp -a /usr/share/dmemcg-booster/runit/user/dmemcg-booster ~/.config/sv/
ln -s ~/.config/sv/dmemcg-booster ~/.service/dmemcg-booster
```

Do not launch that user daemon from a boot-time root `runsvdir` and merely drop privileges: it must start inside the user's delegated session cgroup.

## Manual checks

Preview without writing:

```sh
sudo dmemcg-booster --system --once --dry-run --verbose
```

Run the user scan once:

```sh
dmemcg-booster --user --once --verbose
```

Confirm propagation:

```sh
grep -w dmem /sys/fs/cgroup/cgroup.subtree_control
find /sys/fs/cgroup -name dmem.low -print
```

Configuration lives in the service's `conf` file. For example:

```sh
OPTS="--system --interval-ms 2000"
```

`--once` returns 0 for a clean scan, 1 for hierarchy errors, 2 for invalid CLI usage, and 3 when cgroup v2 or `dmem` support is unavailable. The long-running runit service waits and retries when support is not ready yet.

## CLI

```text
--system              Manage root-owned cgroups and stop at delegated subtrees
--user                Manage the current writable delegated user subtree
--once                Scan once and exit
--interval-ms N       Poll interval (minimum 100 ms; default 1000)
--cgroup-root PATH    Override the cgroup v2 mount
--scan-root PATH      Override the subtree to scan
--dry-run             Show intended changes without writing
-v, --verbose         Print every scan summary
```

## Scope

This program prepares the hierarchy. It does not detect the focused window and does not create per-game cgroups. Gamescope or a desktop-specific foreground booster supplies the leaf-level `dmem.low` changes.

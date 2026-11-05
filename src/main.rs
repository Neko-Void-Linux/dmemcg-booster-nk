use crate::cgroup::{CGroup, DMemLimit};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

mod cgroup;

const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_INTERVAL_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    System,
    User,
}

#[derive(Debug)]
struct Options {
    mode: Mode,
    cgroup_root: PathBuf,
    scan_root: Option<PathBuf>,
    interval: Duration,
    once: bool,
    dry_run: bool,
    verbose: bool,
    used_legacy_flag: bool,
}

#[derive(Default, Debug)]
struct ScanReport {
    visited: usize,
    controllers_enabled: usize,
    limits_updated: usize,
    delegated_subtrees: usize,
    unsafe_cgroups: usize,
    errors: BTreeSet<String>,
}

fn main() -> ExitCode {
    let options = match parse_options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("dmemcg-booster: {message}");
            eprintln!("Try 'dmemcg-booster --help' for usage.");
            return ExitCode::from(2);
        }
    };

    if options.used_legacy_flag {
        eprintln!(
            "dmemcg-booster: warning: --use-system-bus is now a compatibility alias for --system; D-Bus is not used"
        );
    }

    if options.mode == Mode::System && effective_uid().unwrap_or(u32::MAX) != 0 && !options.dry_run
    {
        eprintln!("dmemcg-booster: --system must run as root");
        return ExitCode::from(1);
    }

    let hierarchy_root = match CGroup::hierarchy_root(options.cgroup_root.clone()) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("dmemcg-booster: {error}");
            return ExitCode::from(1);
        }
    };

    let scan_root = match resolve_scan_root(&options, &hierarchy_root) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("dmemcg-booster: could not determine scan root: {error}");
            return ExitCode::from(1);
        }
    };

    eprintln!(
        "dmemcg-booster: mode={} cgroup-root={} scan-root={}{}",
        match options.mode {
            Mode::System => "system",
            Mode::User => "user",
        },
        hierarchy_root.path().display(),
        scan_root.path().display(),
        if options.dry_run { " (dry-run)" } else { "" }
    );

    let mut previous_errors = BTreeSet::new();
    let mut unavailable_was_reported = false;

    loop {
        let once_exit = match scan_hierarchy(&options, &hierarchy_root, &scan_root) {
            Ok(report) => {
                unavailable_was_reported = false;
                let had_errors = !report.errors.is_empty();
                for error in report.errors.difference(&previous_errors) {
                    eprintln!("dmemcg-booster: warning: {error}");
                }
                previous_errors = report.errors;

                if options.verbose || report.controllers_enabled > 0 || report.limits_updated > 0 {
                    eprintln!(
                        "dmemcg-booster: visited={}, enabled={}, limits={}, delegated={}, unsafe={}, errors={}",
                        report.visited,
                        report.controllers_enabled,
                        report.limits_updated,
                        report.delegated_subtrees,
                        report.unsafe_cgroups,
                        previous_errors.len()
                    );
                }

                if had_errors {
                    1
                } else {
                    0
                }
            }
            Err(error) if error.kind() == ErrorKind::Unsupported => {
                if !unavailable_was_reported {
                    eprintln!("dmemcg-booster: waiting for dmem cgroup support: {error}");
                    unavailable_was_reported = true;
                }
                3
            }
            Err(error) => {
                eprintln!("dmemcg-booster: scan failed: {error}");
                1
            }
        };

        if options.once {
            return ExitCode::from(once_exit);
        }
        thread::sleep(options.interval);
    }
}

fn scan_hierarchy(
    options: &Options,
    hierarchy_root: &CGroup,
    scan_root: &CGroup,
) -> io::Result<ScanReport> {
    let available = hierarchy_root.available_controllers().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "{} is not a cgroup v2 hierarchy (cgroup.controllers is missing)",
                    hierarchy_root.path().display()
                ),
            )
        } else {
            error
        }
    })?;
    if !available.contains("dmem") {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "'dmem' is absent from {}/cgroup.controllers",
                hierarchy_root.path().display()
            ),
        ));
    }

    let capacity = hierarchy_root.device_memory_capacity().map_err(|error| {
        io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "cannot read {}/dmem.capacity: {error}",
                hierarchy_root.path().display()
            ),
        )
    })?;
    if capacity.is_empty() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "dmem.capacity contains no device-memory regions",
        ));
    }

    let manager_uid = effective_uid()?;
    let mut report = ScanReport::default();
    visit_cgroup(
        options,
        scan_root,
        scan_root,
        &capacity,
        manager_uid,
        &mut report,
    );
    Ok(report)
}

fn visit_cgroup(
    options: &Options,
    scan_root: &CGroup,
    group: &CGroup,
    capacity: &DMemLimit,
    manager_uid: u32,
    report: &mut ScanReport,
) {
    report.visited += 1;

    let children = match group.children() {
        Ok(children) => children,
        Err(error) => {
            record_error(report, group, "list children", &error);
            return;
        }
    };

    let owner = match group.subtree_control_owner() {
        Ok(owner) => owner,
        Err(error) => {
            record_error(report, group, "inspect cgroup delegation", &error);
            return;
        }
    };
    let delegated_boundary = match options.mode {
        Mode::System => !group.is_hierarchy_root() && owner != manager_uid,
        Mode::User => {
            group != scan_root && owner != manager_uid && !group.subtree_control_is_writable()
        }
    };

    if delegated_boundary {
        report.delegated_subtrees += 1;
        ensure_passthrough_limit(options, group, capacity, report);
        return;
    }

    let active = match group.active_controllers() {
        Ok(active) => active,
        Err(error) => {
            record_error(report, group, "read cgroup.subtree_control", &error);
            return;
        }
    };
    let available = match group.available_controllers() {
        Ok(available) => available,
        Err(error) => {
            record_error(report, group, "read cgroup.controllers", &error);
            return;
        }
    };

    let can_have_managed_children =
        group.is_hierarchy_root() || !children.is_empty() || !active.is_empty();
    if can_have_managed_children && available.contains("dmem") && !active.contains("dmem") {
        let has_processes = !group.is_hierarchy_root()
            && match group.has_local_processes() {
                Ok(has_processes) => has_processes,
                Err(error) => {
                    record_error(report, group, "read cgroup.procs", &error);
                    true
                }
            };

        if has_processes {
            report.unsafe_cgroups += 1;
            report.errors.insert(format!(
                "{} contains processes; refusing to enable the domain controller there",
                group.path().display()
            ));
        } else if options.dry_run {
            report.controllers_enabled += 1;
        } else {
            match group.enable_controller("dmem") {
                Ok(()) => report.controllers_enabled += 1,
                Err(error) => record_error(report, group, "enable dmem", &error),
            }
        }
    }

    // dmem.low is a hierarchical protection. Internal cgroups need a full-capacity
    // pass-through value so a focused leaf can receive protection. The root has no
    // dmem.low file, and a user daemon must not alter its delegation root because
    // that file is owned by the parent manager.
    let is_user_delegation_root = options.mode == Mode::User && group == scan_root;
    if !group.is_hierarchy_root() && !children.is_empty() && !is_user_delegation_root {
        ensure_passthrough_limit(options, group, capacity, report);
    }

    for child in children {
        visit_cgroup(options, scan_root, &child, capacity, manager_uid, report);
    }
}

fn ensure_passthrough_limit(
    options: &Options,
    group: &CGroup,
    capacity: &DMemLimit,
    report: &mut ScanReport,
) {
    if options.dry_run {
        match group.device_memory_low() {
            Ok(current) if current == *capacity => {}
            Ok(_) => report.limits_updated += 1,
            Err(error) => record_error(report, group, "read dmem.low", &error),
        }
        return;
    }

    match group.write_device_memory_low(capacity) {
        Ok(true) => report.limits_updated += 1,
        Ok(false) => {}
        Err(error) => record_error(report, group, "write dmem.low", &error),
    }
}

fn record_error(report: &mut ScanReport, group: &CGroup, action: &str, error: &io::Error) {
    // Cgroups can disappear between readdir and open. Those races are normal and
    // will be picked up on the next scan.
    if error.kind() == ErrorKind::NotFound {
        return;
    }
    report.errors.insert(format!(
        "cannot {action} for {}: {error}",
        group.path().display()
    ));
}

fn resolve_scan_root(options: &Options, hierarchy_root: &CGroup) -> io::Result<CGroup> {
    if let Some(requested) = &options.scan_root {
        return cgroup_from_cli_path(hierarchy_root.path(), requested);
    }

    match options.mode {
        Mode::System => Ok(hierarchy_root.clone()),
        Mode::User => find_user_delegation_root(hierarchy_root),
    }
}

fn cgroup_from_cli_path(hierarchy_root: &Path, requested: &Path) -> io::Result<CGroup> {
    let path = if requested.starts_with(hierarchy_root) {
        requested.to_path_buf()
    } else {
        hierarchy_root.join(requested.strip_prefix(Path::new("/")).unwrap_or(requested))
    };
    CGroup::from_path(hierarchy_root, path)
}

fn find_user_delegation_root(hierarchy_root: &CGroup) -> io::Result<CGroup> {
    let relative = current_cgroup_path()?;
    let current_path = hierarchy_root
        .path()
        .join(relative.strip_prefix(Path::new("/")).unwrap_or(&relative));
    let mut current = CGroup::from_path(hierarchy_root.path(), current_path)?;
    let mut highest_writable = None;

    loop {
        if current.subtree_control_is_writable() {
            highest_writable = Some(current.clone());
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    highest_writable.ok_or_else(|| {
        io::Error::new(
            ErrorKind::PermissionDenied,
            "no writable delegated cgroup was found above this process; start the system service and use a cgroup-aware login/session manager",
        )
    })
}

fn current_cgroup_path() -> io::Result<PathBuf> {
    for line in fs::read_to_string("/proc/self/cgroup")?.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            if path.ends_with(" (deleted)") {
                return Err(io::Error::new(
                    ErrorKind::NotFound,
                    "the process cgroup has already been deleted",
                ));
            }
            return Ok(PathBuf::from(path));
        }
    }

    Err(io::Error::new(
        ErrorKind::Unsupported,
        "no cgroup v2 entry was found in /proc/self/cgroup",
    ))
}

fn effective_uid() -> io::Result<u32> {
    for line in fs::read_to_string("/proc/self/status")?.lines() {
        if let Some(values) = line.strip_prefix("Uid:") {
            let effective = values
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "malformed Uid field"))?;
            return effective.parse::<u32>().map_err(|error| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid effective uid {effective:?}: {error}"),
                )
            });
        }
    }

    Err(io::Error::new(
        ErrorKind::InvalidData,
        "Uid field missing from /proc/self/status",
    ))
}

fn parse_options() -> Result<Options, String> {
    let mut mode = None;
    let mut cgroup_root = env::var_os("DMEMCG_CGROUP_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_ROOT));
    let mut scan_root = env::var_os("DMEMCG_SCAN_ROOT").map(PathBuf::from);
    let mut interval_ms = env::var("DMEMCG_INTERVAL_MS")
        .ok()
        .map(|value| parse_interval(&value))
        .transpose()?
        .unwrap_or(DEFAULT_INTERVAL_MS);
    let mut once = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut used_legacy_flag = false;

    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--system" => set_mode(&mut mode, Mode::System)?,
            "--user" => set_mode(&mut mode, Mode::User)?,
            "--use-system-bus" => {
                set_mode(&mut mode, Mode::System)?;
                used_legacy_flag = true;
            }
            "--once" => once = true,
            "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--cgroup-root" => cgroup_root = PathBuf::from(next_value(&mut args, "--cgroup-root")?),
            "--scan-root" => scan_root = Some(PathBuf::from(next_value(&mut args, "--scan-root")?)),
            "--interval-ms" => {
                interval_ms = parse_interval(&next_value(&mut args, "--interval-ms")?)?
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!(
                    "dmemcg-booster {} (runit/sysfs port)",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            _ if argument.starts_with("--cgroup-root=") => {
                cgroup_root = PathBuf::from(&argument[14..]);
            }
            _ if argument.starts_with("--scan-root=") => {
                scan_root = Some(PathBuf::from(&argument[12..]));
            }
            _ if argument.starts_with("--interval-ms=") => {
                interval_ms = parse_interval(&argument[14..])?;
            }
            _ => return Err(format!("unknown argument {argument:?}")),
        }
    }

    let mode = match mode {
        Some(mode) => mode,
        None if effective_uid().unwrap_or(u32::MAX) == 0 => Mode::System,
        None => Mode::User,
    };

    Ok(Options {
        mode,
        cgroup_root,
        scan_root,
        interval: Duration::from_millis(interval_ms),
        once,
        dry_run,
        verbose,
        used_legacy_flag,
    })
}

fn set_mode(slot: &mut Option<Mode>, requested: Mode) -> Result<(), String> {
    if let Some(existing) = slot {
        if *existing != requested {
            return Err("--system and --user are mutually exclusive".to_owned());
        }
    }
    *slot = Some(requested);
    Ok(())
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_interval(raw: &str) -> Result<u64, String> {
    let value = raw
        .parse::<u64>()
        .map_err(|error| format!("invalid interval {raw:?}: {error}"))?;
    if value < 100 {
        return Err("--interval-ms must be at least 100".to_owned());
    }
    Ok(value)
}

fn print_help() {
    println!(
        r#"dmemcg-booster {version} (systemd-independent)

Usage: dmemcg-booster [OPTIONS]

Modes:
  --system              Manage the system hierarchy; stop at delegated subtrees
  --user                Manage the current writable delegated user subtree

Options:
  --once                Scan once instead of running as a daemon
  --interval-ms N       Poll interval in milliseconds (default: {interval})
  --cgroup-root PATH    cgroup v2 mount (default: {root})
  --scan-root PATH      Override the subtree to scan
  --dry-run             Report actions without writing to cgroupfs
  -v, --verbose         Print a summary after every scan
  -h, --help            Show this help
  -V, --version         Show the version

Environment: DMEMCG_CGROUP_ROOT, DMEMCG_SCAN_ROOT, DMEMCG_INTERVAL_MS
Legacy: --use-system-bus is accepted as an alias for --system."#,
        version = env!("CARGO_PKG_VERSION"),
        interval = DEFAULT_INTERVAL_MS,
        root = DEFAULT_CGROUP_ROOT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_relative_scan_paths_below_mount() {
        let mount = Path::new("/sys/fs/cgroup");
        let group = cgroup_from_cli_path(mount, Path::new("user/1000")).unwrap();
        assert_eq!(group.path(), Path::new("/sys/fs/cgroup/user/1000"));
    }

    #[test]
    fn maps_cgroup_style_absolute_paths_below_mount() {
        let mount = Path::new("/sys/fs/cgroup");
        let group = cgroup_from_cli_path(mount, Path::new("/user/1000")).unwrap();
        assert_eq!(group.path(), Path::new("/sys/fs/cgroup/user/1000"));
    }

    #[test]
    fn rejects_too_fast_polling() {
        assert!(parse_interval("99").is_err());
        assert_eq!(parse_interval("100").unwrap(), 100);
    }

    #[test]
    fn rejects_scan_root_parent_traversal() {
        let mount = Path::new("/sys/fs/cgroup");
        assert!(cgroup_from_cli_path(mount, Path::new("../etc")).is_err());
        assert!(cgroup_from_cli_path(mount, Path::new("/user/../etc")).is_err());
    }

    #[test]
    fn dry_run_scans_a_generic_cgroup_tree() {
        let tree = TestTree::new();
        tree.file("cgroup.controllers", "cpu dmem\n");
        tree.file("cgroup.subtree_control", "cpu\n");
        tree.file("cgroup.procs", "1\n");
        tree.file("dmem.capacity", "gpu0 1024\ngpu1 2048\n");

        tree.file("apps/cgroup.controllers", "cpu dmem\n");
        tree.file("apps/cgroup.subtree_control", "cpu\n");
        tree.file("apps/cgroup.procs", "");
        tree.file("apps/dmem.low", "gpu0 0\ngpu1 0\n");

        tree.file("apps/game/cgroup.controllers", "cpu dmem\n");
        tree.file("apps/game/cgroup.subtree_control", "");
        tree.file("apps/game/cgroup.procs", "42\n");
        tree.file("apps/game/dmem.low", "gpu0 0\ngpu1 0\n");

        let hierarchy_root = CGroup::hierarchy_root(tree.path().to_path_buf()).unwrap();
        let options = test_options(tree.path());
        let report = scan_hierarchy(&options, &hierarchy_root, &hierarchy_root).unwrap();

        assert_eq!(report.visited, 3);
        assert_eq!(report.controllers_enabled, 2);
        assert_eq!(report.limits_updated, 1);
        assert_eq!(report.delegated_subtrees, 0);
        assert_eq!(report.unsafe_cgroups, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn refuses_domain_controller_on_populated_internal_cgroup() {
        let tree = TestTree::new();
        tree.file("cgroup.controllers", "dmem\n");
        tree.file("cgroup.subtree_control", "dmem\n");
        tree.file("cgroup.procs", "1\n");
        tree.file("dmem.capacity", "gpu0 1024\n");

        tree.file("mixed/cgroup.controllers", "dmem\n");
        tree.file("mixed/cgroup.subtree_control", "");
        tree.file("mixed/cgroup.procs", "7\n");
        tree.file("mixed/dmem.low", "gpu0 0\n");

        tree.file("mixed/child/cgroup.controllers", "dmem\n");
        tree.file("mixed/child/cgroup.subtree_control", "");
        tree.file("mixed/child/cgroup.procs", "8\n");
        tree.file("mixed/child/dmem.low", "gpu0 0\n");

        let hierarchy_root = CGroup::hierarchy_root(tree.path().to_path_buf()).unwrap();
        let options = test_options(tree.path());
        let report = scan_hierarchy(&options, &hierarchy_root, &hierarchy_root).unwrap();

        assert_eq!(report.unsafe_cgroups, 1);
        assert_eq!(report.controllers_enabled, 0);
        assert_eq!(report.limits_updated, 1);
        assert_eq!(report.errors.len(), 1);
    }

    fn test_options(root: &Path) -> Options {
        Options {
            mode: Mode::System,
            cgroup_root: root.to_path_buf(),
            scan_root: None,
            interval: Duration::from_millis(DEFAULT_INTERVAL_MS),
            once: true,
            dry_run: true,
            verbose: false,
            used_legacy_flag: false,
        }
    }

    struct TestTree {
        path: PathBuf,
    }

    impl TestTree {
        fn new() -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};

            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "dmemcg-booster-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn file(&self, relative: &str, contents: &str) {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

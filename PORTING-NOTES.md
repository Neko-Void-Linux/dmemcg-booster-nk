# Porting notes

The original daemon used systemd D-Bus only to discover new unit cgroups. The actual controller activation and limit changes already happened through cgroupfs. This port replaces discovery with periodic tree scans and generalizes the policy:

- **System mode:** owns root-managed cgroups, enables `dmem` top-down, writes pass-through protection, and stops at a subtree whose `cgroup.subtree_control` is owned by a non-root user.
- **User mode:** starts from the highest writable ancestor of its own cgroup and manages that delegated subtree. It deliberately does not write `dmem.low` on the delegation root; the parent/system manager owns that file.
- **Leaves:** remain at their current `dmem.low` value. The foreground policy owner, such as Gamescope, controls those values.

The scanner refuses to enable a domain controller in a non-root cgroup containing local processes, honoring cgroup v2's no-internal-process rule. Races with short-lived cgroups are ignored and handled on the next scan.

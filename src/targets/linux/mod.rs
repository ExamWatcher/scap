use super::Target;

/// Always empty: Wayland offers no silent display enumeration (privacy by design),
/// so there is nothing to list without asking the user first.
///
/// The Linux flow is interactive — build one [`crate::capturer::Capturer`] with
/// `target: None` and the portal shows its screen picker (`SelectSources`,
/// `multiple: false`); the chosen screen arrives as a single PipeWire stream of
/// BGRA frames. A cancelled/denied picker is a build `Err`, never a panic, and
/// callers must not re-prompt in a hot loop (the picker waits on the user).
/// At most one capturer runs per process (the portal session is process-global).
pub fn get_all_targets() -> Vec<Target> {
    Vec::new()
}

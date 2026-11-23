//! The label of a tab kept awake: `"<keep_awake_icon> <label>"`, behind the
//! `"<sleep_icon> "` prefix a tab whose agents all sleep already carries.
//!
//! The icon in the label is the whole state: nothing else remembers which
//! tabs are kept awake, so it survives a Herdr restart and renaming the tab
//! without the icon lifts the protection.

/// The label without the sleep prefix, and whether it had one.
fn split_sleep_prefix<'a>(label: &'a str, sleep_icon: &str) -> (&'a str, &'a str) {
    let prefix = format!("{sleep_icon} ");
    if !sleep_icon.is_empty() && label.starts_with(&prefix) {
        label.split_at(prefix.len())
    } else {
        ("", label)
    }
}

/// Whether the label marks a tab kept awake. An empty icon disables the
/// feature.
pub fn keeps_awake(label: &str, sleep_icon: &str, keep_icon: &str) -> bool {
    if keep_icon.is_empty() {
        return false;
    }
    let (_, base) = split_sleep_prefix(label, sleep_icon);
    base.starts_with(&format!("{keep_icon} "))
}

/// The label with the keep-awake icon added, or removed when it is there.
pub fn toggled(label: &str, sleep_icon: &str, keep_icon: &str) -> String {
    let (sleep, base) = split_sleep_prefix(label, sleep_icon);
    let prefix = format!("{keep_icon} ");
    match base.strip_prefix(&prefix) {
        Some(rest) => format!("{sleep}{rest}"),
        None => format!("{sleep}{prefix}{base}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLEEP: &str = "\u{1f4a4}";
    const KEEP: &str = "\u{1f512}";

    #[test]
    fn the_icon_in_the_label_is_the_state() {
        assert!(keeps_awake("\u{1f512} build", SLEEP, KEEP));
        assert!(!keeps_awake("build", SLEEP, KEEP));
        assert!(!keeps_awake("build \u{1f512}", SLEEP, KEEP));
        assert!(!keeps_awake("\u{1f512} build", SLEEP, ""));
    }

    #[test]
    fn the_sleep_prefix_goes_first() {
        assert!(keeps_awake("\u{1f4a4} \u{1f512} build", SLEEP, KEEP));
        assert!(!keeps_awake("\u{1f4a4} build", SLEEP, KEEP));
    }

    #[test]
    fn toggling_adds_then_removes_the_icon() {
        assert_eq!(toggled("build", SLEEP, KEEP), "\u{1f512} build");
        assert_eq!(toggled("\u{1f512} build", SLEEP, KEEP), "build");
        assert_eq!(
            toggled("\u{1f4a4} build", SLEEP, KEEP),
            "\u{1f4a4} \u{1f512} build"
        );
        assert_eq!(
            toggled("\u{1f4a4} \u{1f512} build", SLEEP, KEEP),
            "\u{1f4a4} build"
        );
    }
}

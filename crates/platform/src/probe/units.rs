/// Characters of the random suffix of a snap's scope (`-` and a UUID).
const SNAP_SUFFIX: usize = 37;

/// The application (its `.desktop` identifier) a process runs in, from the
/// path of its cgroup: the nearest unit, from the end, that a desktop
/// started for an application. `None` for a system service or the session.
///
/// Desktops name these units `app[-<launcher>]-<id>-<random>.scope`,
/// `app[-<launcher>]-<id>[@<random>].service` or `app[-<launcher>]-<id>.slice`,
/// the `-` of the identifier escaped (`\x2d`); a snap's
/// `snap.<snap>.<app>-<uuid>.scope` is the `.desktop` `<snap>_<app>`.
#[must_use]
pub fn desktop_id(cgroup_path: &str) -> Option<String> {
    cgroup_path.rsplit('/').find_map(unit_desktop_id)
}

fn unit_desktop_id(unit: &str) -> Option<String> {
    if let Some(snap) = unit.strip_prefix("snap.") {
        return snap_desktop_id(snap);
    }
    let named = unit.strip_prefix("app-")?;
    let body = if let Some(scope) = named.strip_suffix(".scope") {
        scope.rsplit_once('-')?.0
    } else if let Some(service) = named.strip_suffix(".service") {
        service.split_once('@').map_or(service, |(body, _)| body)
    } else {
        named.strip_suffix(".slice")?
    };
    // Raw `-` separate the launcher from the identifier; the identifier's
    // own are escaped.
    let identifier = super::decode_hex_escapes(body.rsplit('-').next()?);
    usable(identifier)
}

/// `<snap>.<app>-<uuid>.scope` → `<snap>_<app>`.
fn snap_desktop_id(snap: &str) -> Option<String> {
    let body = snap.strip_suffix(".scope")?;
    let (name, application) = body.split_once('.')?;
    let cut = application.len().checked_sub(SNAP_SUFFIX)?;
    let (application, suffix) = application.split_at_checked(cut)?;
    let uuid = suffix.strip_prefix('-')?;
    let is_uuid = uuid
        .chars()
        .all(|character| character == '-' || character.is_ascii_hexdigit());
    (is_uuid && !name.is_empty() && !application.is_empty())
        .then(|| format!("{name}_{application}"))
        .and_then(usable)
}

/// An identifier kept only when it names a file in a folder (any user may
/// name a unit `app-..\x2f..\x2fx-1.scope`).
fn usable(identifier: String) -> Option<String> {
    super::names_a_file(&identifier).then_some(identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice";

    fn id(unit: &str) -> Option<String> {
        desktop_id(&format!("{APPS}/{unit}"))
    }

    #[test]
    fn a_scope_gives_its_application_with_or_without_a_launcher() {
        assert_eq!(
            id(r"app-gnome-google\x2dchrome-583420.scope").as_deref(),
            Some("google-chrome")
        );
        assert_eq!(
            id("app-com.google.Chrome-583420.scope").as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(
            id("app-com.anthropic.Claude-662712.scope").as_deref(),
            Some("com.anthropic.Claude")
        );
        assert_eq!(
            id("app-org.kde.konsole-a1b2c3.scope").as_deref(),
            Some("org.kde.konsole")
        );
    }

    #[test]
    fn an_escaped_dash_is_never_a_separator() {
        assert_eq!(
            id(r"app-gnome-org.gnome.Evolution\x2dalarm\x2dnotify-2345.scope").as_deref(),
            Some("org.gnome.Evolution-alarm-notify")
        );
    }

    #[test]
    fn a_service_and_a_slice_give_their_application() {
        assert_eq!(
            id("app-gnome-org.gnome.Settings@a1b2.service").as_deref(),
            Some("org.gnome.Settings")
        );
        assert_eq!(
            id("app-org.gnome.Nautilus.service").as_deref(),
            Some("org.gnome.Nautilus")
        );
        assert_eq!(
            id("app-org.gnome.Terminal.slice/vte-spawn-0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope")
                .as_deref(),
            Some("org.gnome.Terminal")
        );
        assert_eq!(
            id(r"app-gnome\x2dsession\x2dmanager.slice/gnome-session-manager@ubuntu.service")
                .as_deref(),
            Some("gnome-session-manager")
        );
    }

    #[test]
    fn a_snap_scope_gives_its_snap_desktop_file() {
        assert_eq!(
            id("snap.firefox.firefox-0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope").as_deref(),
            Some("firefox_firefox")
        );
        assert_eq!(
            id("snap.code.url-handler-0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope").as_deref(),
            Some("code_url-handler")
        );
        assert_eq!(id("snap.firefox.firefox-short.scope"), None);
    }

    #[test]
    fn the_nearest_unit_names_the_application() {
        assert_eq!(
            id("app-gnome-a-1.slice/app-gnome-b-2.scope").as_deref(),
            Some("b")
        );
        assert_eq!(id("app-.slice"), None);
        assert_eq!(id(r"app-..\x2f..\x2fetc\x2fx-1.scope"), None);
        assert_eq!(id(r"app-gnome-..-1.scope"), None);
        assert_eq!(
            id("snap.firefox.firefox-zzzzzzzz-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope"),
            None
        );
    }

    #[test]
    fn a_service_or_the_session_is_no_application() {
        for path in [
            "/system.slice/cups.service",
            "/user.slice/user-1000.slice/user@1000.service/session.slice/org.gnome.Shell@ubuntu.service",
            "/user.slice/user-1000.slice/session-1.scope",
            "/",
            "",
        ] {
            assert_eq!(desktop_id(path), None, "{path}");
        }
    }
}

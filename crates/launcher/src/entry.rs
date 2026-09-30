//! Desktop entries to launchable apps: visibility rules and `Exec` field-code stripping.
//!
//! Parsing is done by `freedesktop-desktop-entry`; everything here is pure (the only
//! environment it needs, the desktop names, locales and a "does this executable exist"
//! probe, is passed in) so it can be unit tested without touching the system.

use freedesktop_desktop_entry::DesktopEntry;

/// One launchable application, as kept in the in-memory index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// Desktop file id (`org.mozilla.firefox`, `kde-foo`), unique in the index.
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub keywords: Vec<String>,
    /// Icon name or absolute path, as written in the entry.
    pub icon: Option<String>,
    /// `Exec` split into arguments with every field code removed. Never empty.
    pub argv: Vec<String>,
    /// `Terminal=true`: run inside a terminal emulator.
    pub terminal: bool,
}

/// What entry parsing needs to know about the running session.
pub struct Env<'a> {
    /// Lowercased `XDG_CURRENT_DESKTOP` components (`aurora`, `sway`, ...).
    pub desktops: Vec<String>,
    /// Locale names for localized `Name`/`Comment` (`en_US`, `en`).
    pub locales: Vec<String>,
    /// True when an executable named by `TryExec` is present.
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Turns one parsed entry into an [`AppEntry`], or `None` when it must not be listed:
/// not `Type=Application`, `NoDisplay`, `Hidden`, excluded by `OnlyShowIn` / `NotShowIn`,
/// a missing `TryExec`, or no usable `Exec`.
pub fn app_from_desktop(id: &str, de: &DesktopEntry, env: &Env) -> Option<AppEntry> {
    if de.type_().is_some_and(|t| t != "Application") {
        return None;
    }
    if de.no_display() || de.hidden() {
        return None;
    }
    let matches = |list: &[&str]| {
        list.iter()
            .any(|d| env.desktops.iter().any(|e| e.eq_ignore_ascii_case(d)))
    };
    if let Some(only) = de.only_show_in()
        && !only.is_empty()
        && !matches(&only)
    {
        return None;
    }
    if let Some(not) = de.not_show_in()
        && matches(&not)
    {
        return None;
    }
    if let Some(probe) = de.try_exec()
        && !probe.trim().is_empty()
        && !(env.exists)(probe.trim())
    {
        return None;
    }
    let name = de.name(&env.locales)?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let icon = de.icon().map(str::trim).filter(|s| !s.is_empty());
    let argv = strip_exec(de.exec()?, &name, icon, de.path.to_str())?;
    let non_empty = |s: std::borrow::Cow<'_, str>| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    Some(AppEntry {
        id: id.to_string(),
        generic_name: de.generic_name(&env.locales).and_then(non_empty),
        comment: de.comment(&env.locales).and_then(non_empty),
        keywords: de
            .keywords(&env.locales)
            .map(|k| {
                k.iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        icon: icon.map(str::to_string),
        argv,
        terminal: de.terminal(),
        name,
    })
}

/// Splits an `Exec` value into arguments (double quotes group, backslash escapes inside
/// quotes) and removes every field code: `%f %F %u %U %d %D %n %N %v %m` vanish, `%i`
/// becomes `--icon <icon>` (or vanishes without an icon), `%c` is the app name, `%k` the
/// entry's path, `%%` a literal percent. An argument that was nothing but a code is
/// dropped; one that merely contains a code keeps its other text. `None` when nothing
/// executable is left.
pub fn strip_exec(
    exec: &str,
    name: &str,
    icon: Option<&str>,
    path: Option<&str>,
) -> Option<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();
    for raw in split_exec(exec) {
        let (text, quoted) = raw;
        let mut out = String::new();
        let mut had_code = false;
        let mut extra: Option<String> = None;
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('i') => {
                    had_code = true;
                    if let Some(icon) = icon {
                        // `--icon` is its own argument, the name follows as another.
                        extra = Some(icon.to_string());
                        out.push_str("--icon");
                    }
                }
                Some('c') => {
                    had_code = true;
                    out.push_str(name);
                }
                Some('k') => {
                    had_code = true;
                    out.push_str(path.unwrap_or(""));
                }
                Some(_) => had_code = true,
                None => out.push('%'),
            }
        }
        if out.is_empty() && (had_code || !quoted) {
            continue;
        }
        argv.push(out);
        if let Some(icon) = extra {
            argv.push(icon);
        }
    }
    (!argv.is_empty() && !argv[0].is_empty()).then_some(argv)
}

/// Splits on unquoted whitespace. Returns each argument and whether it had quotes.
fn split_exec(exec: &str) -> Vec<(String, bool)> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut quoted = false;
    let mut started = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => in_quotes = false,
                '\\' => match chars.next() {
                    Some(n @ ('"' | '`' | '$' | '\\')) => cur.push(n),
                    Some(n) => {
                        cur.push('\\');
                        cur.push(n);
                    }
                    None => cur.push('\\'),
                },
                _ => cur.push(c),
            }
        } else if c == '"' {
            in_quotes = true;
            quoted = true;
            started = true;
        } else if c.is_whitespace() {
            if started {
                args.push((std::mem::take(&mut cur), quoted));
                quoted = false;
                started = false;
            }
        } else {
            cur.push(c);
            started = true;
        }
    }
    if started {
        args.push((cur, quoted));
    }
    args
}

/// Locale names for `Name[xx]` lookups from `$LC_ALL`, `$LC_MESSAGES`, `$LANG`:
/// `de_DE.UTF-8@euro` yields `de_DE` then `de`.
pub fn locales_from(vars: &[Option<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in vars.iter().flatten() {
        let base = v.split(['.', '@']).next().unwrap_or("");
        if base.is_empty() || base == "C" || base == "POSIX" {
            continue;
        }
        for l in [base, base.split('_').next().unwrap_or(base)] {
            if !out.iter().any(|o| o == l) {
                out.push(l.to_string());
            }
        }
    }
    out
}

/// Lowercased desktop names from `XDG_CURRENT_DESKTOP` (`Aurora:GNOME` -> `aurora`, `gnome`).
pub fn desktops_from(current: Option<&str>) -> Vec<String> {
    current
        .unwrap_or("")
        .split(':')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn strip(exec: &str) -> Option<Vec<String>> {
        strip_exec(exec, "App Name", Some("app-icon"), Some("/x/app.desktop"))
    }

    #[test]
    fn plain_and_quoted_arguments() {
        assert_eq!(strip("firefox"), Some(s(&["firefox"])));
        assert_eq!(
            strip("env FOO=1 app --flag"),
            Some(s(&["env", "FOO=1", "app", "--flag"]))
        );
        assert_eq!(
            strip(r#""/opt/My App/bin/app" --name "two words""#),
            Some(s(&["/opt/My App/bin/app", "--name", "two words"]))
        );
        assert_eq!(
            strip(r#"sh -c "echo \"hi\" \$HOME""#),
            Some(s(&["sh", "-c", r#"echo "hi" $HOME"#]))
        );
    }

    #[test]
    fn field_codes_are_removed() {
        assert_eq!(strip("firefox %u"), Some(s(&["firefox"])));
        assert_eq!(strip("gimp %F %U"), Some(s(&["gimp"])));
        assert_eq!(
            strip("app --open=%f --new-window"),
            Some(s(&["app", "--open=", "--new-window"]))
        );
        for code in ["f", "F", "u", "U", "d", "D", "n", "N", "v", "m"] {
            assert_eq!(strip(&format!("app %{code}")), Some(s(&["app"])), "%{code}");
        }
    }

    #[test]
    fn icon_name_path_and_percent_codes() {
        assert_eq!(strip("app %i"), Some(s(&["app", "--icon", "app-icon"])));
        assert_eq!(strip_exec("app %i", "N", None, None), Some(s(&["app"])));
        assert_eq!(
            strip("app --title=%c"),
            Some(s(&["app", "--title=App Name"]))
        );
        assert_eq!(strip("app %k"), Some(s(&["app", "/x/app.desktop"])));
        assert_eq!(strip("printf 100%%"), Some(s(&["printf", "100%"])));
    }

    #[test]
    fn empty_quoted_argument_is_kept_and_empty_exec_is_none() {
        assert_eq!(strip(r#"app "" x"#), Some(s(&["app", "", "x"])));
        assert_eq!(strip(""), None);
        assert_eq!(strip("   "), None);
        assert_eq!(strip("%u"), None);
    }

    #[test]
    fn locale_and_desktop_lists() {
        assert_eq!(
            locales_from(&[None, Some("de_DE.UTF-8@euro".into()), Some("C".into())]),
            s(&["de_DE", "de"])
        );
        assert_eq!(
            locales_from(&[Some("en".into()), Some("en_US.UTF-8".into())]),
            s(&["en", "en_US"])
        );
        assert!(locales_from(&[Some("POSIX".into())]).is_empty());
        assert_eq!(desktops_from(Some("Aurora:GNOME")), s(&["aurora", "gnome"]));
        assert!(desktops_from(None).is_empty());
    }

    fn parse(text: &str, desktops: &[&str], exists: &dyn Fn(&str) -> bool) -> Option<AppEntry> {
        let de = DesktopEntry::from_str(
            PathBuf::from("/usr/share/applications/test.desktop"),
            text,
            None::<&[&str]>,
        )
        .expect("valid entry");
        let env = Env {
            desktops: desktops.iter().map(|d| d.to_string()).collect(),
            locales: Vec::new(),
            exists,
        };
        app_from_desktop("test", &de, &env)
    }

    const BASIC: &str = "[Desktop Entry]\nType=Application\nName=Test App\nGenericName=Tester\n\
Comment=Does tests\nKeywords=check;verify;\nIcon=test-icon\nExec=test-app %U\nTerminal=true\n";

    #[test]
    fn a_basic_entry_becomes_an_app() {
        let app = parse(BASIC, &[], &|_| true).expect("listed");
        assert_eq!(app.id, "test");
        assert_eq!(app.name, "Test App");
        assert_eq!(app.generic_name.as_deref(), Some("Tester"));
        assert_eq!(app.comment.as_deref(), Some("Does tests"));
        assert_eq!(app.keywords, s(&["check", "verify"]));
        assert_eq!(app.icon.as_deref(), Some("test-icon"));
        assert_eq!(app.argv, s(&["test-app"]));
        assert!(app.terminal);
    }

    #[test]
    fn hidden_and_nodisplay_and_non_applications_are_skipped() {
        let with = |extra: &str| format!("{BASIC}{extra}\n");
        assert!(parse(&with("NoDisplay=true"), &[], &|_| true).is_none());
        assert!(parse(&with("Hidden=true"), &[], &|_| true).is_none());
        assert!(parse(&with("NoDisplay=false"), &[], &|_| true).is_some());
        let link = BASIC.replace("Type=Application", "Type=Link");
        assert!(parse(&link, &[], &|_| true).is_none());
        let no_exec = "[Desktop Entry]\nType=Application\nName=X\n";
        assert!(parse(no_exec, &[], &|_| true).is_none());
    }

    #[test]
    fn only_show_in_and_not_show_in_follow_the_current_desktop() {
        let only = format!("{BASIC}OnlyShowIn=GNOME;KDE;\n");
        assert!(parse(&only, &["aurora"], &|_| true).is_none());
        assert!(parse(&only, &["aurora", "gnome"], &|_| true).is_some());
        let not = format!("{BASIC}NotShowIn=Aurora;\n");
        assert!(parse(&not, &["aurora"], &|_| true).is_none());
        assert!(parse(&not, &["sway"], &|_| true).is_some());
    }

    #[test]
    fn try_exec_must_exist() {
        let t = format!("{BASIC}TryExec=/nope/app\n");
        assert!(parse(&t, &[], &|p| p != "/nope/app").is_none());
        assert!(parse(&t, &[], &|_| true).is_some());
    }
}

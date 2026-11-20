//! `windows`, `launch` and `close` tools.

use std::collections::HashSet;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::thread::sleep;

use niri_ipc::{Action, Request, Response, Window};
use serde_json::Value;

use super::{Server, Tool, ToolOutcome};

/// How long `launch` waits for the new window, and `close` for the window to go away.
pub const LAUNCH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Prefs for a fresh Firefox profile: no first-run or privacy-notice pages, no telemetry or
/// studies. Nothing that weakens security (safe browsing and updates stay on).
const FIREFOX_PREFS: &str = r#"// Written by `niri mcp` for an agent browser.
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("datareporting.policy.firstRunURL", "");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("toolkit.telemetry.unified", false);
user_pref("app.normandy.enabled", false);
user_pref("app.shield.optoutstudies.enabled", false);
user_pref("browser.newtabpage.activity-stream.feeds.section.topstories", false);
"#;

/// Browser flags that would swap the agent profile or app id for the owner's (Chromium keeps
/// the last of a repeated switch; Firefox flags ignore case).
const ISOLATION_FLAGS: [&str; 8] = [
    "user-data-dir",
    "class",
    "profile",
    "P",
    "name",
    "no-remote",
    "new-instance",
    "ProfileManager",
];

/// Poll interval while waiting.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// The sweep on `launch` leaves younger profile dirs alone: their browser may be starting.
const SWEEP_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Apps `launch` can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchApp {
    Foot,
    Kitty,
    Chromium,
    Firefox,
}

impl LaunchApp {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "foot" => Self::Foot,
            "kitty" => Self::Kitty,
            "chromium" => Self::Chromium,
            "firefox" => Self::Firefox,
            _ => return None,
        })
    }

    pub fn app_id(self) -> &'static str {
        match self {
            Self::Foot => "agent-foot",
            Self::Kitty => "agent-kitty",
            Self::Chromium => "agent-chrome",
            Self::Firefox => "agent-firefox",
        }
    }

    fn needs_profile(self) -> bool {
        matches!(self, Self::Chromium | Self::Firefox)
    }
}

/// Command line for niri to spawn.
pub fn launch_argv(
    app: LaunchApp,
    args: &[String],
    url: Option<&str>,
    profile: Option<&Path>,
) -> Vec<String> {
    let mut v: Vec<String> = [
        "env",
        "-u",
        "XDG_ACTIVATION_TOKEN",
        "-u",
        "DESKTOP_STARTUP_ID",
    ]
    .map(String::from)
    .into();
    let profile = profile.map(|p| p.display().to_string());
    match app {
        LaunchApp::Foot => v.extend(["foot".into(), "--app-id=agent-foot".into()]),
        LaunchApp::Kitty => v.extend(["kitty".into(), "--class".into(), "agent-kitty".into()]),
        LaunchApp::Chromium => {
            v.extend(["chromium".into(), "--class=agent-chrome".into()]);
            v.extend(profile.map(|p| format!("--user-data-dir={p}")));
            v.extend(
                [
                    "--mute-audio",
                    "--force-renderer-accessibility",
                    // No first-run pages, no sync. Background networking stays on: it
                    // keeps Safe Browsing lists fresh.
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-sync",
                ]
                .map(String::from),
            );
        }
        LaunchApp::Firefox => {
            v.extend(["firefox".into(), "--no-remote".into()]);
            if let Some(p) = profile {
                v.extend(["--profile".into(), p]);
            }
            v.extend(["--name".into(), "agent-firefox".into()]);
        }
    }
    v.extend_from_slice(args);
    if app.needs_profile() {
        v.extend(url.map(String::from));
    }
    v
}

fn list_windows(s: &mut Server) -> Result<Vec<Window>, String> {
    match (s.ipc)(Request::Windows) {
        Ok(Ok(Response::Windows(w))) => Ok(w),
        Ok(Ok(other)) => Err(format!("unexpected_reply: {other:?}")),
        Ok(Err(e)) => Err(format!("niri_refused: {e}")),
        Err(e) => Err(format!("niri_unreachable: cannot reach niri: {e}")),
    }
}

fn do_action(s: &mut Server, action: Action) -> Result<(), String> {
    match (s.ipc)(Request::Action(action)) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("niri_refused: {e}")),
        Err(e) => Err(format!("niri_unreachable: cannot reach niri: {e}")),
    }
}

/// One-line, control-free, capped text (titles are untrusted).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect()
}

fn windows_tool(s: &mut Server, _args: &Value) -> ToolOutcome {
    let wins = match list_windows(s) {
        Ok(w) => w,
        Err(e) => return ToolOutcome::error(e),
    };
    let mut out = String::from("id\tapp_id\ttitle\tworkspace\towner_focused\tagent_app\n");
    for w in wins {
        let app = w.app_id.as_deref().unwrap_or("");
        out += &format!(
            "{}\t{}\t{:?}\t{}\t{}\t{}\n",
            w.id,
            clean(app),
            clean(w.title.as_deref().unwrap_or("")),
            w.workspace_id.map_or("-".into(), |i| i.to_string()),
            w.is_focused,
            app.starts_with("agent-"),
        );
    }
    ToolOutcome::text(out)
}

/// `$XDG_RUNTIME_DIR/niri-mcp`, where profile dirs live.
pub fn profile_base() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("niri-mcp"))
}

fn create_profile(base: &Path) -> std::io::Result<PathBuf> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700).create(base)?;
    let dir = base.join(format!("profile-{:016x}", fastrand::u64(..)));
    b.recursive(false).mode(0o700).create(&dir)?;
    Ok(dir)
}

/// Removes a profile dir this server created. Never follows symlinks, never crosses a
/// filesystem boundary: refuses the whole removal if anything looks off.
fn remove_profile(base: &Path, dir: &Path) -> Result<(), String> {
    let name_ok = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("profile-"));
    if dir.parent() != Some(base) || !name_ok {
        return Err("not under the niri-mcp runtime dir".into());
    }
    if dir.symlink_metadata().is_ok_and(|m| m.is_dir()) && in_use(dir) {
        return Err("a browser still runs in it; a later launch removes it".into());
    }
    remove_tree(dir)
}

/// True while a browser holds the profile. Chromium's `SingletonLock` and Firefox's `lock` are
/// symlinks to `host-pid` and `ip:+pid`; a lock whose pid is gone is stale.
fn in_use(profile: &Path) -> bool {
    ["SingletonLock", "lock"].iter().any(|name| {
        std::fs::read_link(profile.join(name)).is_ok_and(|target| {
            let pid = target
                .to_str()
                .and_then(|t| t.rsplit([':', '-']).next())
                .and_then(|p| p.parse::<u32>().ok());
            pid.is_none_or(|pid| Path::new("/proc").join(pid.to_string()).exists())
        })
    })
}

/// Removes old profile dirs whose browser is gone: windows the owner closed, crashes, earlier
/// sessions. Anything [`remove_profile`] refuses stays.
fn sweep_profiles(base: &Path) {
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|a| a >= SWEEP_AGE));
        if old {
            let _ = remove_profile(base, &e.path());
        }
    }
}

/// Removes `dir` and everything in it without following symlinks or crossing a filesystem
/// boundary: refuses the whole removal if anything looks off.
fn remove_tree(dir: &Path) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        // Already gone (a sweep got it first).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_dir() {
        return Err("not a plain directory".into());
    }
    // Pre-order list; every entry's st_dev must equal the root's.
    let mut all = vec![(dir.to_path_buf(), true)];
    let mut i = 0;
    while i < all.len() {
        let (p, is_dir) = all[i].clone();
        i += 1;
        if !is_dir {
            continue;
        }
        for e in std::fs::read_dir(&p).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let m = std::fs::symlink_metadata(e.path()).map_err(|e| e.to_string())?;
            if m.dev() != meta.dev() {
                return Err(format!("{} is on another filesystem", e.path().display()));
            }
            all.push((e.path(), m.is_dir()));
        }
    }
    for (p, is_dir) in all.iter().rev() {
        let r = if *is_dir {
            std::fs::remove_dir(p)
        } else {
            std::fs::remove_file(p)
        };
        r.map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

fn launch_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let Some(app) = args
        .get("app")
        .and_then(Value::as_str)
        .and_then(LaunchApp::parse)
    else {
        return ToolOutcome::error("bad_app: app must be foot, kitty, chromium or firefox");
    };
    let extra: Vec<String> = args
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let url = args.get("url").and_then(Value::as_str);
    if app.needs_profile() {
        let overrides = |a: &str| {
            let name = a
                .trim_start_matches('-')
                .split('=')
                .next()
                .unwrap_or_default();
            ISOLATION_FLAGS.iter().any(|f| f.eq_ignore_ascii_case(name))
        };
        if let Some(a) = extra.iter().find(|a| overrides(a)) {
            return ToolOutcome::error(format!(
                "bad_args: {:?} would replace the agent browser's own profile or app id",
                clean(a)
            ));
        }
        if url.is_some_and(|u| u.starts_with('-')) {
            return ToolOutcome::error("bad_args: url must not start with '-'");
        }
    }
    if let Some(b) = &s.profile_base {
        sweep_profiles(b);
    }

    let old: HashSet<u64> = match list_windows(s) {
        Ok(w) => w.iter().map(|w| w.id).collect(),
        Err(e) => return ToolOutcome::error(e),
    };
    let base = s.profile_base.clone();
    let profile = if app.needs_profile() {
        match base.as_deref().map(create_profile) {
            Some(Ok(p)) => Some(p),
            Some(Err(e)) => return ToolOutcome::error(format!("cannot create profile dir: {e}")),
            None => return ToolOutcome::error("XDG_RUNTIME_DIR is not set"),
        }
    } else {
        None
    };
    if let (LaunchApp::Firefox, Some(p)) = (app, &profile) {
        if let Err(e) = std::fs::write(p.join("user.js"), FIREFOX_PREFS) {
            let n = discard_profile(s, Some(p));
            return ToolOutcome::error(format!("cannot write Firefox prefs: {e}.{n}"));
        }
    }
    let argv = launch_argv(app, &extra, url, profile.as_deref());
    if let Err(e) = do_action(s, Action::Spawn { command: argv }) {
        let note = match (&base, &profile) {
            (Some(b), Some(p)) => remove_profile(b, p)
                .err()
                .map(|e| format!(" (profile dir left: {e})")),
            _ => None,
        };
        return ToolOutcome::error(format!("{e}{}", note.unwrap_or_default()));
    }

    let deadline = std::time::Instant::now() + s.launch_wait;
    loop {
        sleep(s.poll);
        let wins = match list_windows(s) {
            Ok(w) => w,
            Err(e) => {
                let n = discard_profile(s, profile.as_deref());
                return ToolOutcome::error(format!("{e} (launch failed).{n}"));
            }
        };
        if let Some(w) = wins
            .iter()
            .find(|w| !old.contains(&w.id) && w.app_id.as_deref() == Some(app.app_id()))
        {
            s.launched.insert(w.id, profile);
            return ToolOutcome::text(format!("window {} ({})", w.id, app.app_id()));
        }
        if std::time::Instant::now() >= deadline {
            let n = discard_profile(s, profile.as_deref());
            return ToolOutcome::error(format!(
                "launch_timeout: launch failed, no new {} window appeared.{n}",
                app.app_id()
            ));
        }
    }
}

/// Removes a profile dir that never got a window; returns a note for the tool text.
fn discard_profile(s: &Server, profile: Option<&Path>) -> String {
    match (profile, s.profile_base.as_deref()) {
        (Some(p), Some(b)) => match remove_profile(b, p) {
            Ok(()) => " The profile dir was removed.".into(),
            Err(e) => format!(" Profile dir {} left in place: {e}.", p.display()),
        },
        (Some(p), None) => format!(" Profile dir {} left in place.", p.display()),
        _ => String::new(),
    }
}

fn close_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let Some(id) = args.get("window").and_then(Value::as_u64) else {
        return ToolOutcome::error("bad_window: window must be a window id");
    };
    if !s.launched.contains_key(&id) {
        return ToolOutcome::error(
            "not_launched_here: only windows launched by this server can be closed",
        );
    }
    match list_windows(s) {
        Ok(w) if w.iter().any(|w| w.id == id && w.is_focused) => {
            return ToolOutcome::error("human_focused: the owner is using this window");
        }
        Ok(_) => {}
        Err(e) => return ToolOutcome::error(e),
    }
    if let Err(e) = do_action(s, Action::CloseWindow { id: Some(id) }) {
        // The owner may have closed it by hand already.
        match list_windows(s) {
            Ok(w) if !w.iter().any(|w| w.id == id) => {}
            _ => return ToolOutcome::error(e),
        }
    }
    let deadline = std::time::Instant::now() + s.launch_wait;
    loop {
        sleep(s.poll);
        let wins = match list_windows(s) {
            Ok(w) => w,
            Err(e) => return ToolOutcome::error(e),
        };
        if !wins.iter().any(|w| w.id == id) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return ToolOutcome::text(format!(
                "Close requested for window {id}, but it is still open (it may be asking to save). Profile dir kept."
            ));
        }
    }
    let profile = s.launched.remove(&id).flatten();
    // The browser outlives its window for a moment: wait for it to release the profile.
    if let Some(p) = &profile {
        let deadline = std::time::Instant::now() + s.launch_wait;
        while in_use(p) && std::time::Instant::now() < deadline {
            sleep(s.poll);
        }
    }
    let note = discard_profile(s, profile.as_deref());
    ToolOutcome::text(format!("window {id} closed.{note}"))
}

pub const TOOLS: [Tool; 3] = [
    Tool {
        name: "windows",
        description: "List the windows on the owner's desktop. Use it to find window ids and to see which are agent-* apps you may drive.",
        schema: r#"{"type":"object","properties":{}}"#,
        handler: windows_tool,
    },
    Tool {
        name: "launch",
        description: "Start foot, kitty, chromium or firefox as an agent-* window and return its window id. Use it when you need a terminal or browser of your own to drive.",
        schema: r#"{"type":"object","properties":{"app":{"type":"string","enum":["foot","kitty","chromium","firefox"]},"args":{"type":"array","items":{"type":"string"},"description":"extra command-line arguments"},"url":{"type":"string","description":"page to open (browsers only)"}},"required":["app"]}"#,
        handler: launch_tool,
    },
    Tool {
        name: "close",
        description: "Close a window you launched with `launch`. Use it when you are done with that window; other windows are refused.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer","description":"window id"}},"required":["window"]}"#,
        handler: close_tool,
    },
];

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::mcp::{Content, Ipc};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn win(id: u64, app: &str, focused: bool) -> Window {
        serde_json::from_value(json!({
            "id": id, "title": "t\nx", "app_id": app, "pid": 1, "workspace_id": 1,
            "is_focused": focused, "is_floating": false, "is_urgent": false,
            "layout": {"pos_in_scrolling_layout": null, "tile_size": [1.0, 1.0],
                "window_size": [1, 1], "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0]},
            "focus_timestamp": null,
        }))
        .unwrap()
    }

    type Log = Rc<RefCell<Vec<Request>>>;

    /// Windows replies come from `wins(call_index_of_Windows)`.
    fn server(wins: impl Fn(usize) -> Vec<Window> + 'static) -> (Server, Log) {
        let log: Log = Rc::default();
        let l = log.clone();
        let mut n = 0;
        let ipc: Ipc = Box::new(move |r| {
            let reply = if matches!(r, Request::Windows) {
                n += 1;
                Response::Windows(wins(n - 1))
            } else {
                Response::Handled
            };
            l.borrow_mut().push(r);
            Ok(Ok(reply))
        });
        let mut srv = Server::new(ipc);
        srv.launch_wait = Duration::from_millis(50);
        srv.poll = Duration::from_millis(1);
        (srv, log)
    }

    fn text(o: &ToolOutcome) -> &str {
        match &o.content[0] {
            Content::Text(t) => t,
            Content::Image(_) => panic!(),
        }
    }

    #[test]
    fn launch_argv_unsets_activation_token() {
        let v = launch_argv(LaunchApp::Foot, &s(&["-e", "sh"]), None, None);
        assert_eq!(
            v[..5],
            s(&[
                "env",
                "-u",
                "XDG_ACTIVATION_TOKEN",
                "-u",
                "DESKTOP_STARTUP_ID"
            ])[..]
        );
        assert_eq!(v[5..], s(&["foot", "--app-id=agent-foot", "-e", "sh"])[..]);
    }

    #[test]
    fn launch_argv_chromium_has_class_profile_and_a11y_flag() {
        let v = launch_argv(
            LaunchApp::Chromium,
            &s(&["--x"]),
            Some("http://a"),
            Some(Path::new("/p")),
        );
        assert_eq!(
            v[5..],
            s(&[
                "chromium",
                "--class=agent-chrome",
                "--user-data-dir=/p",
                "--mute-audio",
                "--force-renderer-accessibility",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--x",
                "http://a"
            ])[..]
        );
    }

    #[test]
    fn launch_argv_firefox_has_no_remote_and_profile() {
        let v = launch_argv(
            LaunchApp::Firefox,
            &[],
            Some("http://a"),
            Some(Path::new("/p")),
        );
        assert_eq!(
            v[5..],
            s(&[
                "firefox",
                "--no-remote",
                "--profile",
                "/p",
                "--name",
                "agent-firefox",
                "http://a"
            ])[..]
        );
        let k = launch_argv(LaunchApp::Kitty, &[], Some("http://ignored"), None);
        assert_eq!(k[5..], s(&["kitty", "--class", "agent-kitty"])[..]);
    }

    #[test]
    fn launch_returns_new_window_id() {
        // Old window 1 already has the app id; the new one is 2.
        let (mut srv, log) = server(|n| {
            let mut w = vec![win(1, "agent-foot", false)];
            if n >= 2 {
                w.push(win(2, "agent-foot", false));
            }
            w
        });
        let o = launch_tool(&mut srv, &json!({"app": "foot"}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(text(&o).contains("window 2"));
        assert!(srv.launched.contains_key(&2));
        assert!(log
            .borrow()
            .iter()
            .any(|r| matches!(r, Request::Action(Action::Spawn { .. }))));
    }

    #[test]
    fn launch_times_out_with_error() {
        let (mut srv, _) = server(|_| vec![win(1, "agent-foot", false)]);
        let o = launch_tool(&mut srv, &json!({"app": "foot"}));
        assert!(o.is_error);
        assert!(text(&o).starts_with("launch_timeout"));
        assert!(srv.launched.is_empty());
    }

    #[test]
    fn launch_refuses_isolation_overrides() {
        for (app, a) in [
            (
                "chromium",
                json!({"args": ["--user-data-dir=/home/u/.config/chromium"]}),
            ),
            (
                "chromium",
                json!({"args": ["--x", "-class", "agent-chrome"]}),
            ),
            (
                "chromium",
                json!({"args": ["--profile-directory=Default", "--User-Data-Dir=/x"]}),
            ),
            (
                "chromium",
                json!({"url": "--user-data-dir=/home/u/.config/chromium"}),
            ),
            ("firefox", json!({"args": ["-P", "default"]})),
            ("firefox", json!({"args": ["-p"]})),
            (
                "firefox",
                json!({"args": ["--profile", "/home/u/.mozilla/x"]}),
            ),
            ("firefox", json!({"args": ["--name=firefox"]})),
            ("firefox", json!({"args": ["-new-instance"]})),
            ("firefox", json!({"args": ["--no-remote"]})),
            ("firefox", json!({"args": ["-ProfileManager"]})),
            ("firefox", json!({"url": "-P"})),
        ] {
            let (mut srv, log) = server(|_| vec![]);
            let mut a = a;
            a["app"] = json!(app);
            let o = launch_tool(&mut srv, &a);
            assert!(o.is_error && text(&o).starts_with("bad_args: "), "{a}");
            assert!(log.borrow().is_empty(), "{a}");
        }
        // Harmless args and a plain url still launch.
        let (mut srv, _) = server(|n| {
            if n >= 1 {
                vec![win(2, "agent-chrome", false)]
            } else {
                vec![]
            }
        });
        let base = scratch("iso");
        srv.profile_base = Some(base.to_path_buf());
        let o = launch_tool(
            &mut srv,
            &json!({"app": "chromium", "args": ["--incognito"], "url": "https://a/-b"}),
        );
        assert!(!o.is_error, "{}", text(&o));
    }

    #[test]
    fn close_refuses_window_not_launched_here() {
        let (mut srv, log) = server(|_| vec![win(7, "agent-foot", false)]);
        let o = close_tool(&mut srv, &json!({"window": 7}));
        assert!(o.is_error);
        assert_eq!(
            text(&o),
            "not_launched_here: only windows launched by this server can be closed"
        );
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn close_refuses_window_owner_is_using() {
        let (mut srv, log) = server(|_| vec![win(7, "agent-foot", true)]);
        srv.launched.insert(7, None);
        let o = close_tool(&mut srv, &json!({"window": 7}));
        assert!(o.is_error);
        assert_eq!(text(&o), "human_focused: the owner is using this window");
        assert!(!log.borrow().iter().any(|r| matches!(r, Request::Action(_))));
        assert!(srv.launched.contains_key(&7));
    }

    #[test]
    fn close_launched_window_sends_close_and_forgets_it() {
        let (mut srv, log) = server(|n| {
            if n == 0 {
                vec![win(7, "agent-foot", false)]
            } else {
                vec![]
            }
        });
        srv.launched.insert(7, None);
        let o = close_tool(&mut srv, &json!({"window": 7}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(log
            .borrow()
            .iter()
            .any(|r| matches!(r, Request::Action(Action::CloseWindow { id: Some(7) }))));
        assert!(srv.launched.is_empty());
    }

    #[test]
    fn niri_errors_have_codes() {
        let replies: [fn() -> std::io::Result<niri_ipc::Reply>; 3] = [
            || Err(std::io::Error::other("no socket")),
            || Ok(Err("nope".into())),
            || Ok(Ok(Response::Handled)),
        ];
        for (reply, code) in replies.into_iter().zip([
            "niri_unreachable: ",
            "niri_refused: nope",
            "unexpected_reply: ",
        ]) {
            let mut srv = Server::new(Box::new(move |_| reply()));
            let o = windows_tool(&mut srv, &json!({}));
            assert!(o.is_error && text(&o).starts_with(code), "{}", text(&o));
            srv.launched.insert(7, None);
            let o = close_tool(&mut srv, &json!({"window": 7}));
            assert!(o.is_error && text(&o).starts_with(code), "{}", text(&o));
        }
    }

    #[test]
    fn windows_marks_agent_apps() {
        let (mut srv, _) = server(|_| vec![win(1, "agent-foot", true), win(2, "firefox", false)]);
        let o = windows_tool(&mut srv, &json!({}));
        let t = text(&o);
        let rows: Vec<&str> = t.lines().skip(1).collect();
        assert!(rows[0].starts_with("1\tagent-foot\t") && rows[0].ends_with("true\ttrue"));
        assert!(rows[1].starts_with("2\tfirefox\t") && rows[1].ends_with("false\tfalse"));
        assert_eq!(rows.len(), 2, "newline in title must not add a row");
    }

    /// A test dir, removed on drop with the same safe walk as profile dirs.
    struct Scratch(PathBuf);
    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = remove_tree(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let d = std::env::temp_dir().join(format!("niri-mcp-apps-{}-{tag}", std::process::id()));
        let _ = remove_tree(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }

    #[test]
    fn scratch_is_removed_on_drop() {
        let outside = scratch("drop-out");
        std::fs::write(outside.join("keep"), "x").unwrap();
        let d = scratch("drop");
        std::fs::create_dir_all(d.join("profile-1/sub")).unwrap();
        std::fs::write(d.join("profile-1/sub/f"), "x").unwrap();
        std::os::unix::fs::symlink(&*outside, d.join("link")).unwrap();
        let p = d.to_path_buf();
        drop(d);
        assert!(!p.exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn remove_profile_deletes_tree_without_following_symlinks() {
        let base = scratch("ok");
        let outside = scratch("outside");
        std::fs::write(outside.join("keep"), "x").unwrap();
        let p = create_profile(&base).unwrap();
        std::fs::create_dir(p.join("sub")).unwrap();
        std::fs::write(p.join("sub/f"), "x").unwrap();
        std::os::unix::fs::symlink(&*outside, p.join("link")).unwrap();
        remove_profile(&base, &p).unwrap();
        assert!(!p.exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn remove_profile_refuses_foreign_paths() {
        let base = scratch("ref");
        let outside = scratch("ref-out");
        assert!(remove_profile(&base, &outside).is_err());
        let notprofile = base.join("other");
        std::fs::create_dir(&notprofile).unwrap();
        assert!(remove_profile(&base, &notprofile).is_err());
        let target = base.join("profile-link");
        std::os::unix::fs::symlink(&*outside, &target).unwrap();
        assert!(remove_profile(&base, &target).is_err());
        assert!(outside.exists());
    }

    /// Like `server`, with a real profile base under a scratch dir.
    fn server_with_base(
        tag: &str,
        wins: impl Fn(usize) -> Vec<Window> + 'static,
        fail_close: bool,
    ) -> (Server, Scratch) {
        let base = scratch(tag);
        let n = Rc::new(RefCell::new(0usize));
        let ipc: Ipc = Box::new(move |r| {
            Ok(match r {
                Request::Windows => {
                    *n.borrow_mut() += 1;
                    Ok(Response::Windows(wins(*n.borrow() - 1)))
                }
                Request::Action(Action::CloseWindow { .. }) if fail_close => {
                    Err("no such window".into())
                }
                _ => Ok(Response::Handled),
            })
        });
        let mut srv = Server::new(ipc);
        srv.launch_wait = Duration::from_millis(50);
        srv.poll = Duration::from_millis(1);
        srv.profile_base = Some(base.to_path_buf());
        (srv, base)
    }

    #[test]
    fn close_cleans_up_window_owner_already_closed() {
        let (mut srv, base) = server_with_base("closed", |_| vec![], true);
        let p = create_profile(&base).unwrap();
        srv.launched.insert(7, Some(p.clone()));
        let o = close_tool(&mut srv, &json!({"window": 7}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(srv.launched.is_empty());
        assert!(!p.exists());
    }

    #[test]
    fn launch_firefox_writes_quiet_prefs_into_its_profile() {
        let (mut srv, base) = server_with_base(
            "ffprefs",
            |n| {
                if n >= 1 {
                    vec![win(3, "agent-firefox", false)]
                } else {
                    vec![]
                }
            },
            false,
        );
        let o = launch_tool(&mut srv, &json!({"app": "firefox"}));
        assert!(!o.is_error, "{}", text(&o));
        let profile = srv.launched[&3].clone().unwrap();
        let prefs = std::fs::read_to_string(profile.join("user.js")).unwrap();
        for p in [
            "datareporting.policy.firstRunURL",
            "browser.aboutwelcome.enabled",
            "datareporting.healthreport.uploadEnabled",
        ] {
            assert!(prefs.contains(p), "{p}");
        }
        // Nothing that weakens security.
        assert!(!prefs.contains("safebrowsing") && !prefs.contains("app.update"));
        remove_profile(&base, &profile).unwrap();
    }

    #[test]
    fn launch_timeout_removes_profile_dir() {
        let (mut srv, base) = server_with_base("lt", |_| vec![], false);
        let o = launch_tool(&mut srv, &json!({"app": "firefox"}));
        assert!(o.is_error);
        assert!(text(&o).contains("launch failed") && text(&o).contains("removed"));
        assert_eq!(std::fs::read_dir(&*base).unwrap().count(), 0);
    }

    /// A pid that is never alive (pid_max is at most 2^22).
    const DEAD_PID: u32 = 999_999_999;

    fn lock(profile: &Path, name: &str, target: String) {
        std::os::unix::fs::symlink(target, profile.join(name)).unwrap();
    }

    fn age(dir: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs);
        std::fs::File::open(dir).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn close_keeps_profile_while_browser_runs() {
        let (mut srv, base) = server_with_base("busy", |_| vec![], false);
        let p = create_profile(&base).unwrap();
        lock(
            &p,
            "SingletonLock",
            format!("my-host-{}", std::process::id()),
        );
        srv.launched.insert(7, Some(p.clone()));
        let o = close_tool(&mut srv, &json!({"window": 7}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(text(&o).contains("left in place"), "{}", text(&o));
        assert!(p.exists());
        assert!(srv.launched.is_empty());
    }

    #[test]
    fn close_removes_profile_once_lock_is_dead() {
        let (mut srv, base) = server_with_base("dead", |_| vec![], false);
        let chrome = create_profile(&base).unwrap();
        lock(&chrome, "SingletonLock", format!("my-host-{DEAD_PID}"));
        let firefox = create_profile(&base).unwrap();
        lock(&firefox, "lock", format!("127.0.1.1:+{DEAD_PID}"));
        srv.launched.insert(7, Some(chrome.clone()));
        srv.launched.insert(8, Some(firefox.clone()));
        for w in [7, 8] {
            let o = close_tool(&mut srv, &json!({"window": w}));
            assert!(text(&o).contains("removed"), "{}", text(&o));
        }
        assert!(!chrome.exists() && !firefox.exists());
    }

    #[test]
    fn launch_sweeps_old_profiles_of_dead_browsers() {
        let (mut srv, base) = server_with_base(
            "sweep",
            |n| {
                if n >= 1 {
                    vec![win(2, "agent-foot", false)]
                } else {
                    vec![]
                }
            },
            false,
        );
        let outside = scratch("sweep-out");
        let dead = create_profile(&base).unwrap();
        lock(&dead, "lock", format!("127.0.1.1:+{DEAD_PID}"));
        let unlocked = create_profile(&base).unwrap();
        std::fs::write(unlocked.join("f"), "x").unwrap();
        let live = create_profile(&base).unwrap();
        lock(
            &live,
            "SingletonLock",
            format!("my-host-{}", std::process::id()),
        );
        let other = base.join("other");
        std::fs::create_dir(&other).unwrap();
        for d in [&dead, &unlocked, &live, &other] {
            age(d, 120);
        }
        let fresh = create_profile(&base).unwrap();
        std::os::unix::fs::symlink(&*outside, base.join("profile-link")).unwrap();

        let o = launch_tool(&mut srv, &json!({"app": "foot"}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(!dead.exists() && !unlocked.exists());
        assert!(live.exists() && fresh.exists() && other.exists());
        assert!(base.join("profile-link").symlink_metadata().is_ok());
        assert!(outside.exists());
    }
}

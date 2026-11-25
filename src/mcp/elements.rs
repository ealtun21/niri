//! Clickable elements of a window, read from its accessibility tree (AT-SPI). Strictly read only:
//! no actions, no focus, no text contents.

use std::fmt::Write as _;
use std::pin::pin;
use std::time::{Duration, Instant};

use async_io::Timer;
use atspi::proxy::accessible::AccessibleProxyBlocking;
use atspi::proxy::bus::BusProxy;
use atspi::proxy::component::ComponentProxyBlocking;
use atspi::{CoordType, Interface, ObjectRefOwned, Role, State, StateSet};
use futures_util::future::{select, Either};
use niri_ipc::{CapturePopup, WindowCapture};
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::Connection;
use zbus::connection::Builder;
use zbus::proxy::CacheProperties;

pub const MAX_DEPTH: usize = 64;
pub const MAX_NODES: usize = 5000;
/// Time budget for one whole `read_elements`, D-Bus connection included.
pub const TREE_BUDGET: Duration = Duration::from_millis(1500);
pub const MAX_ELEMENT_LINES: usize = 300;
pub const NAME_MAX_CHARS: usize = 80;
/// Most a popup root's extents exceed the popup's geometry by: its shadow on both sides
/// (Chromium and GTK 4 menus add 48 and 46 px). Also the most a GTK 4 popover root is placed off
/// its popup (25 px seen).
const POPUP_SHADOW_MAX: f64 = 64.;
/// Said when a web page was dropped because its device-pixel factor could not be read.
const PAGE_NOTE: &str =
    "elements of the web page could not be placed (its size was unreadable); click them by coordinates";
/// Largest decoration margin read off a frame's size (Qt's are 3 and 30 px).
const MARGIN_MAX: i32 = 64;
/// Said when items of an open popup were dropped because they could not be placed.
const UNPLACED_NOTE: &str = "items of an open menu could not be placed; click them by coordinates";
/// Timeout of each D-Bus call, so a hung app costs at most this past the budget.
const DBUS_CALL_TIMEOUT: Duration = Duration::from_millis(300);
/// Why a tree is often missing. Chromium without the flag shows only unnamed frames.
const HINT: &str = "terminals have none; Chromium needs --force-renderer-accessibility";

const KEPT_ROLES: &[Role] = &[
    Role::Button,
    Role::ToggleButton,
    Role::CheckBox,
    Role::RadioButton,
    Role::Link,
    Role::Entry,
    Role::PasswordText,
    Role::ComboBox,
    Role::MenuItem,
    Role::CheckMenuItem,
    Role::RadioMenuItem,
    Role::Menu,
    Role::PageTab,
    Role::ListItem,
    Role::TreeItem,
    Role::SpinButton,
    Role::Slider,
];

/// States shown on an element line (plus `disabled` for a node with neither `enabled` nor
/// `sensitive`: GTK 4 sets only `sensitive`).
const SHOWN_STATES: [(State, &str); 4] = [
    (State::Focused, "focused"),
    (State::Checked, "checked"),
    (State::Selected, "selected"),
    (State::Expanded, "expanded"),
];

/// One clickable element.
pub struct Element {
    pub role: String,
    /// Sanitized: one line, quotes escaped, capped.
    pub name: String,
    pub states: Vec<&'static str>,
    /// Capture pixels: x, y, width, height.
    pub rect: (f64, f64, f64, f64),
}

impl Element {
    pub fn center(&self) -> (f64, f64) {
        let (x, y, w, h) = self.rect;
        (x + w / 2., y + h / 2.)
    }
}

/// The elements of a window.
pub struct Elements {
    pub list: Vec<Element>,
    /// A limit stopped the walk; elements may be missing.
    pub cut: bool,
    /// Why there is no tree, or that some items could not be placed.
    pub note: Option<String>,
}

/// What the walker needs to know about a node.
struct NodeInfo {
    role: Role,
    states: StateSet,
    has_action: bool,
}

/// Index of the frame belonging to the window titled `title`, given the frame names (`None`: the
/// read failed): the one exact match of a non-empty title, else the only frame if the window is
/// the pid's only niri window (`sole_window`). `None` when ambiguous, so another window's elements
/// are never shown.
fn pick_frame(frames: &[Option<String>], title: &str, sole_window: bool) -> Option<usize> {
    let mut matches = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| !title.trim().is_empty() && f.as_deref() == Some(title));
    match (matches.next(), matches.next()) {
        (Some((i, _)), None) => Some(i),
        (None, _) if sole_window && frames.len() == 1 => Some(0),
        _ => None,
    }
}

/// Frames other than `main` that may be popups: showing and read as unnamed (a failed read
/// doesn't count).
fn popup_candidates(names: &[Option<String>], showing: &[bool], main: usize) -> Vec<usize> {
    let unnamed = |n: &Option<String>| n.as_deref().is_some_and(|n| n.trim().is_empty());
    (0..names.len())
        .filter(|&i| i != main && showing[i] && unnamed(&names[i]))
        .collect()
}

fn keep_node(role: Role, states: &StateSet, has_action: bool) -> bool {
    states.contains(State::Showing | State::Visible)
        && (has_action
            || KEPT_ROLES.contains(&role)
            || (role == Role::Text && states.contains(State::Editable)))
}

/// Maps AT-SPI window extents of a node on a surface at `surface_offset` (relative to the main
/// surface) to capture pixels; extents in device pixels are divided by `div` first.
fn to_capture_px(
    ext: (i32, i32, i32, i32),
    surface_offset: (f64, f64),
    div: f64,
    capture: &WindowCapture,
) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = ext;
    let s = capture.scale;
    (
        (f64::from(x) / div + surface_offset.0 - capture.origin.0) * s,
        (f64::from(y) / div + surface_offset.1 - capture.origin.1) * s,
        f64::from(w) / div * s,
        f64::from(h) / div * s,
    )
}

/// Offset of the main frame's WINDOW extents from the main surface origin, judged from the
/// frame's own extents `frame` (x, y, w, h). Chromium, Firefox and GTK 3 measure from the surface
/// origin (their frame is the whole surface, or sits at the geometry inside it). GTK 4 measures
/// from the window geometry, past its CSD shadow; Qt from its content, below the title bar it
/// draws. Both report a frame at 0,0 no bigger than the geometry.
fn frame_offset(frame: Option<(i32, i32, i32, i32)>, capture: &WindowCapture) -> (f64, f64) {
    let (ww, wh) = capture.window_size;
    match frame {
        Some((0, 0, w, h)) if w > 0 && h > 0 && w <= ww && h <= wh => {
            // ponytail: the decoration's side and bottom borders are taken as equal (Qt's are 3
            // px); a toolkit with uneven borders would need its real margins.
            let side = (ww - w) / 2;
            let top = wh - h - side;
            if !(0..=MARGIN_MAX).contains(&side) || !(0..=MARGIN_MAX).contains(&top) {
                return (0., 0.);
            }
            (
                capture.geometry_loc.0 + f64::from(side),
                capture.geometry_loc.1 + f64::from(top),
            )
        }
        _ => (0., 0.),
    }
}

/// Makes an untrusted name safe to quote on one line: no control, bidi or zero-width characters,
/// whitespace runs collapsed, `"` and `\` escaped, at most [`NAME_MAX_CHARS`] characters.
fn sanitize_name(raw: &str) -> String {
    let hidden = |c: char| {
        matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}')
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}')
    };
    let clean: String = raw
        .chars()
        .filter(|&c| (c.is_whitespace() || !c.is_control()) && !hidden(c))
        .collect();
    let flat = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let cost = |c: char| if matches!(c, '"' | '\\') { 2 } else { 1 };
    let fits = flat.chars().map(cost).sum::<usize>() <= NAME_MAX_CHARS;
    let mut out = String::new();
    let mut used = 0;
    for c in flat.chars() {
        // Stop with room left for the ellipsis; never split an escape.
        if !fits && used + cost(c) >= NAME_MAX_CHARS {
            out.push('…');
            break;
        }
        if cost(c) == 2 {
            out.push('\\');
        }
        out.push(c);
        used += cost(c);
    }
    out
}

/// One line per element, `N role "name" [states] x,y wxh`, numbered from `first`, at most
/// `max_lines` of them.
pub fn format_elements(e: &Elements, first: usize, max_lines: usize) -> String {
    let mut out = String::new();
    if let Some(note) = &e.note {
        let _ = writeln!(out, "{note}");
    } else if e.list.is_empty() && !e.cut {
        out.push_str("no clickable elements in the accessibility tree\n");
    }
    for (i, el) in e.list.iter().take(max_lines).enumerate() {
        let (x, y, w, h) = el.rect;
        let _ = writeln!(
            out,
            "{} {} \"{}\" [{}] {},{} {}x{}",
            first + i,
            el.role,
            el.name,
            el.states.join(","),
            x.round() as i64,
            y.round() as i64,
            w.round() as i64,
            h.round() as i64,
        );
    }
    if e.list.len() > max_lines {
        let _ = writeln!(out, "{} more elements not shown", e.list.len() - max_lines);
    }
    if e.cut {
        out.push_str("tree walk cut short by a time or size limit; elements may be missing\n");
    }
    out
}

/// Walks the descendants of `root` depth first until `deadline`, handing each node
/// [`keep_node`] keeps to `keep` at once (in tree order), so a cut walk loses nothing it found.
/// Returns whether a limit cut the walk. The root (the window itself) is never kept. Does not
/// descend into nodes that are not showing (their descendants are not showing either) or nodes
/// `info` cannot read. Each node's place is `place`'s answer for its parent's place (the root's
/// parent's is `root_place`), and `keep` gets it.
fn walk<N, P: Copy>(
    root: N,
    root_place: P,
    deadline: Instant,
    mut children: impl FnMut(&N) -> Vec<N>,
    mut info: impl FnMut(&N) -> Option<NodeInfo>,
    mut place: impl FnMut(&N, &NodeInfo, P) -> P,
    mut keep: impl FnMut(N, NodeInfo, P),
) -> bool {
    let mut stack = vec![(root, 0, root_place)];
    let mut seen = 0;
    let mut cut = false;
    while let Some((node, depth, parent)) = stack.pop() {
        if seen == MAX_NODES || Instant::now() >= deadline {
            return true;
        }
        seen += 1;
        let Some(ni) = info(&node) else { continue };
        let here = place(&node, &ni, parent);
        if ni.states.contains(State::Showing) {
            if depth == MAX_DEPTH {
                cut = true;
            } else if Instant::now() < deadline {
                let kids = children(&node);
                stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1, here)));
            }
        }
        if depth > 0 && keep_node(ni.role, &ni.states, ni.has_action) {
            keep(node, ni, here);
        }
    }
    cut || Instant::now() >= deadline
}

/// How a node's extents map to the main surface. Children start from their parent's.
#[derive(Clone, Copy)]
struct Place {
    /// Added to the extents (after `div`), in logical pixels.
    offset: (f64, f64),
    /// Extents are in device pixels: divided by this first (Chromium web content at a fractional
    /// scale), else 1.
    div: f64,
    /// The node's own extents, read once (`None` if it is not showing or the read failed).
    ext: Option<(i32, i32, i32, i32)>,
    /// The node is a menu.
    is_menu: bool,
    /// The node has a menu ancestor: an item of a menu, which must lie in an open popup.
    in_menu: bool,
    /// The node is in a web page whose device-pixel factor could not be read.
    unplaced: bool,
}

/// Walks each surface's frame with its offset (popups first, the main frame last) and maps the
/// kept nodes to elements as they are found. A node that fits one of `inline_popups` by size and
/// lies near it is that popup's root (a GTK 4 popover: a menu, or a panel for a dropdown): its
/// subtree is placed relative to the popup's surface origin. A web document wider than its parent
/// by a scale factor (Chromium at a fractional scale) reports device pixels, and so does its
/// subtree; a document whose factor can't be read is dropped with [`PAGE_NOTE`]. Items of a menu
/// that lie outside every open popup (GTK 3), and whatever lies under a popup whose root was not
/// found, are dropped with [`UNPLACED_NOTE`].
#[allow(clippy::too_many_arguments)]
fn collect_elements<N: Clone>(
    surfaces: &[(N, (f64, f64))],
    inline_popups: &[CapturePopup],
    capture: &WindowCapture,
    deadline: Instant,
    mut children: impl FnMut(&N) -> Vec<N>,
    mut info: impl FnMut(&N) -> Option<NodeInfo>,
    extents: impl Fn(&N) -> Option<(i32, i32, i32, i32)>,
    mut name: impl FnMut(&N) -> Option<String>,
) -> Elements {
    let mut list = Vec::new();
    let mut cut = false;
    let mut claimed = vec![false; inline_popups.len()];
    let mut unplaced_items = false;
    let mut unplaced_page = false;
    for (frame, offset) in surfaces {
        let place = |node: &N, ni: &NodeInfo, parent: Place| {
            let mut here = Place {
                ext: None,
                is_menu: matches!(ni.role, Role::Menu | Role::PopupMenu),
                in_menu: parent.in_menu || parent.is_menu,
                ..parent
            };
            if !ni.states.contains(State::Showing | State::Visible) {
                return here;
            }
            here.ext = extents(node);
            if ni.role == Role::DocumentWeb {
                // The page's factor is its width over its parent's, snapped to 1/120 (the unit of
                // Wayland fractional scales). Without both widths the page can't be placed.
                match (here.ext, parent.ext) {
                    (Some((_, _, w, _)), Some((_, _, pw, _))) if pw > 0 => {
                        let k = (f64::from(w) / f64::from(pw) * 120.).round() / 120.;
                        if k >= 1.1 {
                            here.div = parent.div * k;
                        }
                    }
                    _ => here.unplaced = true,
                }
            }
            let Some((x, y, w, h)) = here.ext else {
                return here;
            };
            if !claimed.contains(&false) {
                return here;
            }
            let (x, y) = (f64::from(x) / here.div, f64::from(y) / here.div);
            let size = (
                (f64::from(w) / here.div).round() as i32,
                (f64::from(h) / here.div).round() as i32,
            );
            let near = |p: &CapturePopup| {
                (x + here.offset.0 - p.x).abs() <= POPUP_SHADOW_MAX
                    && (y + here.offset.1 - p.y).abs() <= POPUP_SHADOW_MAX
            };
            let mut fits = inline_popups
                .iter()
                .enumerate()
                .filter(|(i, p)| !claimed[*i] && popup_fits(size, p) && near(p));
            if let (Some((i, p)), None) = (fits.next(), fits.next()) {
                claimed[i] = true;
                here.offset = (p.x - x, p.y - y);
            }
            here
        };
        let keep = |node: N, ni: NodeInfo, here: Place| {
            if here.unplaced {
                unplaced_page = true;
                return;
            }
            let Some(ext) = here.ext else { return };
            let rect = to_capture_px(ext, here.offset, here.div, capture);
            if !intersects(rect, capture) {
                return;
            }
            // GTK 3 menu items report their place in the popup's own surface, with no node for
            // the popup to anchor to. An item of a menu outside every open popup was not placed.
            if here.in_menu
                && !capture.popups.is_empty()
                && !capture
                    .popups
                    .iter()
                    .any(|p| inside(rect, popup_area(p, capture)))
            {
                unplaced_items = true;
                return;
            }
            let enabled =
                ni.states.contains(State::Enabled) || ni.states.contains(State::Sensitive);
            list.push(Element {
                role: ni.role.name().to_owned(),
                name: sanitize_name(&name(&node).unwrap_or_default()),
                states: SHOWN_STATES
                    .iter()
                    .filter(|(s, _)| ni.states.contains(*s))
                    .map(|(_, n)| *n)
                    .chain((!enabled).then_some("disabled"))
                    .collect(),
                rect,
            });
        };
        let root = Place {
            offset: *offset,
            div: 1.,
            ext: None,
            is_menu: false,
            in_menu: false,
            unplaced: false,
        };
        cut |= walk(
            frame.clone(),
            root,
            deadline,
            &mut children,
            &mut info,
            place,
            keep,
        );
    }
    // A popup whose root was not found: whatever lies under it may be its items, placed with the
    // window's offset 18-23 px off. Drop them rather than list a wrong click.
    let unplaced: Vec<_> = inline_popups
        .iter()
        .zip(&claimed)
        .filter(|(_, c)| !**c)
        .map(|(p, _)| popup_area(p, capture))
        .collect();
    let before = list.len();
    list.retain(|el| !unplaced.iter().any(|a| contains(*a, el.center())));
    let notes: Vec<&str> = [
        (unplaced_items || list.len() < before).then_some(UNPLACED_NOTE),
        unplaced_page.then_some(PAGE_NOTE),
    ]
    .into_iter()
    .flatten()
    .collect();
    Elements {
        note: (!notes.is_empty()).then(|| notes.join("\n")),
        list,
        cut,
    }
}

/// The a11y bus with a deadline: every call is skipped once the deadline passed.
struct A11y<'a> {
    conn: &'a Connection,
    deadline: Instant,
}

impl<'a> A11y<'a> {
    fn call<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Option<T> {
        (Instant::now() < self.deadline).then(f)?.ok()
    }

    fn accessible(&self, dest: &str, path: &str) -> Option<AccessibleProxyBlocking<'a>> {
        AccessibleProxyBlocking::builder(self.conn)
            .destination(dest.to_owned())
            .ok()?
            .path(path.to_owned())
            .ok()?
            .cache_properties(CacheProperties::No)
            .build()
            .ok()
    }

    fn node(&self, r: &ObjectRefOwned) -> Option<AccessibleProxyBlocking<'a>> {
        self.accessible(r.name_as_str()?, r.path_as_str())
    }

    fn children(&self, r: &ObjectRefOwned) -> Vec<ObjectRefOwned> {
        self.node(r)
            .and_then(|p| self.call(|| p.get_children()))
            .unwrap_or_default()
    }

    fn info(&self, r: &ObjectRefOwned) -> Option<NodeInfo> {
        let p = self.node(r)?;
        let states = self.call(|| p.get_state())?;
        if !states.contains(State::Showing | State::Visible) {
            // Never kept: skip the role and interface calls.
            return Some(NodeInfo {
                role: Role::Invalid,
                states,
                has_action: false,
            });
        }
        Some(NodeInfo {
            role: self.call(|| p.get_role())?,
            states,
            has_action: self
                .call(|| p.get_interfaces())?
                .contains(Interface::Action),
        })
    }

    fn name(&self, r: &ObjectRefOwned) -> Option<String> {
        let p = self.node(r)?;
        self.call(|| p.name())
    }

    /// Extents in window coordinates; screen ones are bogus on Wayland.
    fn extents(&self, r: &ObjectRefOwned) -> Option<(i32, i32, i32, i32)> {
        let p = ComponentProxyBlocking::builder(self.conn)
            .destination(r.name_as_str()?.to_owned())
            .ok()?
            .path(r.path_as_str().to_owned())
            .ok()?
            .cache_properties(CacheProperties::No)
            .build()
            .ok()?;
        self.call(|| p.get_extents(CoordType::Window))
    }
}

/// Connects to the a11y bus (its address comes from `org.a11y.Bus` on the session bus). The auth
/// handshakes have no timeout of their own, so the whole connect races the deadline.
fn connect(deadline: Instant) -> zbus::Result<Connection> {
    let connect = async {
        let session = Builder::session()?
            .method_timeout(DBUS_CALL_TIMEOUT)
            .build()
            .await?;
        let address = BusProxy::builder(&session)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
            .get_address()
            .await?;
        Builder::address(address.as_str())?
            .method_timeout(DBUS_CALL_TIMEOUT)
            .build()
            .await
    };
    match async_io::block_on(select(pin!(connect), Timer::at(deadline))) {
        Either::Left((conn, _)) => conn.map(Connection::from),
        Either::Right(_) => Err(zbus::Error::Failure("timed out".to_owned())),
    }
}

/// Lists the clickable elements of the window of app `pid` titled `title`, in pixels of
/// `capture`. `sole_window`: niri has no other window with this pid, so a lone frame is this
/// window's even if its name differs. Never fails and returns within [`TREE_BUDGET`] plus one
/// D-Bus call timeout.
pub fn read_elements(
    pid: u32,
    title: &str,
    sole_window: bool,
    capture: &WindowCapture,
) -> Elements {
    let deadline = Instant::now() + TREE_BUDGET;
    read(pid, title, sole_window, capture, deadline).unwrap_or_else(|why| Elements {
        list: Vec::new(),
        cut: false,
        note: Some(format!("no accessibility tree: {why}")),
    })
}

fn read(
    pid: u32,
    title: &str,
    sole_window: bool,
    capture: &WindowCapture,
    deadline: Instant,
) -> Result<Elements, String> {
    let conn =
        connect(deadline).map_err(|e| format!("cannot reach the accessibility bus ({e})"))?;
    let bus = A11y {
        conn: &conn,
        deadline,
    };
    let dbus = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(|e| e.to_string())?;
    let registry = bus
        .accessible("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
        .ok_or("no accessibility registry")?;
    let apps = bus
        .call(|| registry.get_children())
        .ok_or("the accessibility registry did not answer")?;

    // Every top-level frame of the apps with this pid (an app may hold several windows).
    let mut frames: Vec<ObjectRefOwned> = Vec::new();
    for app in &apps {
        let Some(name) = app.name() else { continue };
        if bus.call(|| dbus.get_connection_unix_process_id(name.clone().into())) == Some(pid) {
            frames.extend(bus.children(app));
        }
    }
    if frames.is_empty() {
        return Err(format!(
            "the app (pid {pid}) is not on the accessibility bus or did not answer ({HINT})"
        ));
    }
    let names: Vec<Option<String>> = frames.iter().map(|f| bus.name(f)).collect();
    let main = pick_frame(&names, title, sole_window).ok_or(format!(
        "no frame unambiguously matches the window title ({HINT})"
    ))?;

    // Popup frames (unnamed, Chromium) go first: they are on top and must not lose the budget to
    // a big page. A popup frame's extents are taken as relative to its own surface.
    let showing: Vec<bool> = frames
        .iter()
        .map(|f| {
            bus.info(f)
                .is_some_and(|ni| ni.states.contains(State::Showing | State::Visible))
        })
        .collect();
    let popup_frames: Vec<(&ObjectRefOwned, (i32, i32))> = popup_candidates(&names, &showing, main)
        .into_iter()
        .filter(|_| !capture.popups.is_empty())
        .filter_map(|i| {
            bus.extents(&frames[i])
                .map(|(_, _, w, h)| (&frames[i], (w, h)))
        })
        .collect();
    let sizes: Vec<(i32, i32)> = popup_frames.iter().map(|(_, size)| *size).collect();
    let matches = match_popups(&sizes, &capture.popups);
    let mut surfaces: Vec<(ObjectRefOwned, (f64, f64))> = popup_frames
        .iter()
        .zip(&matches)
        .filter_map(|((f, _), m)| {
            let p = &capture.popups[(*m)?];
            Some(((*f).clone(), (p.x, p.y)))
        })
        .collect();
    let offset = frame_offset(bus.extents(&frames[main]), capture);
    surfaces.push((frames[main].clone(), offset));
    // Popups inside the main tree (GTK 4 popovers) are placed off by that offset; anchor them to
    // their capture popups. Surface-relative toolkits place theirs right.
    let inline_popups: Vec<CapturePopup> = if offset == (0., 0.) {
        Vec::new()
    } else {
        (0..capture.popups.len())
            .filter(|i| !matches.contains(&Some(*i)))
            .map(|i| capture.popups[i].clone())
            .collect()
    };

    Ok(collect_elements(
        &surfaces,
        &inline_popups,
        capture,
        deadline,
        |n| bus.children(n),
        |n| bus.info(n),
        |n| bus.extents(n),
        |n| bus.name(n),
    ))
}

/// Whether an accessible of size `w`x`h` can be `popup`'s root: as big as the popup's geometry
/// (within 1 px), or as its whole surface, which adds an even shadow of up to
/// [`POPUP_SHADOW_MAX`] in total on both axes (Chromium 48/48, GTK 4 46/46, Qt 0/0, Firefox
/// -1/-1).
fn popup_fits((w, h): (i32, i32), popup: &CapturePopup) -> bool {
    let (dw, dh) = (f64::from(w) - popup.width, f64::from(h) - popup.height);
    let fits = |d: f64| (-1. ..=POPUP_SHADOW_MAX).contains(&d);
    fits(dw) && fits(dh) && (dw - dh).abs() <= 2.
}

/// The capture pixels a popup's surface can cover: from its surface origin over its geometry
/// plus the most shadow.
pub(super) fn popup_area(p: &CapturePopup, capture: &WindowCapture) -> (f64, f64, f64, f64) {
    let s = capture.scale;
    (
        (p.x - capture.origin.0) * s,
        (p.y - capture.origin.1) * s,
        (p.width + POPUP_SHADOW_MAX) * s,
        (p.height + POPUP_SHADOW_MAX) * s,
    )
}

pub(super) fn contains((x, y, w, h): (f64, f64, f64, f64), (px, py): (f64, f64)) -> bool {
    (x..=x + w).contains(&px) && (y..=y + h).contains(&py)
}

/// Whether `rect` lies inside `area`, give or take 2 px of rounding.
fn inside((x, y, w, h): (f64, f64, f64, f64), area: (f64, f64, f64, f64)) -> bool {
    let (ax, ay, aw, ah) = area;
    x >= ax - 2. && y >= ay - 2. && x + w <= ax + aw + 2. && y + h <= ay + ah + 2.
}

/// For each popup frame size, the index of the capture popup it belongs to: the only popup it
/// fits, and only if no other frame claims that popup too. Best effort; a frame without a match
/// is skipped rather than guessed.
fn match_popups(sizes: &[(i32, i32)], popups: &[CapturePopup]) -> Vec<Option<usize>> {
    let claim = |&size: &(i32, i32)| {
        let mut same = popups
            .iter()
            .enumerate()
            .filter(|(_, p)| popup_fits(size, p));
        match (same.next(), same.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    };
    let claims: Vec<Option<usize>> = sizes.iter().map(claim).collect();
    claims
        .iter()
        .map(|c| c.filter(|i| claims.iter().filter(|d| **d == Some(*i)).count() == 1))
        .collect()
}

/// Whether a non-empty rect overlaps the capture image.
fn intersects((x, y, w, h): (f64, f64, f64, f64), capture: &WindowCapture) -> bool {
    w > 0.
        && h > 0.
        && x < f64::from(capture.width)
        && y < f64::from(capture.height)
        && x + w > 0.
        && y + h > 0.
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use atspi::{Role, State, StateSet};

    use super::*;

    fn capture(origin: (f64, f64), scale: f64, popups: Vec<CapturePopup>) -> WindowCapture {
        WindowCapture {
            id: 1,
            path: String::new(),
            width: 100,
            height: 100,
            scale,
            origin,
            geometry_loc: (0., 0.),
            window_size: (100, 100),
            workspace_visible: true,
            app_id: None,
            title: None,
            popups,
        }
    }

    fn shown() -> StateSet {
        StateSet::new(State::Showing | State::Visible)
    }

    /// Frame names; `None` stands for a failed name read.
    fn frames(names: &[Option<&str>]) -> Vec<Option<String>> {
        names.iter().map(|s| s.map(str::to_owned)).collect()
    }

    #[test]
    fn pick_frame_exact_title() {
        let f = frames(&[
            Some("Other - Chromium"),
            None,
            Some("Doc - Chromium"),
            Some(""),
        ]);
        assert_eq!(pick_frame(&f, "Doc - Chromium", false), Some(2));
    }

    #[test]
    fn pick_frame_single_frame_fallback() {
        assert_eq!(
            pick_frame(&frames(&[Some("Untitled")]), "Doc", true),
            Some(0)
        );
    }

    #[test]
    fn pick_frame_single_frame_not_sole_window_is_none() {
        // The pid has another niri window; the one frame may be that window's.
        assert_eq!(pick_frame(&frames(&[Some("Untitled")]), "Doc", false), None);
    }

    #[test]
    fn pick_frame_two_frames_no_match_is_none() {
        assert_eq!(
            pick_frame(&frames(&[Some("A"), Some("B")]), "C", true),
            None
        );
    }

    #[test]
    fn pick_frame_duplicate_titles_is_none() {
        assert_eq!(
            pick_frame(&frames(&[Some("Doc"), Some("Doc")]), "Doc", true),
            None
        );
    }

    #[test]
    fn pick_frame_empty_title_never_matches() {
        // An untitled window must not take a unique unnamed (popup) frame or a failed read.
        let f = frames(&[Some("Doc"), Some("")]);
        assert_eq!(pick_frame(&f, "", false), None);
        assert_eq!(pick_frame(&frames(&[Some(""), None]), "", false), None);
        assert_eq!(pick_frame(&frames(&[Some("")]), "", false), None);
        assert_eq!(pick_frame(&frames(&[Some(" ")]), " ", false), None);
    }

    #[test]
    fn popup_candidates_only_read_unnamed_frames() {
        let f = frames(&[Some(""), Some("Doc"), None, Some("  "), Some("")]);
        assert_eq!(popup_candidates(&f, &[true; 5], 4), [0, 3]);
    }

    #[test]
    fn keep_node_button_showing_visible() {
        assert!(keep_node(Role::Button, &shown(), false));
    }

    #[test]
    fn keep_node_hidden_dropped() {
        assert!(!keep_node(
            Role::Button,
            &StateSet::new(State::Visible),
            true
        ));
        assert!(!keep_node(
            Role::Button,
            &StateSet::new(State::Showing),
            true
        ));
    }

    #[test]
    fn keep_node_editable_text_kept() {
        let editable = StateSet::new(State::Showing | State::Visible | State::Editable);
        assert!(keep_node(Role::Text, &editable, false));
        assert!(!keep_node(Role::Text, &shown(), false));
    }

    #[test]
    fn keep_node_plain_label_dropped() {
        assert!(!keep_node(Role::Label, &shown(), false));
        assert!(keep_node(Role::Label, &shown(), true));
    }

    #[test]
    fn to_capture_px_scale_1_csd_origin() {
        let c = capture((-23., -23.), 1., vec![]);
        assert_eq!(
            to_capture_px((10, 20, 30, 40), (0., 0.), 1., &c),
            (33., 43., 30., 40.)
        );
    }

    #[test]
    fn to_capture_px_fractional_popup() {
        let c = capture((0., 0.), 1.5, vec![]);
        assert_eq!(
            to_capture_px((5, 5, 10, 10), (100., 50.), 1., &c),
            (157.5, 82.5, 15., 15.)
        );
    }

    /// A capture of a 932x1040 window with geometry at `geo` in its surface.
    fn window_capture(geo: (f64, f64)) -> WindowCapture {
        WindowCapture {
            geometry_loc: geo,
            window_size: (932, 1040),
            ..capture((0., 0.), 1., vec![])
        }
    }

    #[test]
    fn frame_offset_surface_relative_toolkits() {
        // Chromium: the frame is the whole surface, CSD shadow included.
        let c = window_capture((16., 10.));
        assert_eq!(frame_offset(Some((0, 0, 964, 1082)), &c), (0., 0.));
        // Firefox: the frame sits at the geometry inside the surface.
        let c = window_capture((26., 23.));
        assert_eq!(frame_offset(Some((26, 23, 932, 1040)), &c), (0., 0.));
        // Unknown frame extents: surface relative.
        assert_eq!(frame_offset(None, &c), (0., 0.));
    }

    #[test]
    fn frame_offset_gtk4_is_geometry_relative() {
        let c = window_capture((25., 25.));
        assert_eq!(frame_offset(Some((0, 0, 932, 1040)), &c), (25., 25.));
    }

    #[test]
    fn frame_offset_qt_csd_is_content_relative() {
        // Qt draws a 30 px title bar and 3 px borders; its frame is the content below them.
        let c = window_capture((0., 0.));
        assert_eq!(frame_offset(Some((0, 0, 926, 1007)), &c), (3., 30.));
    }

    #[test]
    fn frame_offset_ignores_out_of_range_margins() {
        // A narrower frame (side 66, top -66) or a much shorter one (top 140) is no decoration.
        let c = window_capture((25., 25.));
        assert_eq!(frame_offset(Some((0, 0, 800, 1040)), &c), (0., 0.));
        assert_eq!(frame_offset(Some((0, 0, 932, 900)), &c), (0., 0.));
    }

    #[test]
    fn sanitize_name_strips_newlines_and_controls() {
        assert_eq!(sanitize_name("Sign\nin\t\u{7}"), "Sign in");
        // Bidi controls and zero-width characters are dropped.
        let hidden = "a\u{202A}\u{202E}b\u{2066}\u{2069}\u{61C}c\u{200B}\u{200F}d\u{2060}\u{FEFF}e";
        assert_eq!(sanitize_name(hidden), "abcde");
    }

    #[test]
    fn sanitize_name_caps_length() {
        let s = sanitize_name(&"a".repeat(200));
        assert!(s.chars().count() <= NAME_MAX_CHARS);
        assert!(s.ends_with('…'));
        // Escapes are never split by the cap.
        let s = sanitize_name(&"\"".repeat(200));
        assert!(s.chars().count() <= NAME_MAX_CHARS);
        assert!(s.ends_with("\\\"…"));
    }

    #[test]
    fn sanitize_name_cannot_forge_line() {
        let s = sanitize_name("x\" ]\n7 button \"OK");
        assert_eq!(s, r#"x\" ] 7 button \"OK"#);
        assert!(!s.contains('\n'));
    }

    #[test]
    fn format_caps_lines() {
        let el = |i: usize| Element {
            role: "push button".into(),
            name: format!("b{i}"),
            states: vec!["focused"],
            rect: (1., 2., 3., 4.),
        };
        let e = Elements {
            list: (0..1000).map(el).collect(),
            cut: false,
            note: None,
        };
        let out = format_elements(&e, 1, 300);
        let lines: Vec<&str> = out.lines().collect();
        let element_lines = lines
            .iter()
            .filter(|l| l.contains(" push button \""))
            .count();
        assert_eq!(element_lines, 300);
        assert_eq!(lines[0], r#"1 push button "b0" [focused] 1,2 3x4"#);
        assert!(lines.contains(&"700 more elements not shown"));
    }

    #[test]
    fn format_cut_empty_list_is_not_no_elements() {
        let e = Elements {
            list: Vec::new(),
            cut: true,
            note: None,
        };
        let out = format_elements(&e, 1, 300);
        assert!(!out.contains("no clickable elements"), "{out}");
        assert!(out.contains("cut short"), "{out}");
    }

    #[test]
    fn match_popups_unique_size_only() {
        let popup = |w, h| CapturePopup {
            x: 0.,
            y: 0.,
            width: w,
            height: h,
        };
        let popups = [popup(200., 100.), popup(50., 50.), popup(50., 50.)];
        // Exact and 1 px off match; no popup of that size; two popups of one size; two frames
        // claiming one popup.
        assert_eq!(
            match_popups(&[(200, 101), (10, 10), (50, 50)], &popups),
            [Some(0), None, None]
        );
        assert_eq!(
            match_popups(&[(200, 100), (200, 100)], &popups),
            [None, None]
        );
    }

    #[test]
    fn match_popups_frame_with_shadow() {
        // Chromium's context menu frame is its whole surface: the 474x427 geometry plus a 24 px
        // shadow on each side.
        let popups = [CapturePopup {
            x: 45.,
            y: 170.,
            width: 474.,
            height: 427.,
        }];
        assert_eq!(match_popups(&[(522, 475)], &popups), [Some(0)]);
        // A stale frame much bigger than any popup claims nothing; nor does an uneven margin.
        assert_eq!(match_popups(&[(770, 88)], &popups), [None]);
        assert_eq!(match_popups(&[(522, 445)], &popups), [None]);
    }

    #[test]
    fn gtk4_popover_anchors_to_its_popup() {
        // gnome-text-editor with its main menu open (measured): the frame is geometry relative
        // (+25, +25), but the popover subtree reports its surface (277x568 for a 231x522 popup
        // geometry) 7,6 px off the popup's real place, so it is anchored to the capture popup.
        let popup = CapturePopup {
            x: 682.,
            y: 44.,
            width: 231.,
            height: 522.,
        };
        let c = WindowCapture {
            width: 982,
            height: 1090,
            ..window_capture((25., 25.))
        };
        let children = |n: &u32| match n {
            0 => vec![1, 2],
            2 => vec![3],
            _ => vec![],
        };
        let info = |n: &u32| {
            Some(NodeInfo {
                role: [Role::Frame, Role::Button, Role::Menu, Role::MenuItem][*n as usize],
                states: shown(),
                has_action: true,
            })
        };
        let extents = |n: &u32| {
            Some(
                [
                    (0, 0, 932, 1040),
                    (771, 0, 34, 34),
                    (675, 38, 277, 568),
                    (698, 195, 217, 32),
                ][*n as usize],
            )
        };
        let far = Instant::now() + Duration::from_secs(60);
        let e = collect_elements(
            &[(0u32, (25., 25.))],
            &[popup],
            &c,
            far,
            children,
            info,
            extents,
            |_| Some(String::new()),
        );
        let rects: Vec<_> = e.list.iter().map(|el| el.rect).collect();
        assert_eq!(
            rects,
            [
                (796., 25., 34., 34.),
                (682., 44., 277., 568.),
                (705., 201., 217., 32.),
            ]
        );
    }

    /// AT-SPI extents: x, y, w, h.
    type Ext = (i32, i32, i32, i32);

    /// Elements of a frame (0) with children 1..: `nodes` gives each child's role and extents.
    fn inline_rects(nodes: &[(Role, Ext)], popup: CapturePopup) -> Vec<(f64, f64, f64, f64)> {
        let c = WindowCapture {
            width: 982,
            height: 1090,
            ..window_capture((25., 25.))
        };
        let n = u32::try_from(nodes.len()).unwrap();
        let info = |i: &u32| {
            Some(NodeInfo {
                role: if *i == 0 {
                    Role::Frame
                } else {
                    nodes[*i as usize - 1].0
                },
                states: shown(),
                has_action: true,
            })
        };
        let extents = |i: &u32| {
            Some(if *i == 0 {
                (0, 0, 932, 1040)
            } else {
                nodes[*i as usize - 1].1
            })
        };
        let far = Instant::now() + Duration::from_secs(60);
        let e = collect_elements(
            &[(0u32, (25., 25.))],
            &[popup],
            &c,
            far,
            |i| if *i == 0 { (1..=n).collect() } else { vec![] },
            info,
            extents,
            |_| Some(String::new()),
        );
        e.list.iter().map(|el| el.rect).collect()
    }

    #[test]
    fn gtk4_dropdown_popover_anchors_by_size_and_place() {
        // gnome-text-editor's Line Feed dropdown (measured): its popover root is a panel
        // (251x180 for a 205x134 popup at 721,329), placed 23 px off the popup.
        let popup = CapturePopup {
            x: 721.,
            y: 329.,
            width: 205.,
            height: 134.,
        };
        assert_eq!(
            inline_rects(&[(Role::Panel, (719, 327, 251, 180))], popup)[0],
            (721., 329., 251., 180.)
        );
    }

    #[test]
    fn inline_popup_needs_a_fit_near_it() {
        let popup = CapturePopup {
            x: 100.,
            y: 100.,
            width: 200.,
            height: 100.,
        };
        // A panel of popup size far from the popup keeps the inherited offset. A menu of another
        // size near it claims nothing either, and lies under the unclaimed popup: dropped.
        let nodes = [
            (Role::Panel, (500, 600, 200, 100)),
            (Role::Menu, (80, 80, 50, 50)),
        ];
        assert_eq!(inline_rects(&nodes, popup), [(525., 625., 200., 100.)]);
    }

    #[test]
    fn chromium_web_content_in_device_pixels_at_fractional_scale() {
        // Chromium at scale 1.5 (measured shape): its browser UI reports logical extents, but
        // the web document and everything in it report device pixels, 1.5x their parent's.
        let c = WindowCapture {
            width: 968,
            height: 1088,
            ..capture((0., 0.), 1.5, vec![])
        };
        let children = |n: &u32| if *n < 3 { vec![n + 1] } else { vec![] };
        let info = |n: &u32| {
            Some(NodeInfo {
                role: [Role::Frame, Role::Panel, Role::DocumentWeb, Role::Entry][*n as usize],
                states: shown(),
                has_action: true,
            })
        };
        let extents = |n: &u32| {
            Some(
                [
                    (0, 0, 645, 725),
                    (16, 96, 612, 596),
                    (24, 144, 918, 894),
                    (48, 168, 492, 48),
                ][*n as usize],
            )
        };
        let far = Instant::now() + Duration::from_secs(60);
        let e = collect_elements(
            &[(0u32, (0., 0.))],
            &[],
            &c,
            far,
            children,
            info,
            extents,
            |_| Some(String::new()),
        );
        let rects: Vec<_> = e.list.iter().map(|el| el.rect).collect();
        assert_eq!(
            rects,
            [
                (24., 144., 918., 894.),
                (24., 144., 918., 894.),
                (48., 168., 492., 48.),
            ]
        );
    }

    #[test]
    fn popup_candidates_only_showing_frames() {
        let f = frames(&[Some(""), Some("Doc"), Some("")]);
        assert_eq!(popup_candidates(&f, &[false, true, true], 1), [2]);
    }

    #[test]
    fn popup_fits_needs_even_margins() {
        let popup = |width, height| CapturePopup {
            x: 0.,
            y: 0.,
            width,
            height,
        };
        // Measured: GTK 4 popovers +46/+46, Chromium menus +48/+48, Qt 0/0, Firefox -1/-1.
        assert!(popup_fits((277, 568), &popup(231., 522.)));
        assert!(popup_fits((522, 475), &popup(474., 427.)));
        assert!(popup_fits((365, 516), &popup(365., 516.)));
        assert!(popup_fits((205, 233), &popup(206., 234.)));
        // A 150x46 header-bar box is +30/+16 on a 120x30 tooltip: not its root.
        assert!(!popup_fits((150, 46), &popup(120., 30.)));
    }

    /// `collect_elements` on a tree: node i is `(parent, role, extents)`; node 0, the frame, has
    /// itself as parent. `None` extents stand for a failed read.
    fn tree_elements(
        nodes: &[(u32, Role, Option<Ext>)],
        offset: (f64, f64),
        inline_popups: &[CapturePopup],
        c: &WindowCapture,
    ) -> Elements {
        let n = u32::try_from(nodes.len()).unwrap();
        let far = Instant::now() + Duration::from_secs(60);
        collect_elements(
            &[(0u32, offset)],
            inline_popups,
            c,
            far,
            |i| (1..n).filter(|j| nodes[*j as usize].0 == *i).collect(),
            |i| {
                Some(NodeInfo {
                    role: nodes[*i as usize].1,
                    states: shown(),
                    has_action: true,
                })
            },
            |i| nodes[*i as usize].2,
            |_| Some(String::new()),
        )
    }

    fn rects(e: &Elements) -> Vec<(f64, f64, f64, f64)> {
        e.list.iter().map(|el| el.rect).collect()
    }

    /// gnome-text-editor's window (geometry at 25,25) with the main menu popover open.
    fn gtk4_menu_capture() -> (WindowCapture, CapturePopup) {
        let popup = CapturePopup {
            x: 682.,
            y: 44.,
            width: 231.,
            height: 522.,
        };
        let c = WindowCapture {
            width: 982,
            height: 1090,
            ..window_capture((25., 25.))
        };
        (
            WindowCapture {
                popups: vec![popup.clone()],
                ..c
            },
            popup,
        )
    }

    #[test]
    fn decoy_before_the_real_popover_root_claims_nothing() {
        // A panel near the popup, +30/+16 on its size, comes first in tree order.
        let (c, popup) = gtk4_menu_capture();
        let nodes = [
            (0, Role::Frame, Some((0, 0, 932, 1040))),
            (0, Role::Panel, Some((660, 30, 261, 538))),
            (1, Role::Button, Some((670, 40, 34, 34))),
            (0, Role::Menu, Some((675, 38, 277, 568))),
            (3, Role::MenuItem, Some((698, 195, 217, 32))),
        ];
        let e = tree_elements(&nodes, (25., 25.), &[popup], &c);
        assert_eq!(
            rects(&e),
            [
                (685., 55., 261., 538.),
                (695., 65., 34., 34.),
                (682., 44., 277., 568.),
                (705., 201., 217., 32.),
            ]
        );
        assert!(e.note.is_none());
    }

    #[test]
    fn header_box_does_not_claim_a_tooltip() {
        let tooltip = CapturePopup {
            x: 640.,
            y: 75.,
            width: 120.,
            height: 30.,
        };
        let (c, _) = gtk4_menu_capture();
        let c = WindowCapture {
            popups: vec![tooltip.clone()],
            ..c
        };
        let nodes = [
            (0, Role::Frame, Some((0, 0, 932, 1040))),
            (0, Role::Panel, Some((600, 0, 150, 46))),
            (1, Role::Button, Some((610, 6, 34, 34))),
        ];
        let e = tree_elements(&nodes, (25., 25.), &[tooltip], &c);
        assert_eq!(rects(&e), [(625., 25., 150., 46.), (635., 31., 34., 34.)]);
    }

    #[test]
    fn unclaimed_inline_popup_drops_what_lies_under_it() {
        // The popover root doesn't fit (its size was misread): its rows keep the window's
        // offset, 18-23 px off, so they are dropped with a note; the window's button is kept.
        let (c, popup) = gtk4_menu_capture();
        let nodes = [
            (0, Role::Frame, Some((0, 0, 932, 1040))),
            (0, Role::Button, Some((100, 100, 34, 34))),
            (0, Role::Menu, Some((675, 38, 300, 300))),
            (2, Role::MenuItem, Some((698, 195, 217, 32))),
        ];
        let e = tree_elements(&nodes, (25., 25.), &[popup], &c);
        assert_eq!(rects(&e), [(125., 125., 34., 34.)]);
        assert!(e
            .note
            .as_deref()
            .unwrap()
            .contains("click them by coordinates"));
    }

    #[test]
    fn gtk3_menu_items_outside_every_popup_are_dropped() {
        // mousepad (GTK 3) with Edit open (measured): items report their place in the popup's own
        // surface, so "Paste" would be listed at 6,114, a click there hitting the window or
        // another row. They are dropped with a note; the menubar entry and the window's button
        // stay.
        let popup = CapturePopup {
            x: 62.,
            y: 82.,
            width: 367.,
            height: 409.,
        };
        let c = WindowCapture {
            width: 984,
            height: 1092,
            window_size: (932, 1040),
            geometry_loc: (26., 23.),
            ..capture((0., 0.), 1., vec![popup])
        };
        let nodes = [
            (0, Role::Frame, Some((0, 0, 984, 1092))),
            (0, Role::MenuBar, Some((26, 60, 932, 27))),
            (1, Role::Menu, Some((68, 60, 43, 27))),
            (2, Role::MenuItem, Some((6, 114, 367, 27))),
            (0, Role::Button, Some((100, 500, 30, 30))),
        ];
        let e = tree_elements(&nodes, (0., 0.), &[], &c);
        assert_eq!(
            rects(&e),
            [
                (26., 60., 932., 27.),
                (68., 60., 43., 27.),
                (100., 500., 30., 30.),
            ]
        );
        assert!(e
            .note
            .as_deref()
            .unwrap()
            .contains("click them by coordinates"));
    }

    #[test]
    fn qt_menu_items_inside_their_popup_stay() {
        let popup = CapturePopup {
            x: 86.,
            y: 59.,
            width: 365.,
            height: 516.,
        };
        let c = WindowCapture {
            width: 932,
            height: 1040,
            window_size: (932, 1040),
            ..capture((0., 0.), 1., vec![popup.clone()])
        };
        let nodes = [
            (0, Role::Frame, Some((0, 0, 926, 1007))),
            (0, Role::MenuBar, Some((0, 0, 926, 29))),
            (1, Role::MenuItem, Some((83, 0, 72, 29))),
            (2, Role::PopupMenu, Some((83, 29, 365, 516))),
            (3, Role::MenuItem, Some((87, 33, 357, 29))),
        ];
        let e = tree_elements(&nodes, (3., 30.), &[popup], &c);
        assert_eq!(
            rects(&e),
            [
                (3., 30., 926., 29.),
                (86., 30., 72., 29.),
                (86., 59., 365., 516.),
                (90., 63., 357., 29.),
            ]
        );
        assert!(e.note.is_none());
    }

    #[test]
    fn web_document_without_a_scale_factor_is_dropped() {
        // The factor needs the document's and its parent's widths. If either read fails, the
        // page may be in device pixels: drop it with a note rather than list it 1.5x off.
        let c = WindowCapture {
            width: 968,
            height: 1088,
            ..capture((0., 0.), 1.5, vec![])
        };
        for (parent, doc) in [
            (None, Some((24, 144, 918, 894))),
            (Some((16, 96, 612, 596)), None),
        ] {
            let nodes = [
                (0, Role::Frame, Some((0, 0, 645, 725))),
                (0, Role::Panel, parent),
                (1, Role::DocumentWeb, doc),
                (2, Role::Entry, Some((48, 168, 492, 48))),
                (0, Role::Button, Some((20, 20, 30, 30))),
            ];
            let e = tree_elements(&nodes, (0., 0.), &[], &c);
            let kept: Vec<_> = e.list.iter().map(|el| el.role.as_str()).collect();
            assert!(
                !kept.contains(&"entry") && !kept.contains(&"document web"),
                "{kept:?}"
            );
            assert!(kept.contains(&"button"), "{kept:?}");
            assert!(e
                .note
                .as_deref()
                .unwrap()
                .contains("click them by coordinates"));
        }
    }

    #[test]
    fn walk_respects_budget() {
        let budget = Duration::from_millis(400);
        let slow = Duration::from_millis(200);
        let start = Instant::now();
        let mut kept = Vec::new();
        // An endless tree whose children call hangs like a stopped app's would.
        let cut = walk(
            0u32,
            (0., 0.),
            start + budget,
            |n| {
                std::thread::sleep(slow);
                vec![n + 1, n + 2]
            },
            |_| {
                Some(NodeInfo {
                    role: Role::Button,
                    states: shown(),
                    has_action: true,
                })
            },
            |_, _, o| o,
            |n, _, _| kept.push(n),
        );
        let took = start.elapsed();
        assert!(cut);
        assert!(!kept.is_empty());
        assert!(
            took < budget + slow + Duration::from_millis(100),
            "{took:?}"
        );
    }

    #[test]
    fn walk_synthetic_tree() {
        // 0 frame (with an action, as Chromium's): 1 button, 2 hidden panel (3 button inside),
        // 4 label.
        let children = |n: &u32| match n {
            0 => vec![1, 2, 4],
            2 => vec![3],
            _ => vec![],
        };
        let info = |n: &u32| {
            let (role, states) = match n {
                0 => (Role::Frame, shown()),
                1 | 3 => (Role::Button, shown()),
                2 => (Role::Panel, StateSet::new(State::Visible)),
                _ => (Role::Label, shown()),
            };
            Some(NodeInfo {
                role,
                states,
                has_action: *n == 0,
            })
        };
        let far = Instant::now() + Duration::from_secs(60);
        let mut kept = Vec::new();
        let cut = walk(
            0u32,
            (0., 0.),
            far,
            children,
            info,
            |_, _, o| o,
            |n, _, _| kept.push(n),
        );
        assert_eq!(kept, [1]);
        assert!(!cut);
    }

    /// One-node tree under a frame: the node with `states`, kept as a button at 10,10 20x20.
    fn one_element(states: StateSet) -> Element {
        let c = capture((0., 0.), 1., vec![]);
        let far = Instant::now() + Duration::from_secs(60);
        let mut e = collect_elements(
            &[(0u32, (0., 0.))],
            &[],
            &c,
            far,
            |n| if *n == 0 { vec![1] } else { vec![] },
            |_| {
                Some(NodeInfo {
                    role: Role::Button,
                    states,
                    has_action: true,
                })
            },
            |_| Some((10, 10, 20, 20)),
            |_| Some("b".into()),
        );
        e.list.remove(0)
    }

    #[test]
    fn sensitive_without_enabled_is_not_disabled() {
        // GTK 4.22 sets `sensitive` but never `enabled` on its widgets.
        let gtk4 = StateSet::new(State::Showing | State::Visible | State::Sensitive);
        assert!(!one_element(gtk4).states.contains(&"disabled"));
        let qt = StateSet::new(State::Showing | State::Visible | State::Enabled | State::Sensitive);
        assert!(!one_element(qt).states.contains(&"disabled"));
        assert!(one_element(shown()).states.contains(&"disabled"));
    }

    #[test]
    fn slow_tree_keeps_found_elements() {
        // A tree bigger than the budget: everything found before the cut comes back complete.
        let budget = Duration::from_millis(400);
        let slow = Duration::from_millis(200);
        let start = Instant::now();
        let c = capture((0., 0.), 1., vec![]);
        let e = collect_elements(
            &[(0u32, (0., 0.))],
            &[],
            &c,
            start + budget,
            |n| {
                std::thread::sleep(slow);
                vec![n + 1, n + 2]
            },
            |_| {
                Some(NodeInfo {
                    role: Role::Button,
                    states: shown(),
                    has_action: false,
                })
            },
            |n| Some((i32::try_from(*n).unwrap(), 0, 10, 10)),
            |n| Some(format!("b{n}")),
        );
        assert!(e.cut);
        assert!(!e.list.is_empty());
        assert_eq!(e.list[0].name, "b1");
        assert_eq!(e.list[0].rect, (1., 0., 10., 10.));
        assert!(start.elapsed() < budget + slow + Duration::from_millis(100));
    }
}

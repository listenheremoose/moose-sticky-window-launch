//! moose-sticky-window-launch
//!
//! A small COSMIC (cosmic-comp) daemon that remembers which monitor and
//! workspace each app's window was last on, and moves the app's window back
//! there the next time it opens, as long as that monitor and workspace still
//! exist.
//!
//! Protocols used:
//! - ext_foreign_toplevel_list_v1   : learn about windows (app_id, title, open/close)
//! - zcosmic_toplevel_info_v1 (v3)  : which ext workspace / output each window is on
//! - zcosmic_toplevel_manager_v1 (v4): move_to_ext_workspace
//! - ext_workspace_manager_v1       : workspaces, workspace groups, and their outputs
//! - wl_output (v4)                 : connector names (e.g. "DP-1") for stable matching

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use cosmic_protocols::{
    toplevel_info::v1::client::{zcosmic_toplevel_handle_v1 as cth, zcosmic_toplevel_info_v1 as cti},
    toplevel_management::v1::client::zcosmic_toplevel_manager_v1 as ctm,
};
use serde::{Deserialize, Serialize};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    protocol::{wl_output, wl_registry, wl_seat},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as fth, ext_foreign_toplevel_list_v1 as ftl,
    },
    workspace::v1::client::{
        ext_workspace_group_handle_v1 as wsg, ext_workspace_handle_v1 as wsh,
        ext_workspace_manager_v1 as wsm,
    },
};

const APP: &str = "moose-sticky-window-launch";
/// Ignore placement changes for this long after a monitor is added/removed, so
/// windows the compositor shuffles off an unplugged monitor don't overwrite
/// their remembered spot.
const OUTPUT_CHANGE_GRACE: Duration = Duration::from_secs(10);
/// How many placement updates to wait for a requested move to show up before
/// we give up and start recording the window's actual position again.
const MOVE_CONFIRM_LIMIT: u32 = 10;

macro_rules! log {
    ($($t:tt)*) => { if std::env::var_os("MSWL_QUIET").is_none() { eprintln!($($t)*) } };
}

// ---------------------------------------------------------------------------
// Persistent data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Placement {
    /// Connector name, e.g. "DP-1", "eDP-1", "HDMI-A-1".
    output: String,
    /// Human-readable monitor description (make/model), informational.
    #[serde(default)]
    output_description: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    workspace_coordinates: Vec<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Db {
    #[serde(default)]
    apps: HashMap<String, Placement>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    /// app_ids that should never be moved or remembered.
    exclude: Vec<String>,
    /// Switch to the target workspace and focus the window after moving it.
    follow: bool,
    /// If the remembered workspace no longer exists on that monitor, move the
    /// window to the trailing empty workspace instead (COSMIC then adds a new
    /// empty one after it, so this effectively creates a workspace).
    create_missing_workspace: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { exclude: Vec::new(), follow: false, create_missing_workspace: true }
    }
}

fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join(APP).join("placements.json")
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join(APP).join("config.json")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME not set"))
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log!("warning: could not parse {}: {e}; using defaults", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

fn save_db(db: &Db) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(db)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if let Err(e) = res {
        log!("error: saving {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Live compositor state
// ---------------------------------------------------------------------------

struct Output {
    global: u32,
    obj: wl_output::WlOutput,
    name: Option<String>,
    description: Option<String>,
}

#[derive(Default)]
struct Group {
    outputs: Vec<wl_output::WlOutput>,
    workspaces: Vec<wsh::ExtWorkspaceHandleV1>,
}

#[derive(Default)]
struct Workspace {
    id: Option<String>,
    name: Option<String>,
    coordinates: Vec<u32>,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Restore {
    /// Not decided yet (waiting for app_id + initial cosmic state).
    Pending,
    /// Window existed before the daemon started, or there was nothing to do.
    Skipped,
    /// We asked the compositor to move it; waiting to see it land.
    Moving { tries: u32 },
    Done,
}

struct Window {
    ext: fth::ExtForeignToplevelHandleV1,
    cosmic: Option<cth::ZcosmicToplevelHandleV1>,
    app_id: Option<String>,
    title: Option<String>,
    got_ext_done: bool,
    got_cosmic_state: bool,
    workspaces: Vec<wsh::ExtWorkspaceHandleV1>,
    sticky: bool,
    restore: Restore,
    target: Option<ObjectId>,
    last_placement: Option<Placement>,
}

struct State {
    config: Config,
    db: Db,
    initial_sync_done: bool,
    managers_bound: bool,
    deferred_globals: Vec<(u32, String, u32)>,
    last_output_change: Option<Instant>,

    seat: Option<wl_seat::WlSeat>,
    toplevel_list: Option<ftl::ExtForeignToplevelListV1>,
    cosmic_info: Option<cti::ZcosmicToplevelInfoV1>,
    cosmic_manager: Option<ctm::ZcosmicToplevelManagerV1>,
    workspace_manager: Option<wsm::ExtWorkspaceManagerV1>,

    outputs: HashMap<ObjectId, Output>,
    groups: HashMap<ObjectId, Group>,
    workspaces: HashMap<ObjectId, Workspace>,
    windows: HashMap<ObjectId, Window>,
    /// cosmic handle id -> ext handle id
    cosmic_to_ext: HashMap<ObjectId, ObjectId>,
}

impl State {
    fn excluded(&self, app_id: &str) -> bool {
        self.config.exclude.iter().any(|e| e == app_id)
    }

    fn in_output_grace(&self) -> bool {
        self.last_output_change
            .is_some_and(|t| t.elapsed() < OUTPUT_CHANGE_GRACE)
    }

    fn group_of(&self, ws: &ObjectId) -> Option<&Group> {
        self.groups
            .values()
            .find(|g| g.workspaces.iter().any(|w| &w.id() == ws))
    }

    /// Where is this window right now?
    fn placement_of(&self, win: &Window) -> Option<Placement> {
        if win.sticky || win.workspaces.len() != 1 {
            return None;
        }
        let ws_id = win.workspaces[0].id();
        let ws = self.workspaces.get(&ws_id)?;
        let group = self.group_of(&ws_id)?;
        let out = self.outputs.get(&group.outputs.first()?.id())?;
        Some(Placement {
            output: out.name.clone()?,
            output_description: out.description.clone(),
            workspace_id: ws.id.clone(),
            workspace_name: ws.name.clone(),
            workspace_coordinates: ws.coordinates.clone(),
        })
    }

    /// Find a live workspace + output matching a remembered placement.
    /// Returns the target workspace, its output, and whether it is the
    /// "new workspace" fallback rather than the remembered one.
    /// `moving` is the window being placed (ignored when checking emptiness).
    fn resolve(
        &self,
        p: &Placement,
        moving: &ObjectId,
    ) -> Option<(wsh::ExtWorkspaceHandleV1, wl_output::WlOutput, bool)> {
        let out = self
            .outputs
            .values()
            .find(|o| o.name.as_deref() == Some(p.output.as_str()))?;
        let group = self
            .groups
            .values()
            .find(|g| g.outputs.iter().any(|o| o.id() == out.obj.id()))?;
        let candidates = || {
            group
                .workspaces
                .iter()
                .filter_map(|h| self.workspaces.get(&h.id()).map(|w| (h, w)))
        };
        let found = p
            .workspace_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .and_then(|id| candidates().find(|(_, w)| w.id.as_ref() == Some(id)))
            .or_else(|| {
                (!p.workspace_coordinates.is_empty())
                    .then(|| candidates().find(|(_, w)| w.coordinates == p.workspace_coordinates))
                    .flatten()
            })
            .or_else(|| {
                p.workspace_name
                    .as_ref()
                    .and_then(|n| candidates().find(|(_, w)| w.name.as_ref() == Some(n)))
            });
        if let Some((h, _)) = found {
            return Some((h.clone(), out.obj.clone(), false));
        }

        // Fallback: COSMIC ignores ext_workspace create requests, but always
        // keeps one empty workspace at the end of each monitor. Putting the
        // window there makes it a real workspace and COSMIC appends a new
        // empty one.
        if !self.config.create_missing_workspace {
            return None;
        }
        let (last, _) = candidates().max_by(|(_, a), (_, b)| a.coordinates.cmp(&b.coordinates))?;
        let last_id = last.id();
        let occupied = self
            .windows
            .iter()
            .any(|(wid, w)| wid != moving && w.workspaces.iter().any(|x| x.id() == last_id));
        if occupied {
            return None;
        }
        Some((last.clone(), out.obj.clone(), true))
    }

    /// Called after every zcosmic_toplevel_info_v1.done: decide restores and
    /// record current placements.
    fn on_info_done(&mut self, qh: &QueueHandle<Self>) {
        let ids: Vec<ObjectId> = self.windows.keys().cloned().collect();
        let mut dirty = false;
        for id in ids {
            self.maybe_restore(&id, qh);
            dirty |= self.record(&id);
        }
        if dirty {
            save_db(&self.db);
        }
    }

    fn maybe_restore(&mut self, id: &ObjectId, qh: &QueueHandle<Self>) {
        let Some(win) = self.windows.get(id) else { return };
        if win.restore != Restore::Pending || !win.got_ext_done || !win.got_cosmic_state {
            return;
        }
        let app_id = win.app_id.clone().unwrap_or_default();
        let title = win.title.clone().unwrap_or_default();

        let decision: Result<(wsh::ExtWorkspaceHandleV1, wl_output::WlOutput, bool), String> = (|| {
            if app_id.is_empty() {
                return Err("no app_id".into());
            }
            if self.excluded(&app_id) {
                return Err("excluded".into());
            }
            let others_open = self.windows.iter().any(|(oid, w)| {
                oid != id && w.app_id.as_deref() == Some(app_id.as_str())
            });
            if others_open {
                return Err("another window of this app is already open".into());
            }
            let p = self.db.apps.get(&app_id).ok_or("no remembered placement")?;
            let (ws, out, fallback) = self
                .resolve(p, id)
                .ok_or_else(|| format!("remembered spot {p:?} no longer exists"))?;
            if win.workspaces.iter().any(|w| w.id() == ws.id()) {
                return Err("already on the remembered workspace".into());
            }
            Ok((ws, out, fallback))
        })();

        let (ws, out, fallback) = match decision {
            Ok(t) => t,
            Err(why) => {
                log!("open  {app_id:?} ({title:?}): not moving: {why}");
                self.windows.get_mut(id).unwrap().restore = Restore::Skipped;
                return;
            }
        };

        let (Some(mgr), Some(cosmic)) = (&self.cosmic_manager, &win.cosmic) else { return };
        let p = &self.db.apps[&app_id];
        let target_name = self
            .workspaces
            .get(&ws.id())
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| "?".into());
        if fallback {
            log!(
                "open  {app_id:?} ({title:?}): workspace {} on {} is gone; moving to new workspace {target_name}",
                p.workspace_name.as_deref().unwrap_or("?"),
                p.output,
            );
        } else {
            log!("open  {app_id:?} ({title:?}): moving to output {} workspace {target_name}", p.output);
        }
        mgr.move_to_ext_workspace(cosmic, &ws, &out);
        if self.config.follow {
            ws.activate();
            if let Some(wm) = &self.workspace_manager {
                wm.commit();
            }
            if let Some(seat) = &self.seat {
                mgr.activate(cosmic, seat);
            }
        }
        let _ = qh;
        let win = self.windows.get_mut(id).unwrap();
        win.restore = Restore::Moving { tries: 0 };
        win.target = Some(ws.id());
    }

    /// Update the remembered placement for a window. Returns true if the db changed.
    fn record(&mut self, id: &ObjectId) -> bool {
        let Some(win) = self.windows.get(id) else { return false };
        let placement = self.placement_of(win);

        // Track where a requested move ended up.
        if let Restore::Moving { tries } = win.restore {
            let landed = win
                .target
                .as_ref()
                .is_some_and(|t| win.workspaces.iter().any(|w| &w.id() == t));
            let win = self.windows.get_mut(id).unwrap();
            if landed {
                win.restore = Restore::Done;
            } else if tries + 1 >= MOVE_CONFIRM_LIMIT {
                log!("warning: move of {:?} never landed; recording actual position", win.app_id);
                win.restore = Restore::Done;
            } else {
                win.restore = Restore::Moving { tries: tries + 1 };
                return false;
            }
        }

        let win = self.windows.get(id).unwrap();
        if win.restore == Restore::Pending {
            return false;
        }
        let Some(app_id) = win.app_id.clone().filter(|a| !a.is_empty()) else { return false };
        if self.excluded(&app_id) || self.in_output_grace() {
            return false;
        }
        let Some(placement) = placement else { return false };

        let win = self.windows.get_mut(id).unwrap();
        win.last_placement = Some(placement.clone());
        if self.db.apps.get(&app_id) != Some(&placement) {
            self.db.apps.insert(app_id, placement);
            return true;
        }
        false
    }

    fn on_window_closed(&mut self, id: &ObjectId) {
        let Some(win) = self.windows.remove(id) else { return };
        if let Some(c) = &win.cosmic {
            self.cosmic_to_ext.remove(&c.id());
            c.destroy();
        }
        win.ext.destroy();
        let app_id = win.app_id.unwrap_or_default();
        // The last-closed window of an app defines where it reopens.
        if let Some(p) = win.last_placement
            && !app_id.is_empty()
            && !self.excluded(&app_id)
            && !self.in_output_grace()
        {
            log!(
                "close {app_id:?}: remembering output {} workspace {}",
                p.output,
                p.workspace_name.as_deref().unwrap_or("?")
            );
            if self.db.apps.get(&app_id) != Some(&p) {
                self.db.apps.insert(app_id, p);
                save_db(&self.db);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland dispatch
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_output" => {
                    let obj: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.insert(
                        obj.id(),
                        Output { global: name, obj, name: None, description: None },
                    );
                    if state.initial_sync_done {
                        state.last_output_change = Some(Instant::now());
                    }
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                }
                // Protocols that reference wl_output objects must be bound
                // after the outputs, or their output_enter events are lost.
                _ if !state.managers_bound => state.deferred_globals.push((name, interface, version)),
                "ext_foreign_toplevel_list_v1" => {
                    state.toplevel_list = Some(registry.bind(name, 1, qh, ()));
                }
                "zcosmic_toplevel_info_v1" if version >= 3 => {
                    state.cosmic_info = Some(registry.bind(name, 3, qh, ()));
                }
                "zcosmic_toplevel_manager_v1" if version >= 4 => {
                    state.cosmic_manager = Some(registry.bind(name, 4, qh, ()));
                }
                "ext_workspace_manager_v1" => {
                    state.workspace_manager = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(id) = state
                    .outputs
                    .iter()
                    .find(|(_, o)| o.global == name)
                    .map(|(id, _)| id.clone())
                {
                    let o = state.outputs.remove(&id).unwrap();
                    log!("monitor removed: {}", o.name.as_deref().unwrap_or("?"));
                    o.obj.release();
                    state.last_output_change = Some(Instant::now());
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(&output.id()) else { return };
        match event {
            wl_output::Event::Name { name } => {
                if state.initial_sync_done {
                    log!("monitor added: {name}");
                }
                o.name = Some(name);
            }
            wl_output::Event::Description { description } => o.description = Some(description),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// --- workspaces -----------------------------------------------------------

impl Dispatch<wsm::ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wsm::ExtWorkspaceManagerV1,
        event: wsm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wsm::Event::WorkspaceGroup { workspace_group } => {
                state.groups.insert(workspace_group.id(), Group::default());
            }
            wsm::Event::Workspace { workspace } => {
                state.workspaces.insert(workspace.id(), Workspace::default());
            }
            wsm::Event::Finished => {
                log!("error: compositor stopped the workspace manager");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    event_created_child!(State, wsm::ExtWorkspaceManagerV1, [
        wsm::EVT_WORKSPACE_GROUP_OPCODE => (wsg::ExtWorkspaceGroupHandleV1, ()),
        wsm::EVT_WORKSPACE_OPCODE => (wsh::ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<wsg::ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        group: &wsg::ExtWorkspaceGroupHandleV1,
        event: wsg::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let gid = group.id();
        if let wsg::Event::Removed = event {
            state.groups.remove(&gid);
            group.destroy();
            return;
        }
        let Some(g) = state.groups.get_mut(&gid) else { return };
        match event {
            wsg::Event::OutputEnter { output } => g.outputs.push(output),
            wsg::Event::OutputLeave { output } => g.outputs.retain(|o| o != &output),
            wsg::Event::WorkspaceEnter { workspace } => g.workspaces.push(workspace),
            wsg::Event::WorkspaceLeave { workspace } => g.workspaces.retain(|w| w != &workspace),
            _ => {}
        }
    }
}

impl Dispatch<wsh::ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        ws: &wsh::ExtWorkspaceHandleV1,
        event: wsh::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = ws.id();
        if let wsh::Event::Removed = event {
            state.workspaces.remove(&id);
            for g in state.groups.values_mut() {
                g.workspaces.retain(|w| w.id() != id);
            }
            ws.destroy();
            return;
        }
        let Some(w) = state.workspaces.get_mut(&id) else { return };
        match event {
            wsh::Event::Id { id } => w.id = Some(id).filter(|i| !i.is_empty()),
            wsh::Event::Name { name } => w.name = Some(name),
            wsh::Event::Coordinates { coordinates } => {
                w.coordinates = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            _ => {}
        }
    }
}

// --- toplevels ------------------------------------------------------------

impl Dispatch<ftl::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ftl::ExtForeignToplevelListV1,
        event: ftl::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ftl::Event::Toplevel { toplevel } => {
                let cosmic = state
                    .cosmic_info
                    .as_ref()
                    .map(|info| info.get_cosmic_toplevel(&toplevel, qh, ()));
                if let Some(c) = &cosmic {
                    state.cosmic_to_ext.insert(c.id(), toplevel.id());
                }
                let restore = if state.initial_sync_done { Restore::Pending } else { Restore::Skipped };
                state.windows.insert(
                    toplevel.id(),
                    Window {
                        ext: toplevel,
                        cosmic,
                        app_id: None,
                        title: None,
                        got_ext_done: false,
                        got_cosmic_state: false,
                        workspaces: Vec::new(),
                        sticky: false,
                        restore,
                        target: None,
                        last_placement: None,
                    },
                );
            }
            ftl::Event::Finished => {
                log!("error: compositor stopped the toplevel list");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    event_created_child!(State, ftl::ExtForeignToplevelListV1, [
        ftl::EVT_TOPLEVEL_OPCODE => (fth::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<fth::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &fth::ExtForeignToplevelHandleV1,
        event: fth::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        if let fth::Event::Closed = event {
            state.on_window_closed(&id);
            return;
        }
        let Some(w) = state.windows.get_mut(&id) else { return };
        match event {
            fth::Event::AppId { app_id } => w.app_id = Some(app_id),
            fth::Event::Title { title } => w.title = Some(title),
            fth::Event::Done => w.got_ext_done = true,
            _ => {}
        }
    }
}

impl Dispatch<cti::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &cti::ZcosmicToplevelInfoV1,
        event: cti::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let cti::Event::Done = event
            && state.initial_sync_done
        {
            state.on_info_done(qh);
        }
    }
}

impl Dispatch<cth::ZcosmicToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &cth::ZcosmicToplevelHandleV1,
        event: cth::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(ext_id) = state.cosmic_to_ext.get(&handle.id()) else { return };
        let Some(w) = state.windows.get_mut(ext_id) else { return };
        match event {
            cth::Event::ExtWorkspaceEnter { workspace }
                if !w.workspaces.contains(&workspace) => {
                    w.workspaces.push(workspace);
                }
            cth::Event::ExtWorkspaceLeave { workspace } => w.workspaces.retain(|x| x != &workspace),
            cth::Event::State { state: s } => {
                w.got_cosmic_state = true;
                w.sticky = s
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .any(|v| v == cth::State::Sticky as u32);
            }
            _ => {}
        }
    }
}

impl Dispatch<ctm::ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ctm::ZcosmicToplevelManagerV1,
        _: ctm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn dump(state: &State) {
    println!("Monitors:");
    for o in state.outputs.values() {
        println!("  {:<10} {}", o.name.as_deref().unwrap_or("?"), o.description.as_deref().unwrap_or(""));
    }
    println!("Workspaces:");
    for g in state.groups.values() {
        let outs: Vec<_> = g
            .outputs
            .iter()
            .filter_map(|o| state.outputs.get(&o.id()).and_then(|o| o.name.clone()))
            .collect();
        println!("  on {}:", outs.join(","));
        for h in &g.workspaces {
            if let Some(w) = state.workspaces.get(&h.id()) {
                println!(
                    "    name={:?} id={:?} coords={:?}",
                    w.name.as_deref().unwrap_or(""),
                    w.id.as_deref().unwrap_or(""),
                    w.coordinates
                );
            }
        }
    }
    println!("Windows:");
    for w in state.windows.values() {
        let p = state.placement_of(w);
        println!(
            "  {:<40} {:?} -> {}",
            w.app_id.as_deref().unwrap_or("?"),
            w.title.as_deref().unwrap_or(""),
            p.map(|p| format!("{} / ws {}", p.output, p.workspace_name.unwrap_or_default()))
                .unwrap_or_else(|| "(unknown / sticky)".into())
        );
    }
    println!("\nState file: {}", state_path().display());
    println!("Config file: {}", config_path().display());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dump_only = args.iter().any(|a| a == "--dump");
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "{APP}: reopen COSMIC windows on the monitor/workspace they were last on.\n\n\
             Usage: {APP} [--dump]\n\n  --dump   print monitors, workspaces and windows, then exit\n\n\
             State:  {}\nConfig: {}  (optional) {{\"exclude\": [\"app.id\"], \"follow\": false}}",
            state_path().display(),
            config_path().display()
        );
        return;
    }

    let conn = Connection::connect_to_env().unwrap_or_else(|e| {
        eprintln!("error: cannot connect to the Wayland compositor: {e}");
        std::process::exit(1);
    });
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());

    let mut state = State {
        config: load_json(&config_path()),
        db: load_json(&state_path()),
        initial_sync_done: false,
        managers_bound: false,
        deferred_globals: Vec::new(),
        last_output_change: None,
        seat: None,
        toplevel_list: None,
        cosmic_info: None,
        cosmic_manager: None,
        workspace_manager: None,
        outputs: HashMap::new(),
        groups: HashMap::new(),
        workspaces: HashMap::new(),
        windows: HashMap::new(),
        cosmic_to_ext: HashMap::new(),
    };

    // Bind outputs (and their names) first, then everything that refers to them.
    queue.roundtrip(&mut state).expect("wayland roundtrip failed");
    queue.roundtrip(&mut state).expect("wayland roundtrip failed");
    state.managers_bound = true;
    for (name, interface, version) in std::mem::take(&mut state.deferred_globals) {
        State::event(
            &mut state,
            &registry,
            wl_registry::Event::Global { name, interface, version },
            &(),
            &conn,
            &qh,
        );
    }
    // cosmic-comp sends per-window state on its next refresh rather than in
    // reply to our request, so wait (briefly) until every window has reported.
    for i in 0..80 {
        queue.roundtrip(&mut state).expect("wayland roundtrip failed");
        if i >= 1 && state.windows.values().all(|w| w.got_cosmic_state) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let missing: Vec<&str> = [
        ("ext_foreign_toplevel_list_v1", state.toplevel_list.is_some()),
        ("zcosmic_toplevel_info_v1 (v3+)", state.cosmic_info.is_some()),
        ("zcosmic_toplevel_manager_v1 (v4+)", state.cosmic_manager.is_some()),
        ("ext_workspace_manager_v1", state.workspace_manager.is_some()),
    ]
    .into_iter()
    .filter(|(_, ok)| !ok)
    .map(|(n, _)| n)
    .collect();
    if !missing.is_empty() {
        eprintln!(
            "error: compositor does not offer: {}.\nThis tool needs COSMIC (cosmic-comp) and must run unsandboxed.",
            missing.join(", ")
        );
        std::process::exit(1);
    }

    if dump_only {
        dump(&state);
        return;
    }

    // Windows already open when we start are never moved, but their current
    // spot is remembered.
    state.initial_sync_done = true;
    let ids: Vec<_> = state.windows.keys().cloned().collect();
    let mut dirty = false;
    for id in &ids {
        dirty |= state.record(id);
    }
    if dirty {
        save_db(&state.db);
    }
    let seen: HashSet<_> = state.windows.values().filter_map(|w| w.app_id.clone()).collect();
    log!(
        "{APP} running: {} monitors, {} workspaces, {} windows ({} apps), {} remembered apps",
        state.outputs.len(),
        state.workspaces.len(),
        state.windows.len(),
        seen.len(),
        state.db.apps.len()
    );

    loop {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            eprintln!("error: wayland connection lost: {e}");
            std::process::exit(1);
        }
    }
}

//! X11 sensor + native wheel overlay — the proven path.
//!
//! Lifted from the Stage-3 spike (`uruk-convert-spikes/src/bin/sensor.rs`,
//! three consecutive end-to-end runs) and productionized:
//!
//! - **detect**: XFixes selection-ownership watch on `XdndSelection`
//!   (the XDND spec obliges the source to own it for the whole drag) +
//!   `XQueryPointer` mask polling for button/modifier edges.
//! - **classify**: the source's `XdndTypeList` property is world-readable;
//!   a bystander `XConvertSelection` opportunistically yields the actual
//!   paths (GTK/Qt/generic sources answer non-participants).
//! - **summon**: on the configured modifier's edge mid-drag, a native
//!   override-redirect window (NOT a Tauri/GTK window — GTK interposes
//!   its own DnD stack) is created, marked `XdndAware` BEFORE mapping,
//!   and ANCHORED at the summon point. (The spike let the overlay follow
//!   the cursor; production anchors it — reaching a segment is the whole
//!   interaction.)
//! - **participate**: the source's own traversal hands the drag to the
//!   overlay (`XdndEnter`/`Position`/`Status`); hover = segment under
//!   the position coordinates; the wheel is re-rendered natively from
//!   [`crate::render`].
//! - **convert**: `XdndDrop` → selection conversion → file paths →
//!   segment from the last position → `SensorEvent::ConvertRequested`.
//!   Results come back as [`SensorCommand::ShowToast`] and are shown in
//!   a native toast window.
//!
//! Two spike findings are encoded: sources send `XdndDrop` *after* the
//! physical button release (600 ms grace before concluding a miss), and
//! the harness in `qa/x11-e2e` is the ground truth for this module.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::geom;
use crate::{
    DragSensor, SensorCommand, SensorConfig, SensorError, SensorEvent, SummonModifier, Toast,
    ToastKind, WheelModel, WheelRequest, WheelResolver,
};

const XA_ATOM: u32 = 4;
const XA_CARDINAL: u32 = 6;

const WHEEL_PX: u16 = geom::WHEEL_SIZE as u16;
const GRACE_MS: u64 = 600;

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Parse a `text/uri-list` payload into local file paths.
/// Handles CRLF/LF lines, `#` comments, `file://` and `file://localhost`
/// authorities, and percent-encoding. Non-file URIs are skipped.
pub fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else {
            continue;
        };
        // Authority: up to the first '/'. Only "" and "localhost" are us.
        let path_part = if let Some(stripped) = rest.strip_prefix("localhost/") {
            format!("/{stripped}")
        } else if rest.starts_with('/') {
            rest.to_string()
        } else {
            continue; // file://somehost/... — a remote authority, not a path
        };
        out.push(PathBuf::from(percent_decode(&path_part)));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is this XDND type list a file drag?
pub fn types_are_file_drag(types: &[String]) -> bool {
    types.iter().any(|t| t == "text/uri-list")
}

// ---------------------------------------------------------------------------
// Atoms
// ---------------------------------------------------------------------------

struct Atoms {
    xdnd_selection: u32,
    xdnd_type_list: u32,
    xdnd_enter: u32,
    xdnd_position: u32,
    xdnd_status: u32,
    xdnd_drop: u32,
    xdnd_leave: u32,
    xdnd_finished: u32,
    xdnd_action_copy: u32,
    xdnd_aware: u32,
    text_uri_list: u32,
    wm_name: u32,
    net_wm_name: u32,
    wm_class: u32,
    utf8_string: u32,
    string_atom: u32,
}

fn intern_atoms(conn: &RustConnection) -> Result<Atoms, SensorError> {
    let get = |name: &str| -> Result<u32, SensorError> {
        conn.intern_atom(false, name.as_bytes())
            .map_err(|e| SensorError::new("x11", format!("intern_atom {name}: {e}")))?
            .reply()
            .map(|r| r.atom)
            .map_err(|e| SensorError::new("x11", format!("intern_atom {name}: {e}")))
    };
    Ok(Atoms {
        xdnd_selection: get("XdndSelection")?,
        xdnd_type_list: get("XdndTypeList")?,
        xdnd_enter: get("XdndEnter")?,
        xdnd_position: get("XdndPosition")?,
        xdnd_status: get("XdndStatus")?,
        xdnd_drop: get("XdndDrop")?,
        xdnd_leave: get("XdndLeave")?,
        xdnd_finished: get("XdndFinished")?,
        xdnd_action_copy: get("XdndActionCopy")?,
        xdnd_aware: get("XdndAware")?,
        text_uri_list: get("text/uri-list")?,
        wm_name: get("WM_NAME")?,
        net_wm_name: get("_NET_WM_NAME")?,
        wm_class: get("WM_CLASS")?,
        utf8_string: get("UTF8_STRING")?,
        string_atom: get("STRING")?,
    })
}

fn set_prop32(
    conn: &RustConnection,
    mode: PropMode,
    win: u32,
    prop: u32,
    ty: u32,
    data: &[u32],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    conn.change_property(mode, win, prop, ty, 32, data.len() as u32, &bytes)?;
    Ok(())
}

fn set_prop8(
    conn: &RustConnection,
    mode: PropMode,
    win: u32,
    prop: u32,
    ty: u32,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    conn.change_property(mode, win, prop, ty, 8, data.len() as u32, data)?;
    Ok(())
}

fn send_client_message(
    conn: &RustConnection,
    dest: u32,
    type_atom: u32,
    data: [u32; 5],
) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::protocol::xproto::{ClientMessageData, ClientMessageEvent, CLIENT_MESSAGE_EVENT};
    let ev = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: dest,
        type_: type_atom,
        data: ClientMessageData::from(data),
    };
    conn.send_event(false, dest, EventMask::NO_EVENT, ev)?;
    conn.flush()?;
    Ok(())
}

fn read_property_bytes(
    conn: &RustConnection,
    win: u32,
    property: u32,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    let reply = conn
        .get_property(false, win, property, 0u32, 0u32, 1u32 << 22)?
        .reply()?;
    if reply.type_ == 0 {
        return Ok(None);
    }
    Ok(Some(reply.value))
}

/// Keycodes producing any of `keysyms` in the current keyboard mapping.
fn keycodes_for_keysyms(
    conn: &RustConnection,
    keysyms: &[u32],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - first + 1;
    let map = conn.get_keyboard_mapping(first, count)?.reply()?;
    let per = map.keysyms_per_keycode as usize;
    let mut out = Vec::new();
    for i in 0..count as usize {
        let syms = &map.keysyms[i * per..(i + 1) * per];
        if syms.iter().any(|s| keysyms.contains(s)) {
            out.push(first + i as u8);
        }
    }
    Ok(out)
}

fn modifier_mask_bit(m: SummonModifier) -> u16 {
    match m {
        SummonModifier::Shift => 1 << 0,
        SummonModifier::Control => 1 << 2,
        SummonModifier::Alt => 1 << 3, // Mod1
    }
}

fn modifier_keysyms(m: SummonModifier) -> &'static [u32] {
    match m {
        SummonModifier::Shift => &[0xFFE1, 0xFFE2],
        SummonModifier::Control => &[0xFFE3, 0xFFE4],
        SummonModifier::Alt => &[0xFFE9, 0xFFEA, 0xFF7E],
    }
}

// ---------------------------------------------------------------------------
// Diagnostic log (env-gated): URUK_SENSOR_LOG=/path
// ---------------------------------------------------------------------------

struct SLog {
    file: Option<std::fs::File>,
    t0: Instant,
}

impl SLog {
    fn new() -> SLog {
        let file = std::env::var("URUK_SENSOR_LOG").ok().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        SLog {
            file,
            t0: Instant::now(),
        }
    }
    fn line(&mut self, msg: &str) {
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "[+{:>7}ms] {}", self.t0.elapsed().as_millis(), msg);
            let _ = f.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay + toast windows
// ---------------------------------------------------------------------------

struct Overlay {
    win: u32,
    x: i16,
    y: i16,
    model: Option<WheelModel>,
    paths: Vec<PathBuf>,
    hover: Option<usize>,
    source: Option<u32>,
    last_pos: Option<(i16, i16)>,
    painted: bool,
    drop_handled: bool,
}

struct ToastWin {
    win: u32,
    w: u16,
    h: u16,
    kind: ToastKind,
    title: String,
    body: String,
    until: Instant,
}

// ---------------------------------------------------------------------------
// The sensor
// ---------------------------------------------------------------------------

pub struct X11Sensor;

impl X11Sensor {
    pub fn new() -> Self {
        X11Sensor
    }
}

impl Default for X11Sensor {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_sensor() -> Option<Box<dyn DragSensor>> {
    match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => Some(Box::new(X11Sensor::new())),
        _ => None,
    }
}

impl DragSensor for X11Sensor {
    fn platform(&self) -> &'static str {
        "x11"
    }

    fn start(
        &mut self,
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        let mut run = Run::new(config, events, commands, resolve)?;
        run.event_loop()
    }
}

struct Run {
    log: SLog,
    conn: RustConnection,
    atoms: Atoms,
    root: u32,
    screen_w: u16,
    screen_h: u16,
    depth: u8,
    visual: u32,
    home: u32,
    data_prop: u32,
    peek_prop: u32,
    config: SensorConfig,
    events: Sender<SensorEvent>,
    commands: Receiver<SensorCommand>,
    resolve: WheelResolver,
    esc_codes: Vec<u8>,
    // drag state
    pointer: (i16, i16),
    button_down: bool,
    t_press: Option<Instant>,
    candidate: bool,
    press_point: (i16, i16),
    xdnd_owner: Option<u32>,
    drag_gen: u64,
    types: Vec<String>,
    classified_gen: u64,
    peek_due: Option<Instant>,
    peek_pending_gen: Option<u64>,
    peeked_paths: Vec<PathBuf>,
    mod_held: bool,
    t_mod: Option<Instant>,
    suppress_summon: bool,
    overlay: Option<Overlay>,
    dismiss_due: Option<Instant>,
    drag_ended_sent: bool,
    toast: Option<ToastWin>,
    capture_done: bool,
}

impl Run {
    fn new(
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<Run, SensorError> {
        let mut log = SLog::new();
        let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
            SensorError::new(
                "x11_unavailable",
                format!("cannot connect to X server: {e}"),
            )
        })?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (screen_w, screen_h) = (screen.width_in_pixels, screen.height_in_pixels);
        let depth = screen.root_depth;
        let visual = screen.root_visual;
        let atoms = intern_atoms(&conn)?;
        let data_prop = conn
            .intern_atom(false, b"URUK_XDND_DATA")
            .map_err(|e| SensorError::new("x11", format!("intern data prop: {e}")))?
            .reply()
            .map(|r| r.atom)
            .map_err(|e| SensorError::new("x11", format!("intern data prop: {e}")))?;
        let peek_prop = conn
            .intern_atom(false, b"URUK_XDND_PEEK")
            .map_err(|e| SensorError::new("x11", format!("intern peek prop: {e}")))?
            .reply()
            .map(|r| r.atom)
            .map_err(|e| SensorError::new("x11", format!("intern peek prop: {e}")))?;

        // Unmapped 1x1 home window: requestor identity for bystander peeks.
        let home = conn
            .generate_id()
            .map_err(|e| SensorError::new("x11", format!("gen id: {e}")))?;
        conn.create_window(
            depth,
            home,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new(),
        )
        .map_err(|e| SensorError::new("x11", format!("create home window: {e}")))?;

        // Channel 1: XFixes selection-ownership watch on XdndSelection.
        conn.xfixes_query_version(5, 0)
            .map_err(|e| SensorError::new("x11", format!("xfixes version: {e}")))?;
        conn.xfixes_select_selection_input(
            root,
            atoms.xdnd_selection,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| SensorError::new("x11", format!("xfixes select: {e}")))?;
        // Channel 2: XI2 raw key events (Escape; modifier edges are read
        // off the query_pointer mask like the spike, which proved both).
        let _ = xinput::xi_query_version(&conn, 2, 2);
        let mask = XIEventMask::RAW_KEY_PRESS;
        let _ = xinput::xi_select_events(
            &conn,
            root,
            &[xinput::EventMask {
                deviceid: 1,
                mask: vec![mask],
            }],
        );
        conn.flush()
            .map_err(|e| SensorError::new("x11", format!("flush: {e}")))?;

        let esc_codes = keycodes_for_keysyms(&conn, &[0xFF1B]).unwrap_or_default();
        // Warm the renderer (font discovery + TTF parsing + first
        // rasterization) NOW, at agent startup — otherwise the first
        // summon pays ~150 ms of one-time costs inside the gesture.
        {
            let warm_start = Instant::now();
            let dummy = WheelModel {
                segments: vec!["jpg".into(), "png".into(), "webp".into()],
                hub_ext: "PNG".into(),
                hub_title: "warmup.png".into(),
                file_count: 1,
            };
            let _ = crate::render::render_wheel(&dummy, None);
            log.line(&format!(
                "renderer warmed in {}ms",
                warm_start.elapsed().as_millis()
            ));
        }
        log.line("armed: XFixes watch on XdndSelection + query_pointer mask + XI2 raw keys (Esc)");
        Ok(Run {
            log,
            conn,
            atoms,
            root,
            screen_w: screen_w,
            screen_h: screen_h,
            depth,
            visual,
            home,
            data_prop,
            peek_prop,
            config,
            events,
            commands,
            resolve,
            esc_codes,
            pointer: (0, 0),
            button_down: false,
            t_press: None,
            candidate: false,
            press_point: (0, 0),
            xdnd_owner: None,
            drag_gen: 0,
            types: Vec::new(),
            classified_gen: 0,
            peek_due: None,
            peek_pending_gen: None,
            peeked_paths: Vec::new(),
            mod_held: false,
            t_mod: None,
            suppress_summon: false,
            overlay: None,
            dismiss_due: None,
            drag_ended_sent: false,
            toast: None,
            capture_done: false,
        })
    }

    fn event_loop(&mut self) -> Result<(), SensorError> {
        loop {
            // 1. X events.
            loop {
                let ev = match self.conn.poll_for_event() {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(_) => break,
                };
                self.handle_event(ev);
            }
            // 2. Host commands.
            if self.drain_commands() {
                self.dismiss_overlay("shutdown");
                self.dismiss_toast();
                return Ok(());
            }
            // 3. Pointer edges (button + modifier) from the mask poll.
            self.poll_pointer();
            // 4. Housekeeping: peek, grace, toast expiry.
            if let Some(due) = self.peek_due {
                if Instant::now() >= due {
                    self.peek_due = None;
                    self.run_peek();
                }
            }
            if let Some(due) = self.dismiss_due {
                if Instant::now() >= due {
                    self.dismiss_due = None;
                    if self
                        .overlay
                        .as_ref()
                        .map(|o| !o.drop_handled)
                        .unwrap_or(false)
                    {
                        self.log
                            .line("grace expired with no XdndDrop — the drop missed the overlay");
                        self.dismiss_overlay("grace-expired");
                        self.end_drag_if_finished();
                    }
                }
            }
            if let Some(t) = &self.toast {
                if Instant::now() >= t.until {
                    self.dismiss_toast();
                }
            }
            let busy = self.overlay.is_some() || self.candidate || self.xdnd_owner.is_some();
            std::thread::sleep(Duration::from_millis(if busy { 2 } else { 10 }));
        }
    }

    /// Returns true on Shutdown.
    fn drain_commands(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(SensorCommand::Shutdown) => return true,
                Ok(SensorCommand::ShowToast(t)) => self.show_toast(t),
                Ok(SensorCommand::UpdateConfig(c)) => {
                    self.config = c;
                    if !self.config.enabled {
                        self.dismiss_overlay("disabled");
                    }
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::XfixesSelectionNotify(sn) => {
                if sn.selection == self.atoms.xdnd_selection
                    && sn.subtype == x11rb::protocol::xfixes::SelectionEvent::SET_SELECTION_OWNER
                {
                    self.on_xdnd_owner(sn.owner);
                }
            }
            Event::XinputRawKeyPress(rk) => {
                if self.esc_codes.contains(&(rk.detail as u8)) && self.overlay.is_some() {
                    self.log
                        .line("ESC during overlay — cancelling this drag's summon");
                    self.dismiss_overlay("esc");
                    self.suppress_summon = true;
                }
            }
            Event::ClientMessage(cm) => {
                if self.overlay.as_ref().map(|o| o.win) == Some(cm.window) {
                    self.handle_overlay_message(cm);
                }
            }
            Event::SelectionNotify(sn) => {
                self.handle_selection_notify(sn);
            }
            Event::Expose(ex) => {
                if self.overlay.as_ref().map(|o| o.win) == Some(ex.window) {
                    self.paint_overlay();
                } else if self.toast.as_ref().map(|t| t.win) == Some(ex.window) {
                    self.paint_toast();
                }
            }
            _ => {}
        }
    }

    // -- pointer edges --------------------------------------------------------

    fn poll_pointer(&mut self) {
        let snapshot = {
            let cookie = self.conn.query_pointer(self.root).ok();
            cookie
                .and_then(|c| c.reply().ok())
                .map(|r| (r.root_x, r.root_y, u16::from(r.mask)))
        };
        let Some((rx, ry, mask)) = snapshot else {
            return;
        };
        self.pointer = (rx, ry);
        let btn = mask & (1 << 8) != 0;
        if btn && !self.button_down {
            self.button_down = true;
            self.press_point = (rx, ry);
            self.t_press = Some(Instant::now());
            self.candidate = false;
            self.drag_gen += 1;
            self.drag_ended_sent = false;
        } else if !btn && self.button_down {
            self.button_down = false;
            if let Some(ov) = &self.overlay {
                if !ov.drop_handled {
                    // Real sources send XdndDrop AFTER the physical
                    // release — grace before concluding a miss (spike).
                    self.dismiss_due = Some(Instant::now() + Duration::from_millis(GRACE_MS));
                }
            }
            self.end_drag_if_finished();
        }
        if self.button_down && !self.candidate {
            let dx = (rx - self.press_point.0) as i32;
            let dy = (ry - self.press_point.1) as i32;
            if dx * dx + dy * dy > 64 {
                self.candidate = true;
            }
        }
        // Modifier edge (configured key) from the mask.
        let bit = modifier_mask_bit(self.config.modifier);
        let held = mask & bit != 0;
        if held && !self.mod_held {
            self.mod_held = true;
            self.t_mod = Some(Instant::now());
            self.try_summon();
        } else if !held && self.mod_held {
            self.mod_held = false;
        }
    }

    // -- detection + classification ------------------------------------------

    fn on_xdnd_owner(&mut self, owner: u32) {
        if owner != 0 && self.xdnd_owner != Some(owner) {
            self.xdnd_owner = Some(owner);
            let after_press = self.t_press.map(|t| t.elapsed().as_millis()).unwrap_or(0);
            self.log.line(&format!(
                "XDND owner 0x{owner:x} claimed ({after_press}ms after button press)"
            ));
            self.classify(owner);
            if self.peeked_paths.is_empty() {
                self.peek_due = Some(Instant::now() + Duration::from_millis(120));
            }
        } else if owner == 0 && self.xdnd_owner.is_some() {
            self.xdnd_owner = None;
            self.log.line("XDND owner released");
            if self.overlay.is_some() && self.dismiss_due.is_none() {
                self.dismiss_due = Some(Instant::now() + Duration::from_millis(GRACE_MS));
            }
            self.end_drag_if_finished();
        }
    }

    fn classify(&mut self, owner: u32) {
        let types = (|| -> Option<Vec<String>> {
            let reply = self
                .conn
                .get_property(false, owner, self.atoms.xdnd_type_list, 0u32, 0u32, 64u32)
                .ok()?
                .reply()
                .ok()?;
            let mut names = Vec::new();
            if let Some(iter) = reply.value32() {
                for a in iter {
                    if let Ok(cookie) = self.conn.get_atom_name(a) {
                        if let Ok(r) = cookie.reply() {
                            names.push(String::from_utf8_lossy(&r.name).into_owned());
                        }
                    }
                }
            }
            Some(names)
        })()
        .unwrap_or_default();
        self.types = types;
        if self.classified_gen != self.drag_gen {
            self.classified_gen = self.drag_gen;
            self.log
                .line(&format!("classified: types={:?}", self.types));
            let _ = self.events.send(SensorEvent::DragClassified {
                mime_types: self.types.clone(),
            });
        }
    }

    fn run_peek(&mut self) {
        if self.xdnd_owner.is_none() || self.peek_pending_gen == Some(self.drag_gen) {
            return;
        }
        let req = self.conn.convert_selection(
            self.home,
            self.atoms.xdnd_selection,
            self.atoms.text_uri_list,
            self.peek_prop,
            x11rb::CURRENT_TIME,
        );
        if req.is_ok() {
            let _ = self.conn.flush();
            self.peek_pending_gen = Some(self.drag_gen);
        }
    }

    fn on_peek_bytes(&mut self, bytes: &[u8]) {
        let paths = parse_uri_list(bytes);
        if paths.is_empty() {
            self.log.line("peek: source answered but no file paths");
            return;
        }
        self.log
            .line(&format!("peek: {} path(s) known pre-drop", paths.len()));
        self.peeked_paths = paths;
        // If the wheel is up in the pending state, resolve and paint now.
        let needs_model = self
            .overlay
            .as_ref()
            .map(|o| o.model.is_none())
            .unwrap_or(false);
        if needs_model {
            if let Some(model) = (self.resolve)(&WheelRequest {
                paths: self.peeked_paths.clone(),
                mime_types: self.types.clone(),
            }) {
                if let Some(ov) = &mut self.overlay {
                    ov.model = Some(model);
                    ov.paths = self.peeked_paths.clone();
                }
                self.paint_overlay();
            }
        } else if let Some(ov) = &mut self.overlay {
            if ov.paths.is_empty() {
                ov.paths = self.peeked_paths.clone();
            }
        }
    }

    // -- summon + overlay ------------------------------------------------------

    fn try_summon(&mut self) {
        if !self.config.enabled || self.suppress_summon || self.overlay.is_some() {
            return;
        }
        let confirmed = self.xdnd_owner.is_some();
        if !confirmed && !self.candidate {
            return;
        }
        // A definite non-file drag (type list read, no uri-list) never summons.
        if confirmed && !self.types.is_empty() && !types_are_file_drag(&self.types) {
            return;
        }
        // If we have no paths yet, ask the source right now as well.
        if self.peeked_paths.is_empty() && confirmed {
            self.run_peek();
        }
        let model = (self.resolve)(&WheelRequest {
            paths: self.peeked_paths.clone(),
            mime_types: self.types.clone(),
        });
        match model {
            Some(m) => self.summon_overlay(Some(m), self.peeked_paths.clone()),
            None => {
                // File drag (uri-list) but the host could not resolve a
                // model yet (paths unknown): participate anyway — the
                // drop/enter payload will let us resolve before painting
                // segments. Only when it really is a file drag.
                if types_are_file_drag(&self.types) {
                    self.summon_overlay(None, Vec::new());
                }
            }
        }
    }

    fn summon_overlay(&mut self, model: Option<WheelModel>, paths: Vec<PathBuf>) {
        let t_edge = self.t_mod;
        let win = match self.conn.generate_id() {
            Ok(w) => w,
            Err(_) => return,
        };
        let (px, py) = self.pointer;
        let ox = (px - (WHEEL_PX / 2) as i16)
            .max(0)
            .min((self.screen_w - WHEEL_PX).max(0) as i16);
        let oy = (py - (WHEEL_PX / 2) as i16)
            .max(0)
            .min((self.screen_h - WHEEL_PX).max(0) as i16);
        let aux = CreateWindowAux::new()
            .background_pixel(0x070f22)
            .override_redirect(1u32)
            .event_mask(EventMask::EXPOSURE);
        if self
            .conn
            .create_window(
                self.depth,
                win,
                self.root,
                ox,
                oy,
                WHEEL_PX,
                WHEEL_PX,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &aux,
            )
            .is_err()
        {
            self.log.line("overlay create_window failed");
            return;
        }
        // XdndAware BEFORE mapping — the source's traversal must find it.
        let _ = set_prop32(
            &self.conn,
            PropMode::REPLACE,
            win,
            self.atoms.xdnd_aware,
            XA_CARDINAL,
            &[5],
        );
        // A stable name/class so QA tooling (and screen readers) can find it.
        let _ = set_prop8(
            &self.conn,
            PropMode::REPLACE,
            win,
            self.atoms.wm_name,
            self.atoms.string_atom,
            b"Uruk Convert Wheel",
        );
        let _ = set_prop8(
            &self.conn,
            PropMode::REPLACE,
            win,
            self.atoms.net_wm_name,
            self.atoms.utf8_string,
            b"Uruk Convert Wheel",
        );
        let _ = set_prop8(
            &self.conn,
            PropMode::REPLACE,
            win,
            self.atoms.wm_class,
            self.atoms.string_atom,
            b"uruk-wheel\0UrukConvert",
        );
        let _ = self.conn.map_window(win);
        let _ = self.conn.flush();
        let since_edge = t_edge.map(|t| t.elapsed().as_millis()).unwrap_or(0);
        self.log.line(&format!(
            "SUMMONED wheel 0x{win:x} at ({ox},{oy}) — {since_edge}ms after modifier edge (incl. resolve)"
        ));
        self.overlay = Some(Overlay {
            win,
            x: ox,
            y: oy,
            model,
            paths,
            hover: None,
            source: None,
            last_pos: None,
            painted: false,
            drop_handled: false,
        });
        self.capture_done = false;
        let _ = self.events.send(SensorEvent::Summoned {
            x: px as i32,
            y: py as i32,
        });
        self.paint_overlay();
    }

    fn paint_overlay(&mut self) {
        let (win, model, hover) = match &self.overlay {
            Some(ov) => (ov.win, ov.model.clone(), ov.hover),
            None => return,
        };
        let model = model.unwrap_or(WheelModel {
            segments: Vec::new(),
            hub_ext: "…".into(),
            hub_title: String::new(),
            file_count: 0,
        });
        let pm = crate::render::render_wheel(&model, hover);
        let bgra = crate::render::pixmap_to_bgra(&pm);
        self.put_image(win, WHEEL_PX, WHEEL_PX, &bgra);
        let first = self.overlay.as_ref().map(|o| !o.painted).unwrap_or(false);
        if first {
            if let Some(ov) = &mut self.overlay {
                ov.painted = true;
            }
            let since_edge = self.t_mod.map(|t| t.elapsed().as_millis()).unwrap_or(0);
            self.log.line(&format!(
                "wheel first paint complete — {since_edge}ms after modifier edge"
            ));
            if !self.capture_done {
                self.capture_done = true;
                self.capture_root_png();
            }
        }
    }

    fn dismiss_overlay(&mut self, reason: &str) {
        if let Some(ov) = self.overlay.take() {
            let _ = self.conn.unmap_window(ov.win);
            let _ = self.conn.destroy_window(ov.win);
            let _ = self.conn.flush();
            self.log.line(&format!("overlay dismissed ({reason})"));
        }
        self.dismiss_due = None;
    }

    /// Full drag teardown when the gesture is over: reset per-drag state
    /// and tell the host (once) if it ended without a conversion.
    fn end_drag_if_finished(&mut self) {
        if self.button_down || self.xdnd_owner.is_some() {
            return;
        }
        if self.overlay.is_some() {
            // Still inside the post-release grace window; the dismiss
            // timer will call us again.
            return;
        }
        if !self.drag_ended_sent && (self.candidate || self.classified_gen == self.drag_gen) {
            self.drag_ended_sent = true;
            let _ = self.events.send(SensorEvent::DragEnded);
        }
        self.candidate = false;
        self.types.clear();
        self.peeked_paths.clear();
        self.peek_pending_gen = None;
        self.peek_due = None;
        self.suppress_summon = false;
    }

    // -- XDND participation ----------------------------------------------------

    fn handle_overlay_message(&mut self, cm: x11rb::protocol::xproto::ClientMessageEvent) {
        let data = cm.data.as_data32();
        let (win, ox, oy) = match &self.overlay {
            Some(o) => (o.win, o.x, o.y),
            None => return,
        };
        if cm.type_ == self.atoms.xdnd_enter {
            let src = data[0];
            if let Some(ov) = &mut self.overlay {
                ov.source = Some(src);
            }
            self.log.line(&format!("overlay: XdndEnter from 0x{src:x}"));
            // If we still don't know the paths, ask as the entered target.
            if self.peeked_paths.is_empty() {
                let _ = self.conn.convert_selection(
                    win,
                    self.atoms.xdnd_selection,
                    self.atoms.text_uri_list,
                    self.peek_prop,
                    x11rb::CURRENT_TIME,
                );
                let _ = self.conn.flush();
                self.peek_pending_gen = Some(self.drag_gen);
            }
        } else if cm.type_ == self.atoms.xdnd_position {
            let xy = data[2];
            let px = (xy >> 16) as i16;
            let py = (xy & 0xFFFF) as i16;
            let mut hover_changed = false;
            let mut seg: Option<usize> = None;
            if let Some(ov) = &mut self.overlay {
                ov.last_pos = Some((px, py));
                if let Some(m) = &ov.model {
                    seg = geom::segment_at(
                        m.segments.len(),
                        (px - (ox + (WHEEL_PX / 2) as i16)) as f32,
                        (py - (oy + (WHEEL_PX / 2) as i16)) as f32,
                    );
                    if seg != ov.hover {
                        ov.hover = seg;
                        hover_changed = true;
                    }
                }
            }
            if hover_changed {
                self.paint_overlay();
            }
            // XdndStatus: accept iff over a segment (or model not yet known).
            let accept = match &self.overlay {
                Some(ov) => ov.model.is_none() || seg.is_some(),
                None => false,
            };
            if let Some(src) = self.overlay.as_ref().and_then(|o| o.source) {
                let rect_xy = ((ox as u32) << 16) | (oy as u32 & 0xFFFF);
                let rect_wh = ((WHEEL_PX as u32) << 16) | WHEEL_PX as u32;
                let flags = if accept { 0b11 } else { 0b10 };
                let _ = send_client_message(
                    &self.conn,
                    src,
                    self.atoms.xdnd_status,
                    [win, flags, rect_xy, rect_wh, self.atoms.xdnd_action_copy],
                );
            }
        } else if cm.type_ == self.atoms.xdnd_leave {
            self.log.line("overlay: XdndLeave");
            if let Some(ov) = &mut self.overlay {
                ov.source = None;
                if ov.hover.is_some() {
                    ov.hover = None;
                }
            }
            self.paint_overlay();
        } else if cm.type_ == self.atoms.xdnd_drop {
            let src = data[0];
            self.log.line(&format!(
                "overlay: XdndDrop from 0x{src:x} — fetching payload"
            ));
            if let Some(ov) = &mut self.overlay {
                ov.drop_handled = true;
                ov.source = Some(src);
            }
            self.dismiss_due = None;
            let _ = self.conn.convert_selection(
                win,
                self.atoms.xdnd_selection,
                self.atoms.text_uri_list,
                self.data_prop,
                x11rb::CURRENT_TIME,
            );
            let _ = self.conn.flush();
        }
    }

    fn handle_selection_notify(&mut self, sn: x11rb::protocol::xproto::SelectionNotifyEvent) {
        let is_overlay = self.overlay.as_ref().map(|o| o.win) == Some(sn.requestor);
        if sn.property == self.data_prop && is_overlay {
            // The drop payload.
            match read_property_bytes(&self.conn, sn.requestor, self.data_prop) {
                Ok(Some(bytes)) => {
                    let _ = self.conn.delete_property(sn.requestor, self.data_prop);
                    let paths = parse_uri_list(&bytes);
                    self.finish_drop(Some(paths));
                }
                _ => self.finish_drop(None),
            }
            return;
        }
        if sn.property == self.peek_prop && (sn.requestor == self.home || is_overlay) {
            // Bystander peek (home) or entered-target peek (overlay).
            match read_property_bytes(&self.conn, sn.requestor, self.peek_prop) {
                Ok(Some(bytes)) => {
                    let _ = self.conn.delete_property(sn.requestor, self.peek_prop);
                    self.peek_pending_gen = None;
                    self.on_peek_bytes(&bytes);
                }
                _ => {
                    self.peek_pending_gen = None;
                }
            }
            return;
        }
        if sn.property == 0 {
            // Refused conversion. If a drop was in flight, close it out as
            // a refusal; otherwise it was just a peek the source declined.
            if is_overlay
                && self
                    .overlay
                    .as_ref()
                    .map(|o| o.drop_handled)
                    .unwrap_or(false)
            {
                self.log.line("drop payload refused by source");
                self.finish_drop(None);
            } else {
                self.log.line("peek refused by source");
                self.peek_pending_gen = None;
            }
        }
    }

    /// Complete a drop: decide the segment from the last known position,
    /// answer XdndFinished, emit the conversion, tear the overlay down.
    fn finish_drop(&mut self, paths: Option<Vec<PathBuf>>) {
        let Some(ov) = self.overlay.take() else {
            return;
        };
        let paths = paths.unwrap_or_else(|| ov.paths.clone());
        let seg = ov.model.as_ref().and_then(|m| {
            let (px, py) = ov.last_pos?;
            geom::segment_at(
                m.segments.len(),
                (px - (ov.x + (WHEEL_PX / 2) as i16)) as f32,
                (py - (ov.y + (WHEEL_PX / 2) as i16)) as f32,
            )
        });
        let model_needed = ov.model.is_none() && seg.is_none();
        let (accepted, target) = match (ov.model.as_ref(), seg) {
            (Some(m), Some(i)) => (true, Some(m.segments[i].clone())),
            _ => (false, None),
        };
        let _ = model_needed;
        if let Some(src) = ov.source {
            let _ = send_client_message(
                &self.conn,
                src,
                self.atoms.xdnd_finished,
                [
                    ov.win,
                    if accepted { 1 } else { 0 },
                    if accepted {
                        self.atoms.xdnd_action_copy
                    } else {
                        0
                    },
                    0,
                    0,
                ],
            );
        }
        let _ = self.conn.unmap_window(ov.win);
        let _ = self.conn.destroy_window(ov.win);
        let _ = self.conn.flush();
        self.dismiss_due = None;
        if let (true, Some(target)) = (accepted, target) {
            if !paths.is_empty() {
                self.log.line(&format!(
                    "CONVERT-REQUESTED: {} file(s) -> {target}",
                    paths.len()
                ));
                let _ = self
                    .events
                    .send(SensorEvent::ConvertRequested { paths, target });
                self.drag_ended_sent = true; // a conversion closes this drag
                return;
            }
        }
        self.log
            .line("drop finished without a conversion (hub/missed/refused)");
        self.end_drag_if_finished();
    }

    // -- toast -----------------------------------------------------------------

    fn show_toast(&mut self, toast: Toast) {
        self.dismiss_toast();
        let pm = crate::render::render_toast(
            toast.kind,
            &toast.title,
            &toast.body,
            self.config.lang.is_rtl(),
        );
        let (w, h) = (pm.width() as u16, pm.height() as u16);
        let win = match self.conn.generate_id() {
            Ok(w) => w,
            Err(_) => return,
        };
        let x = ((self.screen_w.saturating_sub(w)) / 2) as i16;
        let y = (self.screen_h.saturating_sub(h) as i16 - 56).max(0);
        let aux = CreateWindowAux::new()
            .background_pixel(0x0B1E3A)
            .override_redirect(1u32)
            .event_mask(EventMask::EXPOSURE);
        if self
            .conn
            .create_window(
                self.depth,
                win,
                self.root,
                x,
                y,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &aux,
            )
            .is_err()
        {
            return;
        }
        let _ = set_prop8(
            &self.conn,
            PropMode::REPLACE,
            win,
            self.atoms.wm_name,
            self.atoms.string_atom,
            b"Uruk Convert Toast",
        );
        let _ = self.conn.map_window(win);
        let _ = self.conn.flush();
        self.toast = Some(ToastWin {
            win,
            w,
            h,
            kind: toast.kind,
            title: toast.title,
            body: toast.body,
            until: Instant::now() + Duration::from_millis(3600),
        });
        self.paint_toast();
    }

    fn paint_toast(&mut self) {
        let (win, w, h, kind, title, body) = match &self.toast {
            Some(t) => (t.win, t.w, t.h, t.kind, t.title.clone(), t.body.clone()),
            None => return,
        };
        let pm = crate::render::render_toast(kind, &title, &body, self.config.lang.is_rtl());
        let bgra = crate::render::pixmap_to_bgra(&pm);
        self.put_image(win, w, h, &bgra);
    }

    fn dismiss_toast(&mut self) {
        if let Some(t) = self.toast.take() {
            let _ = self.conn.unmap_window(t.win);
            let _ = self.conn.destroy_window(t.win);
            let _ = self.conn.flush();
        }
    }

    // -- X blitting + capture ----------------------------------------------------

    /// PutImage in horizontal bands (core-protocol request-size limits).
    fn put_image(&self, win: u32, w: u16, h: u16, bgra: &[u8]) {
        let row = (w as usize) * 4;
        if bgra.len() < row * h as usize {
            return;
        }
        let band_rows: u16 = ((240_000usize / row.max(1)).max(1) as u16).min(h).max(1);
        let gc = match self.conn.generate_id() {
            Ok(g) => g,
            Err(_) => return,
        };
        if self.conn.create_gc(gc, win, &CreateGCAux::new()).is_err() {
            return;
        }
        let mut y = 0u16;
        while y < h {
            let rows = band_rows.min(h - y);
            let start = y as usize * row;
            let end = start + rows as usize * row;
            let _ = self.conn.put_image(
                x11rb::protocol::xproto::ImageFormat::Z_PIXMAP,
                win,
                gc,
                w,
                rows,
                0,
                y as i16,
                0,
                self.depth,
                &bgra[start..end],
            );
            y += rows;
        }
        let _ = self.conn.free_gc(gc);
        let _ = self.conn.flush();
    }

    /// QA proof: capture the root window to URUK_WHEEL_CAPTURE (PNG).
    fn capture_root_png(&self) {
        let Ok(path) = std::env::var("URUK_WHEEL_CAPTURE") else {
            return;
        };
        let (w, h) = (self.screen_w, self.screen_h);
        let img = match self.conn.get_image(
            x11rb::protocol::xproto::ImageFormat::Z_PIXMAP,
            self.root,
            0,
            0,
            w,
            h,
            0xFFFF_FFFF,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(i) => i,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for px in img.data.chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        if let Ok(file) = std::fs::File::create(&path) {
            let mut enc = png::Encoder::new(file, w as u32, h as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = enc.write_header() {
                let _ = writer.write_image_data(&rgb);
                let _ = writer.finish();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_basic() {
        let v = parse_uri_list(b"file:///home/u/photo.png\r\nfile:///tmp/a%20b.jpg\r\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], PathBuf::from("/home/u/photo.png"));
        assert_eq!(v[1], PathBuf::from("/tmp/a b.jpg"));
    }

    #[test]
    fn uri_list_skips_comments_and_remote() {
        let v = parse_uri_list(
            b"# comment\r\nfile://example.com/x.png\r\nfile://localhost/y.png\r\nhttps://z\r\n",
        );
        assert_eq!(v, vec![PathBuf::from("/y.png")]);
    }

    #[test]
    fn uri_list_utf8_percent() {
        // صورة = %D8%B5%D9%88%D8%B1%D8%A9
        let v = parse_uri_list(b"file:///home/u/%D8%B5%D9%88%D8%B1%D8%A9.png\r\n");
        assert_eq!(v[0].to_string_lossy(), "/home/u/صورة.png");
    }

    #[test]
    fn file_drag_classification() {
        assert!(types_are_file_drag(&["text/uri-list".into()]));
        assert!(!types_are_file_drag(&["text/plain".into()]));
        assert!(!types_are_file_drag(&[]));
    }
}

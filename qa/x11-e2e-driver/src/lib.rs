//! Shared XDND helpers for the Uruk Convert X11 E2E driver.
//! Copied from the Stage-3 spike (`uruk-convert-spikes/src/lib.rs`), whose
//! three runs proved this exact protocol dance against the bystander
//! sensor; the product E2E replays it against the shipping app.

use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, ClientMessageData, ClientMessageEvent, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, CLIENT_MESSAGE_EVENT, SELECTION_NOTIFY_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

/// Predefined X atom ids (stable protocol constants).
pub const XA_ATOM: Atom = 4;
pub const XA_CARDINAL: Atom = 6;
pub const XA_STRING: Atom = 31;

/// x11rb 0.13 exposes only the raw byte form of ChangeProperty; these are
/// the format-8 / format-32 conveniences rebuilt on top of it.
pub fn set_prop8(
    conn: &RustConnection,
    mode: PropMode,
    win: u32,
    prop: Atom,
    ty: Atom,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    conn.change_property(mode, win, prop, ty, 8, data.len() as u32, data)?;
    Ok(())
}

pub fn set_prop32(
    conn: &RustConnection,
    mode: PropMode,
    win: u32,
    prop: Atom,
    ty: Atom,
    data: &[u32],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    conn.change_property(mode, win, prop, ty, 32, data.len() as u32, &bytes)?;
    Ok(())
}

/// Millisecond epoch (wall clock) — used only to align logs across processes.
pub fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Line-buffered logger: `[+<rel>ms epoch=<abs>] <TAG> <msg>`.
pub struct Log {
    start: Instant,
    tag: &'static str,
}

impl Log {
    pub fn new(tag: &'static str) -> Self {
        Log {
            start: Instant::now(),
            tag,
        }
    }
    pub fn line(&self, msg: &str) {
        let rel = self.start.elapsed().as_millis();
        println!("[+{:>6}ms epoch={}] {} {}", rel, epoch_ms(), self.tag, msg);
        let _ = std::io::stdout().flush();
    }
    pub fn elapsed_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}

/// All XDND atoms used by the three spike binaries.
pub struct Atoms {
    pub xdnd_selection: Atom,
    pub xdnd_type_list: Atom,
    pub xdnd_enter: Atom,
    pub xdnd_position: Atom,
    pub xdnd_status: Atom,
    pub xdnd_drop: Atom,
    pub xdnd_leave: Atom,
    pub xdnd_finished: Atom,
    pub xdnd_action_copy: Atom,
    pub xdnd_aware: Atom,
    pub text_uri_list: Atom,
}

pub fn intern_atoms(conn: &RustConnection) -> Result<Atoms, Box<dyn std::error::Error>> {
    let get = |name: &str| -> Result<Atom, Box<dyn std::error::Error>> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
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
    })
}

/// Send a format-32 ClientMessage (all XDND messages are these).
pub fn send_client_message(
    conn: &RustConnection,
    dest: u32,
    type_atom: Atom,
    data: [u32; 5],
) -> Result<(), Box<dyn std::error::Error>> {
    let ev = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: dest,
        type_: type_atom,
        data: ClientMessageData::from(data),
    };
    conn.send_event(false, dest, x11rb::protocol::xproto::EventMask::NO_EVENT, ev)?;
    conn.flush()?;
    Ok(())
}

/// Find the keycodes that produce Shift_L / Shift_R (for XTest driving and
/// for recognising the modifier in XI2 raw key events).
pub fn shift_keycodes(conn: &RustConnection) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - first + 1;
    let map = conn.get_keyboard_mapping(first, count)?.reply()?;
    let per = map.keysyms_per_keycode as usize;
    let mut out = Vec::new();
    for i in 0..count as usize {
        let syms = &map.keysyms[i * per..(i + 1) * per];
        if syms.iter().any(|&s| s == 0xFFE1 || s == 0xFFE2) {
            out.push(first + i as u8);
        }
    }
    Ok(out)
}

/// Obtain a plausible current X server timestamp: change a property on our
/// own window and read the timestamp off the resulting PropertyNotify.
/// (XConvertSelection / XDND timestamps must not be in the future.)
pub fn server_time_now(
    conn: &RustConnection,
    win: u32,
) -> Result<u32, Box<dyn std::error::Error>> {
    let prop = conn
        .intern_atom(false, b"URUK_TIMEBASE")?
        .reply()?
        .atom;
    set_prop8(conn, PropMode::REPLACE, win, prop, XA_STRING, &[1u8])?;
    conn.flush()?;
    loop {
        let ev = conn.wait_for_event()?;
        if let x11rb::protocol::Event::PropertyNotify(pn) = ev {
            if pn.window == win && pn.atom == prop {
                return Ok(pn.time);
            }
        }
    }
}

/// The topmost XDND-aware window under root point (x, y), if any — the same
/// traversal a real XDND source performs: scan root's children top-first,
/// keep those containing the point, prefer the first that advertises
/// XdndAware. Returns (window, version, x, y, w, h).
pub fn xdnd_window_under_pointer(
    conn: &RustConnection,
    root: u32,
    x: i16,
    y: i16,
    aware_atom: Atom,
    exclude: &[u32],
) -> Result<Option<(u32, u32, i16, i16, u16, u16)>, Box<dyn std::error::Error>> {
    let tree = conn.query_tree(root)?.reply()?;
    // QueryTree children are in bottom-to-top stacking order; walk reversed.
    for &win in tree.children.iter().rev() {
        if exclude.contains(&win) {
            continue;
        }
        let attrs = match conn.get_window_attributes(win) {
            Ok(c) => match c.reply() {
                Ok(a) => a,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if attrs.map_state != x11rb::protocol::xproto::MapState::VIEWABLE {
            continue;
        }
        let geo = conn.get_geometry(win)?.reply()?;
        let (gx, gy) = (geo.x, geo.y);
        let (gw, gh) = (geo.width, geo.height);
        let inside = x >= gx && x < gx + gw as i16 && y >= gy && y < gy + gh as i16;
        if !inside {
            continue;
        }
        // Is it XDND-aware?
        let reply = conn
            .get_property(false, win, aware_atom, XA_CARDINAL, 0, 1)?
            .reply()?;
        let version = reply.value32().and_then(|mut it| it.next());
        if let Some(version) = version {
            return Ok(Some((win, version, gx, gy, gw, gh)));
        }
        // Containing but not XDND-aware: a real source keeps scanning down
        // the stack for an aware window at this point. We keep scanning too.
    }
    Ok(None)
}

/// Answer a SelectionRequest for XdndSelection the way a generic toolkit
/// drag source does: XdndTypeList -> offered atoms; a data target -> bytes.
/// Logs nothing; the caller logs (requestor identity matters to the spike).
pub fn answer_selection_request(
    conn: &RustConnection,
    atoms: &Atoms,
    ev: &SelectionRequestEvent,
    uri_bytes: &[u8],
    data_type: Atom,
) -> Result<(), Box<dyn std::error::Error>> {
    let property = if ev.property == 0 { ev.target } else { ev.property };
    let mut granted = property;
    if ev.selection != atoms.xdnd_selection {
        granted = 0;
    } else if ev.target == atoms.xdnd_type_list {
        set_prop32(
            conn,
            PropMode::REPLACE,
            ev.requestor,
            property,
            XA_ATOM,
            &[atoms.text_uri_list],
        )?;
    } else if ev.target == atoms.text_uri_list || ev.target == data_type {
        set_prop8(conn, PropMode::REPLACE, ev.requestor, property, ev.target, uri_bytes)?;
    } else {
        granted = 0;
    }
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        time: ev.time,
        sequence: 0,
        requestor: ev.requestor,
        selection: ev.selection,
        target: ev.target,
        property: granted,
    };
    conn.send_event(
        false,
        ev.requestor,
        x11rb::protocol::xproto::EventMask::NO_EVENT,
        notify,
    )?;
    conn.flush()?;
    Ok(())
}

/// Convenience: fire XConvertSelection and let the caller's loop collect the
/// SelectionNotify.
pub fn request_selection(
    conn: &RustConnection,
    requestor: u32,
    selection: Atom,
    target: Atom,
    property: Atom,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.convert_selection(requestor, selection, target, property, CURRENT_TIME)?;
    conn.flush()?;
    Ok(())
}

/// Read a window property fully as bytes.
pub fn read_property_bytes(
    conn: &RustConnection,
    win: u32,
    property: Atom,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    let reply = conn
        .get_property(false, win, property, 0u32, 0, 1 << 22)?
        .reply()?;
    if reply.type_ == 0 {
        return Ok(None);
    }
    Ok(Some(reply.value))
}

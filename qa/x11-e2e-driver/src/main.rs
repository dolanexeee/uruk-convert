//! E2E driver: performs a REAL XDND v5 drag against the running Uruk
//! Convert agent under Xvfb.
//!
//! Drag path: press at (150,150) -> diagonal to (640,400) -> SHIFT at
//! step 26 (the summon gesture) -> wait for the product's native wheel
//! overlay (WM_NAME "Uruk Convert Wheel") -> move to the JPG segment
//! point computed from the overlay's real geometry (segment 0 of the
//! image family wheel, r=151 at +10deg) -> hold -> SHIFT up -> button up
//! -> XdndDrop -> await XdndFinished.
//!
//! Exit codes: 0 = drop accepted by the product overlay;
//!             2 = overlay never appeared; 3 = drop not accepted;
//!             1 = driver error.
//! Env: SPIKE_URI = file:// URI of the fixture being dragged.

use std::time::{Duration, Instant};

use uruk_x11_e2e_driver::*;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xproto::{CreateWindowAux, EventMask, PropMode, WindowClass};
use x11rb::protocol::xtest;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

struct Ctx {
    conn: RustConnection,
    atoms: Atoms,
    own: u32,
    root: u32,
    base_time: u32,
    base_instant: Instant,
    uri: String,
    status_accepts: u32,
    sel_requests: u32,
    finished: Option<bool>,
}

impl Ctx {
    fn server_now(&self) -> u32 {
        self.base_time
            .wrapping_add(self.base_instant.elapsed().as_millis() as u32)
    }

    fn pump(&mut self, log: &Log) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(ev) = self.conn.poll_for_event()? {
            match ev {
                Event::SelectionRequest(req) => {
                    self.sel_requests += 1;
                    log.line(&format!(
                        "SelectionRequest from requestor 0x{:x} (#{}) — answering",
                        req.requestor, self.sel_requests
                    ));
                    let uri_bytes = format!("{}\r\n", self.uri).into_bytes();
                    answer_selection_request(
                        &self.conn,
                        &self.atoms,
                        &req,
                        &uri_bytes,
                        self.atoms.text_uri_list,
                    )?;
                }
                Event::ClientMessage(cm) => {
                    let data = cm.data.as_data32();
                    if cm.type_ == self.atoms.xdnd_status {
                        if data[1] & 1 == 1 {
                            self.status_accepts += 1;
                        }
                    } else if cm.type_ == self.atoms.xdnd_finished {
                        self.finished = Some(data[1] & 1 == 1);
                        log.line(&format!(
                            "XdndFinished received: success={}",
                            data[1] & 1 == 1
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn retarget(
        &mut self,
        log: &Log,
        x: i16,
        y: i16,
        current: &mut Option<u32>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let found = xdnd_window_under_pointer(
            &self.conn,
            self.root,
            x,
            y,
            self.atoms.xdnd_aware,
            &[self.own],
        )?;
        let found_win = found.map(|f| f.0);
        if found_win != *current {
            if let Some(old) = *current {
                send_client_message(&self.conn, old, self.atoms.xdnd_leave, [self.own, 0, 0, 0, 0])?;
                log.line(&format!("XdndLeave -> 0x{:x}", old));
            }
            if let Some(new) = found_win {
                send_client_message(
                    &self.conn,
                    new,
                    self.atoms.xdnd_enter,
                    [self.own, 5 << 24, self.atoms.text_uri_list, 0, 0],
                )?;
                log.line(&format!("XdndEnter -> 0x{:x} at ({},{})", new, x, y));
            }
            *current = found_win;
        }
        Ok(())
    }

    fn send_position(&self, target: u32, x: i16, y: i16) -> Result<(), Box<dyn std::error::Error>> {
        let packed = ((x as u32) << 16) | (y as u32 & 0xFFFF);
        send_client_message(
            &self.conn,
            target,
            self.atoms.xdnd_position,
            [self.own, 0, packed, self.server_now(), self.atoms.xdnd_action_copy],
        )?;
        Ok(())
    }

    /// Find a product window by its WM_NAME over all root children.
    fn find_named(
        &self,
        wm_name: u32,
        want: &[u8],
    ) -> Result<Option<(u32, i16, i16, u16, u16)>, Box<dyn std::error::Error>> {
        let tree = self.conn.query_tree(self.root)?.reply()?;
        for &win in &tree.children {
            let name = read_property_bytes(&self.conn, win, wm_name)?;
            if let Some(bytes) = name {
                if bytes == want {
                    let geo = self.conn.get_geometry(win)?.reply()?;
                    return Ok(Some((win, geo.x, geo.y, geo.width, geo.height)));
                }
            }
        }
        Ok(None)
    }

    fn find_wheel(
        &self,
        wm_name: u32,
    ) -> Result<Option<(u32, i16, i16, u16, u16)>, Box<dyn std::error::Error>> {
        self.find_named(wm_name, b"Uruk Convert Wheel")
    }
}

fn fake(
    conn: &RustConnection,
    type_: u8,
    detail: u8,
    root: u32,
    x: i16,
    y: i16,
) -> Result<(), Box<dyn std::error::Error>> {
    xtest::fake_input(conn, type_, detail, 0, root, x, y, 0)?;
    conn.flush()?;
    Ok(())
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let log = Log::new("E2E");
    let uri = std::env::var("SPIKE_URI")
        .unwrap_or_else(|_| "file:///tmp/uruk-e2e-fixture.png".to_string());
    // Scenario: "convert" (Shift → segment 0/JPG), "tools"
    // (Shift+Alt → tools wheel segment 0/Crop, then drive the HUD
    // panel), "actions" (Control → actions wheel segment 1/Copy path).
    let scenario = std::env::var("SPIKE_SCENARIO").unwrap_or_else(|_| "convert".to_string());
    log.line(&format!("scenario: {scenario}"));

    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let atoms = intern_atoms(&conn)?;
    let wm_name = conn.intern_atom(false, b"WM_NAME")?.reply()?.atom;

    let own = conn.generate_id()?;
    let aux = CreateWindowAux::new()
        .background_pixel(0xB06030)
        .event_mask(EventMask::PROPERTY_CHANGE);
    conn.create_window(
        screen.root_depth,
        own,
        root,
        1180,
        730,
        24,
        24,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &aux,
    )?;
    conn.map_window(own)?;
    conn.flush()?;

    let base_time = server_time_now(&conn, own)?;
    let mut ctx = Ctx {
        conn,
        atoms,
        own,
        root,
        base_time,
        base_instant: Instant::now(),
        uri,
        status_accepts: 0,
        sel_requests: 0,
        finished: None,
    };
    let shift_kc = shift_keycodes(&ctx.conn)?
        .into_iter()
        .next()
        .ok_or("no shift keycode")?;
    let kc_of = |sym: u32| -> Result<u8, Box<dyn std::error::Error>> {
        keycode_for_keysym(&ctx.conn, sym)?.ok_or_else(|| format!("no keycode for keysym {sym:#x}").into())
    };
    let alt_kc = kc_of(0xFFE9)?; // Alt_L
    let ctrl_kc = kc_of(0xFFE3)?; // Control_L
    let return_kc = kc_of(0xFF0D)?; // Return
    let backspace_kc = kc_of(0xFF08)?; // BackSpace
    // Modifier press schedule: (drag step, keycode). Order matters:
    // for the tools chord Alt lands first (no chord on its own), then
    // Shift completes Shift+Alt.
    let schedule: Vec<(i32, u8)> = match scenario.as_str() {
        "tools" => vec![(24, alt_kc), (26, shift_kc)],
        "actions" => vec![(26, ctrl_kc)],
        _ => vec![(26, shift_kc)],
    };

    // P0/P1: warp to drag start, button down.
    fake(&ctx.conn, 6, 0, root, 150, 150)?;
    std::thread::sleep(Duration::from_millis(150));
    fake(&ctx.conn, 4, 1, root, 150, 150)?;
    log.line("pointer button1 DOWN at (150,150)");
    std::thread::sleep(Duration::from_millis(120));

    // P2: claim the drag.
    ctx.conn
        .set_selection_owner(ctx.own, ctx.atoms.xdnd_selection, x11rb::CURRENT_TIME)?;
    set_prop32(
        &ctx.conn,
        PropMode::REPLACE,
        ctx.own,
        ctx.atoms.xdnd_type_list,
        XA_ATOM,
        &[ctx.atoms.text_uri_list],
    )?;
    ctx.conn.flush()?;
    log.line("XDND DRAG BEGUN — XdndSelection claimed, XdndTypeList=[text/uri-list]");

    // P3/P4: diagonal drag; the scenario's modifier chord mid-drag.
    let mut current: Option<u32> = None;
    ctx.retarget(&log, 150, 150, &mut current)?;
    let steps = 44;
    let mut pressed: Vec<u8> = Vec::new();
    for i in 1..=steps {
        let x = 150 + ((640 - 150) * i / steps) as i16;
        let y = 150 + ((400 - 150) * i / steps) as i16;
        fake(&ctx.conn, 6, 0, root, x, y)?;
        ctx.pump(&log)?;
        ctx.retarget(&log, x, y, &mut current)?;
        if let Some(t) = current {
            ctx.send_position(t, x, y)?;
        }
        for &(step, kc) in &schedule {
            if i == step && !pressed.contains(&kc) {
                fake(&ctx.conn, 2, kc, root, x, y)?;
                pressed.push(kc);
                log.line(&format!(
                    "MODIFIER keycode {kc} DOWN mid-drag at ({x},{y}) — the summon gesture"
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // P5: wait for the product's wheel overlay, then aim at the JPG segment.
    let wait_start = Instant::now();
    let mut wheel: Option<(u32, i16, i16, u16, u16)> = None;
    while wait_start.elapsed() < Duration::from_secs(15) {
        ctx.pump(&log)?;
        ctx.retarget(&log, 640, 400, &mut current)?;
        if let Some(t) = current {
            ctx.send_position(t, 640, 400)?;
        }
        if let Some(w) = ctx.find_wheel(wm_name)? {
            wheel = Some(w);
            break;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    let Some((wwin, wx, wy, ww, wh)) = wheel else {
        log.line("FAIL: product wheel overlay never appeared");
        return Ok(2);
    };
    log.line(&format!(
        "WHEEL FOUND: window 0x{wwin:x} geometry=({wx},{wy}) {ww}x{wh} after {}ms",
        wait_start.elapsed().as_millis()
    ));
    let (cx, cy) = (wx + (ww / 2) as i16, wy + (wh / 2) as i16);
    // Drop points at r=151 on the segment mid-angle (segment i of n
    // spans [i*360/n, (i+1)*360/n) clockwise from 12 o'clock):
    //   convert: segment 0 of the PNG wheel at +10deg (Stage-3 proven).
    //   tools:   segment 0 (Crop) of 7 -> mid 25.7deg -> (+65, -136).
    //   actions: segment 1 (Copy path) of 5 -> mid 108deg -> (+144, +47).
    let (tx, ty) = match scenario.as_str() {
        "tools" => (cx + 65, cy - 136),
        "actions" => (cx + 144, cy + 47),
        _ => (cx + 26, cy - 149),
    };
    log.line(&format!("wheel center=({cx},{cy}); segment drop point=({tx},{ty})"));

    // Move onto the segment in a few steps, then hold so the hover paint
    // (and the app's own proof capture) can happen.
    for i in 1..=6 {
        let x = 640 + ((tx - 640) * i / 6) as i16;
        let y = 400 + ((ty - 400) * i / 6) as i16;
        fake(&ctx.conn, 6, 0, root, x, y)?;
        ctx.pump(&log)?;
        ctx.retarget(&log, x, y, &mut current)?;
        if let Some(t) = current {
            ctx.send_position(t, x, y)?;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let hold_start = Instant::now();
    while hold_start.elapsed() < Duration::from_millis(800) {
        ctx.pump(&log)?;
        if let Some(t) = current {
            ctx.send_position(t, tx, ty)?;
        }
        std::thread::sleep(Duration::from_millis(60));
    }

    // P6/P7: modifiers up, button up, XdndDrop on the overlay.
    for kc in pressed.iter().rev() {
        fake(&ctx.conn, 3, *kc, root, tx, ty)?;
    }
    if !pressed.is_empty() {
        log.line("modifiers UP");
    }
    std::thread::sleep(Duration::from_millis(100));
    fake(&ctx.conn, 5, 1, root, tx, ty)?;
    log.line("pointer button1 UP");
    if let Some(t) = current {
        send_client_message(
            &ctx.conn,
            t,
            ctx.atoms.xdnd_drop,
            [ctx.own, 0, ctx.server_now(), 0, 0],
        )?;
        log.line(&format!("XdndDrop -> 0x{:x}", t));
    } else {
        log.line("FAIL: no XDND target at the drop point");
        return Ok(3);
    }

    // P8: answer the payload conversion; await XdndFinished.
    let fin_start = Instant::now();
    while fin_start.elapsed() < Duration::from_millis(4000) {
        ctx.pump(&log)?;
        if ctx.finished.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // Tools scenario: the drop opens the Crop HUD panel — drive it.
    let mut panel_ok = scenario != "tools";
    if scenario == "tools" && ctx.finished == Some(true) {
        panel_ok = drive_crop_panel(&mut ctx, &log, wm_name, return_kc, backspace_kc)?;
    }

    ctx.conn
        .set_selection_owner(0u32, ctx.atoms.xdnd_selection, x11rb::CURRENT_TIME)?;
    ctx.conn.flush()?;
    log.line(&format!(
        "SUMMARY status_accepts={} selection_requests_answered={} finished={:?}",
        ctx.status_accepts, ctx.sel_requests, ctx.finished
    ));
    match ctx.finished {
        Some(true) => {
            if panel_ok {
                log.line("E2E-DRAG-RESULT: ACCEPTED");
                Ok(0)
            } else {
                log.line("E2E-DRAG-RESULT: PANEL-FLOW-FAILED");
                Ok(4)
            }
        }
        _ => {
            log.line("E2E-DRAG-RESULT: NOT-ACCEPTED");
            Ok(3)
        }
    }
}

/// Tap a key (press + release) via XTest.
fn tap_key(ctx: &Ctx, kc: u8, root: u32, x: i16, y: i16) -> Result<(), Box<dyn std::error::Error>> {
    fake(&ctx.conn, 2, kc, root, x, y)?;
    std::thread::sleep(Duration::from_millis(35));
    fake(&ctx.conn, 3, kc, root, x, y)?;
    std::thread::sleep(Duration::from_millis(35));
    Ok(())
}

/// Click at an absolute root point.
fn click(ctx: &Ctx, root: u32, x: i16, y: i16) -> Result<(), Box<dyn std::error::Error>> {
    fake(&ctx.conn, 6, 0, root, x, y)?;
    std::thread::sleep(Duration::from_millis(80));
    fake(&ctx.conn, 4, 1, root, x, y)?;
    std::thread::sleep(Duration::from_millis(80));
    fake(&ctx.conn, 5, 1, root, x, y)?;
    std::thread::sleep(Duration::from_millis(120));
    Ok(())
}

/// Tools scenario, second act: wait for the "Uruk Convert Panel"
/// window, set the Crop W field to 100 and H to 80, apply with
/// Return. Panel layout (shared framework, LTR): text field i's
/// control centre is (panel_x + 174, panel_y + 78 + 61*i); the crop
/// panel's fields are [x, y, w, h, aspect chips] so W is i=2 and H
/// is i=3. The caret always sits at the end, so Backspace spam
/// clears the prefilled value deterministically.
fn drive_crop_panel(
    ctx: &mut Ctx,
    log: &Log,
    wm_name: u32,
    return_kc: u8,
    backspace_kc: u8,
) -> Result<bool, Box<dyn std::error::Error>> {
    let root = ctx.root;
    let wait_start = Instant::now();
    let mut panel: Option<(u32, i16, i16, u16, u16)> = None;
    while wait_start.elapsed() < Duration::from_secs(15) {
        ctx.pump(log)?;
        if let Some(p) = ctx.find_named(wm_name, b"Uruk Convert Panel")? {
            panel = Some(p);
            break;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let Some((pwin, px, py, pw, ph)) = panel else {
        log.line("FAIL: HUD panel never appeared after the tools drop");
        return Ok(false);
    };
    log.line(&format!(
        "PANEL FOUND: window 0x{pwin:x} geometry=({px},{py}) {pw}x{ph} after {}ms",
        wait_start.elapsed().as_millis()
    ));
    std::thread::sleep(Duration::from_millis(300));

    let digit_kc = |c: char| -> Result<u8, Box<dyn std::error::Error>> {
        keycode_for_keysym(&ctx.conn, c as u32)?
            .ok_or_else(|| format!("no keycode for digit {c}").into())
    };
    let set_field = |ctx: &Ctx, field_idx: i16, value: &str| -> Result<(), Box<dyn std::error::Error>> {
        let fx = px + 174;
        let fy = py + 78 + 61 * field_idx;
        click(ctx, root, fx, fy)?;
        for _ in 0..12 {
            tap_key(ctx, backspace_kc, root, fx, fy)?;
        }
        for c in value.chars() {
            tap_key(ctx, digit_kc(c)?, root, fx, fy)?;
        }
        Ok(())
    };
    set_field(ctx, 2, "100")?; // W
    set_field(ctx, 3, "80")?; // H
    log.line("panel fields set: W=100 H=80 — applying with Return");
    tap_key(ctx, return_kc, root, px + 174, py + 200)?;
    std::thread::sleep(Duration::from_millis(400));
    Ok(true)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("driver error: {e}");
            std::process::exit(1);
        }
    }
}

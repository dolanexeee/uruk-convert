//! Wayland tier — capability probe + layer-shell standing orb.
//!
//! The honest maximum on Wayland (see DESIGN.md in the spikes project,
//! section "Linux / Wayland"): the protocol is designed so a regular
//! client **cannot** observe another client's drag, cannot learn global
//! pointer coordinates, and cannot summon a surface at the cursor.
//! What *is* possible, per compositor:
//!
//! * **wlroots (Sway/Hyprland/…) and KDE KWin** — `ext-data-control-v1` /
//!   `zwlr_data_control_manager_v1` can *observe* drag offers seat-wide
//!   (classification only: "someone is dragging `text/uri-list`"), and
//!   `zwlr_layer_shell_v1` provides an anchored always-on-top surface.
//!   A layer surface under the pointer participates in DnD as a normal
//!   surface, so the actionable maximum is a **standing orb**: a small
//!   floating sun-disc, anchored bottom-right, that the user drops
//!   files onto. That is what [`start_orb`] runs.
//! * **GNOME / Mutter** — neither protocol is exposed to regular
//!   clients. The maximum is the MenuOnly tier: a plain drop window +
//!   a file-manager (Nautilus) action, both implemented by the shell
//!   outside this module. This module's job there is only to *report*
//!   the tier correctly — see [`probe_tier`] and [`tier_explanation`].
//! * **XWayland** — drags between X apps live inside the X server,
//!   where the X11 sensor covers them completely (wheel included).
//!   The notes produced here say so, so the shell can surface the
//!   coverage honestly.
//!
//! Binding note: `zwlr_layer_shell_v1` lives in the separate
//! `wayland-protocols-wlr` crate, which is **not** a dependency of this
//! crate (and this file may not add dependencies). The two wlr
//! interfaces are therefore hand-written below in the [`wlr`] module,
//! following `wayland-scanner`'s generated client code pattern exactly
//! (same `Proxy` impl shape, message descriptors and parse/write
//! bodies). Their `Interface::c_ptr` is `None`, which is correct for
//! the pure-Rust backend this crate builds with: `wayland-client` is
//! used here **without** its `system` (libwayland) feature, and the
//! Rust backend never dereferences `c_ptr`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::{SensorError, SensorLang};

// ---------------------------------------------------------------------------
// Tier model
// ---------------------------------------------------------------------------

/// How much of the paradigm a Wayland session can support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandTier {
    /// A native Wayland drag can be *sensed* (data-control offers) but
    /// the at-cursor summon does not exist; the orb is the drop surface.
    Orb,
    /// No data-control, no layer-shell (GNOME/Mutter for regular
    /// clients): a plain drop window + file-manager menu integration
    /// are the maximum. (An X11 sensor may still cover XWayland apps.)
    MenuOnly,
}

#[derive(Debug, Clone)]
pub struct TierInfo {
    pub tier: WaylandTier,
    /// Human-readable compositor guess (e.g. "wlroots", "KWin", "Mutter").
    pub compositor_hint: String,
    pub has_data_control: bool,
    pub has_layer_shell: bool,
    /// e.g. "wayland" | "x11" | "wayland (XWayland display present)".
    pub session_note: String,
}

// ---------------------------------------------------------------------------
// Pure tier decision (env facts + registry facts in, TierInfo out)
// ---------------------------------------------------------------------------

/// Session facts read from the process environment. Kept separate from
/// the decision so the decision is a pure, unit-testable function.
#[derive(Debug, Clone, Default)]
struct EnvFacts {
    session_type: Option<String>,
    wayland_display: Option<String>,
    current_desktop: Option<String>,
    desktop_session: Option<String>,
    /// `DISPLAY` — its presence under Wayland means XWayland exists.
    display: Option<String>,
}

impl EnvFacts {
    fn from_env() -> EnvFacts {
        let get = |k: &str| {
            std::env::var(k).ok().and_then(|v| {
                let v = v.trim().to_string();
                if v.is_empty() {
                    None
                } else {
                    Some(v)
                }
            })
        };
        EnvFacts {
            session_type: get("XDG_SESSION_TYPE"),
            wayland_display: get("WAYLAND_DISPLAY"),
            current_desktop: get("XDG_CURRENT_DESKTOP"),
            desktop_session: get("DESKTOP_SESSION"),
            display: get("DISPLAY"),
        }
    }

    fn is_wayland_session(&self) -> bool {
        self.wayland_display.is_some()
            || self
                .session_type
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("wayland"))
                .unwrap_or(false)
    }
}

/// Facts from one registry roundtrip against the live compositor.
#[derive(Debug, Clone, Copy, Default)]
struct GlobalFacts {
    /// A connection + roundtrip actually succeeded.
    reachable: bool,
    has_layer_shell: bool,
    has_data_control: bool,
}

/// Guess the compositor from the desktop/session names. This is a
/// *hint* for display only — the tier decision itself is driven by
/// the registry globals, never by this string.
fn compositor_hint(env: &EnvFacts) -> String {
    let hay = format!(
        "{} {}",
        env.current_desktop.as_deref().unwrap_or(""),
        env.desktop_session.as_deref().unwrap_or("")
    )
    .to_lowercase();
    if hay.contains("gnome") || hay.contains("mutter") || hay.contains("unity") {
        "Mutter (GNOME)".to_string()
    } else if hay.contains("kde") || hay.contains("plasma") || hay.contains("kwin") {
        "KWin".to_string()
    } else if hay.contains("sway")
        || hay.contains("hyprland")
        || hay.contains("wlroots")
        || hay.contains("labwc")
        || hay.contains("wayfire")
        || hay.contains("river")
        || hay.contains("niri")
        || hay.contains("cosmic")
    {
        "wlroots (Sway/Hyprland/…)".to_string()
    } else {
        "unknown".to_string()
    }
}

/// The tier decision, pure: no I/O, no env access, no panics.
/// Globals win over guesses — a compositor that actually advertises
/// layer-shell gets the orb tier whatever the desktop name says, and
/// a session with no reachable Wayland display gets MenuOnly (GNOME
/// with no connection included) whatever the desktop name says.
fn decide_tier(env: &EnvFacts, globals: GlobalFacts) -> TierInfo {
    let hint = compositor_hint(env);
    if !env.is_wayland_session() {
        return TierInfo {
            tier: WaylandTier::MenuOnly,
            compositor_hint: hint,
            has_data_control: false,
            has_layer_shell: false,
            session_note: format!(
                "not a Wayland session (XDG_SESSION_TYPE={}, WAYLAND_DISPLAY unset): \
                 the Wayland tiers do not apply here",
                env.session_type.as_deref().unwrap_or("unset")
            ),
        };
    }
    let xwayland = if env.display.is_some() {
        " XWayland display present."
    } else {
        ""
    };
    if !globals.reachable {
        return TierInfo {
            tier: WaylandTier::MenuOnly,
            compositor_hint: hint,
            has_data_control: false,
            has_layer_shell: false,
            session_note: format!(
                "wayland session advertised but the display was unreachable during the \
                 probe: menu + drop-window tier only; native Wayland apps: no orb; \
                 X apps are covered by the X11 sensor through XWayland.{xwayland}"
            ),
        };
    }
    let tier = if globals.has_layer_shell {
        WaylandTier::Orb
    } else {
        WaylandTier::MenuOnly
    };
    let session_note = match tier {
        WaylandTier::Orb => format!(
            "wayland session on {hint}: native Wayland apps: orb drop only \
             (no global drag sensing, no at-cursor summon — by protocol design); \
             X apps are covered by the X11 sensor through XWayland.{xwayland}"
        ),
        WaylandTier::MenuOnly => format!(
            "wayland session on {hint}: no layer-shell for regular clients, so \
             menu + drop-window tier only; native Wayland apps: no orb; \
             X apps are covered by the X11 sensor through XWayland.{xwayland}"
        ),
    };
    TierInfo {
        tier,
        compositor_hint: hint,
        has_data_control: globals.has_data_control,
        has_layer_shell: globals.has_layer_shell,
        session_note,
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Probe the current session: env first, compositor globals second.
/// Never panics and returns in bounded time (the registry roundtrip
/// runs on a helper thread with a hard 1.5 s budget; a compositor that
/// stalls the probe is reported as unreachable). A non-Wayland session
/// returns MenuOnly with a note saying so.
pub fn probe_tier() -> TierInfo {
    let env = EnvFacts::from_env();
    if !env.is_wayland_session() {
        return decide_tier(&env, GlobalFacts::default());
    }
    let globals = probe_wayland_globals_bounded();
    decide_tier(&env, globals)
}

/// Connect + one registry roundtrip, on a scratch thread so a hung
/// compositor cannot hang app startup. On timeout the scratch thread
/// is simply abandoned (it owns its connection and dies with it when
/// the socket errors out or the process moves on).
fn probe_wayland_globals_bounded() -> GlobalFacts {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("uruk-wayland-probe".into())
        .spawn(move || {
            let facts = probe_wayland_globals();
            let _ = tx.send(facts);
        });
    match spawned {
        Ok(_) => rx
            .recv_timeout(Duration::from_millis(1500))
            .unwrap_or_default(),
        Err(_) => GlobalFacts::default(),
    }
}

/// The unbounded probe body: connect, read the registry once, report
/// which of the globals we care about exist.
fn probe_wayland_globals() -> GlobalFacts {
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle};

    struct ProbeState {
        names: Vec<String>,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for ProbeState {
        fn event(
            state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event {
                state.names.push(interface);
            }
        }
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => return GlobalFacts::default(),
    };
    let mut queue = conn.new_event_queue::<ProbeState>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = ProbeState { names: Vec::new() };
    if queue.roundtrip(&mut state).is_err() {
        return GlobalFacts::default();
    }
    GlobalFacts {
        reachable: true,
        has_layer_shell: state.names.iter().any(|n| n == "zwlr_layer_shell_v1"),
        has_data_control: state
            .names
            .iter()
            .any(|n| n == "ext_data_control_manager_v1" || n == "zwlr_data_control_manager_v1"),
    }
}

// ---------------------------------------------------------------------------
// Settings copy
// ---------------------------------------------------------------------------

/// The honest one-paragraph explanation the shell shows in Settings
/// for the active Wayland tier, in the user's UI language. Compositor
/// matrix: wlroots/KDE = orb; GNOME = menu + window; XWayland apps are
/// covered by the X11 path either way.
pub fn tier_explanation(tier: &TierInfo, lang: SensorLang) -> String {
    let hint = tier.compositor_hint.as_str();
    let wayland_session = !tier.session_note.starts_with("not a Wayland session");
    match (lang, tier.tier, wayland_session) {
        (SensorLang::En, WaylandTier::Orb, _) => format!(
            "On this compositor ({hint}), Uruk Convert runs in orb mode on Wayland: \
             a small sun-disc stays anchored in the bottom-right corner of the screen, \
             and you convert a file by dragging it onto the disc and dropping it there. \
             Wayland gives regular apps no way to watch drags across the whole desktop \
             or to pop the wheel up at your cursor — that limit is the protocol's \
             design, not a missing setting. Apps running through XWayland are covered \
             by the full X11 path instead, wheel at the cursor included."
        ),
        (SensorLang::En, WaylandTier::MenuOnly, true) => format!(
            "On this compositor ({hint}), Wayland runs in its reduced mode: this \
             compositor does not offer regular apps a layer-shell surface or \
             drag-observation, so there is no floating orb and no sensing. Convert \
             from the file-manager menu (\"Convert with Uruk Convert\") or by dropping \
             files onto the app's drop window. Apps running through XWayland are \
             still covered by the full X11 path, wheel at the cursor included."
        ),
        (SensorLang::En, WaylandTier::MenuOnly, false) => format!(
            "This session is not running on Wayland ({hint}), so the Wayland tiers \
             do not apply: under X11, Uruk Convert's sensor covers the full \
             paradigm — it sees file drags anywhere on the desktop and summons \
             the conversion wheel at your cursor when you tap the modifier."
        ),
        (SensorLang::Ar, WaylandTier::Orb, _) => format!(
            "على هذا المُركِّب ({hint}) يعمل أوروك كونفيرت على Wayland بوضع الكرة \
             العائمة: قرص شمس صغير ثابت في الزاوية السفلية اليمنى من الشاشة، وتحوّل \
             أي ملف بسحبه وإفلاته فوق القرص. بروتوكول Wayland لا يمنح التطبيقات \
             العادية أي وسيلة لمراقبة السحب على سطح المكتب كله أو لإظهار العجلة عند \
             المؤشر — هذا قيد في تصميم البروتوكول نفسه وليس إعدادًا ناقصًا. أما \
             التطبيقات التي تعمل عبر XWayland فيغطيها مسار X11 الكامل، بالعجلة عند \
             المؤشر أيضًا."
        ),
        (SensorLang::Ar, WaylandTier::MenuOnly, true) => format!(
            "على هذا المُركِّب ({hint}) يعمل Wayland بوضعه المخفَّض: المُركِّب لا \
             يتيح للتطبيقات العادية سطح layer-shell ولا مراقبة للسحب، فلا توجد كرة \
             عائمة ولا استشعار. حوّل من قائمة مدير الملفات («حوّل بأوروك كونفيرت») \
             أو بإفلات الملفات على نافذة الإفلات في التطبيق. التطبيقات التي تعمل \
             عبر XWayland يغطيها مسار X11 الكامل رغم ذلك، بالعجلة عند المؤشر أيضًا."
        ),
        (SensorLang::Ar, WaylandTier::MenuOnly, false) => format!(
            "هذه الجلسة لا تعمل على Wayland ({hint})، لذا لا تنطبق عليها مستويات \
             Wayland: تحت X11 يغطي مستشعر أوروك كونفيرت النمط كاملًا — يرى سحب \
             الملفات في أي مكان على سطح المكتب ويستدعي عجلة التحويل عند المؤشر \
             عندما تنقر مفتاح الاستدعاء."
        ),
    }
}

// ---------------------------------------------------------------------------
// text/uri-list parsing (shared by the orb drop path; unit-tested)
// ---------------------------------------------------------------------------

/// Parse a `text/uri-list` payload into local file paths.
///
/// Rules (RFC 2483 + the XDND/Wayland conventions): lines are CRLF
/// separated, `#` starts a comment line, empty lines are ignored, and
/// only `file://` URIs with an empty or `localhost` authority name a
/// local file — anything else (http:, remote hosts) is skipped rather
/// than guessed at. Percent-escapes are decoded as raw bytes so
/// non-ASCII file names survive intact; `+` is *not* a space (that is
/// form encoding, not URI encoding).
fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let text = String::from_utf8_lossy(data);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = strip_file_scheme(line) else {
            continue;
        };
        // rest = [authority]path — authority runs to the first '/'.
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if !(authority.is_empty() || authority.eq_ignore_ascii_case("localhost")) {
            continue; // a file on some other host — not ours to convert
        }
        if path.is_empty() {
            continue;
        }
        let bytes = percent_decode(path.as_bytes());
        out.push(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
    }
    out
}

fn strip_file_scheme(uri: &str) -> Option<&str> {
    let lower = uri.as_bytes();
    if lower.len() >= 7 && lower[..7].eq_ignore_ascii_case(b"file://") {
        Some(&uri[7..])
    } else {
        None
    }
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Hand-written zwlr_layer_shell_v1 client bindings
// ---------------------------------------------------------------------------

#[allow(dead_code)] // scanner-faithful bindings: not every request/constant is used yet
mod wlr {
    //! Minimal client bindings for `zwlr_layer_shell_v1` /
    //! `zwlr_layer_surface_v1` (wlr-layer-shell-unstable-v1, interface
    //! version 5), hand-written after the code `wayland-scanner`
    //! generates — the generated bindings live in the separate
    //! `wayland-protocols-wlr` crate, which is not a dependency here.
    //! Only the requests the orb uses have convenience methods; the
    //! full request enums are implemented so the wire format is
    //! complete. `c_ptr` is `None`: valid for the pure-Rust backend
    //! (see the module note at the top of this file).

    pub mod __interfaces {
        use wayland_client::backend::protocol::{AllowNull, ArgumentType, Interface, MessageDesc};

        pub static ZWLR_LAYER_SHELL_V1_INTERFACE: Interface = Interface {
            name: "zwlr_layer_shell_v1",
            version: 5,
            requests: &[
                MessageDesc {
                    name: "get_layer_surface",
                    signature: &[
                        ArgumentType::NewId,
                        ArgumentType::Object(AllowNull::No),
                        ArgumentType::Object(AllowNull::Yes),
                        ArgumentType::Uint,
                        ArgumentType::Str(AllowNull::No),
                    ],
                    since: 1,
                    is_destructor: false,
                    child_interface: Some(&ZWLR_LAYER_SURFACE_V1_INTERFACE),
                    arg_interfaces: &[
                        &wayland_client::protocol::__interfaces::WL_SURFACE_INTERFACE,
                        &wayland_client::protocol::__interfaces::WL_OUTPUT_INTERFACE,
                    ],
                },
                MessageDesc {
                    name: "destroy",
                    signature: &[],
                    since: 3,
                    is_destructor: true,
                    child_interface: None,
                    arg_interfaces: &[],
                },
            ],
            events: &[],
            c_ptr: None,
        };

        pub static ZWLR_LAYER_SURFACE_V1_INTERFACE: Interface = Interface {
            name: "zwlr_layer_surface_v1",
            version: 5,
            requests: &[
                MessageDesc {
                    name: "set_size",
                    signature: &[ArgumentType::Uint, ArgumentType::Uint],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_anchor",
                    signature: &[ArgumentType::Uint],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_exclusive_zone",
                    signature: &[ArgumentType::Int],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_margin",
                    signature: &[
                        ArgumentType::Int,
                        ArgumentType::Int,
                        ArgumentType::Int,
                        ArgumentType::Int,
                    ],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_keyboard_interactivity",
                    signature: &[ArgumentType::Uint],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "get_popup",
                    signature: &[ArgumentType::Object(AllowNull::No)],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[&wayland_client::backend::protocol::ANONYMOUS_INTERFACE],
                },
                MessageDesc {
                    name: "ack_configure",
                    signature: &[ArgumentType::Uint],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "destroy",
                    signature: &[],
                    since: 1,
                    is_destructor: true,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_layer",
                    signature: &[ArgumentType::Uint],
                    since: 2,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "set_exclusive_edge",
                    signature: &[ArgumentType::Uint],
                    since: 5,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
            ],
            events: &[
                MessageDesc {
                    name: "configure",
                    signature: &[ArgumentType::Uint, ArgumentType::Uint, ArgumentType::Uint],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
                MessageDesc {
                    name: "closed",
                    signature: &[],
                    since: 1,
                    is_destructor: false,
                    child_interface: None,
                    arg_interfaces: &[],
                },
            ],
            c_ptr: None,
        };
    }

    pub mod zwlr_layer_shell_v1 {
        use std::sync::Arc;

        use wayland_client::backend::{InvalidId, ObjectData, ObjectId, WeakBackend};
        use wayland_client::protocol::{wl_output, wl_surface};
        use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

        use super::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

        #[repr(u32)]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum Error {
            Role = 0,
            InvalidLayer = 1,
            AlreadyConstructed = 2,
        }

        impl std::convert::TryFrom<u32> for Error {
            type Error = ();
            fn try_from(val: u32) -> Result<Error, ()> {
                match val {
                    0 => Ok(Error::Role),
                    1 => Ok(Error::InvalidLayer),
                    2 => Ok(Error::AlreadyConstructed),
                    _ => Err(()),
                }
            }
        }
        impl std::convert::From<Error> for u32 {
            fn from(val: Error) -> u32 {
                val as u32
            }
        }

        #[repr(u32)]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum Layer {
            Background = 0,
            Bottom = 1,
            Top = 2,
            Overlay = 3,
        }

        impl std::convert::TryFrom<u32> for Layer {
            type Error = ();
            fn try_from(val: u32) -> Result<Layer, ()> {
                match val {
                    0 => Ok(Layer::Background),
                    1 => Ok(Layer::Bottom),
                    2 => Ok(Layer::Top),
                    3 => Ok(Layer::Overlay),
                    _ => Err(()),
                }
            }
        }
        impl std::convert::From<Layer> for u32 {
            fn from(val: Layer) -> u32 {
                val as u32
            }
        }

        pub const REQ_GET_LAYER_SURFACE_SINCE: u32 = 1;
        pub const REQ_GET_LAYER_SURFACE_OPCODE: u16 = 0;
        pub const REQ_DESTROY_SINCE: u32 = 3;
        pub const REQ_DESTROY_OPCODE: u16 = 1;

        #[derive(Debug)]
        #[non_exhaustive]
        #[allow(non_camel_case_types)] // variant name mirrors wayland-scanner output
        pub enum Request<'a> {
            GetLayerSurface {
                surface: wl_surface::WlSurface,
                output: Option<wl_output::WlOutput>,
                layer: wayland_client::WEnum<Layer>,
                namespace: String,
            },
            Destroy,
            #[doc(hidden)]
            __phantom_lifetime {
                phantom: std::marker::PhantomData<&'a ()>,
                never: std::convert::Infallible,
            },
        }

        impl<'a> Request<'a> {
            pub fn opcode(&self) -> u16 {
                match *self {
                    Request::GetLayerSurface { .. } => 0,
                    Request::Destroy => 1,
                    Request::__phantom_lifetime { never, .. } => match never {},
                }
            }
        }

        #[derive(Debug)]
        #[non_exhaustive]
        pub enum Event {}

        #[derive(Debug, Clone)]
        pub struct ZwlrLayerShellV1 {
            id: ObjectId,
            version: u32,
            data: Option<Arc<dyn ObjectData>>,
            backend: WeakBackend,
        }

        impl std::cmp::PartialEq for ZwlrLayerShellV1 {
            fn eq(&self, other: &ZwlrLayerShellV1) -> bool {
                self.id == other.id
            }
        }
        impl std::cmp::Eq for ZwlrLayerShellV1 {}
        impl PartialEq<wayland_client::Weak<ZwlrLayerShellV1>> for ZwlrLayerShellV1 {
            fn eq(&self, other: &wayland_client::Weak<ZwlrLayerShellV1>) -> bool {
                self.id == other.id()
            }
        }
        impl std::borrow::Borrow<ObjectId> for ZwlrLayerShellV1 {
            fn borrow(&self) -> &ObjectId {
                &self.id
            }
        }
        impl std::hash::Hash for ZwlrLayerShellV1 {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state)
            }
        }

        impl Proxy for ZwlrLayerShellV1 {
            type Request<'request> = Request<'request>;
            type Event = Event;

            #[inline]
            fn interface() -> &'static wayland_client::backend::protocol::Interface {
                &super::__interfaces::ZWLR_LAYER_SHELL_V1_INTERFACE
            }
            #[inline]
            fn id(&self) -> ObjectId {
                self.id.clone()
            }
            #[inline]
            fn version(&self) -> u32 {
                self.version
            }
            #[inline]
            fn data<U: Send + Sync + 'static>(&self) -> Option<&U> {
                self.data
                    .as_ref()
                    .and_then(|arc| arc.data_as_any().downcast_ref::<U>())
            }
            fn object_data(&self) -> Option<&Arc<dyn ObjectData>> {
                self.data.as_ref()
            }
            fn backend(&self) -> &WeakBackend {
                &self.backend
            }
            fn send_request(&self, req: Self::Request<'_>) -> Result<(), InvalidId> {
                let conn = Connection::from_backend(self.backend.upgrade().ok_or(InvalidId)?);
                let id = conn.send_request(self, req, None)?;
                debug_assert!(id.is_null());
                Ok(())
            }
            fn send_constructor<I: Proxy>(
                &self,
                req: Self::Request<'_>,
                data: Arc<dyn ObjectData>,
            ) -> Result<I, InvalidId> {
                let conn = Connection::from_backend(self.backend.upgrade().ok_or(InvalidId)?);
                let id = conn.send_request(self, req, Some(data))?;
                Proxy::from_id(&conn, id)
            }
            #[inline]
            fn from_id(conn: &Connection, id: ObjectId) -> Result<Self, InvalidId> {
                use wayland_client::backend::protocol::same_interface;
                if !same_interface(id.interface(), Self::interface()) && !id.is_null() {
                    return Err(InvalidId);
                }
                let version = conn
                    .object_info(id.clone())
                    .map(|info| info.version)
                    .unwrap_or(0);
                let data = conn.get_object_data(id.clone()).ok();
                let backend = conn.backend().downgrade();
                Ok(ZwlrLayerShellV1 {
                    id,
                    data,
                    version,
                    backend,
                })
            }
            #[inline]
            fn inert(backend: WeakBackend) -> Self {
                ZwlrLayerShellV1 {
                    id: ObjectId::null(),
                    data: None,
                    version: 0,
                    backend,
                }
            }
            fn parse_event(
                conn: &Connection,
                msg: wayland_client::backend::protocol::Message<
                    ObjectId,
                    std::os::unix::io::OwnedFd,
                >,
            ) -> Result<(Self, Self::Event), wayland_client::DispatchError> {
                let me = Self::from_id(conn, msg.sender_id.clone()).unwrap();
                let _ = me;
                Err(wayland_client::DispatchError::BadMessage {
                    sender_id: msg.sender_id,
                    interface: Self::interface().name,
                    opcode: msg.opcode,
                })
            }
            fn write_request<'a>(
                &self,
                conn: &Connection,
                msg: Self::Request<'a>,
            ) -> Result<
                (
                    wayland_client::backend::protocol::Message<
                        ObjectId,
                        std::os::unix::io::BorrowedFd<'a>,
                    >,
                    Option<(&'static wayland_client::backend::protocol::Interface, u32)>,
                ),
                InvalidId,
            > {
                use wayland_client::backend::protocol::{Argument, Message};
                use wayland_client::backend::smallvec;
                match msg {
                    Request::GetLayerSurface {
                        surface,
                        output,
                        layer,
                        namespace,
                    } => {
                        let child_spec = {
                            let my_info = conn.object_info(self.id())?;
                            Some((ZwlrLayerSurfaceV1::interface(), my_info.version))
                        };
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::NewId(ObjectId::null()));
                            vec.push(Argument::Object(Proxy::id(&surface)));
                            vec.push(if let Some(obj) = output {
                                Argument::Object(Proxy::id(&obj))
                            } else {
                                Argument::Object(ObjectId::null())
                            });
                            vec.push(Argument::Uint(layer.into()));
                            vec.push(Argument::Str(Some(Box::new(
                                std::ffi::CString::new(namespace).unwrap(),
                            ))));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 0,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::Destroy => {
                        let child_spec = None;
                        let args = smallvec::SmallVec::new();
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 1,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::__phantom_lifetime { never, .. } => match never {},
                }
            }
        }

        impl ZwlrLayerShellV1 {
            pub fn get_layer_surface<
                U: Send + Sync + 'static,
                D: Dispatch<ZwlrLayerSurfaceV1, U> + 'static,
            >(
                &self,
                surface: &wl_surface::WlSurface,
                output: Option<&wl_output::WlOutput>,
                layer: Layer,
                namespace: String,
                qh: &QueueHandle<D>,
                udata: U,
            ) -> ZwlrLayerSurfaceV1 {
                self.send_constructor(
                    Request::GetLayerSurface {
                        surface: surface.clone(),
                        output: output.cloned(),
                        layer: wayland_client::WEnum::Value(layer),
                        namespace,
                    },
                    qh.make_data::<ZwlrLayerSurfaceV1, U>(udata),
                )
                .unwrap_or_else(|_| Proxy::inert(self.backend.clone()))
            }

            pub fn destroy(&self) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(self, Request::Destroy, None);
            }
        }
    }

    pub mod zwlr_layer_surface_v1 {
        use std::sync::Arc;

        use wayland_client::backend::{InvalidId, ObjectData, ObjectId, WeakBackend};
        use wayland_client::{Connection, Proxy};

        use super::zwlr_layer_shell_v1;

        #[repr(u32)]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum Error {
            InvalidSurfaceState = 0,
            InvalidSize = 1,
            InvalidAnchor = 2,
            InvalidKeyboardInteractivity = 3,
            InvalidExclusiveEdge = 4,
        }

        impl std::convert::TryFrom<u32> for Error {
            type Error = ();
            fn try_from(val: u32) -> Result<Error, ()> {
                match val {
                    0 => Ok(Error::InvalidSurfaceState),
                    1 => Ok(Error::InvalidSize),
                    2 => Ok(Error::InvalidAnchor),
                    3 => Ok(Error::InvalidKeyboardInteractivity),
                    4 => Ok(Error::InvalidExclusiveEdge),
                    _ => Err(()),
                }
            }
        }
        impl std::convert::From<Error> for u32 {
            fn from(val: Error) -> u32 {
                val as u32
            }
        }

        /// Anchor bitfield. Hand-written equivalent of the
        /// `bitflags!` struct the scanner generates (the `bitflags`
        /// crate is not a dependency of the enclosing crate).
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Default)]
        #[allow(non_upper_case_globals)] // const names mirror bitflags!-generated code
        pub struct Anchor(u32);

        #[allow(non_upper_case_globals)] // const names mirror bitflags!-generated code
        impl Anchor {
            pub const Top: Self = Self(1);
            pub const Bottom: Self = Self(2);
            pub const Left: Self = Self(4);
            pub const Right: Self = Self(8);

            pub fn bits(self) -> u32 {
                self.0
            }
            pub fn from_bits(bits: u32) -> Option<Self> {
                if bits & !0b1111 == 0 {
                    Some(Self(bits))
                } else {
                    None
                }
            }
            pub fn empty() -> Self {
                Self(0)
            }
            pub fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }
        }

        impl std::ops::BitOr for Anchor {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }
        impl std::ops::BitOrAssign for Anchor {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }
        impl std::convert::TryFrom<u32> for Anchor {
            type Error = ();
            fn try_from(val: u32) -> Result<Anchor, ()> {
                Anchor::from_bits(val).ok_or(())
            }
        }
        impl std::convert::From<Anchor> for u32 {
            fn from(val: Anchor) -> u32 {
                val.bits()
            }
        }

        #[repr(u32)]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum KeyboardInteractivity {
            None = 0,
            Exclusive = 1,
            OnDemand = 2,
        }

        impl std::convert::TryFrom<u32> for KeyboardInteractivity {
            type Error = ();
            fn try_from(val: u32) -> Result<KeyboardInteractivity, ()> {
                match val {
                    0 => Ok(KeyboardInteractivity::None),
                    1 => Ok(KeyboardInteractivity::Exclusive),
                    2 => Ok(KeyboardInteractivity::OnDemand),
                    _ => Err(()),
                }
            }
        }
        impl std::convert::From<KeyboardInteractivity> for u32 {
            fn from(val: KeyboardInteractivity) -> u32 {
                val as u32
            }
        }

        pub const REQ_SET_SIZE_SINCE: u32 = 1;
        pub const REQ_SET_SIZE_OPCODE: u16 = 0;
        pub const REQ_SET_ANCHOR_SINCE: u32 = 1;
        pub const REQ_SET_ANCHOR_OPCODE: u16 = 1;
        pub const REQ_SET_EXCLUSIVE_ZONE_SINCE: u32 = 1;
        pub const REQ_SET_EXCLUSIVE_ZONE_OPCODE: u16 = 2;
        pub const REQ_SET_MARGIN_SINCE: u32 = 1;
        pub const REQ_SET_MARGIN_OPCODE: u16 = 3;
        pub const REQ_SET_KEYBOARD_INTERACTIVITY_SINCE: u32 = 1;
        pub const REQ_SET_KEYBOARD_INTERACTIVITY_OPCODE: u16 = 4;
        pub const REQ_GET_POPUP_SINCE: u32 = 1;
        pub const REQ_GET_POPUP_OPCODE: u16 = 5;
        pub const REQ_ACK_CONFIGURE_SINCE: u32 = 1;
        pub const REQ_ACK_CONFIGURE_OPCODE: u16 = 6;
        pub const REQ_DESTROY_SINCE: u32 = 1;
        pub const REQ_DESTROY_OPCODE: u16 = 7;
        pub const REQ_SET_LAYER_SINCE: u32 = 2;
        pub const REQ_SET_LAYER_OPCODE: u16 = 8;
        pub const REQ_SET_EXCLUSIVE_EDGE_SINCE: u32 = 5;
        pub const REQ_SET_EXCLUSIVE_EDGE_OPCODE: u16 = 9;
        pub const EVT_CONFIGURE_SINCE: u32 = 1;
        pub const EVT_CONFIGURE_OPCODE: u16 = 0;
        pub const EVT_CLOSED_SINCE: u32 = 1;
        pub const EVT_CLOSED_OPCODE: u16 = 1;

        #[derive(Debug)]
        #[non_exhaustive]
        #[allow(non_camel_case_types)] // variant name mirrors wayland-scanner output
        pub enum Request<'a> {
            SetSize {
                width: u32,
                height: u32,
            },
            SetAnchor {
                anchor: wayland_client::WEnum<Anchor>,
            },
            SetExclusiveZone {
                zone: i32,
            },
            SetMargin {
                top: i32,
                right: i32,
                bottom: i32,
                left: i32,
            },
            SetKeyboardInteractivity {
                keyboard_interactivity: wayland_client::WEnum<KeyboardInteractivity>,
            },
            GetPopup {
                popup: wayland_protocols::xdg::shell::client::xdg_popup::XdgPopup,
            },
            AckConfigure {
                serial: u32,
            },
            Destroy,
            SetLayer {
                layer: wayland_client::WEnum<zwlr_layer_shell_v1::Layer>,
            },
            SetExclusiveEdge {
                edge: wayland_client::WEnum<Anchor>,
            },
            #[doc(hidden)]
            __phantom_lifetime {
                phantom: std::marker::PhantomData<&'a ()>,
                never: std::convert::Infallible,
            },
        }

        impl<'a> Request<'a> {
            pub fn opcode(&self) -> u16 {
                match *self {
                    Request::SetSize { .. } => 0,
                    Request::SetAnchor { .. } => 1,
                    Request::SetExclusiveZone { .. } => 2,
                    Request::SetMargin { .. } => 3,
                    Request::SetKeyboardInteractivity { .. } => 4,
                    Request::GetPopup { .. } => 5,
                    Request::AckConfigure { .. } => 6,
                    Request::Destroy => 7,
                    Request::SetLayer { .. } => 8,
                    Request::SetExclusiveEdge { .. } => 9,
                    Request::__phantom_lifetime { never, .. } => match never {},
                }
            }
        }

        #[derive(Debug)]
        #[non_exhaustive]
        pub enum Event {
            Configure {
                serial: u32,
                width: u32,
                height: u32,
            },
            Closed,
        }

        #[derive(Debug, Clone)]
        pub struct ZwlrLayerSurfaceV1 {
            id: ObjectId,
            version: u32,
            data: Option<Arc<dyn ObjectData>>,
            backend: WeakBackend,
        }

        impl std::cmp::PartialEq for ZwlrLayerSurfaceV1 {
            fn eq(&self, other: &ZwlrLayerSurfaceV1) -> bool {
                self.id == other.id
            }
        }
        impl std::cmp::Eq for ZwlrLayerSurfaceV1 {}
        impl PartialEq<wayland_client::Weak<ZwlrLayerSurfaceV1>> for ZwlrLayerSurfaceV1 {
            fn eq(&self, other: &wayland_client::Weak<ZwlrLayerSurfaceV1>) -> bool {
                self.id == other.id()
            }
        }
        impl std::borrow::Borrow<ObjectId> for ZwlrLayerSurfaceV1 {
            fn borrow(&self) -> &ObjectId {
                &self.id
            }
        }
        impl std::hash::Hash for ZwlrLayerSurfaceV1 {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state)
            }
        }

        impl Proxy for ZwlrLayerSurfaceV1 {
            type Request<'request> = Request<'request>;
            type Event = Event;

            #[inline]
            fn interface() -> &'static wayland_client::backend::protocol::Interface {
                &super::__interfaces::ZWLR_LAYER_SURFACE_V1_INTERFACE
            }
            #[inline]
            fn id(&self) -> ObjectId {
                self.id.clone()
            }
            #[inline]
            fn version(&self) -> u32 {
                self.version
            }
            #[inline]
            fn data<U: Send + Sync + 'static>(&self) -> Option<&U> {
                self.data
                    .as_ref()
                    .and_then(|arc| arc.data_as_any().downcast_ref::<U>())
            }
            fn object_data(&self) -> Option<&Arc<dyn ObjectData>> {
                self.data.as_ref()
            }
            fn backend(&self) -> &WeakBackend {
                &self.backend
            }
            fn send_request(&self, req: Self::Request<'_>) -> Result<(), InvalidId> {
                let conn = Connection::from_backend(self.backend.upgrade().ok_or(InvalidId)?);
                let id = conn.send_request(self, req, None)?;
                debug_assert!(id.is_null());
                Ok(())
            }
            fn send_constructor<I: Proxy>(
                &self,
                req: Self::Request<'_>,
                data: Arc<dyn ObjectData>,
            ) -> Result<I, InvalidId> {
                let conn = Connection::from_backend(self.backend.upgrade().ok_or(InvalidId)?);
                let id = conn.send_request(self, req, Some(data))?;
                Proxy::from_id(&conn, id)
            }
            #[inline]
            fn from_id(conn: &Connection, id: ObjectId) -> Result<Self, InvalidId> {
                use wayland_client::backend::protocol::same_interface;
                if !same_interface(id.interface(), Self::interface()) && !id.is_null() {
                    return Err(InvalidId);
                }
                let version = conn
                    .object_info(id.clone())
                    .map(|info| info.version)
                    .unwrap_or(0);
                let data = conn.get_object_data(id.clone()).ok();
                let backend = conn.backend().downgrade();
                Ok(ZwlrLayerSurfaceV1 {
                    id,
                    data,
                    version,
                    backend,
                })
            }
            #[inline]
            fn inert(backend: WeakBackend) -> Self {
                ZwlrLayerSurfaceV1 {
                    id: ObjectId::null(),
                    data: None,
                    version: 0,
                    backend,
                }
            }
            fn parse_event(
                conn: &Connection,
                msg: wayland_client::backend::protocol::Message<
                    ObjectId,
                    std::os::unix::io::OwnedFd,
                >,
            ) -> Result<(Self, Self::Event), wayland_client::DispatchError> {
                use wayland_client::backend::protocol::Argument;
                use wayland_client::DispatchError;
                let me = Self::from_id(conn, msg.sender_id.clone()).unwrap();
                let mut arg_iter = msg.args.into_iter();
                match msg.opcode {
                    0 => {
                        if let (
                            Some(Argument::Uint(serial)),
                            Some(Argument::Uint(width)),
                            Some(Argument::Uint(height)),
                        ) = (arg_iter.next(), arg_iter.next(), arg_iter.next())
                        {
                            Ok((
                                me,
                                Event::Configure {
                                    serial,
                                    width,
                                    height,
                                },
                            ))
                        } else {
                            Err(DispatchError::BadMessage {
                                sender_id: msg.sender_id,
                                interface: Self::interface().name,
                                opcode: msg.opcode,
                            })
                        }
                    }
                    1 => Ok((me, Event::Closed)),
                    _ => Err(DispatchError::BadMessage {
                        sender_id: msg.sender_id,
                        interface: Self::interface().name,
                        opcode: msg.opcode,
                    }),
                }
            }
            fn write_request<'a>(
                &self,
                _conn: &Connection,
                msg: Self::Request<'a>,
            ) -> Result<
                (
                    wayland_client::backend::protocol::Message<
                        ObjectId,
                        std::os::unix::io::BorrowedFd<'a>,
                    >,
                    Option<(&'static wayland_client::backend::protocol::Interface, u32)>,
                ),
                InvalidId,
            > {
                use wayland_client::backend::protocol::{Argument, Message};
                use wayland_client::backend::smallvec;
                match msg {
                    Request::SetSize { width, height } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(width));
                            vec.push(Argument::Uint(height));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 0,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetAnchor { anchor } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(anchor.into()));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 1,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetExclusiveZone { zone } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Int(zone));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 2,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetMargin {
                        top,
                        right,
                        bottom,
                        left,
                    } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Int(top));
                            vec.push(Argument::Int(right));
                            vec.push(Argument::Int(bottom));
                            vec.push(Argument::Int(left));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 3,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetKeyboardInteractivity {
                        keyboard_interactivity,
                    } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(keyboard_interactivity.into()));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 4,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::GetPopup { popup } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Object(Proxy::id(&popup)));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 5,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::AckConfigure { serial } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(serial));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 6,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::Destroy => {
                        let child_spec = None;
                        let args = smallvec::SmallVec::new();
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 7,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetLayer { layer } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(layer.into()));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 8,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::SetExclusiveEdge { edge } => {
                        let child_spec = None;
                        let args = {
                            let mut vec = smallvec::SmallVec::new();
                            vec.push(Argument::Uint(edge.into()));
                            vec
                        };
                        Ok((
                            Message {
                                sender_id: self.id.clone(),
                                opcode: 9,
                                args,
                            },
                            child_spec,
                        ))
                    }
                    Request::__phantom_lifetime { never, .. } => match never {},
                }
            }
        }

        impl ZwlrLayerSurfaceV1 {
            pub fn set_size(&self, width: u32, height: u32) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(self, Request::SetSize { width, height }, None);
            }
            pub fn set_anchor(&self, anchor: Anchor) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(
                    self,
                    Request::SetAnchor {
                        anchor: wayland_client::WEnum::Value(anchor),
                    },
                    None,
                );
            }
            pub fn set_exclusive_zone(&self, zone: i32) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(self, Request::SetExclusiveZone { zone }, None);
            }
            pub fn set_margin(&self, top: i32, right: i32, bottom: i32, left: i32) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(
                    self,
                    Request::SetMargin {
                        top,
                        right,
                        bottom,
                        left,
                    },
                    None,
                );
            }
            pub fn set_keyboard_interactivity(
                &self,
                keyboard_interactivity: KeyboardInteractivity,
            ) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(
                    self,
                    Request::SetKeyboardInteractivity {
                        keyboard_interactivity: wayland_client::WEnum::Value(
                            keyboard_interactivity,
                        ),
                    },
                    None,
                );
            }
            pub fn ack_configure(&self, serial: u32) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(self, Request::AckConfigure { serial }, None);
            }
            pub fn destroy(&self) {
                let backend = match self.backend.upgrade() {
                    Some(b) => b,
                    None => return,
                };
                let conn = Connection::from_backend(backend);
                let _ = conn.send_request(self, Request::Destroy, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The standing orb
// ---------------------------------------------------------------------------

/// Orb surface size in pixels (square). Anchored bottom-right.
pub const ORB_SIZE: u32 = 96;
/// Gap between the orb and the screen corner, in pixels.
pub const ORB_MARGIN: i32 = 24;
/// Layer-shell namespace for the orb surface.
const ORB_NAMESPACE: &str = "uruk-convert-orb";

#[derive(Debug, Clone)]
pub enum OrbEvent {
    Dropped { paths: Vec<PathBuf> },
}

pub struct OrbHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OrbHandle {
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start the standing orb on a layer-shell surface. Errors when the
/// compositor offers no layer-shell (GNOME) — callers fall back to the
/// plain drop window and must not treat it as fatal.
///
/// The orb thread runs its own Wayland connection. If the compositor
/// later removes layer-shell or closes the surface (compositor
/// restart), the thread exits cleanly and logs one line to stderr;
/// the returned handle stays stoppable throughout.
pub fn start_orb(events: Sender<OrbEvent>) -> Result<OrbHandle, SensorError> {
    // Fail synchronously where we can: probing is bounded (1.5 s) and
    // turns the common GNOME case into a clean error, not a dead thread.
    let info = probe_tier();
    if info.tier != WaylandTier::Orb {
        return Err(SensorError::new(
            "no_layer_shell",
            format!(
                "Wayland orb unavailable on this session ({}): {}",
                info.compositor_hint, info.session_note
            ),
        ));
    }
    let stop_flag = Arc::new(AtomicBool::new(false));
    let flag = stop_flag.clone();
    let thread = std::thread::Builder::new()
        .name("uruk-wayland-orb".into())
        .spawn(move || {
            if let Err(e) = run_orb(events, flag) {
                // The compositor disappearing mid-run lands here:
                // swallowed into a log line by design — nothing the
                // host can do about it, and the handle must remain
                // stoppable, which it does (the thread has exited).
                eprintln!("uruk-convert: wayland orb stopped: {e}");
            }
        })
        .map_err(|e| SensorError::new("io", format!("cannot spawn orb thread: {e}")))?;
    Ok(OrbHandle {
        stop_flag,
        thread: Some(thread),
    })
}

// --- orb rendering ----------------------------------------------------------

/// Render the orb: a lapis disc with a gold Shamash star (the product
/// mark), drawn with tiny-skia exactly like the wheel hub in
/// [`crate::render`]. `hover` brightens disc and star while a file
/// drag is over the surface.
fn render_orb(width: u32, height: u32, hover: bool) -> Option<tiny_skia::Pixmap> {
    use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

    let mut pm = Pixmap::new(width.max(1), height.max(1))?;
    let (w, h) = (width as f32, height as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r = (w.min(h) / 2.0 - 2.0).max(4.0);

    let lapis: (u8, u8, u8) = if hover { (24, 55, 105) } else { (11, 30, 58) };
    let gold: (u8, u8, u8) = if hover {
        (243, 213, 110)
    } else {
        (201, 162, 39)
    };

    let paint = |c: (u8, u8, u8), a: u8| {
        let mut p = Paint::default();
        p.set_color(tiny_skia::Color::from_rgba8(c.0, c.1, c.2, a));
        p.anti_alias = true;
        p
    };
    let circle = |cx: f32, cy: f32, r: f32| {
        let k = 0.552_284_8 * r;
        let mut b = PathBuilder::new();
        b.move_to(cx + r, cy);
        b.cubic_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
        b.cubic_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
        b.cubic_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
        b.cubic_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
        b.close();
        b.finish().expect("circle path")
    };

    let disc = circle(cx, cy, r);
    pm.fill_path(
        &disc,
        &paint(lapis, 245),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut stroke = Stroke::default();
    stroke.width = 2.5;
    pm.stroke_path(
        &disc,
        &paint(gold, 255),
        &stroke,
        Transform::identity(),
        None,
    );

    let star_pts = crate::geom::star_points(cx, cy, r * 0.55, r * 0.23);
    let mut b = PathBuilder::new();
    for (i, (x, y)) in star_pts.iter().enumerate() {
        if i == 0 {
            b.move_to(*x, *y);
        } else {
            b.line_to(*x, *y);
        }
    }
    b.close();
    if let Some(star) = b.finish() {
        pm.fill_path(
            &star,
            &paint(gold, 255),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let core = circle(cx, cy, r * 0.14);
    pm.fill_path(
        &core,
        &paint(lapis, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    Some(pm)
}

// --- orb thread -------------------------------------------------------------

mod orb_impl {
    use std::os::unix::io::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
        wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};

    use super::wlr::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
    use super::wlr::zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1};
    use super::{parse_uri_list, render_orb, OrbEvent, ORB_MARGIN, ORB_NAMESPACE, ORB_SIZE};
    use crate::SensorError;

    pub struct OrbState {
        pub events: Sender<OrbEvent>,
        // Globals, bound as the registry announces them.
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        layer_shell: Option<ZwlrLayerShellV1>,
        layer_shell_name: Option<u32>,
        seat: Option<wl_seat::WlSeat>,
        data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
        data_device: Option<wl_data_device::WlDataDevice>,
        // The orb surface pair.
        surface: Option<wl_surface::WlSurface>,
        layer_surface: Option<ZwlrLayerSurfaceV1>,
        configured: bool,
        size: (u32, u32),
        attached_size: Option<(u32, u32)>,
        hover: bool,
        attached_hover: Option<bool>,
        closed: bool,
        // Buffers are kept alive for the orb's lifetime: a compositor
        // may still be reading one after we attach the next, and at
        // 96×96×4 B a handful of retained buffers is a few hundred KB.
        pools: Vec<wl_shm_pool::WlShmPool>,
        buffers: Vec<wl_buffer::WlBuffer>,
        // Drag-and-drop tracking. `live_offer` is the newest offer the
        // device announced; `drag_offer` is the one an `enter` named.
        live_offer: Option<wl_data_offer::WlDataOffer>,
        live_offer_is_selection: bool,
        offer_mimes: Vec<String>,
        drag_offer: Option<wl_data_offer::WlDataOffer>,
        drag_accepted: bool,
    }

    impl OrbState {
        fn new(events: Sender<OrbEvent>) -> Self {
            OrbState {
                events,
                compositor: None,
                shm: None,
                layer_shell: None,
                layer_shell_name: None,
                seat: None,
                data_manager: None,
                data_device: None,
                surface: None,
                layer_surface: None,
                configured: false,
                size: (ORB_SIZE, ORB_SIZE),
                attached_size: None,
                hover: false,
                attached_hover: None,
                closed: false,
                pools: Vec::new(),
                buffers: Vec::new(),
                live_offer: None,
                live_offer_is_selection: false,
                offer_mimes: Vec::new(),
                drag_offer: None,
                drag_accepted: false,
            }
        }

        fn maybe_create_layer_surface(&mut self, qh: &QueueHandle<Self>) {
            if self.surface.is_some() {
                return;
            }
            let (Some(compositor), Some(shell)) =
                (self.compositor.as_ref(), self.layer_shell.as_ref())
            else {
                return;
            };
            let surface = compositor.create_surface(qh, ());
            let layer_surface = shell.get_layer_surface(
                &surface,
                None,
                zwlr_layer_shell_v1::Layer::Overlay,
                ORB_NAMESPACE.to_string(),
                qh,
                (),
            );
            layer_surface.set_anchor(
                zwlr_layer_surface_v1::Anchor::Right | zwlr_layer_surface_v1::Anchor::Bottom,
            );
            layer_surface.set_margin(0, ORB_MARGIN, ORB_MARGIN, 0);
            layer_surface.set_size(ORB_SIZE, ORB_SIZE);
            layer_surface
                .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            // -1: float over panels rather than reserving an edge zone.
            layer_surface.set_exclusive_zone(-1);
            // Initial commit with no buffer: the compositor answers
            // with the first configure, and only then may we attach.
            surface.commit();
            self.surface = Some(surface);
            self.layer_surface = Some(layer_surface);
        }

        fn maybe_create_data_device(&mut self, qh: &QueueHandle<Self>) {
            if self.data_device.is_some() {
                return;
            }
            let (Some(manager), Some(seat)) = (self.data_manager.as_ref(), self.seat.as_ref())
            else {
                return;
            };
            self.data_device = Some(manager.get_data_device(seat, qh, ()));
        }

        /// Attach a freshly rendered orb buffer if anything the user
        /// would see has changed (first configure, resize, hover).
        fn maybe_attach(&mut self, qh: &QueueHandle<Self>) {
            if !self.configured || self.closed {
                return;
            }
            if self.attached_size == Some(self.size) && self.attached_hover == Some(self.hover) {
                return;
            }
            let (Some(surface), Some(shm)) = (self.surface.clone(), self.shm.clone()) else {
                return;
            };
            let (w, h) = self.size;
            let Some((pool, buffer)) = Self::make_buffer_impl(&shm, qh, w, h, self.hover) else {
                return;
            };
            self.pools.push(pool);
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            self.buffers.push(buffer);
            self.attached_size = Some(self.size);
            self.attached_hover = Some(self.hover);
        }

        fn make_buffer_impl(
            shm: &wl_shm::WlShm,
            qh: &QueueHandle<Self>,
            w: u32,
            h: u32,
            hover: bool,
        ) -> Option<(wl_shm_pool::WlShmPool, wl_buffer::WlBuffer)> {
            let pm = render_orb(w, h, hover)?;
            let bgra = crate::render::pixmap_to_bgra(&pm);
            let size = (w * h * 4) as i32;
            if bgra.len() as i32 != size {
                return None;
            }
            // memfd-backed shm pool: no file to clean up, no /tmp use.
            let fd = unsafe {
                libc::memfd_create(
                    b"uruk-orb\0".as_ptr() as *const libc::c_char,
                    libc::MFD_CLOEXEC,
                )
            };
            if fd < 0 {
                return None;
            }
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
                return None;
            }
            let map = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if map == libc::MAP_FAILED {
                return None;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), map as *mut u8, bgra.len());
                libc::munmap(map, size as usize);
            }
            let pool = shm.create_pool(owned.as_fd(), size, qh, ());
            let buffer = pool.create_buffer(
                0,
                w as i32,
                h as i32,
                (w * 4) as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            Some((pool, buffer))
        }
    }

    // -- registry ------------------------------------------------------------

    impl Dispatch<wl_registry::WlRegistry, ()> for OrbState {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _data: &(),
            _conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            match event {
                wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                } => match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor =
                            Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                                name,
                                version.min(4),
                                qh,
                                (),
                            ));
                        state.maybe_create_layer_surface(qh);
                    }
                    "wl_shm" => {
                        state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ));
                    }
                    "wl_seat" => {
                        state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ));
                        state.maybe_create_data_device(qh);
                    }
                    "wl_data_device_manager" => {
                        state.data_manager = Some(
                            registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                                name,
                                version.min(1),
                                qh,
                                (),
                            ),
                        );
                        state.maybe_create_data_device(qh);
                    }
                    "zwlr_layer_shell_v1" => {
                        state.layer_shell = Some(registry.bind::<ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        ));
                        state.layer_shell_name = Some(name);
                        state.maybe_create_layer_surface(qh);
                    }
                    _ => {}
                },
                wl_registry::Event::GlobalRemove { name } => {
                    // The compositor took layer-shell away (restart):
                    // the surface will be closed; end the orb cleanly.
                    if state.layer_shell_name == Some(name) {
                        state.closed = true;
                    }
                }
                _ => {}
            }
        }
    }

    // -- layer surface ---------------------------------------------------------

    impl Dispatch<ZwlrLayerShellV1, ()> for OrbState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwlrLayerShellV1,
            event: zwlr_layer_shell_v1::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            // zwlr_layer_shell_v1 has no events.
            match event {}
        }
    }

    impl Dispatch<ZwlrLayerSurfaceV1, ()> for OrbState {
        fn event(
            state: &mut Self,
            layer_surface: &ZwlrLayerSurfaceV1,
            event: zwlr_layer_surface_v1::Event,
            _data: &(),
            _conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    layer_surface.ack_configure(serial);
                    // A zero dimension means "you decide" — keep the
                    // orb's own size on that axis.
                    let w = if width == 0 { ORB_SIZE } else { width };
                    let h = if height == 0 { ORB_SIZE } else { height };
                    if (w, h) != state.size {
                        state.size = (w, h);
                        state.attached_size = None;
                    }
                    state.configured = true;
                    state.maybe_attach(qh);
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    state.closed = true;
                }
            }
        }
    }

    // -- data device (the DnD target plumbing) --------------------------------

    impl Dispatch<wl_data_device::WlDataDevice, ()> for OrbState {
        fn event(
            state: &mut Self,
            _proxy: &wl_data_device::WlDataDevice,
            event: wl_data_device::Event,
            _data: &(),
            conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            match event {
                wl_data_device::Event::DataOffer { id } => {
                    // A fresh offer (drag or selection — told apart
                    // when an enter/selection event names it). Replace
                    // the tracked one; a stale drag offer is destroyed
                    // so compositors don't accumulate dead offers.
                    if let Some(old) = state.live_offer.replace(id) {
                        if state.drag_offer.as_ref().map(|o| o.id()) != Some(old.id()) {
                            old.destroy();
                        }
                    }
                    state.live_offer_is_selection = false;
                    state.offer_mimes.clear();
                }
                wl_data_device::Event::Enter {
                    serial,
                    surface,
                    id,
                    ..
                } => {
                    let on_orb = state
                        .surface
                        .as_ref()
                        .map(|s| s.id() == surface.id())
                        .unwrap_or(false);
                    if !on_orb {
                        return;
                    }
                    state.drag_offer = id.clone();
                    let Some(offer) = id else { return };
                    // The offer's mime list may not have been fully
                    // delivered if our tracking was reset by a
                    // selection offer; an unknown (empty) list is
                    // accepted optimistically — a bogus drop then
                    // simply reads nothing and emits nothing.
                    let known = state
                        .live_offer
                        .as_ref()
                        .map(|o| o.id() == offer.id())
                        .unwrap_or(false)
                        && !state.live_offer_is_selection;
                    let has_uri_list = state.offer_mimes.iter().any(|m| m == "text/uri-list");
                    if known && !state.offer_mimes.is_empty() && !has_uri_list {
                        offer.accept(serial, None);
                        state.drag_accepted = false;
                        return;
                    }
                    offer.accept(serial, Some("text/uri-list".to_string()));
                    if offer.version() >= 3 {
                        let _ = offer.send_request(wl_data_offer::Request::SetActions {
                            dnd_actions: WEnum::Value(wl_data_device_manager::DndAction::Copy),
                            preferred_action: WEnum::Value(wl_data_device_manager::DndAction::Copy),
                        });
                    }
                    state.drag_accepted = true;
                    if !state.hover {
                        state.hover = true;
                        state.maybe_attach(qh);
                    }
                }
                wl_data_device::Event::Leave => {
                    state.drag_offer = None;
                    state.drag_accepted = false;
                    if state.hover {
                        state.hover = false;
                        state.maybe_attach(qh);
                    }
                }
                wl_data_device::Event::Motion { .. } => {}
                wl_data_device::Event::Drop => {
                    let offer = state.drag_offer.take();
                    let accepted = state.drag_accepted;
                    state.drag_accepted = false;
                    if state.hover {
                        state.hover = false;
                        state.maybe_attach(qh);
                    }
                    let Some(offer) = offer else { return };
                    if !accepted {
                        offer.destroy();
                        return;
                    }
                    if let Some(data) = receive_offer(&offer, conn) {
                        let paths: Vec<PathBuf> = parse_uri_list(&data);
                        if !paths.is_empty() {
                            let _ = state.events.send(OrbEvent::Dropped { paths });
                        }
                    }
                    if offer.version() >= 3 {
                        offer.finish();
                    }
                    offer.destroy();
                    if state.live_offer.as_ref().map(|o| o.id()) == Some(offer.id()) {
                        state.live_offer = None;
                    }
                }
                wl_data_device::Event::Selection { id } => {
                    // Clipboard selection offers arrive through the
                    // same DataOffer event; flag the tracked offer so
                    // its mime list is never mistaken for a drag's.
                    if let (Some(sel), Some(live)) = (id.as_ref(), state.live_offer.as_ref()) {
                        if sel.id() == live.id() {
                            state.live_offer_is_selection = true;
                        }
                    }
                }
                _ => {}
            }
        }

        wayland_client::event_created_child!(OrbState, wl_data_device::WlDataDevice, [
            wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ())
        ]);
    }

    impl Dispatch<wl_data_offer::WlDataOffer, ()> for OrbState {
        fn event(
            state: &mut Self,
            offer: &wl_data_offer::WlDataOffer,
            event: wl_data_offer::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            if let wl_data_offer::Event::Offer { mime_type } = event {
                if state
                    .live_offer
                    .as_ref()
                    .map(|o| o.id() == offer.id())
                    .unwrap_or(false)
                {
                    state.offer_mimes.push(mime_type);
                }
            }
        }
    }

    delegate_noop!(OrbState: ignore wl_compositor::WlCompositor);
    delegate_noop!(OrbState: ignore wl_surface::WlSurface);
    delegate_noop!(OrbState: ignore wl_shm::WlShm);
    delegate_noop!(OrbState: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(OrbState: ignore wl_buffer::WlBuffer);
    delegate_noop!(OrbState: ignore wl_seat::WlSeat);
    delegate_noop!(OrbState: ignore wl_data_device_manager::WlDataDeviceManager);

    /// Ask the source for the uri-list payload and read it.
    ///
    /// This is a blocking read on the orb thread: the `receive`
    /// request must be flushed first (the source only writes once it
    /// sees it), then the source writes the whole (small) payload and
    /// closes the fd. A 3 s poll deadline guards against a source
    /// that never answers — a stalled drop must not wedge the orb.
    fn receive_offer(offer: &wl_data_offer::WlDataOffer, conn: &Connection) -> Option<Vec<u8>> {
        let mut fds: [libc::c_int; 2] = [-1, -1];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return None;
        }
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        offer.receive("text/uri-list".to_string(), write_end.as_fd());
        drop(write_end);
        if conn.flush().is_err() {
            return None;
        }
        let raw = read_end.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if Instant::now() >= deadline {
                break;
            }
            let mut pfd = libc::pollfd {
                fd: raw,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 250) };
            if rc < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if rc == 0 {
                continue; // poll tick — re-check the deadline
            }
            if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n =
                    unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n == 0 {
                    break; // source closed its end — payload complete
                }
                if n < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
        }
        Some(out)
    }

    // -- thread body -------------------------------------------------------------

    pub fn run_orb(events: Sender<OrbEvent>, stop: Arc<AtomicBool>) -> Result<(), SensorError> {
        let conn = Connection::connect_to_env().map_err(|e| {
            SensorError::new("connect", format!("cannot connect to Wayland display: {e}"))
        })?;
        let mut queue = conn.new_event_queue::<OrbState>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = OrbState::new(events);
        queue
            .roundtrip(&mut state)
            .map_err(|e| SensorError::new("dispatch", format!("registry roundtrip: {e}")))?;
        if state.layer_surface.is_none() {
            return Err(SensorError::new(
                "no_layer_shell",
                "compositor stopped advertising zwlr_layer_shell_v1 during startup",
            ));
        }
        state.maybe_attach(&qh);

        // Stop-check cadence: ~100 ms. blocking_dispatch would park
        // the thread on the socket indefinitely and `stop()` could
        // not join it, so the loop instead does a prepare-read +
        // poll(100 ms) on the connection fd and checks the flag each
        // turn. The cost is 10 wakeups/s on an otherwise idle agent —
        // accepted and documented rather than hidden, because a
        // calloop-style cross-thread wakeup is not available to this
        // crate without another dependency.
        while !stop.load(Ordering::SeqCst) && !state.closed {
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| SensorError::new("dispatch", format!("orb dispatch: {e}")))?;
            let _ = conn.flush();
            match conn.prepare_read() {
                Some(guard) => {
                    let fd = guard.connection_fd();
                    let mut pfd = libc::pollfd {
                        fd: fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if rc > 0 && pfd.revents & libc::POLLIN != 0 {
                        guard
                            .read()
                            .map_err(|e| SensorError::new("io", format!("orb socket read: {e}")))?;
                    }
                    // Otherwise the guard drops here, cancelling the
                    // prepared read; the loop re-checks the stop flag.
                }
                None => {
                    // Events are already queued: dispatch them now.
                    queue
                        .dispatch_pending(&mut state)
                        .map_err(|e| SensorError::new("dispatch", format!("orb dispatch: {e}")))?;
                }
            }
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| SensorError::new("dispatch", format!("orb dispatch: {e}")))?;
        }

        // Polite teardown: destroy the layer surface before dropping
        // the connection, so the compositor doesn't log a dead role.
        if let Some(layer_surface) = state.layer_surface.take() {
            layer_surface.destroy();
        }
        if let Some(surface) = state.surface.take() {
            surface.destroy();
        }
        let _ = conn.flush();
        Ok(())
    }
}

use orb_impl::run_orb;

// ---------------------------------------------------------------------------
// Tests (headless: no display needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn env(session_type: Option<&str>, display: Option<&str>, desktop: &str) -> EnvFacts {
        EnvFacts {
            session_type: session_type.map(str::to_string),
            wayland_display: display.map(str::to_string),
            current_desktop: Some(desktop.to_string()),
            desktop_session: Some(desktop.to_lowercase()),
            display: None,
        }
    }

    fn globals(reachable: bool, layer_shell: bool, data_control: bool) -> GlobalFacts {
        GlobalFacts {
            reachable,
            has_layer_shell: layer_shell,
            has_data_control: data_control,
        }
    }

    #[test]
    fn gnome_env_is_menu_only_even_with_no_connection() {
        // Mutter exposes neither global to regular clients; with no
        // connection at all the answer must still be a clean MenuOnly.
        let info = decide_tier(
            &env(Some("wayland"), Some("wayland-1"), "GNOME"),
            globals(false, false, false),
        );
        assert_eq!(info.tier, WaylandTier::MenuOnly);
        assert_eq!(info.compositor_hint, "Mutter (GNOME)");
        assert!(!info.has_layer_shell);
        assert!(!info.has_data_control);
    }

    #[test]
    fn wlroots_globals_give_orb() {
        let info = decide_tier(
            &env(Some("wayland"), Some("wayland-1"), "Sway"),
            globals(true, true, true),
        );
        assert_eq!(info.tier, WaylandTier::Orb);
        assert_eq!(info.compositor_hint, "wlroots (Sway/Hyprland/…)");
        assert!(info.has_layer_shell);
        assert!(info.has_data_control);
        // The note must state the coverage honestly.
        assert!(info.session_note.contains("orb drop only"));
        assert!(info.session_note.contains("XWayland"));
    }

    #[test]
    fn kde_with_layer_shell_only_still_gives_orb() {
        // Data-control is a bonus (classification), never a
        // precondition: layer-shell alone is enough for the orb tier.
        let info = decide_tier(
            &env(Some("wayland"), Some("wayland-0"), "KDE"),
            globals(true, true, false),
        );
        assert_eq!(info.tier, WaylandTier::Orb);
        assert_eq!(info.compositor_hint, "KWin");
        assert!(info.has_layer_shell);
        assert!(!info.has_data_control);
    }

    #[test]
    fn kde_without_layer_shell_is_menu_only() {
        let info = decide_tier(
            &env(Some("wayland"), Some("wayland-0"), "KDE"),
            globals(true, false, true),
        );
        assert_eq!(info.tier, WaylandTier::MenuOnly);
        assert!(info.has_data_control);
        assert!(!info.has_layer_shell);
    }

    #[test]
    fn no_wayland_display_is_not_a_wayland_session() {
        let info = decide_tier(&env(Some("x11"), None, "KDE"), globals(false, false, false));
        assert_eq!(info.tier, WaylandTier::MenuOnly);
        assert!(info.session_note.contains("not a Wayland session"));
    }

    #[test]
    fn unreachable_wayland_display_is_menu_only() {
        let info = decide_tier(
            &env(Some("wayland"), Some("wayland-9"), "Hyprland"),
            globals(false, false, false),
        );
        assert_eq!(info.tier, WaylandTier::MenuOnly);
        assert!(info.session_note.contains("unreachable"));
    }

    #[test]
    fn uri_list_parses_crlf_comments_and_blanks() {
        let payload = b"# a comment line\r\n\r\nfile:///home/u/a%20b.png\r\nfile:///tmp/x.jpg\n";
        let paths = parse_uri_list(payload);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/u/a b.png"),
                PathBuf::from("/tmp/x.jpg")
            ]
        );
    }

    #[test]
    fn uri_list_decodes_percent_escapes_as_bytes() {
        // %D8%B3%D9%84%D8%A7%D9%85 is the UTF-8 for سلام — the decoded
        // path must be the real bytes, not mojibake.
        let payload = b"file:///tmp/%D8%B3%D9%84%D8%A7%D9%85.txt\r\n";
        let paths = parse_uri_list(payload);
        assert_eq!(paths, vec![PathBuf::from("/tmp/سلام.txt")]);
        // '+' is literal in a URI, not a space.
        let payload = b"file:///tmp/a+b.png\n";
        assert_eq!(parse_uri_list(payload), vec![PathBuf::from("/tmp/a+b.png")]);
        // A truncated escape stays literal.
        let payload = b"file:///tmp/100%.png\n";
        assert_eq!(
            parse_uri_list(payload),
            vec![PathBuf::from("/tmp/100%.png")]
        );
    }

    #[test]
    fn uri_list_skips_non_local_and_non_file() {
        let payload = b"file://localhost/etc/hostname\r\n\
                        file://otherhost/share/x.png\r\n\
                        https://example.com/x.png\r\n\
                        file:///ok.png\r\n";
        let paths = parse_uri_list(payload);
        assert_eq!(
            paths,
            vec![PathBuf::from("/etc/hostname"), PathBuf::from("/ok.png")]
        );
        assert!(parse_uri_list(b"").is_empty());
        assert!(parse_uri_list(b"\r\n# only a comment\r\n").is_empty());
    }

    #[test]
    fn orb_geometry_constants_are_sane() {
        assert_eq!(ORB_SIZE, 96);
        assert_eq!(ORB_MARGIN, 24);
        // The Shamash star at orb scale must sit inside the surface.
        let c = ORB_SIZE as f32 / 2.0;
        let pts = crate::geom::star_points(c, c, c * 0.55, c * 0.23);
        assert_eq!(pts.len(), 16);
        for (x, y) in pts {
            assert!(x >= 0.0 && x <= ORB_SIZE as f32, "star x {x} outside orb");
            assert!(y >= 0.0 && y <= ORB_SIZE as f32, "star y {y} outside orb");
        }
        // The rendered orb is opaque-sized and non-empty at rest and
        // brightened on hover (pixels must actually differ).
        let rest = render_orb(ORB_SIZE, ORB_SIZE, false).expect("orb pixmap");
        let hover = render_orb(ORB_SIZE, ORB_SIZE, true).expect("orb pixmap");
        assert_eq!(rest.data().len(), (ORB_SIZE * ORB_SIZE * 4) as usize);
        assert_ne!(rest.data(), hover.data());
    }

    #[test]
    fn tier_explanation_covers_the_matrix_in_both_languages() {
        let orb = decide_tier(
            &env(Some("wayland"), Some("wayland-1"), "Sway"),
            globals(true, true, true),
        );
        let en = tier_explanation(&orb, SensorLang::En);
        assert!(en.contains("orb"));
        assert!(en.contains("XWayland"));
        let ar = tier_explanation(&orb, SensorLang::Ar);
        assert!(ar.contains("XWayland"));
        assert_ne!(en, ar);

        let gnome = decide_tier(
            &env(Some("wayland"), Some("wayland-1"), "GNOME"),
            globals(true, false, false),
        );
        let en = tier_explanation(&gnome, SensorLang::En);
        assert!(en.contains("drop window"));
        assert!(en.contains("Mutter (GNOME)"));
    }
}

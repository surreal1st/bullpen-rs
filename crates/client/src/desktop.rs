//! Desktop shell (S13a-02): launches the same [`crate::app::App`] the web
//! build renders, through `dioxus-desktop` (Dioxus 0.7's `desktop` feature,
//! already declared in `Cargo.toml`) instead of `dioxus-web`. Only in this
//! crate when the `desktop` feature is enabled (`main.rs` picks this or the
//! plain `dioxus::launch(App)` web path, never both - see that file).
//!
//! This module owns window chrome only (title, default size). The server
//! URL is NOT this module's concern: `transport::native` (S13a-01) already
//! reads `BULLPEN_URL` - default `http://100.119.100.103:4380` - for every
//! API call the desktop build makes, the same whether the window itself was
//! ever involved. There is nothing left for this file to read from that
//! variable.
//!
//! S13b-01 adds one more thing before the window opens: a silent sign-in
//! attempt, so Josh does not see the password gate on every launch the way
//! a plain native transport (no cookie persistence across processes) would
//! otherwise force - see `transport::native`'s top doc for the gap this
//! closes and why re-authenticating fresh each launch (rather than
//! persisting a session to disk) is the chosen fix.
//!
//! **S13b-02 adds window-geometry persistence and a tray icon.** The pure
//! restore-guard/(de)serialization logic lives in [`crate::window_state`]
//! (deliberately free of `dioxus`/`tao` so it can be unit tested without a
//! running window); everything below is the live wiring that module's own
//! doc said would live here: restore-on-launch, in-memory geometry capture
//! on `Moved`/`Resized`, a save on `CloseRequested`, hide-to-tray on close,
//! and the tray icon itself (Show/Hide, Quit).

use dioxus::desktop::muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use dioxus::desktop::tao::dpi::{PhysicalPosition, PhysicalSize};
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::Window;
use dioxus::desktop::trayicon::{DioxusTrayIcon, TrayIcon, TrayIconBuilder};
use dioxus::desktop::{Config, LogicalSize, WindowBuilder, WindowCloseBehaviour, WindowEvent};
use dioxus::prelude::Element;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::window_state::{self, Rect, WindowState};

/// A sensible default: wide enough for the rail + a chat pane side by side
/// without either feeling cramped, short enough to fit a 1080p display with
/// room for the taskbar. **Logical** pixels - this is the one place in the
/// desktop build that still is; see [`restore_or_center`]'s doc for how the
/// physical-pixel half of this ticket (`window_state`) avoids ever
/// converting between the two.
const DEFAULT_WIDTH: f64 = 1200.0;
const DEFAULT_HEIGHT: f64 = 800.0;

/// How long a silent sign-in attempt gets before launch proceeds without it.
/// Generous for a LAN/tailnet round trip, short enough that an unreachable
/// server never meaningfully delays the window opening - see
/// `attempt_silent_sign_in_before_launch`'s doc.
const SILENT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5);

/// The id of the tray menu's "Show/Hide" item - the only custom item on the
/// tray menu, so [`install_tray_menu_handler`]'s callback needs to
/// distinguish nothing else. "Quit" is deliberately a
/// [`PredefinedMenuItem::quit`] instead of a second custom item - see that
/// function's doc for why that also sidesteps this same handler entirely.
const TRAY_SHOW_HIDE_ID: &str = "bullpen-tray-show-hide";

/// The live OS window, populated once inside [`restore_or_center`]'s caller
/// (`launch`'s `with_on_window` closure) right after the window is created.
/// [`install_tray_menu_handler`]'s callback is registered *before* that
/// window exists (deliberately - see that function's doc), so it cannot
/// close over `window` the way `with_on_window`/`with_custom_event_handler`
/// do; it reads this static instead. `Arc<Window>` is `Send + Sync` on every
/// desktop platform tao supports (checked `tao 0.34.8`'s own
/// `platform_impl::windows::window::WindowWrapper`: `unsafe impl Send`,
/// `unsafe impl Sync`), which is what makes storing it in a `static` sound.
static WINDOW: OnceLock<Arc<Window>> = OnceLock::new();

/// Native window title (attention badge). Web uses `document.title` instead.
pub fn set_window_title(title: &str) {
    if let Some(window) = WINDOW.get() {
        window.set_title(title);
    }
}

/// Launch `app` in a desktop window titled "Bullpen". Mirrors
/// `dioxus::launch(app)` (the web entry point `main.rs` uses when the
/// `desktop` feature is off) but through `LaunchBuilder::desktop()` so the
/// window itself can be configured first.
pub fn launch(app: fn() -> Element) {
    attempt_silent_sign_in_before_launch();
    install_tray_menu_handler();

    // Deliberately NOT `.with_visible(false)` here, even though
    // `with_on_window` (below) is where this window gets positioned, before
    // anything is shown - `dioxus-desktop` already does exactly that
    // itself, and fighting it was a real bug caught by this ticket's own
    // proof run, not a hypothetical: `App::handle_start_cause_init`
    // (`dioxus-desktop 0.7.10`'s `app.rs:257-258`) reads *this* builder's
    // visibility into `is_visible_before_start` and then *unconditionally*
    // forces the real window invisible for the whole positioning dance
    // (`app.rs:260`, non-Linux); it only becomes visible again once the
    // page has actually finished its first load, in `handle_initialize_msg`
    // (`app.rs:302-303`), which sets it back to whatever
    // `is_visible_before_start` was. Building with `.with_visible(false)`
    // and then calling `window.set_visible(true)` from `with_on_window`
    // (tried first) compiled fine and ran with no error, but the window
    // stayed invisible forever: `is_visible_before_start` had already
    // latched `false`, so `handle_initialize_msg` silently set it back to
    // hidden *after* this module's own call ran, since that call runs
    // before the page has loaded, not after. Leaving this builder's default
    // (`true`) rides dioxus's existing hide-until-ready mechanism instead
    // of reimplementing it.
    let window = WindowBuilder::new()
        .with_title("Bullpen")
        .with_inner_size(LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT));

    // Shared between `with_on_window` (seeds it with the restored/centred
    // geometry) and `with_custom_event_handler` (updates it on every
    // `Moved`/`Resized`, writes it out on `CloseRequested`) - both closures
    // run on the same (event-loop) thread, so a plain `Rc<RefCell<_>>` is
    // enough; see `window_state`'s own doc for why writing on every
    // `Resized` instead (rather than accumulating in memory) is exactly the
    // "400 writes during one drag" this ticket's rule 2 rules out.
    let geometry: Rc<RefCell<Option<WindowState>>> = Rc::new(RefCell::new(None));
    let geometry_for_window = Rc::clone(&geometry);
    let geometry_for_events = Rc::clone(&geometry);

    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new()
                .with_window(window)
                // Ticket decision 3: closing the window hides it to the
                // tray rather than quitting. `Config`'s own doc + S13b-02's
                // research confirm `CloseRequested` still reaches
                // `with_custom_event_handler` regardless of this setting,
                // which is what lets the handler below persist geometry on
                // hide too (rule 2's own 🔴).
                .with_close_behaviour(WindowCloseBehaviour::WindowHides)
                .with_on_window(move |window, _vdom| {
                    let _ = WINDOW.set(Arc::clone(&window));
                    let state = restore_or_center(&window);
                    *geometry_for_window.borrow_mut() = Some(state);
                    // No `window.set_visible(true)` here - see the
                    // `WindowBuilder` above's own comment: the window is
                    // still invisible at this point (dioxus's own doing,
                    // not this module's), and `dioxus-desktop` reveals it
                    // itself once the page has actually loaded.
                    //
                    // Per `tray_icon`'s own doc, a tray icon can only be
                    // built once the event loop is actually running -
                    // `with_on_window` fires from inside window creation,
                    // which only happens after that, so this is the
                    // earliest safe point (confirmed against
                    // `dioxus-desktop 0.7.10`'s own `webview.rs`, which
                    // calls `on_window` immediately after `window.build()`
                    // succeeds).
                    build_tray_icon();
                })
                .with_custom_event_handler(move |event, _target| {
                    let TaoEvent::WindowEvent { event, .. } = event else {
                        return;
                    };
                    match event {
                        WindowEvent::Moved(position) => {
                            if let Some(state) = geometry_for_events.borrow_mut().as_mut() {
                                state.x = position.x as f64;
                                state.y = position.y as f64;
                            }
                        }
                        WindowEvent::Resized(size) => {
                            if let Some(state) = geometry_for_events.borrow_mut().as_mut() {
                                state.width = size.width as f64;
                                state.height = size.height as f64;
                            }
                        }
                        WindowEvent::CloseRequested => {
                            let geometry = geometry_for_events.borrow();
                            if let (Some(state), Some(path)) =
                                (geometry.as_ref(), window_state::window_state_file_path())
                            {
                                window_state::save(&path, state);
                            }
                        }
                        _ => {}
                    }
                }),
        )
        .launch(app);
}

/// S13b-02's restore-on-launch, called once from `launch`'s `with_on_window`
/// closure - after the OS window (and therefore its real monitor geometry)
/// exists. Returns the geometry actually applied, which seeds the in-memory
/// value `with_custom_event_handler` keeps current afterward.
///
/// **The ticket's named trap, and how this avoids it.** `window_state`'s
/// `WindowState`/`usable()` work in physical pixels; this file's
/// `DEFAULT_WIDTH`/`DEFAULT_HEIGHT` are logical (`LogicalSize::new`, fed to
/// the `WindowBuilder` before any monitor - and so any DPI scale factor - is
/// known). This function never converts between the two: by the time it
/// runs, tao/the OS has already resolved that logical size into the
/// window's real physical size for whatever monitor it was created on, so
/// `window.outer_size()`/`inner_size()` (both physical, both already
/// DPI-correct) stand in for "the default size in physical pixels" directly,
/// with no `scale_factor()` multiplication anywhere in this module - exactly
/// the kind of easy-to-get-backwards arithmetic the trap warns about.
fn restore_or_center(window: &Window) -> WindowState {
    let monitors: Vec<Rect> = window
        .available_monitors()
        .map(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            Rect {
                x: position.x as f64,
                y: position.y as f64,
                width: size.width as f64,
                height: size.height as f64,
            }
        })
        .collect();

    let restored = window_state::window_state_file_path()
        .and_then(|path| window_state::load(&path))
        .and_then(|state| window_state::usable(&state, &monitors));

    if let Some(state) = restored {
        window.set_outer_position(PhysicalPosition::new(state.x as i32, state.y as i32));
        window.set_inner_size(PhysicalSize::new(state.width as u32, state.height as u32));
        return state;
    }

    // No usable saved geometry (first launch, or the saved rect no longer
    // fits any connected monitor) - centre the window, at the physical size
    // it was already built with, on the primary monitor. `primary_monitor`
    // is `None` on some platforms/configurations (tao's own doc flags
    // Wayland); fall back to whatever monitor is reported first rather than
    // skip centring entirely.
    let target_monitor = window
        .primary_monitor()
        .or_else(|| window.available_monitors().next());
    let outer_size = window.outer_size();
    if let Some(monitor) = target_monitor {
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let x = monitor_position.x + (monitor_size.width as i32 - outer_size.width as i32) / 2;
        let y = monitor_position.y + (monitor_size.height as i32 - outer_size.height as i32) / 2;
        window.set_outer_position(PhysicalPosition::new(x, y));
    }

    let position = window
        .outer_position()
        .unwrap_or(PhysicalPosition::new(0, 0));
    let inner_size = window.inner_size();
    WindowState {
        width: inner_size.width as f64,
        height: inner_size.height as f64,
        x: position.x as f64,
        y: position.y as f64,
    }
}

/// S13b-02: registers the callback that answers a tray "Show/Hide" click.
/// Called from `launch`, before `LaunchBuilder::desktop()...launch()` runs -
/// that ordering is load-bearing, not incidental. Two things this project's
/// research (previous session, this ticket's own `## Results`) had left
/// unresolved, both settled by reading `dioxus-desktop 0.7.10` and
/// `muda 0.17.2`'s own source rather than guessing:
///
/// 1. **The dead end was real, and there is no dioxus-provided way around
///    it.** `UserWindowEvent::TrayMenuEvent` (what `with_custom_event_handler`
///    would need to match to read a tray click) lives in `dioxus_desktop`'s
///    private `ipc` module (`mod ipc;`, never `pub`, and `UserWindowEvent`
///    is never re-exported anywhere in `lib.rs`) - confirmed by reading
///    `lib.rs`'s full re-export list, not by trusting the previous
///    session's grep.
/// 2. **The "safer alternative" the previous session hadn't finished
///    evaluating - calling `muda::MenuEvent::set_event_handler` directly -
///    only works if it runs *before* `dioxus_desktop::App::new`.** Reading
///    `muda 0.17.2`'s own `MenuEvent::set_event_handler` shows the handler
///    is stored in a `once_cell::sync::OnceCell<Option<Handler>>`, and every
///    call site does `let _ = MENU_EVENT_HANDLER.set(...)` - the *first*
///    call wins; every later one is silently discarded, no error, no panic.
///    `dioxus_desktop::App::new` (which runs inside `.launch()`, well after
///    this module's own code has a chance to run if called naively from
///    `with_on_window`) calls `set_event_handler` itself, twice
///    (`set_menubar_receiver` for the default menu bar, then
///    `set_tray_icon_receiver` for the tray - both target the *same* global
///    slot, since `tray_icon::menu::MenuEvent` is `pub use muda::*;`, i.e.
///    literally `muda::MenuEvent`). Registering from inside `with_on_window`
///    (which fires *after* `App::new`) would have silently lost every
///    click - not a compile error, not a panic, just a handler that is
///    never called. Registering here, from `launch` before `.launch()` is
///    invoked at all, wins that race instead.
///
/// **What losing dioxus's own handler costs: nothing observable.**
/// `dioxus_desktop::App::handle_tray_menu_event` (what would have received
/// events under dioxus's own registration) is already a no-op in this
/// version (`_ = event;`, read directly from `app.rs`). And every item in
/// the default "Window" menu bar tao attaches automatically
/// (Minimize/Maximize/Hide/Close/Quit) is a `PredefinedMenuItem`, which
/// muda's own Windows implementation self-executes via a direct Win32 call
/// (`ShowWindow`/`PostQuitMessage`/etc.) *without* ever sending a
/// `MenuEvent` at all (`dispatch = false` for `MenuItemType::Predefined`,
/// read directly from `muda`'s `platform_impl::windows` source) - so none of
/// those items depended on this handler either way. Only a genuinely custom
/// item does, and the only one that exists in this app is the tray's own
/// "Show/Hide" - which is exactly what this function answers.
fn install_tray_menu_handler() {
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        if event.id().0 != TRAY_SHOW_HIDE_ID {
            return;
        }
        let Some(window) = WINDOW.get() else {
            return;
        };
        if window.is_visible() {
            window.set_visible(false);
        } else {
            window.set_visible(true);
            window.set_focus();
        }
    }));
}

/// S13b-02: builds the tray icon itself (Show/Hide, a separator, Quit).
/// Called once, from `launch`'s `with_on_window` closure - see that call
/// site's own comment for why that is the earliest safe point.
///
/// Deliberately bypasses `dioxus::desktop::trayicon::init_tray_icon`
/// (the dioxus-provided way to build one) rather than using it: that
/// function's last step is `provide_context(tray)`, which stores the
/// `TrayIcon` in the *current dioxus scope's* context map for later
/// `use_tray_icon()` hook consumers - meaningless here (nothing in this
/// app calls that hook) and, per the previous session's own unresolved
/// note, would need a `VirtualDom::in_scope(ScopeId::ROOT, ..)` wrapper
/// whose safety this ticket never had to establish, because this function
/// does not need dioxus's scope machinery at all: it only needs the
/// `TrayIcon` to stay alive (a plain `Box::leak` below - `TrayIcon` itself
/// is not `Send`/`Sync` (`tray-icon 0.21.3`'s own `TrayIcon` wraps an
/// `Rc<RefCell<_>>` internally), so it cannot live in a `static OnceLock`
/// the way [`WINDOW`] does; leaking a `Box` needs neither bound, and this
/// value is meant to live exactly as long as the process anyway) and its
/// clicks answered (handled by [`install_tray_menu_handler`], a plain
/// global callback, nothing scope-shaped about it). Building the
/// `tray_icon::TrayIconBuilder` directly - the same builder
/// `init_tray_icon` itself calls under the hood - sidesteps the open
/// question entirely instead of resolving it.
fn build_tray_icon() {
    let menu = Menu::new();
    let show_hide = MenuItem::with_id(TRAY_SHOW_HIDE_ID, "Show/Hide", true, None);
    if let Err(e) = menu.append_items(&[
        &show_hide,
        &PredefinedMenuItem::separator(),
        // A `PredefinedMenuItem`, not a second custom id: muda's own
        // Windows implementation runs `PostQuitMessage(0)` directly from
        // its WM_COMMAND handler for this item, with no `MenuEvent`
        // dispatch involved at all - "Quit" works independently of
        // `install_tray_menu_handler` and of the OnceCell race that
        // function's doc describes, which is exactly what makes it safe to
        // call the *only* exit: nothing else in this app can trigger it.
        &PredefinedMenuItem::quit(Some("Quit")),
    ]) {
        eprintln!("bullpen: could not build the tray menu ({e}); no tray icon this run");
        return;
    }

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Bullpen");
    match dioxus::desktop::default_icon::<DioxusTrayIcon>() {
        Ok(icon) => builder = builder.with_icon(icon),
        Err(e) => {
            eprintln!("bullpen: no tray icon image available ({e}); using the platform default")
        }
    }

    match builder.build() {
        Ok(tray) => {
            // Leaked on purpose - see this function's own doc for why a
            // `static` cannot hold a `TrayIcon` directly, and why "leak for
            // the rest of the process's life" is the correct lifetime here
            // regardless (there is exactly one tray icon, for exactly one
            // window, for the life of the app).
            let _: &'static TrayIcon = Box::leak(Box::new(tray));
        }
        Err(e) => {
            eprintln!("bullpen: could not create the tray icon ({e}); continuing without one");
        }
    }
}

/// S13b-01: mirrors the Electron app's own `signIn()`
/// (`projects/bullpen/desktop/main.cjs`) - reads
/// `%USERPROFILE%\.bullpen\password.key` and signs in before the window
/// shows anything, so Josh never types a password into this machine's
/// desktop app. Failure here is deliberately quiet, same as `main.cjs`'s
/// own doc comment on itself: a missing key file, a rejected password, a
/// timeout, or any other error all fall through to the sign-in gate that
/// already works (`app.rs::App`'s own `auth_status` check, run
/// unconditionally once the window is up) - this function's only job is to
/// skip that gate when it safely can, never to report a problem to Josh.
///
/// Runs in a throwaway, single-purpose Tokio runtime because this is called
/// from plain synchronous `main()` before handing off to `dioxus-desktop`
/// (which brings its own long-lived runtime only once `.launch()` below is
/// called) - there is no async context yet to `.await` inside. Bounded by
/// [`SILENT_SIGN_IN_TIMEOUT`] via `tokio::time::timeout` so an unreachable
/// server can never turn into a hang: whatever the outcome, this function
/// always returns and `launch` always proceeds to open the window. See
/// `transport::native`'s top doc for why the sign-in attempt itself uses
/// its own one-off HTTP client rather than the shared one - the same
/// property that makes this throwaway runtime safe to build and drop here
/// without corrupting anything the real app uses later.
///
/// Never logs the key's contents - only the file path (via the error
/// strings `transport::native::silent_sign_in` already builds, which name
/// the path and the failure kind, never what was read from it) and the
/// outcome tag.
fn attempt_silent_sign_in_before_launch() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!(
                "bullpen: could not start the startup sign-in runtime ({e}); showing the sign-in gate"
            );
            return;
        }
    };
    let outcome = runtime.block_on(async {
        tokio::time::timeout(SILENT_SIGN_IN_TIMEOUT, crate::transport::silent_sign_in()).await
    });
    match outcome {
        Ok(Ok(crate::transport::SilentSignInOutcome::SignedIn)) => {
            eprintln!("bullpen: signed in silently");
        }
        Ok(Ok(crate::transport::SilentSignInOutcome::NoKeyFile)) => {
            eprintln!("bullpen: no stored credential; the sign-in gate will ask");
        }
        Ok(Err(reason)) => {
            eprintln!("bullpen: silent sign-in refused ({reason}); the sign-in gate will ask");
        }
        Err(_) => {
            eprintln!("bullpen: silent sign-in timed out; the sign-in gate will ask");
        }
    }
}

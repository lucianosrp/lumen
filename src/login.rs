//! The sign-in screen, shown instead of everything else while no API key is saved.
//!
//! It asks for one thing, the key, and signs in as soon as a complete-looking key is pasted
//! or typed: no button to press. The server's addresses are remembered across sign-outs and
//! tucked behind "Change"; they open by themselves when none is known yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::account::{self, Config};
use crate::immich::Immich;
use crate::ui::{Align, Quad, rgb, theme};
use crate::widgets::{self, Ctx, Edit, Input, TextField, colors};
use crate::{Core, Switch};

/// Immich keys are 43 characters; anything this long without spaces is worth trying at once.
const LOOKS_COMPLETE: usize = 30;
/// Typing pauses this long before a complete-looking key is checked.
const DEBOUNCE: Duration = Duration::from_millis(250);
const WIDTH: f32 = 380.0;
const FIELD: f32 = 46.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Key,
    Local,
    Remote,
}

enum State {
    Idle,
    Checking,
    Failed(String),
}

pub struct Login {
    key: TextField,
    local: TextField,
    remote: TextField,
    focus: Field,
    /// The address fields are open.
    server: bool,
    state: State,
    /// Bumped per check; answers to older ones are dropped.
    attempt: u64,
    check_at: Option<Instant>,
    tx: Sender<(u64, Result<(Immich, account::Me), String>)>,
    rx: Receiver<(u64, Result<(Immich, account::Me), String>)>,
    rt: tokio::runtime::Handle,
    wake: Arc<dyn Fn() + Send + Sync>,
    input: Input,
    anims: HashMap<String, f32>,
    last: Instant,
    started: Instant,
    /// Field rectangles from the last frame, for clicks.
    rects: Vec<(Field, [f32; 4])>,
}

impl Login {
    pub fn new(rt: tokio::runtime::Handle, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let saved = Config::load();
        let (tx, rx) = channel();
        let field = |t: &str| {
            let mut f = TextField::default();
            f.set(t);
            f
        };
        let mut key = TextField::default();
        key.mask = true;
        Self {
            key,
            local: field(&saved.url),
            remote: field(&saved.fallback_url),
            focus: if saved.url.is_empty() { Field::Local } else { Field::Key },
            server: saved.url.is_empty(),
            state: State::Idle,
            attempt: 0,
            check_at: None,
            tx,
            rx,
            rt,
            wake,
            input: Input::default(),
            anims: HashMap::new(),
            last: Instant::now(),
            started: Instant::now(),
            rects: Vec::new(),
        }
    }

    fn config(&self) -> Config {
        // A remote address typed here is meant to be used.
        Config { url: self.local.text.trim().into(), fallback_url: self.remote.text.trim().into(), use_fallback: true, api_key: self.key.text.trim().into() }
    }

    fn field(&mut self, f: Field) -> &mut TextField {
        match f {
            Field::Key => &mut self.key,
            Field::Local => &mut self.local,
            Field::Remote => &mut self.remote,
        }
    }

    fn looks_complete(&self) -> bool {
        let k = self.key.text.trim();
        k.len() >= LOOKS_COMPLETE && !k.contains(' ')
    }

    /// Checks the key now (Enter) or, when `soon`, after a short pause in typing.
    fn check(&mut self, soon: bool) {
        self.attempt += 1;
        if self.key.text.trim().is_empty() {
            self.state = State::Idle;
            return;
        }
        if soon {
            self.check_at = Some(Instant::now() + DEBOUNCE);
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(DEBOUNCE + Duration::from_millis(10));
                wake();
            });
            return;
        }
        self.check_at = None;
        if self.local.text.trim().is_empty() {
            self.server = true;
            self.focus = Field::Local;
            self.state = State::Failed("Enter the server's address first".into());
            return;
        }
        self.state = State::Checking;
        let (config, tx, wake, attempt) = (self.config(), self.tx.clone(), self.wake.clone(), self.attempt);
        self.rt.spawn(async move {
            let _ = tx.send((attempt, account::verify(&config).await));
            wake();
        });
    }

    /// Headless: types a key as if pasted.
    pub fn paste(&mut self, key: &str) {
        self.key.set(key);
        self.check(false);
    }

    /// Headless: the outcome once a check has finished.
    pub fn failed(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn checking(&self) -> bool {
        matches!(self.state, State::Checking) || self.check_at.is_some()
    }

    /// Takes the window's input. Returns false for what the window should still see (resizes,
    /// pointer moves, redraws, Ctrl+Q).
    pub fn event(&mut self, event: &WindowEvent, cursor: (f32, f32), mods: ModifiersState) -> bool {
        self.input.cursor = cursor;
        match event {
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => self.input.press(cursor),
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => self.input.release(cursor),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if mods.control_key() && matches!(event.logical_key.as_ref(), Key::Character("q")) {
                    return false;
                }
                let fields: &[Field] = if self.server { &[Field::Key, Field::Local, Field::Remote] } else { &[Field::Key] };
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Enter) => self.check(false),
                    Key::Named(NamedKey::Tab) => {
                        let i = fields.iter().position(|&f| f == self.focus).unwrap_or(0);
                        let n = fields.len();
                        self.focus = fields[if mods.shift_key() { (i + n - 1) % n } else { (i + 1) % n }];
                    }
                    Key::Named(NamedKey::Escape) if self.server && !self.local.text.trim().is_empty() => {
                        (self.server, self.focus) = (false, Field::Key);
                    }
                    _ => {
                        let focus = self.focus;
                        if self.field(focus).key(event, mods) == Edit::Changed {
                            // Paste-and-go: a whole key checks itself; a new address rechecks a typed key.
                            if focus == Field::Key && self.looks_complete() {
                                self.check(true);
                            } else {
                                self.attempt += 1;
                                self.check_at = None;
                                self.state = State::Idle;
                            }
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { .. } => return false,
            _ => return false,
        }
        true
    }
}

/// Draws the screen and runs pending checks. Returns true while anything animates.
pub fn update(core: &mut Core, now: Instant) -> bool {
    let Some(login) = core.login.as_mut() else { return false };
    let dt = now.duration_since(login.last).as_secs_f32().min(0.05);
    login.last = now;
    if login.check_at.is_some_and(|t| now >= t) {
        login.check(false);
    }
    while let Ok((attempt, result)) = login.rx.try_recv() {
        if attempt != login.attempt {
            continue;
        }
        match result {
            Ok((immich, me)) => {
                let config = login.config();
                if let Err(e) = config.save() {
                    log::warn!("saving the account: {e:#}");
                }
                log::info!("signed in as {} via {}", me.name, immich.base());
                core.switch = Some(Switch::SignIn(immich));
                return true;
            }
            Err(e) => {
                // Unreachable: probably away from home without a remote address yet.
                if e.starts_with("Can't reach") && !login.server {
                    login.server = true;
                    if login.remote.text.trim().is_empty() {
                        login.focus = Field::Remote;
                    }
                }
                login.state = State::Failed(e);
            }
        }
    }

    core.instances.clear();
    core.ui.clear();
    let s = core.scale;
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    let pal = theme::palette();
    let mut ctx = Ctx {
        layer: &mut core.ui.base,
        uir: &mut core.renderer.ui,
        input: &login.input,
        anims: &mut login.anims,
        s,
        dt,
        clip: [0.0, 0.0, w, h],
        animating: false,
    };
    ctx.quad([0.0, 0.0, w, h], rgb(pal.bg, 1.0), 0.0);

    // Fades and rises in on first show.
    let t = (now.duration_since(login.started).as_secs_f32() / 0.45).min(1.0);
    let rise = (1.0 - crate::ease_out(t)) * 18.0 * s;
    if t < 1.0 {
        ctx.animating = true;
    }
    let cw = (WIDTH * s).min(w - 48.0 * s);
    let x = ((w - cw) / 2.0).round();
    let cx = w / 2.0;
    let block = if login.server { 470.0 } else { 360.0 } * s;
    let mut y = ((h - block) / 2.0 - 20.0 * s).max(24.0 * s).round() + rise;

    // The app mark: an accent tile with the aperture.
    let m = (68.0 * s).round();
    ctx.quad([cx - m / 2.0, y, m, m], rgb(pal.accent, 1.0), 17.0 * s);
    let fg = theme::text_rgba(pal.on_accent, 1.0);
    ctx.icon("aperture", cx, y + m / 2.0, 36.0, fg);
    y += m + 26.0 * s;
    ctx.text("Welcome to Lumen", cx, y, 26.0, 700, theme::text(), Align::Center);
    y += 30.0 * s;
    ctx.text("Paste your Immich API key to sign in", cx, y, 14.0, 400, theme::text_dim(), Align::Center);
    y += 34.0 * s;

    login.rects.clear();
    let focus = login.focus;
    let checking = matches!(login.state, State::Checking) || login.check_at.is_some();
    let failed = matches!(login.state, State::Failed(_));

    // The key field.
    let r = [x, y, cw, (FIELD * s).round()];
    field_box(&mut ctx, r, focus == Field::Key, failed && focus == Field::Key);
    ctx.icon("key-round", r[0] + 22.0 * s, r[1] + r[3] / 2.0, 17.0, theme::text_faint());
    let go = (30.0 * s).round();
    let go_r = [r[0] + r[2] - go - 8.0 * s, r[1] + (r[3] - go) / 2.0, go, go];
    let text_r = [r[0] + 42.0 * s, r[1], go_r[0] - r[0] - 50.0 * s, r[3]];
    login.key.draw(ctx.layer, ctx.uir, text_r, 15.0, s, "API key", focus == Field::Key);
    let mid = [go_r[0] + go / 2.0, go_r[1] + go / 2.0];
    if checking {
        widgets::spinner(ctx.layer, mid, 9.0 * s, now.duration_since(login.started).as_secs_f32(), pal.fg_dim);
        ctx.animating = true;
    } else if !login.key.is_empty() {
        let hover = ctx.input.hovered(go_r);
        let bg = if hover { colors::accent_hover() } else { pal.accent };
        ctx.quad(go_r, rgb(bg, 1.0), go / 2.0);
        ctx.icon("arrow-right", mid[0], mid[1], 16.0, fg);
        if ctx.input.clicked(go_r) {
            login.check_at = Some(now);
        }
    }
    login.rects.push((Field::Key, r));
    y += r[3] + 12.0 * s;

    // What's happening, or how to get a key.
    let (icon, msg, color) = match &login.state {
        State::Failed(e) => (Some("circle-alert"), e.clone(), theme::text_rgba(pal.red, 1.0)),
        _ if checking => (None, "Signing in…".to_string(), theme::text_dim()),
        _ => (None, "In Immich: Account Settings › API Keys › New API Key".to_string(), theme::text_faint()),
    };
    let fs = 12.5;
    let tw = ctx.measure(&msg, fs, 400) + if icon.is_some() { 20.0 * s } else { 0.0 };
    let mut tx = cx - tw / 2.0;
    if let Some(i) = icon {
        ctx.icon(i, tx + 7.0 * s, y + 9.0 * s, 14.0, color);
        tx += 20.0 * s;
    }
    ctx.text(msg, tx, y + 9.0 * s, fs, 400, color, Align::Left);
    y += 46.0 * s;

    // The server: a summary line, or the two address fields.
    if login.server {
        for (f, label, placeholder, icon) in [
            (Field::Local, "Local address", "192.168.1.10:2283", "house"),
            (Field::Remote, "Remote address · optional", "photos.example.com", "globe"),
        ] {
            ctx.text(label, x + 4.0 * s, y + 7.0 * s, 12.0, 600, theme::text_dim(), Align::Left);
            y += 18.0 * s;
            let r = [x, y, cw, (40.0 * s).round()];
            field_box(&mut ctx, r, focus == f, false);
            ctx.icon(icon, r[0] + 20.0 * s, r[1] + r[3] / 2.0, 15.0, theme::text_faint());
            let text_r = [r[0] + 38.0 * s, r[1], r[2] - 50.0 * s, r[3]];
            let tf = match f {
                Field::Local => &mut login.local,
                _ => &mut login.remote,
            };
            tf.draw(ctx.layer, ctx.uir, text_r, 14.0, s, placeholder, focus == f);
            login.rects.push((f, r));
            y += r[3] + 14.0 * s;
        }
        ctx.text("The local address is used whenever it answers,", cx, y + 4.0 * s, 12.0, 400, theme::text_faint(), Align::Center);
        ctx.text("the remote one everywhere else.", cx, y + 21.0 * s, 12.0, 400, theme::text_faint(), Align::Center);
    } else {
        let cfg = Config { url: login.local.text.trim().into(), fallback_url: login.remote.text.trim().into(), use_fallback: true, api_key: String::new() };
        let summary = cfg.urls().iter().map(|u| u.trim_start_matches("http://").trim_start_matches("https://")).collect::<Vec<_>>().join("  ·  ");
        let link = "Change";
        let lw = ctx.measure(link, 12.5, 600);
        let summary = ctx.ellipsize(&summary, 12.5, 400, cw - lw - 60.0 * s);
        let sw = ctx.measure(&summary, 12.5, 400);
        let total = 20.0 * s + sw + 12.0 * s + lw;
        let mut lx = cx - total / 2.0;
        let mid = y + 9.0 * s;
        ctx.icon("server", lx + 7.0 * s, mid, 14.0, theme::text_faint());
        lx += 20.0 * s;
        ctx.text(summary, lx, mid, 12.5, 400, theme::text_faint(), Align::Left);
        lx += sw + 12.0 * s;
        let lr = [lx - 4.0 * s, mid - 11.0 * s, lw + 8.0 * s, 22.0 * s];
        let hover = ctx.input.hovered(lr);
        let c = if hover { colors::accent_hover() } else { pal.accent };
        ctx.text(link, lx, mid, 12.5, 600, theme::text_rgba(c, 1.0), Align::Left);
        if ctx.input.clicked(lr) {
            (login.server, login.focus) = (true, Field::Local);
        }
    }

    // Clicking a field focuses it.
    for &(f, r) in &login.rects {
        if ctx.input.clicked(r) {
            login.focus = f;
        }
    }
    let animating = ctx.animating;
    login.input.end_frame();
    animating
}

/// A rounded input box with an accent ring when focused (red after a failed check).
fn field_box(ctx: &mut Ctx, r: [f32; 4], focused: bool, error: bool) {
    let s = ctx.s;
    let pal = theme::palette();
    let radius = 11.0 * s;
    let ring = (2.0 * s).round().max(1.0);
    if focused || error {
        let c = if error { pal.red } else { pal.accent };
        ctx.quad([r[0] - ring, r[1] - ring, r[2] + 2.0 * ring, r[3] + 2.0 * ring], rgb(c, 0.85), radius + ring);
    } else {
        let line = s.round().max(1.0);
        ctx.quad([r[0] - line, r[1] - line, r[2] + 2.0 * line, r[3] + 2.0 * line], rgb(pal.shade(pal.card(), 0.14), 1.0), radius + line);
    }
    ctx.layer.quad(Quad::solid(r, rgb(pal.card(), 1.0), radius));
}

/// Headless: `--login` shows the screen; `--login-paste <key|config>` pastes a key (`config`:
/// the saved one, never printed) and waits for the check; `--login-server` opens the address
/// fields; `--login-local` / `--login-remote` fill them.
pub struct Shot {
    pub paste: Option<String>,
    pub server: bool,
    pub local: Option<String>,
    pub remote: Option<String>,
}

impl Shot {
    pub fn parse(args: &[String]) -> Self {
        let flag = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
        Self {
            paste: flag("--login-paste"),
            server: args.iter().any(|a| a == "--login-server"),
            local: flag("--login-local"),
            remote: flag("--login-remote"),
        }
    }
}

pub fn drive(core: &mut Core, shot: &Shot) -> anyhow::Result<()> {
    let frames = |core: &mut Core, ms: u64| {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            std::thread::sleep(Duration::from_millis(16));
            core.pump();
            core.update(Instant::now());
        }
    };
    let login = core.login.as_mut().unwrap();
    if shot.server || shot.local.is_some() || shot.remote.is_some() {
        login.server = true;
    }
    if let Some(u) = &shot.local {
        login.local.set(u);
    }
    if let Some(u) = &shot.remote {
        login.remote.set(u);
    }
    if let Some(k) = &shot.paste {
        let key = if k == "config" { Config::saved().api_key } else { k.clone() };
        login.paste(&key);
        let start = Instant::now();
        loop {
            frames(core, 30);
            match &core.login {
                _ if core.switch.is_some() => {
                    println!("login: signed in");
                    return Ok(());
                }
                Some(l) if !l.checking() => {
                    println!("login: {}", l.failed().unwrap_or("idle"));
                    break;
                }
                _ if start.elapsed() > Duration::from_secs(30) => anyhow::bail!("timed out signing in"),
                _ => {}
            }
        }
    }
    frames(core, 600);
    Ok(())
}

//! Fenêtres natives (winit) : sortie programme (HDMI) et multiview, rendues par wgpu.
//!
//! La sortie programme se comporte comme celle d'une régie vidéo (QLab, Resolume…) : une
//! fenêtre sans bordure qui couvre exactement l'écran choisi, placée au-dessus de tout (niveau
//! économiseur d'écran), présente sur tous les bureaux, curseur masqué, veille bloquée. Elle
//! suit les branchements d'écran : si le projecteur est branché après le lancement ou revient
//! après une coupure, la fenêtre s'y replace toute seule.

use crate::config::Config;
use crate::engine::Engine;
use crate::layout;
use crate::render::{Renderer, SurfaceState};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Program,
    Multiview,
}

/// Emplacement d'une fenêtre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Fenêtre classique, centrée sur l'écran choisi.
    Windowed,
    /// Plein écran sans bordure géré par winit (écran principal, ou multiview).
    Fullscreen,
    /// Écran secondaire couvert en exclusivité : sans bordure, au-dessus de tout, sur tous les
    /// bureaux, curseur masqué (macOS uniquement).
    Exclusive,
}

/// Intervalle de surveillance des branchements d'écran.
const MONITOR_POLL: Duration = Duration::from_secs(1);

/// Nom lisible d'un écran. Sur macOS, winit ne donne qu'un numéro de modèle : on lit
/// `NSScreen.localizedName` (le nom affiché dans les Réglages Système, ex. « LG HDR 4K »).
pub fn monitor_name(m: &MonitorHandle) -> String {
    #[cfg(target_os = "macos")]
    if let Some(screen) = platform::ns_screen(m) {
        let name = screen.localizedName().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    m.name().unwrap_or_else(|| "?".into())
}

/// Résultat du choix d'écran : l'écran retenu, et si le sélecteur de la config a été honoré
/// (`false` = écran nommé absent, on est retombé sur un autre).
struct Pick {
    monitor: Option<MonitorHandle>,
    matched: bool,
}

struct WindowState {
    target: Target,
    window: Arc<Window>,
    surface: SurfaceState,
    monitor: Option<MonitorHandle>,
    placement: Placement,
    /// Souhait de l'utilisateur : couvrir l'écran (config, touche F / Échap).
    cover: bool,
}

pub struct App {
    cfg: Arc<Config>,
    engine: Arc<Engine>,
    renderer: Option<Renderer>,
    windows: Vec<WindowState>,
    cursor: PhysicalPosition<f64>,
    last_click: Option<(usize, Instant)>,
    stats_text: String,
    stats_at: Instant,
    /// Cadence du rendu (période de l'écran programme) et prochaine échéance.
    frame_period: Duration,
    next_frame: Instant,
    /// Instantané des écrans (nom + géométrie) et date du dernier relevé, pour détecter les
    /// branchements et débranchements.
    monitors: Vec<String>,
    monitors_at: Instant,
    /// Empêche la veille de l'écran et du Mac tant que la sortie tourne.
    #[cfg(target_os = "macos")]
    _sleep_guard: Option<platform::SleepGuard>,
}

impl App {
    pub fn new(cfg: Arc<Config>, engine: Arc<Engine>) -> Self {
        Self {
            cfg,
            engine,
            renderer: None,
            windows: Vec::new(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            last_click: None,
            stats_text: String::new(),
            stats_at: Instant::now() - Duration::from_secs(5),
            frame_period: Duration::from_micros(16_667),
            next_frame: Instant::now(),
            monitors: Vec::new(),
            monitors_at: Instant::now(),
            #[cfg(target_os = "macos")]
            _sleep_guard: None,
        }
    }

    pub fn build_event_loop() -> Result<EventLoop<()>> {
        Ok(EventLoop::new()?)
    }

    fn primary(event_loop: &ActiveEventLoop) -> Option<MonitorHandle> {
        event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
    }

    fn pick_monitor(event_loop: &ActiveEventLoop, selector: &str, prefer_secondary: bool) -> Pick {
        let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
        let primary = Self::primary(event_loop);
        let sel = selector.trim();
        if !sel.is_empty() {
            if let Ok(idx) = sel.parse::<usize>() {
                if let Some(m) = monitors.get(idx) {
                    return Pick {
                        monitor: Some(m.clone()),
                        matched: true,
                    };
                }
            }
            let low = sel.to_lowercase();
            if let Some(m) = monitors
                .iter()
                .find(|m| monitor_name(m).to_lowercase().contains(&low))
            {
                return Pick {
                    monitor: Some(m.clone()),
                    matched: true,
                };
            }
            warn!(
                "écran « {sel} » introuvable ; écrans : {:?}",
                monitors.iter().map(monitor_name).collect::<Vec<_>>()
            );
            return Pick {
                monitor: primary,
                matched: false,
            };
        }
        if prefer_secondary {
            if let Some(p) = &primary {
                if let Some(m) = monitors.iter().find(|m| *m != p) {
                    return Pick {
                        monitor: Some(m.clone()),
                        matched: true,
                    };
                }
            }
        }
        Pick {
            monitor: primary,
            matched: true,
        }
    }

    /// Nom + géométrie de chaque écran, pour détecter un changement de configuration.
    fn monitors_snapshot(event_loop: &ActiveEventLoop) -> Vec<String> {
        event_loop
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                format!(
                    "{}@{},{} {}x{}",
                    monitor_name(&m),
                    p.x,
                    p.y,
                    s.width,
                    s.height
                )
            })
            .collect()
    }

    fn window_size(&self, target: Target) -> PhysicalSize<u32> {
        match target {
            Target::Program => {
                PhysicalSize::new(self.cfg.video.width as u32, self.cfg.video.height as u32)
            }
            Target::Multiview => PhysicalSize::new(
                self.cfg.multiview.width as u32,
                self.cfg.multiview.height as u32,
            ),
        }
    }

    /// Choisit l'écran et l'emplacement d'une fenêtre d'après la config et l'état des écrans.
    fn decide(
        &self,
        event_loop: &ActiveEventLoop,
        target: Target,
        cover: bool,
    ) -> (Option<MonitorHandle>, Placement) {
        let (selector, prefer_secondary) = match target {
            Target::Program => (&self.cfg.output.display, true),
            Target::Multiview => (&self.cfg.multiview.display, false),
        };
        let pick = Self::pick_monitor(event_loop, selector, prefer_secondary);
        if !cover {
            return (pick.monitor, Placement::Windowed);
        }
        if target == Target::Multiview {
            return (pick.monitor, Placement::Fullscreen);
        }
        if !pick.matched {
            // L'écran demandé est absent : on ne couvre surtout pas l'écran de la régie ; la
            // sortie reste en fenêtre et se placera sur l'écran dès qu'il sera branché.
            warn!(
                "écran de sortie « {} » absent : programme en fenêtre en attendant",
                selector.trim()
            );
            return (None, Placement::Windowed);
        }
        let primary = Self::primary(event_loop);
        match pick.monitor {
            // Écran secondaire (projecteur) : couverture exclusive, au-dessus de tout.
            #[cfg(target_os = "macos")]
            Some(m) if Some(&m) != primary.as_ref() => (Some(m), Placement::Exclusive),
            // Écran principal : plein écran classique, pour ne pas enfermer l'opérateur.
            other => (other, Placement::Fullscreen),
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop, target: Target) -> Result<()> {
        let (title, cover) = match target {
            Target::Program => ("Streame — PROGRAMME", self.cfg.output.fullscreen),
            Target::Multiview => ("Streame — MULTIVIEW", false),
        };
        let size = self.window_size(target);
        let (monitor, placement) = self.decide(event_loop, target, cover);
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(size)
            .with_decorations(placement != Placement::Exclusive);
        if let Some(m) = &monitor {
            attrs = attrs.with_position(Self::centered(m, size));
            if placement == Placement::Fullscreen {
                attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(Some(m.clone()))));
            }
        }
        let window = Arc::new(event_loop.create_window(attrs)?);
        let canvas = (self.cfg.video.width as u32, self.cfg.video.height as u32);
        let surface = match &mut self.renderer {
            None => {
                let (r, s) = Renderer::new(window.clone(), canvas)?;
                self.renderer = Some(r);
                s
            }
            Some(r) => r.create_surface(window.clone(), false)?,
        };
        let initial = match placement {
            Placement::Fullscreen => Placement::Fullscreen,
            _ => Placement::Windowed,
        };
        self.windows.push(WindowState {
            target,
            window,
            surface,
            monitor: monitor.clone(),
            placement: initial,
            cover,
        });
        let idx = self.windows.len() - 1;
        self.apply(idx, monitor, placement, true);
        Ok(())
    }

    /// Position (logique) pour centrer une fenêtre de `size` sur l'écran `m`.
    fn centered(m: &MonitorHandle, size: PhysicalSize<u32>) -> LogicalPosition<f64> {
        let sf = m.scale_factor();
        let pos = m.position().to_logical::<f64>(sf);
        let ms = m.size().to_logical::<f64>(sf);
        let s = size.to_logical::<f64>(sf);
        LogicalPosition::new(
            pos.x + ((ms.width - s.width) / 2.0).max(0.0),
            pos.y + ((ms.height - s.height) / 2.0).max(0.0),
        )
    }

    /// Recalcule l'écran et l'emplacement d'une fenêtre (touche F, changement d'écrans).
    fn place(&mut self, event_loop: &ActiveEventLoop, idx: usize) {
        let (target, cover) = (self.windows[idx].target, self.windows[idx].cover);
        let (monitor, placement) = self.decide(event_loop, target, cover);
        self.apply(idx, monitor, placement, false);
    }

    /// Applique un emplacement à une fenêtre existante.
    fn apply(
        &mut self,
        idx: usize,
        monitor: Option<MonitorHandle>,
        placement: Placement,
        created: bool,
    ) {
        let target = self.windows[idx].target;
        let size = self.window_size(target);
        let ws = &mut self.windows[idx];
        let win = &ws.window;
        if !created && ws.placement == placement && ws.monitor == monitor {
            // Même écran, même mode : on réajuste seulement le cadre (changement de définition).
            #[cfg(target_os = "macos")]
            if placement == Placement::Exclusive {
                if let Some(m) = &monitor {
                    platform::cover_screen(win, m);
                }
            }
            return;
        }
        // Sortie de l'emplacement précédent.
        match ws.placement {
            Placement::Fullscreen => win.set_fullscreen(None),
            #[cfg(target_os = "macos")]
            Placement::Exclusive => {
                platform::leave_exclusive(win);
                win.set_decorations(true);
            }
            _ => {}
        }
        match placement {
            Placement::Windowed => {
                win.set_cursor_visible(true);
                if let Some(m) = &monitor {
                    let _ = win.request_inner_size(size);
                    win.set_outer_position(Self::centered(m, size));
                }
            }
            Placement::Fullscreen => {
                win.set_cursor_visible(false);
                if !created {
                    win.set_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
                }
            }
            #[cfg(target_os = "macos")]
            Placement::Exclusive => {
                win.set_decorations(false);
                win.set_cursor_visible(false);
                if let Some(m) = &monitor {
                    platform::enter_exclusive(win, m);
                }
            }
            #[cfg(not(target_os = "macos"))]
            Placement::Exclusive => unreachable!(),
        }
        ws.placement = placement;
        ws.monitor = monitor;
        let inner = win.inner_size();
        info!(
            "fenêtre {:?} {:?} sur « {} » ({}x{})",
            target,
            placement,
            ws.monitor
                .as_ref()
                .map(monitor_name)
                .unwrap_or_else(|| "?".into()),
            inner.width,
            inner.height
        );
        if target == Target::Program {
            if let Some(mhz) = ws
                .monitor
                .as_ref()
                .and_then(|m| m.refresh_rate_millihertz())
            {
                if mhz > 0 {
                    self.frame_period = Duration::from_secs_f64(1000.0 / mhz as f64);
                    info!("cadence de rendu : {:.1} Hz", mhz as f64 / 1000.0);
                }
            }
        }
    }

    /// Surveille les branchements d'écran et replace les fenêtres quand ils changent.
    fn watch_monitors(&mut self, event_loop: &ActiveEventLoop) {
        if self.monitors_at.elapsed() < MONITOR_POLL {
            return;
        }
        self.monitors_at = Instant::now();
        let snap = Self::monitors_snapshot(event_loop);
        if snap == self.monitors {
            return;
        }
        info!("écrans modifiés : {snap:?}");
        self.monitors = snap;
        for idx in 0..self.windows.len() {
            self.place(event_loop, idx);
        }
    }

    fn redraw(&mut self, idx: usize) {
        if self.stats_at.elapsed() >= Duration::from_secs(1) {
            let st = self.engine.stats();
            let mut parts = Vec::new();
            if st.phone_width > 0 {
                parts.push(format!(
                    "Tél. {}x{} {:.0} i/s",
                    st.phone_width, st.phone_height, st.phone_fps
                ));
            }
            if let Some(r) = &st.rtp {
                parts.push(format!("{} {:.1} Mb/s", r.codec, r.bitrate_kbps / 1000.0));
                parts.push(format!(
                    "perte {:.1}% gigue {:.0} ms NACK {} PLI {}",
                    r.loss_percent, r.jitter_ms, r.nack_count, r.pli_count
                ));
                if let Some(rtt) = r.rtt_ms {
                    parts.push(format!("RTT {rtt:.0} ms"));
                }
            }
            if let Some(p) = &st.phone {
                parts.push(format!(
                    "envoi {}x{} {:.0} i/s {:.1} Mb/s limite:{}",
                    p.width,
                    p.height,
                    p.fps,
                    p.bitrate_kbps / 1000.0,
                    p.quality_limitation
                ));
            }
            parts.push(format!("rendu {:.0} i/s", st.render_fps));
            self.stats_text = parts.join(" · ");
            self.stats_at = Instant::now();
        }
        // Horloge (UTC, ms) pour la mesure de latence verre à verre avec la page /latency.
        let overlay = if self.engine.config().multiview.clock {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let (s, m) = (ms / 1000 % 86400, ms % 1000);
            format!(
                "{} · {:02}:{:02}:{:02}.{:03} UTC",
                self.stats_text,
                s / 3600,
                s / 60 % 60,
                s % 60,
                m
            )
        } else {
            self.stats_text.clone()
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let ws = &self.windows[idx];
        let res = match ws.target {
            Target::Program => renderer.render_program(&ws.surface, &self.engine),
            Target::Multiview => renderer
                .render_multiview(&ws.surface, &self.engine, &overlay)
                .map(|_| true),
        };
        match res {
            Ok(true) if ws.target == Target::Program => self.engine.render_fps.tick(),
            Ok(_) => {}
            Err(e) => error!("rendu {:?} : {e:#}", ws.target),
        }
    }

    fn multiview_click(&mut self, idx: usize) {
        let ws = &self.windows[idx];
        let (w, h) = ws.surface.size();
        let lay = layout::compute(
            w as i32,
            h as i32,
            self.engine.scenes_ref().len(),
            self.cfg.multiview.columns,
        );
        let Some(tile) = lay.tile_at(self.cursor.x as i32, self.cursor.y as i32) else {
            return;
        };
        let now = Instant::now();
        let double = matches!(self.last_click, Some((t, at)) if t == tile && now.duration_since(at) < Duration::from_millis(400));
        self.last_click = Some((tile, now));
        if double {
            self.engine.transition_to(tile);
        } else {
            self.engine.set_preview(tile);
        }
    }

    fn key(&mut self, idx: usize, key: Key, event_loop: &ActiveEventLoop) {
        match key {
            Key::Character(c) => {
                if let Some(d) = c.chars().next().and_then(|ch| ch.to_digit(10)) {
                    if d >= 1 {
                        self.engine.transition_to(d as usize - 1);
                    }
                } else if c.eq_ignore_ascii_case("f") {
                    self.windows[idx].cover = !self.windows[idx].cover;
                    self.place(event_loop, idx);
                } else if c.eq_ignore_ascii_case("q") {
                    event_loop.exit();
                }
            }
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => self.engine.take(),
            Key::Named(NamedKey::Escape) => {
                self.windows[idx].cover = false;
                self.place(event_loop, idx);
            }
            _ => {}
        }
    }
}

/// Description d'un écran (pour `streame devices`).
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

struct MonitorLister(Vec<MonitorInfo>);

impl ApplicationHandler for MonitorLister {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.0 = event_loop
            .available_monitors()
            .map(|m| {
                let s = m.size();
                let p = m.position();
                MonitorInfo {
                    name: monitor_name(&m),
                    width: s.width,
                    height: s.height,
                    x: p.x,
                    y: p.y,
                }
            })
            .collect();
        event_loop.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

/// Liste les écrans (doit être appelé depuis le thread principal).
pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let event_loop = EventLoop::new()?;
    let mut lister = MonitorLister(Vec::new());
    event_loop.run_app(&mut lister)?;
    Ok(lister.0)
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if let Err(e) = self.create_window(event_loop, Target::Program) {
            error!("fenêtre programme : {e:#}");
        }
        if self.cfg.multiview.enabled {
            if let Err(e) = self.create_window(event_loop, Target::Multiview) {
                error!("fenêtre multiview : {e:#}");
            }
        }
        if self.windows.is_empty() {
            event_loop.exit();
            return;
        }
        self.monitors = Self::monitors_snapshot(event_loop);
        self.monitors_at = Instant::now();
        #[cfg(target_os = "macos")]
        {
            self._sleep_guard = Some(platform::SleepGuard::new("Sortie video Streame"));
        }
        self.next_frame = Instant::now();
    }

    /// Cadence le rendu : une image par période d'écran, pour toutes les fenêtres.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.watch_monitors(event_loop);
        let now = Instant::now();
        if now >= self.next_frame {
            for w in &self.windows {
                w.window.request_redraw();
            }
            self.next_frame = if now - self.next_frame > self.frame_period {
                now + self.frame_period
            } else {
                self.next_frame + self.frame_period
            };
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(idx) = self.windows.iter().position(|w| w.window.id() == id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                if self.windows[idx].target == Target::Program {
                    event_loop.exit();
                } else {
                    self.windows.remove(idx);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(&mut self.windows[idx].surface, size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(idx),
            WindowEvent::CursorMoved { position, .. } => self.cursor = position,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if self.windows[idx].target == Target::Multiview {
                    self.multiview_click(idx);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.key(idx, event.logical_key, event_loop);
            }
            _ => {}
        }
    }
}

/// Lance les fenêtres et la boucle d'événements (bloquant, thread principal).
pub fn run(cfg: Arc<Config>, engine: Arc<Engine>) -> Result<()> {
    let event_loop = App::build_event_loop().context("boucle d'événements (écran requis)")?;
    let mut app = App::new(cfg, engine);
    event_loop
        .run_app(&mut app)
        .context("boucle d'événements")?;
    Ok(())
}

/// Réglages AppKit de la fenêtre programme, hors de portée de winit : niveau de fenêtre,
/// comportement vis-à-vis des bureaux (Spaces), cadre exact de l'écran, veille.
#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSNormalWindowLevel, NSScreen, NSScreenSaverWindowLevel, NSView, NSWindow,
        NSWindowCollectionBehavior,
    };
    use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};
    use tracing::info;
    use wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    use winit::monitor::MonitorHandle;
    use winit::platform::macos::MonitorHandleExtMacOS;
    use winit::window::Window;

    /// `NSScreen` d'un écran winit.
    pub fn ns_screen(m: &MonitorHandle) -> Option<&NSScreen> {
        let ptr = m.ns_screen()?;
        // SAFETY : winit renvoie un pointeur NSScreen valide tant que le MonitorHandle vit ;
        // AppKit garde la liste des écrans vivante.
        Some(unsafe { &*(ptr as *const NSScreen) })
    }

    /// `NSWindow` d'une fenêtre winit.
    fn ns_window(win: &Window) -> Option<Retained<NSWindow>> {
        let handle = win.window_handle().ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return None;
        };
        // SAFETY : winit fournit un NSView valide tant que la fenêtre vit ; on est sur le
        // thread principal (boucle d'événements winit).
        let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
        view.window()
    }

    /// Plaque la fenêtre sur le cadre exact de l'écran.
    pub fn cover_screen(win: &Window, m: &MonitorHandle) {
        if let (Some(w), Some(s)) = (ns_window(win), ns_screen(m)) {
            w.setFrame_display(s.frame(), true);
        }
    }

    /// Couverture exclusive d'un écran : au-dessus de tout (y compris économiseur d'écran,
    /// Dock, notifications), sur tous les bureaux, ignorée par Mission Control et Cmd+`,
    /// pas de transition Spaces, pas d'ombre, reste visible quand l'app passe en arrière-plan.
    pub fn enter_exclusive(win: &Window, m: &MonitorHandle) {
        let Some(w) = ns_window(win) else {
            return;
        };
        w.setLevel(NSScreenSaverWindowLevel);
        w.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        w.setHidesOnDeactivate(false);
        w.setHasShadow(false);
        w.setMovable(false);
        if let Some(s) = ns_screen(m) {
            w.setFrame_display(s.frame(), true);
        }
        w.orderFrontRegardless();
    }

    /// Retour à une fenêtre ordinaire.
    pub fn leave_exclusive(win: &Window) {
        let Some(w) = ns_window(win) else {
            return;
        };
        w.setLevel(NSNormalWindowLevel);
        w.setCollectionBehavior(NSWindowCollectionBehavior::Default);
        w.setHasShadow(true);
        w.setMovable(true);
    }

    /// Bloque la veille de l'écran et du système tant que l'objet vit.
    pub struct SleepGuard(Retained<ProtocolObject<dyn NSObjectProtocol>>);

    impl SleepGuard {
        pub fn new(reason: &str) -> Self {
            let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiated | NSActivityOptions::IdleDisplaySleepDisabled,
                &NSString::from_str(reason),
            );
            info!("veille de l'écran et du Mac bloquée pendant la sortie");
            Self(token)
        }
    }

    impl Drop for SleepGuard {
        fn drop(&mut self) {
            // SAFETY : le jeton vient de beginActivityWithOptions:reason:.
            unsafe { NSProcessInfo::processInfo().endActivity(&self.0) };
        }
    }
}

//! Fenêtres natives (winit) : sortie programme (HDMI) et multiview, rendues par wgpu.

use crate::config::Config;
use crate::engine::Engine;
use crate::layout;
use crate::render::{Renderer, SurfaceState};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
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

/// Nom lisible d'un écran. Sur macOS, winit ne donne qu'un numéro de modèle : on lit
/// `NSScreen.localizedName` (le nom affiché dans les Réglages Système, ex. « LG HDR 4K »).
pub fn monitor_name(m: &MonitorHandle) -> String {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::MonitorHandleExtMacOS;
        if let Some(ptr) = m.ns_screen() {
            // SAFETY : winit renvoie un pointeur NSScreen valide tant que le MonitorHandle vit.
            let screen: &objc2_app_kit::NSScreen =
                unsafe { &*(ptr as *const objc2_app_kit::NSScreen) };
            let name = unsafe { screen.localizedName() }.to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    m.name().unwrap_or_else(|| "?".into())
}

struct WindowState {
    target: Target,
    window: Arc<Window>,
    surface: SurfaceState,
    monitor: Option<MonitorHandle>,
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
        }
    }

    pub fn build_event_loop() -> Result<EventLoop<()>> {
        Ok(EventLoop::new()?)
    }

    fn pick_monitor(
        event_loop: &ActiveEventLoop,
        selector: &str,
        prefer_secondary: bool,
    ) -> Option<MonitorHandle> {
        let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
        let primary = event_loop
            .primary_monitor()
            .or_else(|| monitors.first().cloned());
        let sel = selector.trim();
        if !sel.is_empty() {
            if let Ok(idx) = sel.parse::<usize>() {
                if let Some(m) = monitors.get(idx) {
                    return Some(m.clone());
                }
            }
            let low = sel.to_lowercase();
            if let Some(m) = monitors
                .iter()
                .find(|m| monitor_name(m).to_lowercase().contains(&low))
            {
                return Some(m.clone());
            }
            warn!(
                "écran « {sel} » introuvable ; écrans : {:?}",
                monitors.iter().map(monitor_name).collect::<Vec<_>>()
            );
        }
        if prefer_secondary {
            if let Some(p) = &primary {
                if let Some(m) = monitors
                    .iter()
                    .find(|m| m.name() != p.name() || m.position() != p.position())
                {
                    return Some(m.clone());
                }
            }
        }
        primary
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop, target: Target) -> Result<()> {
        let (title, size, selector, fullscreen, prefer_secondary) = match target {
            Target::Program => (
                "Streame — PROGRAMME",
                (self.cfg.video.width as u32, self.cfg.video.height as u32),
                self.cfg.output.display.clone(),
                self.cfg.output.fullscreen,
                true,
            ),
            Target::Multiview => (
                "Streame — MULTIVIEW",
                (
                    self.cfg.multiview.width as u32,
                    self.cfg.multiview.height as u32,
                ),
                self.cfg.multiview.display.clone(),
                false,
                false,
            ),
        };
        let monitor = Self::pick_monitor(event_loop, &selector, prefer_secondary);
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(size.0, size.1));
        if let Some(m) = &monitor {
            let pos = m.position();
            let msize = m.size();
            attrs = attrs.with_position(PhysicalPosition::new(
                pos.x + ((msize.width as i32 - size.0 as i32) / 2).max(0),
                pos.y + ((msize.height as i32 - size.1 as i32) / 2).max(0),
            ));
            if fullscreen {
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
        let inner = window.inner_size();
        info!(
            "fenêtre {title} sur « {} » ({}x{})",
            monitor
                .as_ref()
                .map(monitor_name)
                .unwrap_or_else(|| "?".into()),
            inner.width,
            inner.height
        );
        if target == Target::Program {
            if let Some(mhz) = monitor.as_ref().and_then(|m| m.refresh_rate_millihertz()) {
                if mhz > 0 {
                    self.frame_period = Duration::from_secs_f64(1000.0 / mhz as f64);
                    info!("cadence de rendu : {:.1} Hz", mhz as f64 / 1000.0);
                }
            }
        }
        self.windows.push(WindowState {
            target,
            window,
            surface,
            monitor,
        });
        Ok(())
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
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let ws = &self.windows[idx];
        let res = match ws.target {
            Target::Program => renderer.render_program(&ws.surface, &self.engine),
            Target::Multiview => renderer
                .render_multiview(&ws.surface, &self.engine, &self.stats_text)
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
                    let ws = &self.windows[idx];
                    if ws.window.fullscreen().is_some() {
                        ws.window.set_fullscreen(None);
                    } else {
                        ws.window
                            .set_fullscreen(Some(Fullscreen::Borderless(ws.monitor.clone())));
                    }
                } else if c.eq_ignore_ascii_case("q") {
                    event_loop.exit();
                }
            }
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => self.engine.take(),
            Key::Named(NamedKey::Escape) => self.windows[idx].window.set_fullscreen(None),
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
        self.next_frame = Instant::now();
    }

    /// Cadence le rendu : une image par période d'écran, pour toutes les fenêtres.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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

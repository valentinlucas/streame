//! Fenêtres natives (winit + softbuffer) : sortie programme (HDMI) et multiview.

use crate::config::Config;
use crate::engine::{Engine, Target};
use anyhow::Result;
use gst_video::prelude::*;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

#[derive(Debug)]
pub enum UserEvent {
    Frame(Target),
}

/// Dernière image reçue pour chaque cible ; le moteur y écrit, la fenêtre y lit.
pub struct FrameStore {
    program: Mutex<Option<gst::Sample>>,
    multiview: Mutex<Option<gst::Sample>>,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl FrameStore {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Arc<Self> {
        Arc::new(Self {
            program: Mutex::new(None),
            multiview: Mutex::new(None),
            proxy: Mutex::new(proxy),
        })
    }

    fn slot(&self, t: Target) -> &Mutex<Option<gst::Sample>> {
        match t {
            Target::Program => &self.program,
            Target::Multiview => &self.multiview,
        }
    }

    pub fn push(&self, t: Target, sample: gst::Sample) {
        *self.slot(t).lock().unwrap() = Some(sample);
        let _ = self.proxy.lock().unwrap().send_event(UserEvent::Frame(t));
    }

    fn take(&self, t: Target) -> Option<gst::Sample> {
        self.slot(t).lock().unwrap().clone()
    }
}

struct WindowState {
    target: Target,
    window: Arc<Window>,
    _context: softbuffer::Context<Arc<Window>>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    size: (u32, u32),
    monitor: Option<MonitorHandle>,
}

pub struct App {
    cfg: Arc<Config>,
    engine: Arc<Engine>,
    frames: Arc<FrameStore>,
    windows: Vec<WindowState>,
    cursor: PhysicalPosition<f64>,
    last_click: Option<(usize, Instant)>,
}

impl App {
    pub fn new(cfg: Arc<Config>, engine: Arc<Engine>, frames: Arc<FrameStore>) -> Self {
        Self {
            cfg,
            engine,
            frames,
            windows: Vec::new(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            last_click: None,
        }
    }

    pub fn build_event_loop() -> Result<EventLoop<UserEvent>> {
        Ok(EventLoop::<UserEvent>::with_user_event().build()?)
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
                .find(|m| m.name().unwrap_or_default().to_lowercase().contains(&low))
            {
                return Some(m.clone());
            }
            warn!(
                "écran « {sel} » introuvable ; écrans : {:?}",
                monitors
                    .iter()
                    .map(|m| m.name().unwrap_or_default())
                    .collect::<Vec<_>>()
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
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer : {e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer : {e}"))?;
        let inner = window.inner_size();
        info!(
            "fenêtre {title} sur « {} » ({}x{})",
            monitor
                .as_ref()
                .and_then(|m| m.name())
                .unwrap_or_else(|| "?".into()),
            inner.width,
            inner.height
        );
        let mut ws = WindowState {
            target,
            window,
            _context: context,
            surface,
            size: (0, 0),
            monitor,
        };
        self.resize(&mut ws, inner);
        self.windows.push(ws);
        Ok(())
    }

    fn resize(&self, ws: &mut WindowState, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 || ws.size == (size.width, size.height) {
            return;
        }
        ws.size = (size.width, size.height);
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            let _ = ws.surface.resize(w, h);
        }
        self.engine
            .set_output_size(ws.target, size.width, size.height);
    }

    fn redraw(&mut self, idx: usize) {
        let target = self.windows[idx].target;
        let Some(sample) = self.frames.take(target) else {
            return;
        };
        let Some(buffer) = sample.buffer() else {
            return;
        };
        let Some(caps) = sample.caps() else { return };
        let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
            return;
        };
        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info) else {
            return;
        };
        let fw = frame.width() as usize;
        let fh = frame.height() as usize;
        let stride = frame.plane_stride()[0] as usize;
        let Ok(data) = frame.plane_data(0) else {
            return;
        };

        let (program, preview) = (self.engine.program_index(), self.engine.preview_index());
        let layout = self.engine.layout().clone();
        let ws = &mut self.windows[idx];
        let (ww, wh) = (ws.size.0 as usize, ws.size.1 as usize);
        if ww == 0 || wh == 0 {
            return;
        }
        let Ok(mut out) = ws.surface.buffer_mut() else {
            return;
        };
        if out.len() != ww * wh {
            return;
        }
        if fw == ww && fh == wh {
            for y in 0..fh {
                let row = &data[y * stride..y * stride + fw * 4];
                let dst = &mut out[y * ww..(y + 1) * ww];
                for (d, px) in dst.iter_mut().zip(row.chunks_exact(4)) {
                    *d = u32::from_le_bytes([px[0], px[1], px[2], 0]);
                }
            }
        } else {
            // Taille transitoire (redimensionnement) : mise à l'échelle au plus proche voisin.
            for y in 0..wh {
                let sy = y * fh / wh;
                let row = &data[sy * stride..];
                let dst = &mut out[y * ww..(y + 1) * ww];
                for (x, d) in dst.iter_mut().enumerate() {
                    let sx = (x * fw / ww) * 4;
                    *d = u32::from_le_bytes([row[sx], row[sx + 1], row[sx + 2], 0]);
                }
            }
        }

        if target == Target::Multiview {
            let to = (ww as i32, wh as i32);
            let thickness = (ww / 320).max(2) as i32;
            for (i, r) in layout.tiles.iter().enumerate() {
                let color = if i == program {
                    Some(0x00E0_2020)
                } else if i == preview {
                    Some(0x0020_C040)
                } else {
                    None
                };
                if let Some(c) = color {
                    draw_rect(&mut out, ww, wh, r.scaled(layout.canvas, to), thickness, c);
                }
            }
            draw_rect(
                &mut out,
                ww,
                wh,
                layout.program.scaled(layout.canvas, to),
                thickness,
                0x00E0_2020,
            );
            draw_rect(
                &mut out,
                ww,
                wh,
                layout.preview.scaled(layout.canvas, to),
                thickness,
                0x0020_C040,
            );
        }
        let _ = out.present();
    }

    fn multiview_click(&mut self, idx: usize) {
        let ws = &self.windows[idx];
        let layout = self.engine.layout();
        let x = (self.cursor.x * layout.canvas.0 as f64 / ws.size.0.max(1) as f64) as i32;
        let y = (self.cursor.y * layout.canvas.1 as f64 / ws.size.1.max(1) as f64) as i32;
        let Some(tile) = layout.tile_at(x, y) else {
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

fn draw_rect(buf: &mut [u32], w: usize, h: usize, r: crate::layout::Rect, t: i32, color: u32) {
    let x0 = r.x.max(0) as usize;
    let y0 = r.y.max(0) as usize;
    let x1 = ((r.x + r.w) as usize).min(w);
    let y1 = ((r.y + r.h) as usize).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let t = t as usize;
    for y in y0..y1 {
        let edge_y = y < y0 + t || y + t >= y1;
        for x in x0..x1 {
            if edge_y || x < x0 + t || x + t >= x1 {
                buf[y * w + x] = color;
            }
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
                    name: m.name().unwrap_or_else(|| "?".into()),
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

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if let Err(e) = self.create_window(event_loop, Target::Program) {
            warn!("fenêtre programme : {e:#}");
        }
        if self.cfg.multiview.enabled {
            if let Err(e) = self.create_window(event_loop, Target::Multiview) {
                warn!("fenêtre multiview : {e:#}");
            }
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame(t) => {
                if let Some(ws) = self.windows.iter().find(|w| w.target == t) {
                    ws.window.request_redraw();
                }
            }
        }
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
                let mut ws = self.windows.remove(idx);
                self.resize(&mut ws, size);
                self.windows.insert(idx, ws);
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

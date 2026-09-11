//! Rendu GPU (wgpu ; Metal sur macOS) : composition des scènes, programme et multiview.
//!
//! Chaque scène est rendue dans une texture hors écran à la taille du canevas, puis
//! composée dans les fenêtres (programme : fondu entre deux scènes ; multiview : tuiles).

use crate::config::Geometry;
use crate::engine::{Engine, LayerKind, RenderState, NO_PHONE_TEXT};
use crate::frame::{with_planes, FrameSlot, PixelFormat};
use crate::layout::{self, Rect};
use crate::text;
use anyhow::{anyhow, Context, Result};
use bytemuck::{Pod, Zeroable};
use image::RgbaImage;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};
use winit::window::Window;

const UNIFORM_SLOT: u64 = 256;
const MAX_DRAWS: u64 = 2048;

const KIND_RGBA: u32 = 0;
const KIND_BGRA: u32 = 1;
const KIND_NV12: u32 = 2;
const KIND_SOLID: u32 = 3;

// Espaces de clés du cache de textures.
const KEY_SLOT: u64 = 1 << 56;
const KEY_LAYER: u64 = 2 << 56;
const KEY_SCENE: u64 = 3 << 56;
const KEY_LABEL: u64 = 4 << 56;

const SHADER: &str = r#"
struct U {
    rect: vec4<f32>,
    uv: vec4<f32>,
    color: vec4<f32>,
    alpha: f32,
    kind: u32,
    _p0: u32,
    _p1: u32,
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var t0: texture_2d<f32>;
@group(0) @binding(2) var t1: texture_2d<f32>;
@group(0) @binding(3) var s: sampler;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
    let c = corners[i];
    var out: VOut;
    out.pos = vec4<f32>(u.rect.xy + c * u.rect.zw, 0.0, 1.0);
    out.uv = u.uv.xy + c * u.uv.zw;
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    var c: vec4<f32>;
    if u.kind == 0u {
        c = textureSample(t0, s, in.uv);
    } else if u.kind == 1u {
        c = textureSample(t0, s, in.uv).bgra;
    } else if u.kind == 2u {
        // NV12, BT.709 plage limitée.
        let y = (textureSample(t0, s, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
        let cbcr = (textureSample(t1, s, in.uv).rg - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0);
        let r = y + 1.5748 * cbcr.y;
        let g = y - 0.1873 * cbcr.x - 0.4681 * cbcr.y;
        let b = y + 1.8556 * cbcr.x;
        c = vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    } else {
        c = u.color;
    }
    return vec4<f32>(c.rgb, c.a * u.alpha);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniform {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    alpha: f32,
    kind: u32,
    _pad: [u32; 2],
}

struct GpuTex {
    width: u32,
    height: u32,
    kind: u32,
    tex0: wgpu::Texture,
    tex1: Option<wgpu::Texture>,
    view0: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    /// Séquence de la dernière image envoyée (sources vidéo).
    seq: u64,
}

/// Une commande de dessin : rectangle en pixels de la cible, texture ou couleur.
#[derive(Clone, Copy)]
pub struct Draw {
    pub rect: [f32; 4],
    pub alpha: f32,
    pub key: Option<u64>,
    pub color: [f32; 4],
}

impl Draw {
    fn tex(rect: [f32; 4], key: u64, alpha: f32) -> Self {
        Draw {
            rect,
            alpha,
            key: Some(key),
            color: [0.0; 4],
        }
    }
    fn solid(rect: [f32; 4], color: [f32; 4]) -> Self {
        Draw {
            rect,
            alpha: 1.0,
            key: None,
            color,
        }
    }
}

pub struct SurfaceState {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

impl SurfaceState {
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }
}

pub struct Renderer {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    shader: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    uniforms: wgpu::Buffer,
    slot: u64,
    sampler: wgpu::Sampler,
    dummy: GpuTex,
    textures: HashMap<u64, GpuTex>,
    labels: HashMap<String, u64>,
    dyn_labels: HashMap<String, (String, u64)>,
    next_label: u64,
    canvas: (u32, u32),
}

fn to_rect(r: Rect) -> [f32; 4] {
    [r.x as f32, r.y as f32, r.w as f32, r.h as f32]
}

/// Rectangle 16:9 (ratio du canevas) contenu et centré dans `dst`.
fn contain(dst: [f32; 4], aspect: f32) -> [f32; 4] {
    let (w, h) = if dst[2] / dst[3] > aspect {
        (dst[3] * aspect, dst[3])
    } else {
        (dst[2], dst[2] / aspect)
    };
    [
        dst[0] + (dst[2] - w) / 2.0,
        dst[1] + (dst[3] - h) / 2.0,
        w,
        h,
    ]
}

impl Renderer {
    pub fn new(window: Arc<Window>, canvas: (u32, u32)) -> Result<(Renderer, SurfaceState)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
        });
        let surface = instance
            .create_surface(window.clone())
            .context("surface wgpu")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| anyhow!("aucun GPU compatible : {e}"))?;
        let info = adapter.get_info();
        info!("GPU : {} ({:?})", info.name, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("streame"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .context("périphérique wgpu")?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<Uniform>() as u64
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: UNIFORM_SLOT * MAX_DRAWS,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let dummy = Self::make_tex(
            &device, &layout, &uniforms, &sampler, 1, 1, KIND_RGBA, false,
        );
        let mut r = Renderer {
            instance,
            adapter,
            device,
            queue,
            shader,
            layout,
            pipeline_layout,
            pipelines: HashMap::new(),
            uniforms,
            slot: 0,
            sampler,
            dummy,
            textures: HashMap::new(),
            labels: HashMap::new(),
            dyn_labels: HashMap::new(),
            next_label: 1,
            canvas,
        };
        let state = r.configure_surface(
            surface,
            window.inner_size().width,
            window.inner_size().height,
            true,
        )?;
        Ok((r, state))
    }

    pub fn create_surface(&mut self, window: Arc<Window>, vsync: bool) -> Result<SurfaceState> {
        let surface = self
            .instance
            .create_surface(window.clone())
            .context("surface wgpu")?;
        self.configure_surface(
            surface,
            window.inner_size().width,
            window.inner_size().height,
            vsync,
        )
    }

    fn configure_surface(
        &mut self,
        surface: wgpu::Surface<'static>,
        w: u32,
        h: u32,
        vsync: bool,
    ) -> Result<SurfaceState> {
        let caps = surface.get_capabilities(&self.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .context("aucun format de surface")?;
        let present_mode = if vsync {
            wgpu::PresentMode::AutoVsync
        } else if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w.max(1),
            height: h.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.device, &config);
        self.pipeline_for(format);
        Ok(SurfaceState { surface, config })
    }

    pub fn resize(&mut self, s: &mut SurfaceState, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        s.config.width = w;
        s.config.height = h;
        s.surface.configure(&self.device, &s.config);
    }

    fn pipeline_for(&mut self, format: wgpu::TextureFormat) -> &wgpu::RenderPipeline {
        if !self.pipelines.contains_key(&format) {
            let p = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("quad"),
                    layout: Some(&self.pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &self.shader,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &self.shader,
                        entry_point: Some("fs"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    multiview_mask: None,
                    cache: None,
                });
            self.pipelines.insert(format, p);
        }
        &self.pipelines[&format]
    }

    fn make_tex(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniforms: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
        kind: u32,
        render_target: bool,
    ) -> GpuTex {
        let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        if render_target {
            usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        let desc = |label: &'static str, w: u32, h: u32, format: wgpu::TextureFormat| {
            wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            }
        };
        let (tex0, tex1) = if kind == KIND_NV12 {
            (
                device.create_texture(&desc("y", width, height, wgpu::TextureFormat::R8Unorm)),
                Some(device.create_texture(&desc(
                    "uv",
                    width.div_ceil(2),
                    height.div_ceil(2),
                    wgpu::TextureFormat::Rg8Unorm,
                ))),
            )
        } else {
            (
                device.create_texture(&desc(
                    "rgba",
                    width,
                    height,
                    wgpu::TextureFormat::Rgba8Unorm,
                )),
                None,
            )
        };
        let view0 = tex0.create_view(&Default::default());
        let view1 = tex1.as_ref().map(|t| t.create_view(&Default::default()));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: uniforms,
                        offset: 0,
                        size: wgpu::BufferSize::new(std::mem::size_of::<Uniform>() as u64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view0),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(view1.as_ref().unwrap_or(&view0)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        GpuTex {
            width,
            height,
            kind,
            tex0,
            tex1,
            view0,
            bind_group,
            seq: 0,
        }
    }

    fn ensure_tex(&mut self, key: u64, width: u32, height: u32, kind: u32, render_target: bool) {
        let ok = self
            .textures
            .get(&key)
            .map(|t| t.width == width && t.height == height && t.kind == kind)
            .unwrap_or(false);
        if !ok {
            let t = Self::make_tex(
                &self.device,
                &self.layout,
                &self.uniforms,
                &self.sampler,
                width,
                height,
                kind,
                render_target,
            );
            self.textures.insert(key, t);
        }
    }

    fn write_plane(
        &self,
        tex: &wgpu::Texture,
        data: &[u8],
        stride: u32,
        width: u32,
        height: u32,
        bpp: u32,
    ) {
        let rows = height.min(data.len() as u32 / stride.max(1));
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d {
                width: (width * bpp).min(stride) / bpp,
                height: rows,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Envoie la dernière image d'une source vidéo si elle a changé. Retourne la clé si une image existe.
    fn upload_slot(&mut self, slot: &FrameSlot) -> Option<u64> {
        let key = KEY_SLOT | slot.id();
        let (seq, sample) = slot.latest()?;
        if self
            .textures
            .get(&key)
            .map(|t| t.seq == seq)
            .unwrap_or(false)
        {
            return Some(key);
        }
        let uploaded = with_planes(&sample, |p| {
            let kind = match p.format {
                PixelFormat::Rgba => KIND_RGBA,
                PixelFormat::Bgra => KIND_BGRA,
                PixelFormat::Nv12 => KIND_NV12,
            };
            self.ensure_tex(key, p.width, p.height, kind, false);
            let t = &self.textures[&key];
            match p.format {
                PixelFormat::Nv12 => {
                    if p.data.len() < 2 {
                        return false;
                    }
                    self.write_plane(&t.tex0, p.data[0].0, p.data[0].1, p.width, p.height, 1);
                    if let Some(uv) = &t.tex1 {
                        self.write_plane(
                            uv,
                            p.data[1].0,
                            p.data[1].1,
                            p.width.div_ceil(2),
                            p.height.div_ceil(2),
                            2,
                        );
                    }
                }
                _ => self.write_plane(&t.tex0, p.data[0].0, p.data[0].1, p.width, p.height, 4),
            }
            true
        })
        .unwrap_or(false);
        if !uploaded {
            return None;
        }
        if let Some(t) = self.textures.get_mut(&key) {
            t.seq = seq;
        }
        Some(key)
    }

    fn upload_image(&mut self, key: u64, img: &RgbaImage) -> u64 {
        if !self.textures.contains_key(&key) {
            let (w, h) = img.dimensions();
            self.ensure_tex(key, w, h, KIND_RGBA, false);
            let t = &self.textures[&key];
            self.write_plane(&t.tex0, img.as_raw(), w * 4, w, h, 4);
        }
        key
    }

    /// Texture d'un libellé statique (mise en cache par texte/taille).
    fn label(&mut self, text_: &str, px: f32, color: [u8; 4], bg: Option<[u8; 4]>) -> u64 {
        let cache_key = format!("{text_}|{px}|{color:?}|{bg:?}");
        if let Some(k) = self.labels.get(&cache_key) {
            return *k;
        }
        let key = KEY_LABEL | self.next_label;
        self.next_label += 1;
        let img = text::render_label(text_, px, color, bg, (px * 0.25) as u32);
        self.upload_image(key, &img);
        self.labels.insert(cache_key, key);
        key
    }

    /// Texture d'un libellé qui change souvent (statistiques) : une seule texture par nom.
    fn dyn_label(
        &mut self,
        name: &str,
        text_: &str,
        px: f32,
        color: [u8; 4],
        bg: Option<[u8; 4]>,
    ) -> u64 {
        if let Some((cur, k)) = self.dyn_labels.get(name) {
            if cur == text_ {
                return *k;
            }
        }
        let key = self
            .dyn_labels
            .get(name)
            .map(|(_, k)| *k)
            .unwrap_or_else(|| {
                let k = KEY_LABEL | self.next_label;
                self.next_label += 1;
                k
            });
        self.textures.remove(&key);
        let img = text::render_label(text_, px, color, bg, (px * 0.25) as u32);
        self.upload_image(key, &img);
        self.dyn_labels
            .insert(name.to_string(), (text_.to_string(), key));
        key
    }

    fn tex_size(&self, key: u64) -> (f32, f32) {
        self.textures
            .get(&key)
            .map(|t| (t.width as f32, t.height as f32))
            .unwrap_or((1.0, 1.0))
    }

    /// Rectangle d'un calque dans le canevas.
    fn layer_rect(&self, g: &Geometry, src: Option<(f32, f32)>) -> [f32; 4] {
        let (cw, ch) = (self.canvas.0 as f32, self.canvas.1 as f32);
        let (x, y) = (g.x as f32, g.y as f32);
        match (g.width, g.height, src) {
            (Some(w), Some(h), _) => [x, y, w as f32, h as f32],
            (Some(w), None, Some((sw, sh))) => [x, y, w as f32, w as f32 * sh / sw],
            (None, Some(h), Some((sw, sh))) => [x, y, h as f32 * sw / sh, h as f32],
            (None, None, Some((sw, sh))) => {
                let r = contain([0.0, 0.0, cw, ch], sw / sh);
                [r[0] + x, r[1] + y, r[2], r[3]]
            }
            _ => [x, y, cw, ch],
        }
    }

    /// Rend toutes les scènes dans leurs textures hors écran.
    fn render_scenes(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        engine: &Engine,
        rs: &RenderState,
    ) {
        let phone_key = engine
            .phone_slot()
            .latest()
            .and_then(|_| self.upload_slot(engine.phone_slot()));
        let (cw, ch) = (self.canvas.0 as f32, self.canvas.1 as f32);
        let waiting = self.label(
            NO_PHONE_TEXT,
            ch / 22.0,
            [255, 255, 255, 255],
            Some([0, 0, 0, 160]),
        );
        for (i, scene) in engine.scenes_ref().iter().enumerate() {
            let key = KEY_SCENE | i as u64;
            self.ensure_tex(key, self.canvas.0, self.canvas.1, KIND_RGBA, true);
            let mut draws = Vec::new();
            let mut wants_phone = false;
            for layer in &scene.layers {
                match &layer.kind {
                    LayerKind::Phone => {
                        wants_phone = true;
                        if let Some(k) = phone_key {
                            let rect = self.layer_rect(&layer.geometry, Some(self.tex_size(k)));
                            draws.push(Draw::tex(rect, k, layer.opacity));
                        }
                    }
                    LayerKind::Color(c) => {
                        let rect = self.layer_rect(&layer.geometry, None);
                        draws.push(Draw::solid(rect, *c));
                    }
                    LayerKind::Image(img) | LayerKind::Text(img) => {
                        let k = self.upload_image(KEY_LAYER | layer.id, img);
                        let (tw, th) = self.tex_size(k);
                        let rect = if matches!(layer.kind, LayerKind::Text(_)) {
                            let g = &layer.geometry;
                            match (g.width, g.height) {
                                (Some(w), Some(h)) => [
                                    g.x as f32 + (w as f32 - tw) / 2.0,
                                    g.y as f32 + (h as f32 - th) / 2.0,
                                    tw,
                                    th,
                                ],
                                _ => [g.x as f32, g.y as f32, tw, th],
                            }
                        } else {
                            self.layer_rect(&layer.geometry, Some((tw, th)))
                        };
                        draws.push(Draw::tex(rect, k, layer.opacity));
                    }
                    LayerKind::Video(slot) => {
                        if let Some(k) = self.upload_slot(slot) {
                            let rect = self.layer_rect(&layer.geometry, Some(self.tex_size(k)));
                            draws.push(Draw::tex(rect, k, layer.opacity));
                        }
                    }
                }
            }
            if wants_phone && (phone_key.is_none() || !rs.phone_connected) {
                let (tw, th) = self.tex_size(waiting);
                draws.push(Draw::tex(
                    [(cw - tw) / 2.0, (ch - th) / 2.0, tw, th],
                    waiting,
                    1.0,
                ));
            }
            let view = self.textures[&key].view0.clone();
            self.encode_pass(
                encoder,
                &view,
                wgpu::TextureFormat::Rgba8Unorm,
                (self.canvas.0, self.canvas.1),
                Some(wgpu::Color::BLACK),
                &draws,
            );
        }
    }

    /// Dessins du programme (avec fondu) dans `dst`.
    fn program_draws(&self, rs: &RenderState, dst: [f32; 4]) -> Vec<Draw> {
        let rect = contain(dst, self.canvas.0 as f32 / self.canvas.1 as f32);
        let mut v = Vec::new();
        if let Some((from, p)) = rs.fade {
            v.push(Draw::tex(rect, KEY_SCENE | from as u64, 1.0));
            v.push(Draw::tex(rect, KEY_SCENE | rs.program as u64, p));
        } else {
            v.push(Draw::tex(rect, KEY_SCENE | rs.program as u64, 1.0));
        }
        v
    }

    fn border(draws: &mut Vec<Draw>, r: [f32; 4], t: f32, color: [f32; 4]) {
        draws.push(Draw::solid([r[0], r[1], r[2], t], color));
        draws.push(Draw::solid([r[0], r[1] + r[3] - t, r[2], t], color));
        draws.push(Draw::solid([r[0], r[1], t, r[3]], color));
        draws.push(Draw::solid([r[0] + r[2] - t, r[1], t, r[3]], color));
    }

    /// Rend la fenêtre programme. Retourne `false` si l'image a été sautée (fenêtre masquée).
    pub fn render_program(&mut self, s: &SurfaceState, engine: &Engine) -> Result<bool> {
        let rs = engine.render_state();
        let (w, h) = s.size();
        let Some(frame) = self.acquire(s)? else {
            return Ok(false);
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.slot = 0;
        self.render_scenes(&mut encoder, engine, &rs);
        let draws = self.program_draws(&rs, [0.0, 0.0, w as f32, h as f32]);
        self.encode_pass(
            &mut encoder,
            &view,
            s.config.format,
            (w, h),
            Some(wgpu::Color::BLACK),
            &draws,
        );
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(true)
    }

    /// Rend la fenêtre multiview.
    pub fn render_multiview(
        &mut self,
        s: &SurfaceState,
        engine: &Engine,
        stats_text: &str,
    ) -> Result<()> {
        let rs = engine.render_state();
        let (w, h) = s.size();
        let n = engine.scenes_ref().len();
        let lay = layout::compute(w as i32, h as i32, n, engine.config().multiview.columns);
        let Some(frame) = self.acquire(s)? else {
            return Ok(());
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.slot = 0;
        self.render_scenes(&mut encoder, engine, &rs);

        let aspect = self.canvas.0 as f32 / self.canvas.1 as f32;
        let px = (lay.tiles.first().map(|t| t.h).unwrap_or(100) as f32 / 9.0).max(11.0);
        let thick = (w as f32 / 400.0).max(2.0);
        let red = [0.88, 0.13, 0.13, 1.0];
        let green = [0.13, 0.75, 0.25, 1.0];
        let mut draws = Vec::new();

        // Programme et preview
        let prog_rect = contain(to_rect(lay.program), aspect);
        draws.extend(self.program_draws(&rs, to_rect(lay.program)));
        let prev_rect = contain(to_rect(lay.preview), aspect);
        draws.push(Draw::tex(prev_rect, KEY_SCENE | rs.preview as u64, 1.0));
        Self::border(&mut draws, prog_rect, thick, red);
        Self::border(&mut draws, prev_rect, thick, green);
        for (label, r) in [("PROGRAMME", prog_rect), ("PREVIEW", prev_rect)] {
            let k = self.label(label, px * 1.2, [255, 255, 255, 255], Some([0, 0, 0, 150]));
            let (tw, th) = self.tex_size(k);
            draws.push(Draw::tex([r[0] + thick, r[1] + thick, tw, th], k, 1.0));
        }
        // Statistiques
        let k = self.dyn_label(
            "stats",
            stats_text,
            px,
            [255, 255, 255, 255],
            Some([0, 0, 0, 150]),
        );
        let (tw, th) = self.tex_size(k);
        draws.push(Draw::tex(
            [
                prog_rect[0] + prog_rect[2] - tw - thick,
                prog_rect[1] + thick,
                tw,
                th,
            ],
            k,
            1.0,
        ));

        // Tuiles de scènes
        for (i, tile) in lay.tiles.iter().enumerate() {
            let r = contain(to_rect(*tile), aspect);
            draws.push(Draw::tex(r, KEY_SCENE | i as u64, 1.0));
            if i == rs.program {
                Self::border(&mut draws, r, thick, red);
            } else if i == rs.preview {
                Self::border(&mut draws, r, thick, green);
            }
            let name = format!("{} · {}", i + 1, engine.scenes_ref()[i].name);
            let k = self.label(&name, px, [255, 255, 255, 255], Some([0, 0, 0, 150]));
            let (tw, th) = self.tex_size(k);
            draws.push(Draw::tex(
                [r[0] + thick, r[1] + r[3] - th - thick, tw, th],
                k,
                1.0,
            ));
        }

        // VU-mètres audio (coin bas-gauche)
        let meters = engine.meters();
        if !meters.is_empty() {
            let mpx = (px * 0.85).max(11.0);
            let pad = mpx * 0.5;
            let row_h = mpx * 1.7;
            let panel_w = (w as f32 * 0.26).clamp(200.0, 460.0);
            let panel_h = row_h * meters.len() as f32 + pad;
            let px0 = thick;
            let py0 = h as f32 - panel_h - thick;
            draws.push(Draw::solid(
                [px0, py0, panel_w, panel_h],
                [0.0, 0.0, 0.0, 0.55],
            ));
            let label_w = panel_w * 0.40;
            let bar_x = px0 + label_w + pad;
            let bar_w = panel_w - label_w - 2.0 * pad;
            let frac = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            for (i, m) in meters.iter().enumerate() {
                let ry = py0 + pad * 0.5 + i as f32 * row_h;
                let bar_h = mpx * 0.7;
                let by = ry + (row_h - bar_h) / 2.0;

                let short = match m.id.as_str() {
                    "stream" => "Stream",
                    "branding" => "Habillage",
                    "return" => "Retour",
                    _ => m.label.as_str(),
                };
                let k = self.label(short, mpx, [230, 230, 230, 255], None);
                let (tw, th) = self.tex_size(k);
                draws.push(Draw::tex(
                    [px0 + pad, by + (bar_h - th) / 2.0, tw, th],
                    k,
                    1.0,
                ));

                draws.push(Draw::solid(
                    [bar_x, by, bar_w, bar_h],
                    [0.16, 0.16, 0.16, 1.0],
                ));
                let peak = m.peak_db.iter().copied().fold(-100.0f32, f32::max);
                let rms = m.rms_db.iter().copied().fold(-100.0f32, f32::max);
                let color = if peak > -3.0 {
                    [0.88, 0.13, 0.13, 1.0]
                } else if peak > -12.0 {
                    [0.88, 0.75, 0.13, 1.0]
                } else {
                    [0.13, 0.75, 0.25, 1.0]
                };
                let fw = bar_w * frac(rms);
                if fw > 0.5 {
                    draws.push(Draw::solid([bar_x, by, fw, bar_h], color));
                }
                let peak_x = bar_x + bar_w * frac(peak);
                draws.push(Draw::solid(
                    [peak_x - 1.0, by, 2.0, bar_h],
                    [1.0, 1.0, 1.0, 0.9],
                ));
            }
        }

        self.encode_pass(
            &mut encoder,
            &view,
            s.config.format,
            (w, h),
            Some(wgpu::Color {
                r: 0.08,
                g: 0.08,
                b: 0.09,
                a: 1.0,
            }),
            &draws,
        );
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(())
    }

    /// Image de la surface à dessiner ; `None` = fenêtre masquée ou délai, on saute l'image.
    fn acquire(&mut self, s: &SurfaceState) -> Result<Option<wgpu::SurfaceTexture>> {
        use wgpu::CurrentSurfaceTexture as C;
        match s.surface.get_current_texture() {
            C::Success(t) | C::Suboptimal(t) => Ok(Some(t)),
            C::Timeout | C::Occluded | C::Validation => Ok(None),
            C::Outdated | C::Lost => {
                s.surface.configure(&self.device, &s.config);
                match s.surface.get_current_texture() {
                    C::Success(t) | C::Suboptimal(t) => Ok(Some(t)),
                    _ => Err(anyhow!("surface perdue")),
                }
            }
        }
    }

    fn encode_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        clear: Option<wgpu::Color>,
        draws: &[Draw],
    ) {
        let (tw, th) = (size.0 as f32, size.1 as f32);
        // Uniformes écrits avant l'encodage (offsets dynamiques distincts par dessin).
        let first_slot = self.slot;
        for d in draws {
            if self.slot >= MAX_DRAWS {
                warn!("trop de dessins dans une image");
                break;
            }
            let kind = match d.key {
                Some(k) => self.textures.get(&k).map(|t| t.kind).unwrap_or(KIND_SOLID),
                None => KIND_SOLID,
            };
            let u = Uniform {
                rect: [
                    d.rect[0] / tw * 2.0 - 1.0,
                    1.0 - d.rect[1] / th * 2.0,
                    d.rect[2] / tw * 2.0,
                    -d.rect[3] / th * 2.0,
                ],
                uv: [0.0, 0.0, 1.0, 1.0],
                color: d.color,
                alpha: d.alpha,
                kind,
                _pad: [0; 2],
            };
            self.queue.write_buffer(
                &self.uniforms,
                self.slot * UNIFORM_SLOT,
                bytemuck::bytes_of(&u),
            );
            self.slot += 1;
        }
        let pipeline = self.pipeline_for(format).clone();
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: match clear {
                        Some(c) => wgpu::LoadOp::Clear(c),
                        None => wgpu::LoadOp::Load,
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        let mut slot = first_slot;
        for d in draws {
            if slot >= MAX_DRAWS {
                break;
            }
            let bg = match d.key.and_then(|k| self.textures.get(&k)) {
                Some(t) => &t.bind_group,
                None => &self.dummy.bind_group,
            };
            pass.set_bind_group(0, bg, &[(slot * UNIFORM_SLOT) as u32]);
            pass.draw(0..6, 0..1);
            slot += 1;
        }
    }
}

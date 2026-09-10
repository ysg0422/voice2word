//! Unified media pipeline policy (Jianying-style).
//!
//! One NV12 WGSL shader is used for both GPU adapters and wgpu's CPU/WARP
//! software backend. Preview always prefers an H.264 proxy on machines without
//! a discrete/integrated GPU; export / independent playback keeps the original.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBackend {
    Gpu,
    CpuSoftware,
}

#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub backend: RenderBackend,
    pub adapter_name: String,
    pub hardware_decode: bool,
    pub force_proxy: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodePolicy {
    pub try_hwaccel: bool,
    pub software_threads: u32,
}

impl HardwareProfile {
    /// Enumerate adapters through wgpu. A software adapter is classified as CPU
    /// so proxy generation stays mandatory on that path.
    pub fn detect() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::all());
        adapters.sort_by_key(|a| match a.get_info().backend {
            wgpu::Backend::Vulkan | wgpu::Backend::Dx12 | wgpu::Backend::Metal => 0,
            wgpu::Backend::Gl => 1,
            _ => 2,
        });

        if let Some(adapter) = adapters.into_iter().find(|a| {
            let info = a.get_info();
            !matches!(
                info.device_type,
                wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
            )
        }) {
            let info = adapter.get_info();
            info!(
                adapter = %info.name,
                backend = ?info.backend,
                device_type = ?info.device_type,
                "GPU media pipeline enabled"
            );
            return Self {
                backend: RenderBackend::Gpu,
                adapter_name: info.name,
                hardware_decode: true,
                force_proxy: false,
            };
        }

        warn!("No hardware GPU adapter found; using FFmpeg software decode + wgpu software rasterizer");
        Self {
            backend: RenderBackend::CpuSoftware,
            adapter_name: "software (wgpu WARP / lavapipe)".into(),
            hardware_decode: false,
            force_proxy: true,
        }
    }

    pub fn use_gpu_pipeline(&self) -> bool {
        self.backend == RenderBackend::Gpu
    }

    pub fn decode_policy(&self) -> DecodePolicy {
        DecodePolicy {
            try_hwaccel: self.hardware_decode,
            software_threads: 0, // ffmpeg 0 = auto
        }
    }
}

/// H.264 proxy generator. Proxy files are never HEVC.
pub struct ProxyManager {
    ffmpeg_path: PathBuf,
    lock: Mutex<()>,
}

impl ProxyManager {
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    pub fn preview_height<P: AsRef<Path>>(&self, source: P, cpu_machine: bool) -> u32 {
        let height = self.probe_height(source.as_ref()).unwrap_or(1080);
        if cpu_machine && height >= 2160 {
            540
        } else {
            720
        }
    }

    pub fn proxy_path<P: AsRef<Path>>(&self, source: P, height: u32) -> PathBuf {
        let source = source.as_ref();
        let name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("media");
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let (len, mtime) = match std::fs::metadata(source) {
            Ok(meta) => {
                let ts = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (meta.len(), ts)
            }
            Err(_) => (0, 0),
        };
        std::env::temp_dir().join(format!(
            "voice2word_proxy_{}_{}_{}_{}p.mp4",
            safe, len, mtime, height
        ))
    }

    pub fn existing_proxy<P: AsRef<Path>>(&self, source: P, height: u32) -> Option<PathBuf> {
        let source = source.as_ref();
        if let Some(h) = self.probe_height(source) {
            if h <= height {
                return Some(source.to_path_buf());
            }
        }
        let out = self.proxy_path(source, height);
        if out.exists() && out.metadata().map(|m| m.len() > 1024).unwrap_or(false) {
            Some(out)
        } else {
            None
        }
    }

    /// Returns the path that preview should play. Original is returned when the
    /// source is already H.264-friendly and not taller than the proxy height.
    pub fn ensure_proxy<P: AsRef<Path>>(&self, source: P, height: u32) -> Result<PathBuf> {
        let source = source.as_ref();
        if let Some(h) = self.probe_height(source) {
            if h <= height {
                info!(height = h, "source already within proxy resolution; skip transcode");
                return Ok(source.to_path_buf());
            }
        }

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let out = self.proxy_path(source, height);
        if out.exists() && out.metadata().map(|m| m.len() > 1024).unwrap_or(false) {
            return Ok(out);
        }

        info!(
            source = %source.display(),
            height,
            out = %out.display(),
            "generating H.264 proxy (libx264, never HEVC)"
        );

        let vf = format!("scale=-2:{}", height);
        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_no_window(&mut cmd);
        let status = cmd
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(source)
            .args([
                "-vf",
                &vf,
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv420p",
                "-profile:v",
                "baseline",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-movflags",
                "+faststart",
                "-threads",
                "0",
            ])
            .arg(&out)
            .status()
            .with_context(|| format!("启动代理生成失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            let _ = std::fs::remove_file(&out);
            anyhow::bail!("FFmpeg 代理生成失败");
        }
        Ok(out)
    }

    fn probe_height(&self, source: &Path) -> Option<u32> {
        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_no_window(&mut cmd);
        let output = cmd.arg("-i").arg(source).output().ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_ffmpeg_height(&stderr)
    }
}

pub fn parse_ffmpeg_height(ffmpeg_stderr: &str) -> Option<u32> {
    for token in ffmpeg_stderr.split_whitespace() {
        let token = token.trim_end_matches(',');
        if let Some((w, h)) = token.split_once('x') {
            if let (Ok(ww), Ok(hh)) = (w.parse::<u32>(), h.parse::<u32>()) {
                if (16..20000).contains(&ww) && (16..20000).contains(&hh) {
                    return Some(hh);
                }
            }
        }
    }
    None
}

pub fn apply_no_window(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// Shared NV12 → RGB shader. Runs on a hardware adapter when one exists,
/// otherwise on wgpu's CPU/WARP software backend — same pipeline either way.
pub const NV12_WGSL_SHADER: &str = r#"
@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var uv_plane: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var x = -1.0;
    var y = -1.0;
    if (i == 1u) { x = 3.0; y = -1.0; }
    else if (i == 2u) { x = -1.0; y = 3.0; }
    return vec4(x, y, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(pos.xy);
    let y = textureLoad(y_plane, coord, 0).r - 0.0625;
    let c = textureLoad(uv_plane, coord / 2, 0).rg - vec2<f32>(0.5, 0.5);
    // BT.601 limited-range, column-major mat3 * vec3(y, u, v)
    let rgb = mat3x3<f32>(
        vec3<f32>(1.164, 1.164, 1.164),
        vec3<f32>(0.0, -0.392, 2.017),
        vec3<f32>(1.596, -0.813, 0.0)
    ) * vec3<f32>(y, c.x, c.y);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// CPU fallback of the same BT.601 limited-range conversion (R,G,B,A).
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w.checked_mul(h).context("nv12 size overflow")?;
    let need = y_size + y_size / 2;
    if nv12.len() < need {
        anyhow::bail!("NV12 buffer too small: {} < {}", nv12.len(), need);
    }
    let y_plane = &nv12[..y_size];
    let uv_plane = &nv12[y_size..need];
    let mut out = vec![0u8; y_size * 4];
    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col] as f32;
            let uv_index = (row / 2) * w + (col & !1);
            let u = uv_plane[uv_index] as f32;
            let v = uv_plane[uv_index + 1] as f32;
            let c = y - 16.0;
            let d = u - 128.0;
            let e = v - 128.0;
            let r = (1.164 * c + 1.596 * e).clamp(0.0, 255.0) as u8;
            let g = (1.164 * c - 0.392 * d - 0.813 * e).clamp(0.0, 255.0) as u8;
            let b = (1.164 * c + 2.017 * d).clamp(0.0, 255.0) as u8;
            let i = (row * w + col) * 4;
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            out[i + 3] = 255;
        }
    }
    Ok(out)
}

#[derive(Clone)]
pub struct DecodedFrame {
    pub pts: f64,
    pub rgba: Vec<u8>,
}

pub struct FrameRing {
    inner: VecDeque<DecodedFrame>,
    capacity: usize,
}

impl FrameRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, frame: DecodedFrame) {
        while self.inner.len() >= self.capacity {
            self.inner.pop_front();
        }
        self.inner.push_back(frame);
    }

    pub fn latest(&self) -> Option<&DecodedFrame> {
        self.inner.back()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Drop frames older than `pts` and return the closest remaining frame.
    pub fn take_closest(&mut self, pts: f64) -> Option<DecodedFrame> {
        while self.inner.len() > 1 {
            let older_gap = (self.inner[0].pts - pts).abs();
            let newer_gap = (self.inner[1].pts - pts).abs();
            if older_gap >= newer_gap {
                self.inner.pop_front();
            } else {
                break;
            }
        }
        self.inner.pop_front()
    }
}

/// wgpu NV12 converter. Prefers a hardware adapter, falls back to a CPU adapter
/// so the same WGSL runs on machines without a GPU.
pub struct Nv12Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    y_tex: wgpu::Texture,
    uv_tex: wgpu::Texture,
    out_tex: wgpu::Texture,
    out_buf: wgpu::Buffer,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    padded_bpr: u32,
}

impl Nv12Renderer {
    pub fn new(prefer_gpu: bool, width: u32, height: u32) -> Result<Self> {
        anyhow::ensure!(width % 2 == 0 && height % 2 == 0, "NV12 size must be even");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::all());
        adapters.sort_by_key(|a| {
            let info = a.get_info();
            let ty = match info.device_type {
                wgpu::DeviceType::DiscreteGpu => 0,
                wgpu::DeviceType::IntegratedGpu => 1,
                wgpu::DeviceType::VirtualGpu => 2,
                wgpu::DeviceType::Cpu => 4,
                wgpu::DeviceType::Other => 5,
            };
            if prefer_gpu { ty } else { 4u8.saturating_sub(ty.min(4)) }
        });
        let adapter = adapters
            .into_iter()
            .next()
            .context("wgpu 未枚举到任何 adapter（含软件后端）")?;
        let info = adapter.get_info();
        info!(
            adapter = %info.name,
            backend = ?info.backend,
            device_type = ?info.device_type,
            "NV12 renderer adapter selected"
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("voice2word-nv12"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
            },
            None,
        ))
        .context("wgpu request_device 失败")?;

        let y_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-uv"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let out_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-out"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let padded_bpr = padded_bytes_per_row(width * 4);
        let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12-readback"),
            size: padded_bpr as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12-shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_WGSL_SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12-bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12-pll"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12-rp"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        Ok(Self {
            device,
            queue,
            y_tex,
            uv_tex,
            out_tex,
            out_buf,
            pipeline,
            bind_group,
            width,
            height,
            padded_bpr,
        })
    }

    pub fn convert(&self, nv12: &[u8]) -> Result<Vec<u8>> {
        let y_size = (self.width * self.height) as usize;
        let need = y_size + y_size / 2;
        if nv12.len() < need {
            anyhow::bail!("NV12 buffer too small");
        }
        let y_plane = &nv12[..y_size];
        let uv_plane = &nv12[y_size..need];

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.y_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            y_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.width),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.uv_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            uv_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.width),
                rows_per_image: Some(self.height / 2),
            },
            wgpu::Extent3d {
                width: self.width / 2,
                height: self.height / 2,
                depth_or_array_layers: 1,
            },
        );

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nv12-enc"),
            });
        {
            let view = self
                .out_tex
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &self.out_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &self.out_buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bpr),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = self.out_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .context("nv12 readback channel closed")?
            .context("nv12 map_async failed")?;

        let mapped = slice.get_mapped_range();
        let row_rgba = (self.width * 4) as usize;
        let mut out = vec![0u8; row_rgba * self.height as usize];
        for row in 0..self.height as usize {
            let src = row * self.padded_bpr as usize;
            let dst = row * row_rgba;
            out[dst..dst + row_rgba].copy_from_slice(&mapped[src..src + row_rgba]);
        }
        drop(mapped);
        self.out_buf.unmap();
        Ok(out)
    }
}

fn padded_bytes_per_row(unpadded: u32) -> u32 {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    ((unpadded + align - 1) / align) * align
}

pub fn convert_nv12_frame(
    renderer: Option<&Nv12Renderer>,
    nv12: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    if let Some(gpu) = renderer {
        match gpu.convert(nv12) {
            Ok(rgba) => return Ok(rgba),
            Err(err) => warn!(error = %err, "wgpu NV12 convert failed; CPU fallback"),
        }
    }
    nv12_to_rgba(nv12, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_black_is_near_black() {
        let w = 2u32;
        let h = 2u32;
        let mut nv12 = vec![16u8; 2 * 2];
        nv12.extend_from_slice(&[128, 128]);
        let rgba = nv12_to_rgba(&nv12, w, h).unwrap();
        assert_eq!(rgba.len(), 16);
        for px in rgba.chunks(4) {
            assert!(px[0] < 8 && px[1] < 8 && px[2] < 8);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn parse_height_from_ffmpeg_banner() {
        let s = "Stream #0:0: Video: h264 (High), yuv420p, 3840x2160, 30 fps";
        assert_eq!(parse_ffmpeg_height(s), Some(2160));
    }

    #[test]
    fn ring_drops_old_frames() {
        let mut ring = FrameRing::new(3);
        for i in 0..5 {
            ring.push(DecodedFrame {
                pts: i as f64,
                rgba: vec![i as u8],
            });
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.latest().unwrap().pts, 4.0);
        let closest = ring.take_closest(3.2).unwrap();
        assert_eq!(closest.pts, 3.0);
    }

    #[test]
    fn proxy_path_includes_height_and_is_mp4() {
        let mgr = ProxyManager::new("ffmpeg");
        let p = mgr.proxy_path(Path::new("C:/media/demo clip.mkv"), 720);
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.contains("720p"));
        assert!(name.ends_with(".mp4"));
        assert!(!name.to_lowercase().contains("hevc"));
    }

    #[test]
    fn detect_does_not_panic() {
        let _ = HardwareProfile::detect();
    }

    #[test]
    fn wgpu_shader_converts_limited_black() {
        let w = 16u32;
        let h = 16u32;
        let mut nv12 = vec![16u8; (w * h) as usize];
        nv12.extend(std::iter::repeat(128u8).take((w * h / 2) as usize));
        let renderer = Nv12Renderer::new(false, w, h).expect("software wgpu adapter required");
        let rgba = renderer.convert(&nv12).expect("shader convert");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        for px in rgba.chunks(4) {
            assert!(px[0] < 12 && px[1] < 12 && px[2] < 12, "got {:?}", px);
            assert_eq!(px[3], 255);
        }
    }
}

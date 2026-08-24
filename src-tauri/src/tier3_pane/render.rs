//! OSR render pipeline: imports CEF's paint output into a wgpu texture and
//! composites every open pane into GTK's shared `GLArea` framebuffer.
//!
//! Adapted from the retained spike's proven pipeline
//! (qr-spike-192/cef-rs/examples/osr/src/{main,webrender}.rs). Phase A
//! (single pane, in-memory CEF context) narrowed this to one static texture
//! slot and no cookie-jar wiring; Phase B (items.id=202 piece 5 / items.id=
//! 223) generalized both -- one texture slot per pane, keyed by
//! `tier3_pane::PaneKey`, and real per-provider `RequestContext` isolation
//! (see pane_host.rs). items.id=234 (host-owned popup subsystem) added a
//! second, parallel texture slot per pane (`POPUP_TEXTURES`, keyed by the
//! *parent* pane's `PaneKey`) for `window.open()`-style OAuth popups --
//! `<select>` dropdown/same-browser popups remain out of scope (see
//! `PopupRenderHandler`'s own doc for the scope boundary).
//!
//! `RenderState` itself (items.id=202 real positioning fix, 2026-08-07):
//! previously one instance per pane, each owning its own `winit::Window` and
//! `wgpu::Surface`, manually position-synced against the main Tauri window --
//! the code path with the confirmed Wayland positioning bug (see
//! pane_host.rs's module docs). Now a single shared instance for the whole
//! app, constructed once from GTK's own external GL context
//! (`wgpu_hal::gles::Adapter::new_external`) rather than from a
//! `winit::Window`. `render()` draws every open pane's texture from
//! `PANE_TEXTURES` into its own `glViewport`-scoped region of ONE shared
//! framebuffer -- GTK's `GLArea`, not a separate surface per pane -- in a
//! single render pass, using `RenderPass::set_viewport`/`set_scissor_rect`
//! rather than raw GL calls (wgpu normalizes viewport coordinate space --
//! origin top-left -- consistently across backends, matching the
//! top-left-origin fractions `paneLayout.ts` already computes from DOM
//! `getBoundingClientRect()`, so no axis flip is needed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use cef::*;
use wgpu::util::DeviceExt;
use wgpu_hal::Adapter as _;

use crate::commands::tier3_pane::PaneRectFraction;
use crate::tier3_pane::PaneKey;

// ---------------------------------------------------------------------------
// items.id=312: GL-context single-thread-ownership guard
// ---------------------------------------------------------------------------
//
// Originally a diagnostic: `RenderState::render()` (main/GTK thread) and
// `on_accelerated_paint`/`on_paint` (CEF's own UI thread -- confirmed a
// genuinely separate OS thread by `multi_threaded_message_loop: 1`, see
// `bootstrap.rs:159-183`'s own prior investigation notes) both called into
// the SAME externally-owned GL context via the shared `wgpu::Device`, with
// nothing serializing GL-context access between those two threads --
// `PANE_TEXTURES`/`POPUP_TEXTURES`'s mutexes only ever protected the
// resulting `BindGroup` maps, not the GL calls that produced them. This
// guard proved that overlap directly (two different thread ids both active
// at once), which is what confirmed items.id=312's root cause.
//
// The fix (see the "CEF-thread paint mailbox" section below) removes the
// race by construction: none of the four CEF callback functions touch the
// device/GL context anymore, only `RenderState::render()` does. This guard
// now wraps that single remaining call site and stays in as a permanent,
// cheap regression check -- `log::error!` (not `warn`/`debug`) if it ever
// observes a second thread entering while one is already active, which
// should now be structurally impossible.
static DIAG_312_GL_ACTIVE: LazyLock<Mutex<HashMap<std::thread::ThreadId, &'static str>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[must_use]
struct Diag312GlGuard {
    tid: std::thread::ThreadId,
}

impl Diag312GlGuard {
    fn enter(site: &'static str) -> Self {
        let tid = std::thread::current().id();
        let mut active = DIAG_312_GL_ACTIVE.lock().unwrap();
        let others: Vec<String> = active
            .iter()
            .filter(|(t, _)| **t != tid)
            .map(|(t, s)| format!("{t:?}={s}"))
            .collect();
        if others.is_empty() {
            log::debug!("DIAG items.id=312: GL enter {site} thread={tid:?}");
        } else {
            log::error!(
                "DIAG items.id=312: CONFIRMED CONCURRENT GL ACCESS -- {site} \
                 entering on thread {tid:?} while already active: {others:?}"
            );
        }
        active.insert(tid, site);
        Self { tid }
    }
}

impl Drop for Diag312GlGuard {
    fn drop(&mut self) {
        DIAG_312_GL_ACTIVE.lock().unwrap().remove(&self.tid);
    }
}

/// Single shared wgpu render state for the whole app: device/queue (from
/// GTK's own external GL context, not a `winit`-owned surface), the
/// single-textured-quad pipeline that draws CEF's paint output, and the
/// current size of GTK's `GLArea` (the one shared framebuffer every open
/// pane composites into).
pub struct RenderState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    surface_format: wgpu::TextureFormat,
    size: (u32, u32),
    quad: Geometry,
}

impl RenderState {
    /// `loader` must be a valid GL proc-address function -- GTK's `GLArea`
    /// guarantees its GL context is current for the duration of the
    /// `realize`/`render` signal handlers this is called from (see
    /// pane_host.rs), which is `wgpu_hal::gles::Adapter::new_external`'s own
    /// safety requirement.
    ///
    /// Constructs the wgpu `Device`/`Queue` by hand-wiring wgpu-hal's
    /// external-GLES-adapter path (`Adapter::new_external` ->
    /// `hal::Adapter::open` -> `Instance::create_adapter_from_hal` ->
    /// `Adapter::create_device_from_hal`) rather than the usual
    /// `Instance::request_adapter`/`request_device` -- there is no
    /// `wgpu::Surface` here at all (see `render()`'s doc): GTK owns the
    /// actual GL context and framebuffer, wgpu is a guest renderer inside it.
    pub async fn new(
        loader: impl FnMut(&str) -> *const std::ffi::c_void,
        initial_size: (u32, u32),
    ) -> Self {
        let exposed = unsafe {
            wgpu_hal::gles::Adapter::new_external(loader, wgpu_types::GlBackendOptions::default())
        }
        .expect(
            "tier3_pane::render: failed to create wgpu-hal GLES adapter from GTK's external GL context",
        );

        let required_limits = wgpu_types::Limits {
            max_non_sampler_bindings: 2048,
            ..Default::default()
        };
        let open_device = unsafe {
            exposed
                .adapter
                .open(exposed.features, &required_limits, &Default::default())
        }
        .expect("tier3_pane::render: failed to open wgpu-hal GLES device");

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = unsafe { instance.create_adapter_from_hal::<wgpu_hal::api::Gles>(exposed) };
        let (device, queue) = unsafe {
            adapter.create_device_from_hal::<wgpu_hal::api::Gles>(
                open_device,
                &wgpu::DeviceDescriptor {
                    required_limits,
                    ..Default::default()
                },
            )
        }
        .expect(
            "tier3_pane::render: failed to create wgpu Device/Queue from external GLES adapter",
        );

        // GL's own default framebuffer is natively RGBA (unlike Vulkan/D3D/
        // Metal swapchains, which commonly prefer BGRA) -- Bgra8Unorm was
        // Phase A/B's format for a real wgpu::Surface swapchain and does not
        // apply here.
        let surface_format = wgpu::TextureFormat::Rgba8Unorm;

        let texture_bind_group_layout = texture_bind_group_layout(&device);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tier3_pane cef texture shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tier3_pane cef pipeline layout"),
            bind_group_layouts: &[Some(&texture_bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tier3_pane cef render pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(Vertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent::OVER,
                        alpha: wgpu::BlendComponent::OVER,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: Some(wgpu::Face::Back),
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });
        let quad = Geometry::new(&device);

        Self {
            device,
            queue,
            pipeline,
            surface_format,
            size: initial_size,
            quad,
        }
    }

    /// Called from the GLArea's own `resize` signal (fires when GTK's
    /// widget layout actually changes its allocated size) -- not from a
    /// raw native `WindowEvent::Resized` flood the way Phase A/B's
    /// per-pane `winit::Window` needed `RESIZE_DEBOUNCE` to coalesce.
    /// Normal GTK widget layout, not a second OS window being fought with
    /// the window manager -- if manual verification shows this firing at a
    /// similarly pathological rate, a debounce can be reintroduced here, but
    /// there is no evidence yet that it's needed.
    pub fn resize(&mut self, new_size: (u32, u32)) {
        if new_size.0 > 0 && new_size.1 > 0 {
            self.size = new_size;
        }
    }

    /// DIAG items.id=312 (temporary): exposes `self.size` so
    /// `pane_host.rs`'s `connect_render` can cross-check it against the
    /// `GLArea`'s own live `allocated_width()/height()` at render time.
    /// Remove alongside the rest of this session's items.id=312
    /// instrumentation once root cause is confirmed.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Composites every open pane's current CEF paint texture into its own
    /// `glViewport`/scissor-scoped region of GTK's `GLArea` framebuffer, in
    /// one render pass. `layout` is `PaneLayoutState`'s live contents --
    /// fractions (0..1) of the whole window's content area, unchanged
    /// semantics from the old per-window `sync_to` design (see
    /// `commands::tier3_pane::PaneRectFraction`'s own doc) -- multiplied
    /// here against `self.size` (the GLArea's own physical pixel size,
    /// which already *is* the window's content area, since the GLArea fills
    /// it) instead of against a `PhysicalRect` obtained from
    /// `window.inner_position()`/`inner_size()` the old design needed and
    /// that's confirmed broken on Wayland. A pane with no reported layout
    /// yet is skipped, not drawn at a placeholder position -- matches the
    /// old design's same choice (see main.rs's prior `MainEventsCleared`
    /// handler).
    ///
    /// Renders into GTK's OWN current draw framebuffer (via
    /// `wgpu_hal::gles::Texture::default_framebuffer`), not a `wgpu::Surface`
    /// -- there is no swapchain here, GTK already owns presentation for this
    /// widget. The caller (pane_host.rs's `GLArea::render` signal handler)
    /// is responsible for capturing the real bound framebuffer id via a raw
    /// GL call *before* calling this, and explicitly rebinding it
    /// afterward -- confirmed this session that wgpu-hal's own internal
    /// calls silently rebind to their own scratch target, so GTK would
    /// otherwise composite from the wrong framebuffer once this returns.
    /// `popup_layout` is the analogous live-fraction map for open popups
    /// (items.id=234), keyed by the *parent* pane's `PaneKey` -- drawn in a
    /// second pass after every pane's own texture, so a popup always paints
    /// on top of its parent (no depth buffer exists here; draw order is
    /// submission order).
    pub fn render(
        &mut self,
        layout: &HashMap<PaneKey, PaneRectFraction>,
        popup_layout: &HashMap<PaneKey, PaneRectFraction>,
    ) {
        let _diag_312_guard = Diag312GlGuard::enter("RenderState::render");
        // DIAG items.id=312 (temporary): this is the exact call site that
        // panics with "Unable to create Texture object" -- log the size
        // it's about to wrap immediately beforehand so a crash log always
        // has the last-known value even if the panic's own backtrace is
        // hard to symbolize.
        log::debug!(
            "DIAG items.id=312: about to create_texture_from_hal size={:?}",
            self.size
        );
        // items.id=312: drain this tick's CEF-thread paint captures into
        // PANE_TEXTURES/POPUP_TEXTURES *before* the render pass below reads
        // them -- this is the only place any of that captured data reaches
        // the device/GL context (see `resolve_bind_group`, `PendingPaint`).
        // A pane/popup with no new frame this tick simply has no entry
        // here, so its previous BindGroup is left untouched and reused --
        // this *is* the dirty tracking, no separate bookkeeping needed.
        //
        // `.drain().collect()` into a `Vec`, not a bare `for .. in
        // MAP.lock().unwrap().drain()`: the latter's `MutexGuard` (a
        // temporary in the for-loop's head expression) lives for the
        // *entire* loop body under Rust's temporary-lifetime-extension
        // rules -- holding PANE_PENDING_PAINT/POPUP_PENDING_PAINT locked
        // across every iteration's `resolve_bind_group` GPU-import work.
        // CEF's UI thread needs that same lock on every single paint to
        // stash the next frame, so a slow import would leave it blocked
        // for the whole drain, not just a moment -- collecting first keeps
        // the lock held only as long as it takes to drain the map.
        let pending_panes: Vec<_> = PANE_PENDING_PAINT.lock().unwrap().drain().collect();
        for (key, paint) in pending_panes {
            if let Some(bind_group) = resolve_bind_group(&self.device, &self.queue, paint) {
                PANE_TEXTURES.lock().unwrap().insert(key, bind_group);
            }
        }
        let pending_popups: Vec<_> = POPUP_PENDING_PAINT.lock().unwrap().drain().collect();
        for (key, paint) in pending_popups {
            if let Some(bind_group) = resolve_bind_group(&self.device, &self.queue, paint) {
                POPUP_TEXTURES.lock().unwrap().insert(key, bind_group);
            }
        }

        let hal_texture = wgpu_hal::gles::Texture::default_framebuffer(self.surface_format);
        let target = unsafe {
            self.device.create_texture_from_hal::<wgpu_hal::api::Gles>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("tier3_pane GLArea default framebuffer"),
                    size: wgpu::Extent3d {
                        width: self.size.0,
                        height: self.size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.surface_format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                // GTK's default framebuffer already holds whatever was
                // there from the last frame (or the compositor's own
                // initial clear) -- COLOR_TARGET, not UNINITIALIZED,
                // matches this render pass's own use of it (a color
                // attachment we clear ourselves via LoadOp::Clear below,
                // not a texture wgpu needs to treat as needing its content
                // preserved/validated from an unknown prior state).
                wgpu_types::TextureUses::COLOR_TARGET,
            )
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor {
            label: Some("tier3_pane GLArea framebuffer view"),
            format: Some(self.surface_format),
            ..Default::default()
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tier3_pane render encoder"),
            });
        {
            let pane_textures = PANE_TEXTURES.lock().unwrap();
            // Alpha-blended (see the pipeline's BlendComponent::OVER,
            // unchanged) over a transparent clear -- outside every pane's
            // own viewport, this frame draws nothing at all, so Tauri's own
            // webview (stacked underneath the GLArea in the same window,
            // see pane_host.rs) shows through untouched. This is the actual
            // single-window coexistence mechanism, not an approximation of
            // it.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tier3_pane render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.quad.vertex_buffer.slice(..));

            for (key, bind_group) in pane_textures.iter() {
                let Some(frac) = layout.get(key) else {
                    continue;
                };
                let (w, h) = (self.size.0 as f64, self.size.1 as f64);
                let x = (frac.x * w).round().clamp(0.0, w) as f32;
                let y = (frac.y * h).round().clamp(0.0, h) as f32;
                let width = (frac.width * w).round().clamp(0.0, w - x as f64) as f32;
                let height = (frac.height * h).round().clamp(0.0, h - y as f64) as f32;
                if width <= 0.0 || height <= 0.0 {
                    continue;
                }

                pass.set_viewport(x, y, width, height, 0.0, 1.0);
                pass.set_scissor_rect(x as u32, y as u32, width as u32, height as u32);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..self.quad.vertex_count, 0..1);
            }

            // items.id=234: open popups, drawn after every pane so they
            // always composite on top of their parent -- same viewport/
            // scissor math, reading POPUP_TEXTURES/popup_layout instead.
            let popup_textures = POPUP_TEXTURES.lock().unwrap();
            for (key, bind_group) in popup_textures.iter() {
                let Some(frac) = popup_layout.get(key) else {
                    continue;
                };
                let (w, h) = (self.size.0 as f64, self.size.1 as f64);
                let x = (frac.x * w).round().clamp(0.0, w) as f32;
                let y = (frac.y * h).round().clamp(0.0, h) as f32;
                let width = (frac.width * w).round().clamp(0.0, w - x as f64) as f32;
                let height = (frac.height * h).round().clamp(0.0, h - y as f64) as f32;
                if width <= 0.0 || height <= 0.0 {
                    continue;
                }

                pass.set_viewport(x, y, width, height, 0.0, 1.0);
                pass.set_scissor_rect(x as u32, y as u32, width as u32, height as u32);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..self.quad.vertex_count, 0..1);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    pub fn device(&self) -> wgpu::Device {
        self.device.clone()
    }

    pub fn queue(&self) -> wgpu::Queue {
        self.queue.clone()
    }
}

fn texture_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("tier3_pane cef texture bind group layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

// Phase B (items.id=202 piece 5): one texture slot per pane, keyed by
// PaneKey -- the Vec<Option<_>>/HashMap<PaneId, _> multi-instance form
// items.id=195 step 1 already anticipated. Plain HashMap, not IndexMap:
// this is a pure key->value lookup (each pane's own RenderState only ever
// reads its own key), never iterated, so insertion order has no observer.
//
// A plain (not thread_local!) static: on_paint/on_accelerated_paint write
// this from CEF's own UI thread while RenderState::render reads it from the
// main thread (see multi_threaded_message_loop docs in bootstrap.rs). A
// thread_local! here would give each thread its own independent cell, so
// the main thread would never observe CEF's writes -- found during the
// items.id=203 thread-safety audit (2026-08-03).
static PANE_TEXTURES: LazyLock<Mutex<HashMap<PaneKey, wgpu::BindGroup>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drops a closed pane's texture entry. Must be called as part of that
/// pane's teardown (pane_host.rs), before/alongside dropping its
/// RenderState/wgpu::Device -- leaving a stale entry around risks a future
/// redraw (of some *other* pane, since this pane's own window is gone and
/// can no longer trigger one) finding a BindGroup built from an
/// already-dropped device. Not currently reachable that way (each pane's
/// render() only ever reads its own key), but removing eagerly costs
/// nothing and closes off the failure mode by construction rather than by
/// argument.
pub fn remove_pane_texture(key: &PaneKey) {
    PANE_TEXTURES.lock().unwrap().remove(key);
}

/// items.id=234: mirrors `PANE_TEXTURES` exactly, but for open OAuth
/// popups -- a separate map, keyed by the *parent* pane's `PaneKey` (one
/// active popup per pane, see `pane_host::PaneManager.popups`'s own doc),
/// not a repurposed key scheme on `PANE_TEXTURES`.
static POPUP_TEXTURES: LazyLock<Mutex<HashMap<PaneKey, wgpu::BindGroup>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Mirrors `remove_pane_texture` -- must be called as part of a popup's own
/// teardown (`pane_host.rs`'s `force_close_popup`).
pub fn remove_popup_texture(key: &PaneKey) {
    POPUP_TEXTURES.lock().unwrap().remove(key);
}

// ---------------------------------------------------------------------------
// items.id=312: CEF-thread paint mailbox
// ---------------------------------------------------------------------------
//
// CEF's UI thread (on_accelerated_paint/on_paint, both Pane and Popup
// variants) used to call directly into the shared wgpu::Device/GL context
// to build each frame's BindGroup -- the confirmed source of items.id=312's
// crash (see the GL-context single-thread-ownership guard above). Those
// callbacks now do CPU-only capture (dup the accelerated path's dmabuf
// fds, memcpy the software path's pixel buffer) and stash it here; only
// `RenderState::render()`, on the main/GTK thread, ever turns a capture
// into a `BindGroup`.
//
// Latest-value-wins, not a queue: CEF always delivers a complete current
// frame, never a delta, so an undrained previous entry is simply stale and
// safe to overwrite -- the same assumption PANE_TEXTURES/POPUP_TEXTURES
// already make.
enum PendingPaint {
    Software {
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    },
    Accelerated { info: DupedAcceleratedPaintInfo },
}

/// `info`'s plane fds have already been `dup()`'d by the capturing
/// callback -- CEF's raw `AcceleratedPaintInfo::planes[i].fd` is not owned,
/// it references a buffer CEF's renderer process may recycle the instant
/// the callback returns -- so this wraps independent copies, valid until
/// `Drop` closes them. `DmaBufImporter`'s own Vulkan import path
/// (`osr_texture_import/dmabuf.rs`) `dup()`s its own copy right before
/// handing it to the driver rather than taking ownership of what we pass
/// in, so our copies stay ours to close after `import_texture()` returns,
/// success or failure alike.
///
/// A separate newtype, not a `Drop` impl directly on `PendingPaint`
/// itself: Rust forbids moving fields out of a value whose *own* type
/// implements `Drop` (E0509), which `resolve_bind_group`'s `match paint {
/// .. }` needs to do for both variants. Moving this newtype out whole (its
/// own fields are never individually destructured) has no such
/// restriction.
struct DupedAcceleratedPaintInfo {
    info: cef::AcceleratedPaintInfo,
}

impl Drop for DupedAcceleratedPaintInfo {
    fn drop(&mut self) {
        let plane_count = (self.info.plane_count as usize).min(self.info.planes.len());
        for plane in &self.info.planes[..plane_count] {
            if plane.fd >= 0 {
                unsafe {
                    libc::close(plane.fd);
                }
            }
        }
    }
}

/// Duplicates every populated plane's fd in `info` (`0..info.plane_count`,
/// not just plane 0 -- multi-plane DRM formats like NV12 populate more)
/// into a fresh, independently-owned `AcceleratedPaintInfo`, safe to stash
/// past the end of the CEF callback that received the original borrow. On
/// a `dup()` failure partway through, closes whatever was already dup'd
/// and returns `None` -- the frame is dropped, the same degradation CEF's
/// own paint cadence already tolerates (the next paint simply overwrites).
///
/// Checks `SharedTextureHandle::new(info)` for `Unsupported` up front and
/// bails before touching any fd -- pre-refactor behavior, restored: this
/// used to be the first thing `on_accelerated_paint` checked, before ever
/// calling `import_texture`. Without this check here, an unsupported
/// platform would still pay a full dup()+close() of every plane every
/// frame only to have `resolve_bind_group`'s `import_texture` reject it
/// later with a generic import-failure log line.
fn capture_accelerated_paint(info: &cef::AcceleratedPaintInfo) -> Option<PendingPaint> {
    use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;
    if let SharedTextureHandle::Unsupported = SharedTextureHandle::new(info) {
        log::warn!("tier3_pane::render: platform does not support accelerated OSR painting");
        return None;
    }

    let mut owned = info.clone();
    let plane_count = (owned.plane_count as usize).min(owned.planes.len());
    for i in 0..plane_count {
        let dup_fd = unsafe { libc::dup(owned.planes[i].fd) };
        if dup_fd < 0 {
            log::error!(
                "tier3_pane::render: items.id=312: dup() failed capturing accelerated \
                 paint plane {i}/{plane_count}, errno={}",
                std::io::Error::last_os_error()
            );
            for plane in &owned.planes[..i] {
                unsafe {
                    libc::close(plane.fd);
                }
            }
            return None;
        }
        owned.planes[i].fd = dup_fd;
    }
    Some(PendingPaint::Accelerated {
        info: DupedAcceleratedPaintInfo { info: owned },
    })
}

/// Turns a captured `PendingPaint` into a `BindGroup` -- the only place any
/// of this data touches `device`/`queue` (the GL context), always called
/// from `RenderState::render()` on the main/GTK thread. Consumes `paint`
/// by value so `PendingPaint::Accelerated`'s dup'd fds are always closed
/// (via `Drop`) once this returns, whether the import succeeded or not.
fn resolve_bind_group(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    paint: PendingPaint,
) -> Option<wgpu::BindGroup> {
    match paint {
        PendingPaint::Software {
            pixels,
            width,
            height,
        } => {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tier3_pane cef paint texture (software path)"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            Some(build_bind_group(device, &texture))
        }
        PendingPaint::Accelerated { info } => {
            use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;
            let shared_handle = SharedTextureHandle::new(&info.info);
            match shared_handle.import_texture(device) {
                Ok(texture) => Some(build_bind_group(device, &texture)),
                Err(e) => {
                    log::warn!(
                        "tier3_pane::render: items.id=312: failed to import shared texture: {e:?}"
                    );
                    None
                }
            }
            // `info` drops here regardless of which branch was taken above
            // -- closes the dup'd fds, see `DupedAcceleratedPaintInfo`'s
            // `Drop` impl.
        }
    }
}

/// Mirrors `PANE_TEXTURES`'s own doc: a plain (not `thread_local!`) static
/// so CEF's UI thread (writer) and the main/GTK thread (reader, via
/// `RenderState::render()`) observe the same map.
static PANE_PENDING_PAINT: LazyLock<Mutex<HashMap<PaneKey, PendingPaint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Mirrors `POPUP_TEXTURES`'s own doc: keyed by the *parent* pane's
/// `PaneKey`, same as `POPUP_TEXTURES` itself.
static POPUP_PENDING_PAINT: LazyLock<Mutex<HashMap<PaneKey, PendingPaint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drops a closed pane's not-yet-drained capture, if any -- must be called
/// as part of that pane's teardown (`pane_host.rs`'s `close_pane`),
/// alongside `remove_pane_texture`. `PendingPaint::Drop` closes any owned
/// fds; without this call they'd sit in the map, undrained (the pane no
/// longer has a live entry in `layout` for `render()`'s dirty-tracking to
/// pick up), until process exit.
pub fn remove_pane_pending_paint(key: &PaneKey) {
    PANE_PENDING_PAINT.lock().unwrap().remove(key);
}

/// Mirrors `remove_pane_pending_paint` -- must be called as part of a
/// popup's own teardown (`pane_host.rs`'s `force_close_popup`), alongside
/// `remove_popup_texture`.
pub fn remove_popup_pending_paint(key: &PaneKey) {
    POPUP_PENDING_PAINT.lock().unwrap().remove(key);
}

/// Replaces `winit::dpi::LogicalSize<f32>` (winit dropped from `tier3_pane`
/// entirely, items.id=202 real positioning fix, 2026-08-07 -- see
/// pane_host.rs's module docs) -- same two fields, no winit dependency.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

// ---------------------------------------------------------------------------
// items.id=234: host-owned popup subsystem
// ---------------------------------------------------------------------------
//
// Scope: `window.open()`-style new-browser-window popups only (CEF
// `LifeSpanHandler::on_before_popup`, see `PaneLifeSpanHandler::on_before_popup`
// below) -- used for OAuth provider logins launched from an open Tier 3
// pane. NOT `<select>` dropdowns/autofill/context menus: those are a
// structurally different, same-browser CEF mechanism
// (`RenderHandler::on_popup_show`/`on_popup_size` + `PaintElementType::Popup`
// on the *parent's own* RenderHandler, not a separate `Browser`) -- the
// `type_ != PaintElementType::default()` guards in `PaneRenderHandler::
// on_paint`/`on_accelerated_paint` above stay untouched and still discard
// that surface; a host-owned dropdown subsystem is a separate future item.
//
// Mechanism: on `on_before_popup`, the popup's own `WindowInfo` is forced
// windowless (`windowless_rendering_enabled = true`, matching the parent
// pane's own `accelerated_osr` setting) and handed a fresh `PopupClientBuilder`
// -- so the popup becomes a second, host-managed, windowless `cef::Browser`,
// composited into the same shared `gtk::GLArea`/wgpu pipeline as regular
// panes (`POPUP_TEXTURES`, `RenderState::render`'s second draw loop) rather
// than a real native OS window. This is the design items.id=192's original
// spike recommended and the ADR's final disposition chose to ship --
// deliberately not the separate "Views-based popup delegation" avenue
// (items.id=199/200/201), which hit an unresolved CEF compositor defect
// specific to constructing a Views browser synchronously inside
// `on_before_popup` and was parked as non-blocking, deferred future work.

/// Extracted, plain-int copy of the `PopupFeatures` CEF hands to
/// `on_before_popup` -- not the CEF struct itself, matching this codebase's
/// established practice (see `commands/tier3_pane.rs`'s `CollectCookiesVisitor`
/// doc) of never forwarding a CEF-owned type across the CEF-UI-thread ->
/// GTK-main-thread boundary. `x`/`y` are captured but currently unused --
/// `pane_host::resolve_popup_rect` centers the popup over its parent pane
/// rather than honoring a page-requested position, see that function's own
/// doc.
#[derive(Debug, Clone, Copy, Default)]
pub struct PopupFeatureInts {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// Signals a popup browser's lifecycle to `pane_host.rs`'s per-tick drain --
/// analogous to `PaneLifeSpanHandler`'s plain `cef::Browser` channel, but a
/// popup needs two distinct signals where a pane's own channel only ever
/// needed one ("here is your browser"): a popup can also self-close (a real
/// browser window navigating away or calling `window.close()`), which a
/// pane's own channel has no equivalent of.
#[derive(Clone)]
pub enum PopupLifecycleEvent {
    Ready(cef::Browser),
    Closed,
}

/// One `on_before_popup` request, handed from CEF's UI thread (where that
/// callback runs -- see `PaneLifeSpanHandler::on_before_popup`'s own doc for
/// why it cannot touch `PaneManager`/GTK directly) to `pane_host.rs`'s
/// per-tick `drain_popup_requests`, which does have GTK-main-thread access
/// to `glarea_size` for resolving the popup's actual on-screen rect.
pub struct PopupRequested {
    pub parent_key: PaneKey,
    pub events_rx: std::sync::mpsc::Receiver<PopupLifecycleEvent>,
    pub size: Arc<Mutex<LogicalSize>>,
    pub features: PopupFeatureInts,
}

/// CEF `RenderHandler` implementation: receives paint callbacks and imports
/// the result into `CEF_TEXTURE` for `RenderState::render` to draw.
#[derive(Clone)]
pub struct PaneRenderHandler {
    device_scale_factor: f32,
    // Arc<Mutex<>>, not Rc<RefCell<>>: view_rect (CEF's UI thread) reads this
    // while PaneApp::apply_resize (main thread) writes it -- Rc's non-atomic
    // refcount would race across those two real OS threads under
    // multi_threaded_message_loop=true. Found during the items.id=203
    // thread-safety audit (2026-08-03).
    size: std::sync::Arc<std::sync::Mutex<LogicalSize>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Which `PANE_TEXTURES` slot on_paint/on_accelerated_paint (CEF's own
    /// UI thread) write into. See PaneKey docs (tier3_pane::mod) -- this is
    /// the provider ID the pane this handler belongs to was opened for.
    pane_key: PaneKey,
}

impl PaneRenderHandler {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        device_scale_factor: f32,
        initial_size: LogicalSize,
        pane_key: PaneKey,
    ) -> (Self, std::sync::Arc<std::sync::Mutex<LogicalSize>>) {
        let size = std::sync::Arc::new(std::sync::Mutex::new(initial_size));
        (
            Self {
                device_scale_factor,
                size: size.clone(),
                device,
                queue,
                pane_key,
            },
            size,
        )
    }
}

wrap_render_handler! {
    pub struct RenderHandlerBuilder {
        handler: PaneRenderHandler,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let size = self.handler.size.lock().unwrap();
                if size.width > 0.0 && size.height > 0.0 {
                    rect.width = size.width as _;
                    rect.height = size.height as _;
                }
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            if let Some(screen_info) = screen_info {
                screen_info.device_scale_factor = self.handler.device_scale_factor;
                return true as _;
            }
            false as _
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            _view_x: ::std::os::raw::c_int,
            _view_y: ::std::os::raw::c_int,
            _screen_x: Option<&mut ::std::os::raw::c_int>,
            _screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            false as _
        }

        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            if type_ != PaintElementType::default() {
                // Popup paint surface -- items.id=192's known deferred cost
                // (host-owned popup subsystem). Not handled this phase.
                return;
            }

            // items.id=312: CPU-only capture (dup every plane's fd) -- the
            // actual import into a wgpu texture happens later, on the
            // main/GTK thread inside RenderState::render(). See
            // capture_accelerated_paint's own doc for why the dup is
            // mandatory here rather than deferred.
            let Some(pending) = capture_accelerated_paint(info) else {
                return;
            };
            PANE_PENDING_PAINT
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), pending);
        }

        // items.id=207: on_paint's signature (including the raw `buffer:
        // *const u8` parameter) is generated by the wrap_render_handler!
        // macro above to match CEF's own C++ callback ABI -- it cannot be
        // changed to `unsafe fn` without breaking that FFI contract. The
        // null/bounds checks immediately below (buffer.is_null(), width/
        // height <= 0) are this function's actual safety enforcement for
        // the from_raw_parts call further down; the lint just can't see
        // that from the signature alone.
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if type_ != PaintElementType::default() {
                return; // popup surface, not handled this phase
            }
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }

            let buffer_size = (width * height * 4) as usize;
            let buffer_slice = unsafe { std::slice::from_raw_parts(buffer, buffer_size) };

            // items.id=312: CPU-only capture (owned memcpy) -- texture
            // upload happens later, on the main/GTK thread inside
            // RenderState::render().
            let pending = PendingPaint::Software {
                pixels: buffer_slice.to_vec(),
                width: width as u32,
                height: height as u32,
            };
            PANE_PENDING_PAINT
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), pending);
        }
    }
}

impl RenderHandlerBuilder {
    pub fn build(handler: PaneRenderHandler) -> RenderHandler {
        Self::new(handler)
    }
}

/// items.id=234: near-copy of `PaneRenderHandler`, writing into
/// `POPUP_TEXTURES` (keyed by the *parent* pane's `PaneKey`) instead of
/// `PANE_TEXTURES`.
///
/// **Dual capture path**, built in from the start rather than as a
/// follow-up: items.id=199's spike found `on_after_created` never fired for
/// a naive *windowed* popup attempt. This design uses a windowless/OSR
/// popup (a materially different CEF code path regular panes already prove
/// reliable), so `on_after_created` firing here is expected but not yet
/// separately proven -- see this item's plan, Step 1 ("verification
/// spike"). Rather than gate the whole feature on that answer, both paths
/// are wired unconditionally: on this handler's *first* paint callback, it
/// also sends `PopupLifecycleEvent::Ready` (a fallback capture, using the
/// `browser` paint callbacks already receive); `PopupLifeSpanHandler::
/// on_after_created` sends the same event independently. Whichever fires
/// first wins; a duplicate `Ready` (if both do) is a harmless no-op
/// transition in `PopupLifecycleState`.
#[derive(Clone)]
pub struct PopupRenderHandler {
    device_scale_factor: f32,
    size: Arc<Mutex<LogicalSize>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// The *parent* pane's key -- which `POPUP_TEXTURES` slot this popup's
    /// paint output belongs to.
    pane_key: PaneKey,
    events_tx: std::sync::mpsc::Sender<PopupLifecycleEvent>,
    /// Guards the dual-capture `Ready` send above (send at most once, on
    /// this handler's first paint callback) -- `Arc<AtomicBool>`, not a
    /// plain `bool` field, since this handler is `Clone` (CEF's own
    /// ref-counting clones it) but every clone must observe the same
    /// "already sent" state.
    captured: Arc<AtomicBool>,
}

impl PopupRenderHandler {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        device_scale_factor: f32,
        initial_size: LogicalSize,
        pane_key: PaneKey,
        events_tx: std::sync::mpsc::Sender<PopupLifecycleEvent>,
    ) -> (Self, Arc<Mutex<LogicalSize>>) {
        let size = Arc::new(Mutex::new(initial_size));
        (
            Self {
                device_scale_factor,
                size: size.clone(),
                device,
                queue,
                pane_key,
                events_tx,
                captured: Arc::new(AtomicBool::new(false)),
            },
            size,
        )
    }

    fn maybe_capture(&self, browser: &Option<&mut Browser>) {
        if self.captured.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(browser) = browser {
            let _ = self
                .events_tx
                .send(PopupLifecycleEvent::Ready((*browser).clone()));
        }
    }
}

wrap_render_handler! {
    pub struct PopupRenderHandlerBuilder {
        handler: PopupRenderHandler,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let size = self.handler.size.lock().unwrap();
                if size.width > 0.0 && size.height > 0.0 {
                    rect.width = size.width as _;
                    rect.height = size.height as _;
                }
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            if let Some(screen_info) = screen_info {
                screen_info.device_scale_factor = self.handler.device_scale_factor;
                return true as _;
            }
            false as _
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            _view_x: ::std::os::raw::c_int,
            _view_y: ::std::os::raw::c_int,
            _screen_x: Option<&mut ::std::os::raw::c_int>,
            _screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            false as _
        }

        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        fn on_accelerated_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            // Same defensive guard as PaneRenderHandler -- this popup IS its
            // own browser (not a same-browser popup-paint-layer surface), so
            // type_ is expected to always be the default view, but guarding
            // costs nothing and matches the established pattern.
            if type_ != PaintElementType::default() {
                return;
            }
            self.handler.maybe_capture(&browser);

            let Some(info) = info else { return };

            // items.id=312: CPU-only capture -- see PaneRenderHandler's
            // on_accelerated_paint for the full rationale.
            let Some(pending) = capture_accelerated_paint(info) else {
                return;
            };
            POPUP_PENDING_PAINT
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), pending);
        }

        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if type_ != PaintElementType::default() {
                return;
            }
            self.handler.maybe_capture(&browser);

            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }

            let buffer_size = (width * height * 4) as usize;
            let buffer_slice = unsafe { std::slice::from_raw_parts(buffer, buffer_size) };

            // items.id=312: CPU-only capture -- see PaneRenderHandler's
            // on_paint for the full rationale.
            let pending = PendingPaint::Software {
                pixels: buffer_slice.to_vec(),
                width: width as u32,
                height: height as u32,
            };
            POPUP_PENDING_PAINT
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), pending);
        }
    }
}

impl PopupRenderHandlerBuilder {
    pub fn build(handler: PopupRenderHandler) -> RenderHandler {
        Self::new(handler)
    }
}

fn build_bind_group(device: &wgpu::Device, texture: &wgpu::Texture) -> wgpu::BindGroup {
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });
    let layout = texture_bind_group_layout(device);
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tier3_pane cef texture bind group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture.create_view(
                    &wgpu::TextureViewDescriptor {
                        label: Some("tier3_pane cef texture view"),
                        ..Default::default()
                    },
                )),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    })
}

wrap_client! {
    pub(crate) struct ClientBuilder {
        render_handler: RenderHandler,
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<cef::RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn load_handler(&self) -> Option<cef::LoadHandler> {
            Some(self.load_handler.clone())
        }
    }
}

impl ClientBuilder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build(
        render_handler: PaneRenderHandler,
        browser_ready_tx: std::sync::mpsc::Sender<cef::Browser>,
        pane_key: PaneKey,
        popup_requested_tx: std::sync::mpsc::Sender<PopupRequested>,
        popup_close_tx: std::sync::mpsc::Sender<PaneKey>,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Client {
        Self::new(
            RenderHandlerBuilder::build(render_handler),
            LifeSpanHandlerBuilder::build(PaneLifeSpanHandler::new(
                browser_ready_tx,
                pane_key.clone(),
                popup_requested_tx,
                device,
                queue,
            )),
            LoadHandlerBuilder::build(PaneLoadHandler::new(pane_key, popup_close_tx)),
        )
    }
}

// items.id=234: mirrors `ClientBuilder`, for a popup's own browser -- wires
// `PopupRenderHandler`/`PopupLifeSpanHandler`/`PopupLoadHandler`
// (deliberately *not* the parent's `PaneLoadHandler` -- see
// `PopupLoadHandler`'s own doc for why reusing it would have been wrong).
// A plain comment, not a doc comment: `wrap_client!` (a macro invocation in
// item position) does not forward an outer `///` doc comment to anything,
// which trips the `unused_doc_comments` lint.
wrap_client! {
    pub(crate) struct PopupClientBuilder {
        render_handler: RenderHandler,
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<cef::RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn load_handler(&self) -> Option<cef::LoadHandler> {
            Some(self.load_handler.clone())
        }
    }
}

impl PopupClientBuilder {
    pub(crate) fn build(
        render_handler: PopupRenderHandler,
        events_tx: std::sync::mpsc::Sender<PopupLifecycleEvent>,
    ) -> Client {
        Self::new(
            PopupRenderHandlerBuilder::build(render_handler),
            PopupLifeSpanHandlerBuilder::build(PopupLifeSpanHandler::new(events_tx)),
            PopupLoadHandlerBuilder::build(PopupLoadHandler),
        )
    }
}

/// Delivers the `Browser` back to the pane's main (GTK) thread once CEF's
/// UI thread has actually constructed it.
///
/// FOUND THE HARD WAY (2026-08-01, after switching to
/// multi_threaded_message_loop=true): `browser_host_create_browser_sync`
/// requires being called FROM CEF's own UI thread and returns `None`
/// otherwise -- confirmed directly (real run: `-> false`, no crash, no
/// error, just silent failure to construct a browser). With
/// multi_threaded_message_loop, CEF's UI thread is no longer whatever
/// thread calls into CEF, so sync creation from our own thread cannot
/// work. Fix: use the async `browser_host_create_browser` (callable from
/// any thread) and receive the constructed `Browser` via
/// `LifeSpanHandler::on_after_created`, which CEF calls once the browser
/// actually exists on its own UI thread. That callback fires on CEF's UI
/// thread, not ours -- hence the channel (mpsc::Sender is Send + Sync;
/// cef::Browser itself is designed to be used from any thread per CEF's
/// own thread-safety model for CefBrowser, unlike CefBrowserHost's
/// UI-thread-only methods).
#[derive(Clone)]
pub struct PaneLifeSpanHandler {
    browser_ready_tx: std::sync::mpsc::Sender<cef::Browser>,
    /// This pane's own key -- identifies which pane an `on_before_popup`
    /// request (items.id=234) belongs to, since `PopupRequested` is
    /// dispatched to `pane_host.rs`'s per-tick drain rather than handled
    /// synchronously here (this callback runs on CEF's UI thread, which
    /// must not touch `PaneManager`/GTK directly).
    pane_key: PaneKey,
    popup_requested_tx: std::sync::mpsc::Sender<PopupRequested>,
    /// items.id=234: needed to construct a fresh `PopupRenderHandler` when
    /// `on_before_popup` fires -- cheap `Arc`-backed clones (same handles
    /// `PaneRenderHandler` itself holds), not a new device/queue.
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl PaneLifeSpanHandler {
    fn new(
        browser_ready_tx: std::sync::mpsc::Sender<cef::Browser>,
        pane_key: PaneKey,
        popup_requested_tx: std::sync::mpsc::Sender<PopupRequested>,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Self {
        Self {
            browser_ready_tx,
            pane_key,
            popup_requested_tx,
            device,
            queue,
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct LifeSpanHandlerBuilder {
        handler: PaneLifeSpanHandler,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut cef::Browser>) {
            let Some(browser) = browser else { return; };
            log::info!("tier3_pane::render: on_after_created fired, browser constructed");
            // A closed receiver (pane window already torn down) just means
            // this browser has nowhere to report to anymore -- not a
            // condition worth panicking over.
            let _ = self.handler.browser_ready_tx.send(browser.clone());
        }

        // items.id=234: intercepts `window.open()`-style OAuth popups.
        // Forces the popup windowless/OSR (matching the parent pane's own
        // configuration -- see pane_host.rs's `open_pane`) rather than
        // letting CEF create a real native popup window, and hands it a
        // fresh `PopupClientBuilder` so its paint output flows into
        // POPUP_TEXTURES exactly like a regular pane's does. Runs on CEF's
        // UI thread (multi_threaded_message_loop=true, bootstrap.rs) --
        // dispatches a `PopupRequested` over a channel rather than touching
        // `PaneManager`/GTK directly; `pane_host.rs`'s `drain_popup_requests`
        // (GTK main thread, which does have `glarea_size`) resolves the
        // popup's actual on-screen rect and inserts its `PopupState`.
        //
        // Returns 0 (allow) unconditionally -- every popup this callback
        // sees is treated as this item's in-scope case (see this file's
        // "items.id=234" section doc for the scope boundary; dropdown/
        // context-menu popups never reach `on_before_popup` at all, so
        // there is nothing to filter by `target_disposition` here for v1).
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            log::info!(
                "tier3_pane::render: on_before_popup fired for pane={} target_url={:?}",
                self.handler.pane_key,
                target_url.map(|u| u.to_string()),
            );

            let accelerated_osr = cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            ));
            if let Some(window_info) = window_info {
                window_info.windowless_rendering_enabled = true as _;
                window_info.shared_texture_enabled = accelerated_osr as _;
                window_info.external_begin_frame_enabled = accelerated_osr as _;
            }

            let features = popup_features
                .map(|f| PopupFeatureInts {
                    x: (f.x_set != 0).then_some(f.x),
                    y: (f.y_set != 0).then_some(f.y),
                    width: (f.width_set != 0).then_some(f.width),
                    height: (f.height_set != 0).then_some(f.height),
                })
                .unwrap_or_default();

            let (events_tx, events_rx) = std::sync::mpsc::channel();
            // Initial size is a placeholder -- pane_host.rs's
            // `drain_popup_requests`/`sync_popup_sizes` apply the real
            // resolved size (and push it into this same `size` handle) on
            // the very next GTK render tick, same as a freshly-opened
            // pane's initial `PaneRenderHandler` size is never load-bearing
            // for more than one frame.
            let (render_handler, size) = PopupRenderHandler::new(
                self.handler.device.clone(),
                self.handler.queue.clone(),
                1.0,
                LogicalSize {
                    width: 480.0,
                    height: 640.0,
                },
                self.handler.pane_key.clone(),
                events_tx.clone(),
            );

            if let Some(client_slot) = client {
                *client_slot = Some(PopupClientBuilder::build(render_handler, events_tx));
            }

            let _ = self.handler.popup_requested_tx.send(PopupRequested {
                parent_key: self.handler.pane_key.clone(),
                events_rx,
                size,
                features,
            });

            0
        }
    }
}

impl LifeSpanHandlerBuilder {
    pub(crate) fn build(handler: PaneLifeSpanHandler) -> cef::LifeSpanHandler {
        Self::new(handler)
    }
}

/// items.id=234: the popup's own `LifeSpanHandler` -- reports its lifecycle
/// (`on_after_created`, the primary capture path; `on_before_close`, the
/// only signal for a self-closing popup, e.g. the OAuth page itself calling
/// `window.close()`) back through the same `events_tx` channel
/// `PopupRenderHandler`'s dual-capture path also feeds -- see that
/// handler's own doc for why both are wired.
#[derive(Clone)]
pub struct PopupLifeSpanHandler {
    events_tx: std::sync::mpsc::Sender<PopupLifecycleEvent>,
}

impl PopupLifeSpanHandler {
    fn new(events_tx: std::sync::mpsc::Sender<PopupLifecycleEvent>) -> Self {
        Self { events_tx }
    }
}

wrap_life_span_handler! {
    pub(crate) struct PopupLifeSpanHandlerBuilder {
        handler: PopupLifeSpanHandler,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut cef::Browser>) {
            let Some(browser) = browser else { return; };
            log::info!("tier3_pane::render: popup on_after_created fired");
            let _ = self
                .handler
                .events_tx
                .send(PopupLifecycleEvent::Ready(browser.clone()));
        }

        fn on_before_close(&self, _browser: Option<&mut cef::Browser>) {
            log::info!("tier3_pane::render: popup on_before_close fired (self-closed)");
            let _ = self.handler.events_tx.send(PopupLifecycleEvent::Closed);
        }
    }
}

impl PopupLifeSpanHandlerBuilder {
    pub(crate) fn build(handler: PopupLifeSpanHandler) -> cef::LifeSpanHandler {
        Self::new(handler)
    }
}

/// Load diagnostics for a pane's browser. Previously nothing wired a
/// `LoadHandler` at all -- a stalled or failed pane load (DNS failure, TLS
/// error, a bad redirect) produced zero log signal, indistinguishable from
/// a page that was simply still loading. `on_load_error`/`on_load_end` are
/// the two `ImplLoadHandler` methods with a diagnostic payload worth
/// logging.
///
/// items.id=234 added `on_load_start`, filtered to the main frame: the
/// navigate-away popup-close trigger (that item's plan, Judgment call 6.2).
/// This is deliberately attached ONLY to a parent pane's own browser, never
/// to a popup's own browser -- see `PopupLoadHandler`'s doc for why a first
/// draft that reused this same type for both was wrong, and why the fix is
/// two distinct types rather than a runtime check.
#[derive(Clone)]
pub struct PaneLoadHandler {
    pane_key: PaneKey,
    popup_close_tx: std::sync::mpsc::Sender<PaneKey>,
}

impl PaneLoadHandler {
    fn new(pane_key: PaneKey, popup_close_tx: std::sync::mpsc::Sender<PaneKey>) -> Self {
        Self {
            pane_key,
            popup_close_tx,
        }
    }
}

wrap_load_handler! {
    pub(crate) struct LoadHandlerBuilder {
        handler: PaneLoadHandler,
    }

    impl LoadHandler {
        fn on_load_error(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            error_code: cef::Errorcode,
            error_text: Option<&cef::CefString>,
            failed_url: Option<&cef::CefString>,
        ) {
            log::warn!(
                "tier3_pane::render: on_load_error: code={error_code:?} url={failed_url:?} \
                 text={error_text:?}"
            );
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            log::info!("tier3_pane::render: on_load_end: status={http_status_code}");
        }

        // items.id=234: unconditional -- any main-frame navigation of this
        // PANE's own browser closes that pane's popup, if one is open (a
        // no-op send if none is; pane_host.rs's drain simply finds nothing
        // to close). Filtered to the main frame only: an iframe navigation
        // inside the pane's own page must not close an unrelated popup.
        fn on_load_start(
            &self,
            _browser: Option<&mut cef::Browser>,
            frame: Option<&mut cef::Frame>,
            _transition_type: cef::TransitionType,
        ) {
            let is_main = frame.map(|f| f.is_main() != 0).unwrap_or(false);
            if is_main {
                let _ = self.handler.popup_close_tx.send(self.handler.pane_key.clone());
            }
        }
    }
}

impl LoadHandlerBuilder {
    pub(crate) fn build(handler: PaneLoadHandler) -> cef::LoadHandler {
        Self::new(handler)
    }
}

/// items.id=234: the popup's own `LoadHandler` -- same diagnostic-only
/// `on_load_error`/`on_load_end` behavior as `PaneLoadHandler`, but
/// deliberately WITHOUT an `on_load_start` close-trigger. A popup's own
/// internal navigations (an OAuth flow is itself a chain of main-frame
/// navigations -- the consent screen, redirects, the final callback URL)
/// must never close the popup they're happening inside; only its PARENT
/// pane's navigation should. Reusing `PaneLoadHandler` for both roles (this
/// design's first draft) would have made that impossible to distinguish
/// safely: main-frame navigation is main-frame navigation regardless of
/// which browser it fires on, so the close-trigger would have fired on the
/// popup's own first internal hop and broken every real login. The fix is
/// structural, not a runtime check: parent-pane browsers and popup browsers
/// are wired to two different Rust types at two different construction
/// sites (`PaneManager::open_pane` vs. `PaneLifeSpanHandler::on_before_popup`),
/// so the close-trigger code path simply does not exist here.
#[derive(Clone)]
pub struct PopupLoadHandler;

wrap_load_handler! {
    pub(crate) struct PopupLoadHandlerBuilder {
        handler: PopupLoadHandler,
    }

    impl LoadHandler {
        fn on_load_error(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            error_code: cef::Errorcode,
            error_text: Option<&cef::CefString>,
            failed_url: Option<&cef::CefString>,
        ) {
            log::warn!(
                "tier3_pane::render: popup on_load_error: code={error_code:?} \
                 url={failed_url:?} text={error_text:?}"
            );
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            log::info!("tier3_pane::render: popup on_load_end: status={http_status_code}");
        }
    }
}

impl PopupLoadHandlerBuilder {
    pub(crate) fn build(handler: PopupLoadHandler) -> cef::LoadHandler {
        Self::new(handler)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];

    fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

struct Geometry {
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,
}

impl Geometry {
    fn new(device: &wgpu::Device) -> Self {
        let (x, y, width, height, z) = (-1.0f32, 1.0f32, 2.0f32, 2.0f32, 1.0f32);
        let vertices = [
            Vertex {
                position: [x, y, z],
                tex_coords: [0.0, 0.0],
            },
            Vertex {
                position: [x + width, y, z],
                tex_coords: [1.0, 0.0],
            },
            Vertex {
                position: [x, y - height, z],
                tex_coords: [0.0, 1.0],
            },
            Vertex {
                position: [x + width, y - height, z],
                tex_coords: [1.0, 1.0],
            },
        ];
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tier3_pane quad vertex buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        Self {
            vertex_buffer,
            vertex_count: vertices.len() as u32,
        }
    }
}

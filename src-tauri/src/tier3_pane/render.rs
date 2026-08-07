//! OSR render pipeline: imports CEF's paint output into a wgpu texture and
//! composites every open pane into GTK's shared `GLArea` framebuffer.
//!
//! Adapted from the retained spike's proven pipeline
//! (qr-spike-192/cef-rs/examples/osr/src/{main,webrender}.rs). Phase A
//! (single pane, in-memory CEF context) narrowed this to one static texture
//! slot and no cookie-jar wiring; Phase B (items.id=202 piece 5 / items.id=
//! 223) generalized both -- one texture slot per pane, keyed by
//! `tier3_pane::PaneKey`, and real per-provider `RequestContext` isolation
//! (see pane_host.rs). Popup handling remains items.id=192's known deferred
//! cost, still out of scope here.
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
use std::sync::{LazyLock, Mutex};

use cef::*;
use wgpu::util::DeviceExt;
use wgpu_hal::Adapter as _;

use crate::commands::tier3_pane::PaneRectFraction;
use crate::tier3_pane::PaneKey;

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
    pub fn render(&mut self, layout: &HashMap<PaneKey, PaneRectFraction>) {
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

/// Replaces `winit::dpi::LogicalSize<f32>` (winit dropped from `tier3_pane`
/// entirely, items.id=202 real positioning fix, 2026-08-07 -- see
/// pane_host.rs's module docs) -- same two fields, no winit dependency.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
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

            use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;
            let shared_handle = SharedTextureHandle::new(info);
            if let SharedTextureHandle::Unsupported = shared_handle {
                log::warn!("tier3_pane::render: platform does not support accelerated OSR painting");
                return;
            }
            let src_texture = match shared_handle.import_texture(&self.handler.device) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("tier3_pane::render: failed to import shared texture: {e:?}");
                    return;
                }
            };

            let bind_group = build_bind_group(&self.handler.device, &src_texture);
            PANE_TEXTURES
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), bind_group);
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

            let texture = self.handler.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tier3_pane cef paint texture (software path)"),
                size: wgpu::Extent3d {
                    width: width as u32,
                    height: height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.handler.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                buffer_slice,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width as u32),
                    rows_per_image: Some(height as u32),
                },
                wgpu::Extent3d {
                    width: width as u32,
                    height: height as u32,
                    depth_or_array_layers: 1,
                },
            );

            let bind_group = build_bind_group(&self.handler.device, &texture);
            PANE_TEXTURES
                .lock()
                .unwrap()
                .insert(self.handler.pane_key.clone(), bind_group);
        }
    }
}

impl RenderHandlerBuilder {
    pub fn build(handler: PaneRenderHandler) -> RenderHandler {
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
    }

    impl Client {
        fn render_handler(&self) -> Option<cef::RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }
    }
}

impl ClientBuilder {
    pub(crate) fn build(
        render_handler: PaneRenderHandler,
        browser_ready_tx: std::sync::mpsc::Sender<cef::Browser>,
    ) -> Client {
        Self::new(
            RenderHandlerBuilder::build(render_handler),
            LifeSpanHandlerBuilder::build(PaneLifeSpanHandler::new(browser_ready_tx)),
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
}

impl PaneLifeSpanHandler {
    fn new(browser_ready_tx: std::sync::mpsc::Sender<cef::Browser>) -> Self {
        Self { browser_ready_tx }
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
    }
}

impl LifeSpanHandlerBuilder {
    pub(crate) fn build(handler: PaneLifeSpanHandler) -> cef::LifeSpanHandler {
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

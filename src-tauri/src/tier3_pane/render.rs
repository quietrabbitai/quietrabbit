//! OSR render pipeline: imports CEF's paint output into a wgpu texture and
//! draws it into a dedicated winit window's surface.
//!
//! Adapted from the retained spike's proven pipeline
//! (qr-spike-192/cef-rs/examples/osr/src/{main,webrender}.rs), narrowed to
//! Phase A's single-pane scope -- no multi-instance texture slots, no
//! popup handling (items.id=192's known deferred cost), no cookie-jar
//! wiring yet (Phase B, when real provider RequestContexts are added; this
//! phase uses CEF's default in-memory context to isolate the render/window
//! integration question from the isolation question, which items.id=195
//! already separately proved).

use cef::*;
use std::cell::RefCell;
use wgpu::util::DeviceExt;

/// wgpu render state for the sync window's surface: device/queue, the
/// single-textured-quad pipeline that draws CEF's paint output, and the
/// window's own swapchain surface.
pub struct RenderState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    size: winit::dpi::PhysicalSize<u32>,
    quad: Geometry,
}

impl RenderState {
    pub async fn new(window: std::sync::Arc<winit::window::Window>) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            #[cfg(target_os = "windows")]
            backends: wgpu::Backends::from_comma_list("dx12"),
            #[cfg(target_os = "macos")]
            backends: wgpu::Backends::from_comma_list("metal"),
            #[cfg(target_os = "linux")]
            backends: wgpu::Backends::from_comma_list("vulkan"),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("tier3_pane::render: no wgpu adapter available");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: wgpu::Limits {
                    max_non_sampler_bindings: 2048,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("tier3_pane::render: wgpu device request failed");

        let size = window.inner_size();
        let surface = instance
            .create_surface(window.clone())
            .expect("tier3_pane::render: failed to create wgpu surface on sync window");
        let surface_format = wgpu::TextureFormat::Bgra8Unorm;

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

        let state = Self {
            device,
            queue,
            pipeline,
            surface,
            surface_format,
            size,
            quad,
        };
        state.configure_surface();
        state
    }

    fn configure_surface(&self) {
        self.surface.configure(
            &self.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.surface_format,
                color_space: wgpu::SurfaceColorSpace::Auto,
                view_formats: vec![self.surface_format],
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                width: self.size.width,
                height: self.size.height,
                desired_maximum_frame_latency: 2,
                present_mode: wgpu::PresentMode::AutoVsync,
            },
        );
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.configure_surface();
        }
    }

    /// Draws the current CEF paint texture (if one has arrived yet) into
    /// this frame. No-op (clears to black, per the spike's convention)
    /// until CEF's first paint callback has fired.
    pub fn render(&mut self, window: &winit::window::Window) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(success) => success,
            wgpu::CurrentSurfaceTexture::Suboptimal(suboptimal) => {
                self.configure_surface();
                suboptimal
            }
            _ => return,
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                label: Some("tier3_pane surface view"),
                format: Some(self.surface_format),
                ..Default::default()
            });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tier3_pane render encoder"),
            });
        CEF_TEXTURE.with_borrow(|bind_group| {
            let Some(bind_group) = bind_group.as_ref() else {
                return;
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tier3_pane render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.set_vertex_buffer(0, self.quad.vertex_buffer.slice(..));
            pass.draw(0..self.quad.vertex_count, 0..1);
        });
        self.queue.submit(std::iter::once(encoder.finish()));

        window.pre_present_notify();
        self.queue.present(surface_texture);
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

// Single-pane slot for Phase A -- deliberately not the spike's Vec<Option<_>>
// multi-instance form (items.id=195 step 1) since only one pane exists this
// phase. Revisit when Phase B adds multiple panes.
thread_local! {
    static CEF_TEXTURE: RefCell<Option<wgpu::BindGroup>> = const { RefCell::new(None) };
}

/// CEF `RenderHandler` implementation: receives paint callbacks and imports
/// the result into `CEF_TEXTURE` for `RenderState::render` to draw.
#[derive(Clone)]
pub struct PaneRenderHandler {
    device_scale_factor: f32,
    size: std::rc::Rc<RefCell<winit::dpi::LogicalSize<f32>>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl PaneRenderHandler {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        device_scale_factor: f32,
        initial_size: winit::dpi::LogicalSize<f32>,
    ) -> (Self, std::rc::Rc<RefCell<winit::dpi::LogicalSize<f32>>>) {
        let size = std::rc::Rc::new(RefCell::new(initial_size));
        (
            Self {
                device_scale_factor,
                size: size.clone(),
                device,
                queue,
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
                let size = self.handler.size.borrow();
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
            CEF_TEXTURE.with_borrow_mut(|slot| *slot = Some(bind_group));
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
            CEF_TEXTURE.with_borrow_mut(|slot| *slot = Some(bind_group));
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

/// Delivers the `Browser` back to the pane's winit thread once CEF's UI
/// thread has actually constructed it.
///
/// FOUND THE HARD WAY (2026-08-01, after switching to
/// multi_threaded_message_loop=true): `browser_host_create_browser_sync`
/// requires being called FROM CEF's own UI thread and returns `None`
/// otherwise -- confirmed directly (real run: `-> false`, no crash, no
/// error, just silent failure to construct a browser). With
/// multi_threaded_message_loop, CEF's UI thread is no longer whatever
/// thread calls into CEF, so sync creation from our winit thread cannot
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

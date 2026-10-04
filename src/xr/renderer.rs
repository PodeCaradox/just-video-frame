//! Vulkan resources for drawing video into the OpenXR swapchains: YUV plane
//! textures fed through a staging buffer, one ray-casting pipeline, per-eye
//! swapchains, and an optional readback of the left eye for screenshots.

use super::context::{XrContext, choose_color_format};
use crate::media::{Frame, Matrix, PlaneLayout, Transfer};
use anyhow::{Context, bail};
use ash::vk;
use openxr as xr;
use std::cell::Cell;

const SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/video.spv"));

/// Per-eye shader parameters; layout matches `Eye` in video.wgsl (8 × vec4).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EyeParams {
    pub rot: [[f32; 4]; 3],
    pub tan: [f32; 4],
    pub origin: [f32; 4],
    pub mode: [f32; 4],
    pub screen: [f32; 4],
    pub tex: [f32; 4],
}

/// Matches `Color` in video.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ColorParams {
    rows: [[f32; 4]; 3],
    params: [f32; 4],
    /// Brightness, contrast, saturation, quarter turns (see `Color` in video.wgsl).
    adjust: [f32; 4],
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    size: u64,
    /// Properties of the memory type it was given.
    flags: vk::MemoryPropertyFlags,
}

struct Texture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct VideoKey {
    layout: PlaneLayout,
    bits: u32,
    width: u32,
    height: u32,
}

struct VideoTextures {
    key: VideoKey,
    planes: Vec<(Texture, vk::Format, u32, u32)>,
}

pub struct EyeTarget {
    pub swapchain: xr::Swapchain<xr::Vulkan>,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    pub width: u32,
    pub height: u32,
}

/// A swapchain shown on an OpenXR quad layer, filled by CPU uploads.
pub struct QuadTarget {
    pub swapchain: xr::Swapchain<xr::Vulkan>,
    images: Vec<vk::Image>,
    pub width: u32,
    pub height: u32,
    /// True once an image has been released (the layer may be submitted).
    pub ready: bool,
}

pub struct Renderer {
    device: ash::Device,
    queue: vk::Queue,
    memory_types: vk::PhysicalDeviceMemoryProperties,
    color_format: vk::Format,
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    adjust: [f32; 4],
    /// Colour conversion for the current picture (without `adjust`).
    color_params: ColorParams,
    ui_staging: Option<Buffer>,
    readback: Option<Buffer>,
    dummy: Texture,
    video: Option<VideoTextures>,
    command_pool: vk::CommandPool,
    /// Two frames' resources, used in turn: the CPU records one frame while
    /// the GPU may still run the previous one.
    slots: Vec<Slot>,
    /// The slot being recorded.
    slot: usize,
    pub eyes: Vec<EyeTarget>,
    /// Time the last upload spent copying the picture into the staging buffer.
    pub copy_ms: f64,
    timestamp_ns: f64,
    gpu_ms: Cell<[f64; 3]>,
}

/// What one frame in flight needs to itself.
struct Slot {
    command: vk::CommandBuffer,
    fence: vk::Fence,
    /// GPU timestamps: frame start, after the upload, after each eye.
    queries: vk::QueryPool,
    /// A submission may still be running (its fence not yet waited for)…
    in_flight: Cell<bool>,
    /// …and it is a frame (with timestamps).
    frame_in_flight: Cell<bool>,
    staging: Option<Buffer>,
    /// Colour conversion uniform, and the descriptor set using it.
    color: Buffer,
    set: vk::DescriptorSet,
}

const SLOTS: usize = 2;

const TIMESTAMPS: u32 = 4;

fn find_memory(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    wanted: vk::MemoryPropertyFlags,
) -> anyhow::Result<u32> {
    (0..props.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(wanted)
        })
        .context("No suitable GPU memory type")
}

fn plane_format(layout: PlaneLayout, bits: u32, plane: usize) -> vk::Format {
    let wide = bits > 8;
    match (layout, plane, wide) {
        (_, 0, false) | (PlaneLayout::Planar, _, false) => vk::Format::R8_UNORM,
        (_, 0, true) | (PlaneLayout::Planar, _, true) => vk::Format::R16_UNORM,
        (_, _, false) => vk::Format::R8G8_UNORM,
        (_, _, true) => vk::Format::R16G16_UNORM,
    }
}

impl Buffer {
    fn null() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            mapped: std::ptr::null_mut(),
            size: 0,
            flags: vk::MemoryPropertyFlags::empty(),
        }
    }
}

impl Renderer {
    pub fn new(ctx: &XrContext) -> anyhow::Result<Self> {
        let device = ctx.device.clone();
        let memory_types = unsafe {
            ctx.vk
                .get_physical_device_memory_properties(ctx.physical_device)
        };
        let color_format = choose_color_format(&ctx.swapchain_formats()?)?;
        unsafe {
            let attachment = [vk::AttachmentDescription::default()
                .format(color_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let color_ref = [vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            let render_pass = device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpass),
                None,
            )?;

            let bindings = [
                (0, vk::DescriptorType::SAMPLER),
                (1, vk::DescriptorType::SAMPLED_IMAGE),
                (2, vk::DescriptorType::SAMPLED_IMAGE),
                (3, vk::DescriptorType::SAMPLED_IMAGE),
                (4, vk::DescriptorType::UNIFORM_BUFFER),
            ]
            .map(|(binding, ty)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(ty)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            });
            let set_layout = device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?;
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .size(size_of::<EyeParams>() as u32)];
            let set_layouts = [set_layout];
            let pipeline_layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )?;

            let code = ash::util::read_spv(&mut std::io::Cursor::new(SHADER))?;
            let module = device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(module)
                    .name(c"vs_main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(module)
                    .name(c"fs_main"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend =
                vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&blend)
                .dynamic_state(&dynamic)
                .layout(pipeline_layout)
                .render_pass(render_pass)];
            let pipeline = device
                .create_graphics_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
                .map_err(|(_, e)| e)?[0];
            device.destroy_shader_module(module, None);

            let pool_sizes = [
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLER)
                    .descriptor_count(SLOTS as u32),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(3 * SLOTS as u32),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(SLOTS as u32),
            ];
            let descriptor_pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(SLOTS as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            let sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .max_lod(0.0),
                None,
            )?;

            let command_pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(ctx.queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let commands = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .command_buffer_count(SLOTS as u32),
            )?;
            let sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&[set_layout; SLOTS]),
            )?;
            let mut slots = Vec::new();
            for (command, set) in commands.into_iter().zip(sets) {
                slots.push(Slot {
                    command,
                    fence: device.create_fence(&vk::FenceCreateInfo::default(), None)?,
                    queries: device.create_query_pool(
                        &vk::QueryPoolCreateInfo::default()
                            .query_type(vk::QueryType::TIMESTAMP)
                            .query_count(TIMESTAMPS),
                        None,
                    )?,
                    in_flight: Cell::new(false),
                    frame_in_flight: Cell::new(false),
                    staging: None,
                    color: Buffer::null(),
                    set,
                });
            }
            let limits = ctx
                .vk
                .get_physical_device_properties(ctx.physical_device)
                .limits;

            let mut renderer = Self {
                device,
                queue: ctx.queue,
                memory_types,
                color_format,
                render_pass,
                set_layout,
                pipeline_layout,
                pipeline,
                descriptor_pool,
                sampler,
                adjust: [0.0, 1.0, 1.0, 0.0],
                color_params: ColorParams::default(),
                ui_staging: None,
                readback: None,
                dummy: Texture {
                    image: vk::Image::null(),
                    memory: vk::DeviceMemory::null(),
                    view: vk::ImageView::null(),
                },
                video: None,
                command_pool,
                slots,
                slot: 0,
                eyes: Vec::new(),
                copy_ms: 0.0,
                timestamp_ns: limits.timestamp_period as f64,
                gpu_ms: Cell::new([0.0; 3]),
            };
            for i in 0..SLOTS {
                renderer.slots[i].color = renderer.create_buffer(
                    size_of::<ColorParams>() as u64,
                    vk::BufferUsageFlags::UNIFORM_BUFFER,
                )?;
            }
            renderer.dummy = renderer.create_texture(vk::Format::R8_UNORM, 1, 1)?;
            // Bring the placeholder texture into a sampleable layout once.
            renderer.begin_commands()?;
            renderer.transition(
                renderer.dummy.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            renderer.submit_and_wait()?;
            for (i, view) in ctx.views.iter().enumerate() {
                let eye = renderer
                    .create_eye(ctx, view)
                    .with_context(|| format!("Create swapchain {i}"))?;
                renderer.eyes.push(eye);
            }
            Ok(renderer)
        }
    }

    fn create_eye(
        &self,
        ctx: &XrContext,
        view: &xr::ViewConfigurationView,
    ) -> anyhow::Result<EyeTarget> {
        let (width, height) = (
            view.recommended_image_rect_width,
            view.recommended_image_rect_height,
        );
        let swapchain = ctx.session.create_swapchain(&xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::TRANSFER_SRC,
            format: self.color_format.as_raw() as u32,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        })?;
        let images: Vec<vk::Image> = swapchain
            .enumerate_images()?
            .into_iter()
            .map(vk::Handle::from_raw)
            .collect();
        let mut views = Vec::new();
        let mut framebuffers = Vec::new();
        for &image in &images {
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.color_format)
                        .subresource_range(color_range()),
                    None,
                )?
            };
            let attachments = [view];
            let framebuffer = unsafe {
                self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&attachments)
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )?
            };
            views.push(view);
            framebuffers.push(framebuffer);
        }
        Ok(EyeTarget {
            swapchain,
            images,
            views,
            framebuffers,
            width,
            height,
        })
    }

    fn create_buffer(&self, size: u64, usage: vk::BufferUsageFlags) -> anyhow::Result<Buffer> {
        unsafe {
            let buffer = self.device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                None,
            )?;
            let requirements = self.device.get_buffer_memory_requirements(buffer);
            let memory_type = find_memory(
                &self.memory_types,
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let memory = self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )?;
            self.device.bind_buffer_memory(buffer, memory, 0)?;
            let mapped = self
                .device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())?
                as *mut u8;
            Ok(Buffer {
                buffer,
                memory,
                mapped,
                size,
                flags: self.memory_types.memory_types[memory_type as usize].property_flags,
            })
        }
    }

    fn destroy_buffer(&self, buffer: Buffer) {
        unsafe {
            self.device.destroy_buffer(buffer.buffer, None);
            self.device.free_memory(buffer.memory, None);
        }
    }

    fn create_texture(
        &self,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Texture> {
        unsafe {
            let image = self.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST),
                None,
            )?;
            let requirements = self.device.get_image_memory_requirements(image);
            let memory = self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(find_memory(
                        &self.memory_types,
                        requirements.memory_type_bits,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )?),
                None,
            )?;
            self.device.bind_image_memory(image, memory, 0)?;
            let view = self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_range()),
                None,
            )?;
            Ok(Texture {
                image,
                memory,
                view,
            })
        }
    }

    fn destroy_texture(&self, texture: &Texture) {
        unsafe {
            self.device.destroy_image_view(texture.view, None);
            self.device.destroy_image(texture.image, None);
            self.device.free_memory(texture.memory, None);
        }
    }

    /// The command buffer being recorded.
    fn cmd(&self) -> vk::CommandBuffer {
        self.slots[self.slot].command
    }

    fn begin_commands(&self) -> anyhow::Result<()> {
        self.wait_slot(self.slot)?;
        unsafe {
            self.device
                .reset_command_buffer(self.cmd(), vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                self.cmd(),
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        Ok(())
    }

    fn submit_and_wait(&self) -> anyhow::Result<()> {
        let slot = &self.slots[self.slot];
        unsafe {
            self.device.end_command_buffer(slot.command)?;
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&[slot.command])],
                slot.fence,
            )?;
        }
        slot.in_flight.set(true);
        self.wait_slot(self.slot)
    }

    /// Waits for every submission to finish.
    fn wait_all(&self) -> anyhow::Result<()> {
        (0..SLOTS).try_for_each(|i| self.wait_slot(i))
    }

    /// Waits for slot `i`'s last submission (if any) to finish; after a
    /// frame, reads its GPU times.
    fn wait_slot(&self, i: usize) -> anyhow::Result<()> {
        let slot = &self.slots[i];
        if !slot.in_flight.get() {
            return Ok(());
        }
        unsafe {
            self.device.wait_for_fences(&[slot.fence], true, u64::MAX)?;
            self.device.reset_fences(&[slot.fence])?;
        }
        slot.in_flight.set(false);
        if slot.frame_in_flight.replace(false) {
            let mut stamps = [0u64; TIMESTAMPS as usize];
            // Every query was written in that submission, which has finished.
            let read = unsafe {
                self.device.get_query_pool_results(
                    slot.queries,
                    0,
                    &mut stamps,
                    vk::QueryResultFlags::TYPE_64,
                )
            };
            self.gpu_ms.set(match read {
                Ok(()) => std::array::from_fn(|i| {
                    stamps[i + 1].saturating_sub(stamps[i]) as f64 * self.timestamp_ns / 1e6
                }),
                Err(_) => [0.0; 3],
            });
        }
        Ok(())
    }

    fn transition(&self, image: vk::Image, from: vk::ImageLayout, to: vk::ImageLayout) {
        let (src_access, src_stage) = match from {
            vk::ImageLayout::TRANSFER_DST_OPTIMAL => (
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TRANSFER,
            ),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            ),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL => (
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::TRANSFER,
            ),
            // Previous contents are discarded; wait for earlier sampling to finish.
            _ => (
                vk::AccessFlags::empty(),
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            ),
        };
        let (dst_access, dst_stage) = match to {
            vk::ImageLayout::TRANSFER_DST_OPTIMAL => (
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TRANSFER,
            ),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL => (
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::TRANSFER,
            ),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            ),
            _ => (
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            ),
        };
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(from)
            .new_layout(to)
            .src_access_mask(src_access)
            .dst_access_mask(dst_access)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range());
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.cmd(),
                src_stage,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
    }

    /// (Re)creates plane textures for this frame's shape and rebinds them.
    fn ensure_video_textures(&mut self, frame: &Frame) -> anyhow::Result<()> {
        let key = VideoKey {
            layout: frame.layout(),
            bits: frame.bits(),
            width: frame.width(),
            height: frame.height(),
        };
        if self.video.as_ref().is_some_and(|v| v.key == key) {
            return Ok(());
        }
        // The textures (and descriptor sets) may be in use by the frame in flight.
        self.wait_all()?;
        if let Some(old) = self.video.take() {
            for (texture, ..) in &old.planes {
                self.destroy_texture(texture);
            }
        }
        let mut planes = Vec::new();
        for plane in 0..frame.plane_count() {
            let (w, h, _) = frame.plane_size(plane);
            let format = plane_format(key.layout, key.bits, plane);
            planes.push((self.create_texture(format, w, h)?, format, w, h));
        }
        let video = VideoTextures { key, planes };
        let sampler_info = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
        let image_info = |view: vk::ImageView| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        };
        let y = image_info(video.planes[0].0.view);
        let u = image_info(video.planes[1].0.view);
        let v = image_info(video.planes.get(2).map_or(self.dummy.view, |p| p.0.view));
        for slot in &self.slots {
            let color_info = [vk::DescriptorBufferInfo::default()
                .buffer(slot.color.buffer)
                .range(size_of::<ColorParams>() as u64)];
            let write = |binding: u32, ty: vk::DescriptorType| {
                vk::WriteDescriptorSet::default()
                    .dst_set(slot.set)
                    .dst_binding(binding)
                    .descriptor_type(ty)
            };
            let writes = [
                write(0, vk::DescriptorType::SAMPLER).image_info(&sampler_info),
                write(1, vk::DescriptorType::SAMPLED_IMAGE).image_info(&y),
                write(2, vk::DescriptorType::SAMPLED_IMAGE).image_info(&u),
                write(3, vk::DescriptorType::SAMPLED_IMAGE).image_info(&v),
                write(4, vk::DescriptorType::UNIFORM_BUFFER).buffer_info(&color_info),
            ];
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        }
        self.video = Some(video);
        Ok(())
    }

    /// Copies a decoded frame into the staging buffer and records its upload.
    /// Must be called between `begin_frame` and `draw_eye`.
    fn record_upload(&mut self, frame: &Frame) -> anyhow::Result<()> {
        self.ensure_video_textures(frame)?;
        let bytes = frame.bytes_per_sample();
        let total: u64 = (0..frame.plane_count())
            .map(|p| {
                let (w, h, c) = frame.plane_size(p);
                (w * h * c) as u64 * bytes as u64
            })
            .sum();
        if self.slots[self.slot]
            .staging
            .as_ref()
            .is_none_or(|s| s.size < total)
        {
            if let Some(old) = self.slots[self.slot].staging.take() {
                self.destroy_buffer(old);
            }
            let first = self.slots.iter().all(|s| s.staging.is_none());
            let buffer = self.create_buffer(total, vk::BufferUsageFlags::TRANSFER_SRC)?;
            if first {
                eprintln!(
                    "Renderer: video {}x{} ({} MB per picture), staging memory {:?}, eyes {}x{}",
                    frame.width(),
                    frame.height(),
                    total / 1_000_000,
                    buffer.flags,
                    self.eyes[0].width,
                    self.eyes[0].height,
                );
            }
            self.slots[self.slot].staging = Some(buffer);
        }
        let copy_started = std::time::Instant::now();
        let staging = self.slots[self.slot]
            .staging
            .as_ref()
            .expect("staging buffer");
        let mut regions = Vec::new();
        let mut offset = 0u64;
        for plane in 0..frame.plane_count() {
            regions.push(offset);
            offset += frame.rows(plane).map(|r| r.len() as u64).sum::<u64>();
        }
        // SAFETY: the staging buffer is mapped and holds `total` bytes, the
        // sum of all rows; the GPU isn't reading this slot's buffer (its fence passed).
        let dst = unsafe { std::slice::from_raw_parts_mut(staging.mapped, total as usize) };
        frame.copy_to(dst);
        self.copy_ms = copy_started.elapsed().as_secs_f64() * 1e3;
        self.color_params = color_params(frame);
        self.write_color();
        let video = self.video.as_ref().expect("video textures");
        for (plane, (texture, _, w, h)) in video.planes.iter().enumerate() {
            self.transition(
                texture.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(regions[plane])
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: *w,
                    height: *h,
                    depth: 1,
                });
            unsafe {
                self.device.cmd_copy_buffer_to_image(
                    self.cmd(),
                    staging.buffer,
                    texture.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            self.transition(
                texture.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
        }
        Ok(())
    }

    pub fn create_quad(
        &self,
        ctx: &XrContext,
        width: u32,
        height: u32,
    ) -> anyhow::Result<QuadTarget> {
        let swapchain = ctx.session.create_swapchain(&xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::TRANSFER_DST,
            format: self.color_format.as_raw() as u32,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        })?;
        let images = swapchain
            .enumerate_images()?
            .into_iter()
            .map(vk::Handle::from_raw)
            .collect();
        Ok(QuadTarget {
            swapchain,
            images,
            width,
            height,
            ready: false,
        })
    }

    /// Uploads sRGB-encoded RGBA pixels into the quad's next swapchain image.
    pub fn upload_quad(&mut self, target: &mut QuadTarget, rgba: &[u8]) -> anyhow::Result<()> {
        let size = target.width as u64 * target.height as u64 * 4;
        anyhow::ensure!(rgba.len() as u64 == size, "UI image size mismatch");
        if self.ui_staging.as_ref().is_none_or(|b| b.size < size) {
            if let Some(old) = self.ui_staging.take() {
                self.destroy_buffer(old);
            }
            self.ui_staging = Some(self.create_buffer(size, vk::BufferUsageFlags::TRANSFER_SRC)?);
        }
        let staging = self.ui_staging.as_ref().expect("ui staging");
        let dst = unsafe { std::slice::from_raw_parts_mut(staging.mapped, size as usize) };
        if self.color_format == vk::Format::B8G8R8A8_SRGB {
            for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        } else {
            dst.copy_from_slice(rgba);
        }
        let index = target.swapchain.acquire_image()?;
        target.swapchain.wait_image(xr::Duration::INFINITE)?;
        let image = target.images[index as usize];
        self.begin_commands()?;
        self.transition(
            image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: target.width,
                height: target.height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                self.cmd(),
                self.ui_staging.as_ref().expect("ui staging").buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
        }
        // OpenXR expects released color images in COLOR_ATTACHMENT_OPTIMAL.
        self.transition(
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        );
        self.submit_and_wait()?;
        target.swapchain.release_image()?;
        target.ready = true;
        Ok(())
    }

    /// This slot's colour uniform: the current picture's conversion and corrections.
    fn write_color(&self) {
        let color = ColorParams {
            adjust: self.adjust,
            ..self.color_params
        };
        // SAFETY: the mapped uniform holds a whole ColorParams, and the slot's
        // last frame has finished (`begin_commands` waited for it).
        unsafe {
            std::ptr::copy_nonoverlapping(
                &color,
                self.slots[self.slot].color.mapped as *mut ColorParams,
                1,
            )
        };
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Starts recording a frame; uploads `frame` first when it changed.
    pub fn begin_frame(&mut self, upload: Option<&Frame>) -> anyhow::Result<()> {
        self.begin_commands()?;
        let queries = self.slots[self.slot].queries;
        unsafe {
            self.device
                .cmd_reset_query_pool(self.cmd(), queries, 0, TIMESTAMPS);
            self.device.cmd_write_timestamp(
                self.cmd(),
                vk::PipelineStageFlags::TOP_OF_PIPE,
                queries,
                0,
            );
        }
        // Each slot has its own copy; corrections change without a new
        // frame (e.g. while paused).
        self.write_color();
        if let Some(frame) = upload {
            self.record_upload(frame)?;
        }
        self.timestamp(1);
        Ok(())
    }

    /// Picture corrections for the video from now on.
    pub fn set_adjust(&mut self, image: &crate::config::ImageAdjust) {
        self.adjust = [
            image.brightness,
            image.contrast,
            image.saturation,
            image.rotation as f32,
        ];
    }

    /// Luma texture size, for the shader's half-texel clamp.
    pub fn video_size(&self) -> (u32, u32) {
        self.video
            .as_ref()
            .map_or((1, 1), |v| (v.key.width, v.key.height))
    }

    /// Draws one eye; clears to black when `show_video` is false.
    pub fn draw_eye(&self, eye: usize, image_index: u32, params: &EyeParams, show_video: bool) {
        let target = &self.eyes[eye];
        let extent = vk::Extent2D {
            width: target.width,
            height: target.height,
        };
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd(),
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(target.framebuffers[image_index as usize])
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_set_viewport(
                self.cmd(),
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                self.cmd(),
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                }],
            );
            self.device.cmd_bind_pipeline(
                self.cmd(),
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
            if show_video && self.video.is_some() {
                self.device.cmd_bind_descriptor_sets(
                    self.cmd(),
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[self.slots[self.slot].set],
                    &[],
                );
                let bytes = std::slice::from_raw_parts(
                    params as *const EyeParams as *const u8,
                    size_of::<EyeParams>(),
                );
                self.device.cmd_push_constants(
                    self.cmd(),
                    self.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytes,
                );
                self.device.cmd_draw(self.cmd(), 3, 1, 0, 0);
            } else {
                // Nothing decoded yet: clear to black.
                self.device.cmd_clear_attachments(
                    self.cmd(),
                    &[vk::ClearAttachment {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        color_attachment: 0,
                        clear_value: vk::ClearValue::default(),
                    }],
                    &[vk::ClearRect {
                        rect: vk::Rect2D {
                            offset: vk::Offset2D::default(),
                            extent,
                        },
                        base_array_layer: 0,
                        layer_count: 1,
                    }],
                );
            }
            self.device.cmd_end_render_pass(self.cmd());
        }
        self.timestamp(2 + eye as u32);
    }

    /// Records a copy of an eye image for [`Renderer::take_screenshot`].
    pub fn record_readback(&mut self, eye: usize, image_index: u32) -> anyhow::Result<()> {
        let (width, height) = (self.eyes[eye].width, self.eyes[eye].height);
        let size = width as u64 * height as u64 * 4;
        if self.readback.as_ref().is_none_or(|b| b.size < size) {
            if let Some(old) = self.readback.take() {
                self.destroy_buffer(old);
            }
            self.readback = Some(self.create_buffer(size, vk::BufferUsageFlags::TRANSFER_DST)?);
        }
        let image = self.eyes[eye].images[image_index as usize];
        self.transition(
            image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.cmd(),
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback.as_ref().expect("readback").buffer,
                &[region],
            );
        }
        self.transition(
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        );
        Ok(())
    }

    /// Writes the last recorded readback as PNG (after `end_frame`).
    pub fn take_screenshot(&self, eye: usize, path: &std::path::Path) -> anyhow::Result<()> {
        let Some(buffer) = &self.readback else {
            bail!("No readback recorded");
        };
        self.wait_all()?;
        let (width, height) = (self.eyes[eye].width, self.eyes[eye].height);
        let mut pixels =
            unsafe { std::slice::from_raw_parts(buffer.mapped, (width * height * 4) as usize) }
                .to_vec();
        if self.color_format == vk::Format::B8G8R8A8_SRGB {
            for px in pixels.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.write_header()?.write_image_data(&pixels)?;
        Ok(())
    }

    /// Submits the recorded frame without waiting for it: OpenXR waits for
    /// the queue before it uses the swapchain images, and the next frame
    /// waits before reusing the command buffer and staging memory. (Waiting
    /// here cost most of a display period: the GPU runs our 2–3 ms of work
    /// only after the compositor's.)
    pub fn end_frame(&mut self) -> anyhow::Result<()> {
        let slot = &self.slots[self.slot];
        unsafe {
            self.device.end_command_buffer(slot.command)?;
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&[slot.command])],
                slot.fence,
            )?;
        }
        slot.in_flight.set(true);
        slot.frame_in_flight.set(true);
        // The next frame records into the other slot, while this one runs.
        self.slot = (self.slot + 1) % SLOTS;
        Ok(())
    }

    /// GPU time of the last finished frame: upload, then each eye (ms).
    pub fn gpu_ms(&self) -> [f64; 3] {
        self.gpu_ms.get()
    }

    /// Records GPU timestamp `index` once the work so far is done.
    fn timestamp(&self, index: u32) {
        unsafe {
            self.device.cmd_write_timestamp(
                self.cmd(),
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.slots[self.slot].queries,
                index,
            );
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(video) = self.video.take() {
                for (texture, ..) in &video.planes {
                    self.destroy_texture(texture);
                }
            }
            self.destroy_texture(&self.dummy);
            let mut buffers = vec![self.readback.take(), self.ui_staging.take()];
            for slot in &mut self.slots {
                buffers.push(slot.staging.take());
                buffers.push(Some(std::mem::replace(&mut slot.color, Buffer::null())));
            }
            for buffer in buffers.into_iter().flatten() {
                self.destroy_buffer(buffer);
            }
            for eye in &self.eyes {
                for &fb in &eye.framebuffers {
                    self.device.destroy_framebuffer(fb, None);
                }
                for &view in &eye.views {
                    self.device.destroy_image_view(view, None);
                }
            }
            for slot in &self.slots {
                self.device.destroy_fence(slot.fence, None);
                self.device.destroy_query_pool(slot.queries, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// YUV→R'G'B' for this frame as affine rows over normalized samples.
fn color_params(frame: &Frame) -> ColorParams {
    let (kr, kb) = match frame.matrix() {
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let m = [
        [1.0, 0.0, 2.0 * (1.0 - kr)],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0],
    ];
    let bits = frame.bits();
    let max = ((1u32 << bits) - 1) as f32;
    let k = (1u32 << (bits - 8)) as f32;
    // Texture samples → code / max, per storage format.
    let sample_scale = match (frame.layout(), bits > 8) {
        (_, false) => 1.0,
        (PlaneLayout::Planar, true) => 65535.0 / max,
        (_, true) => 65535.0 / (max * 64.0),
    };
    // Normalized code n: Y' = n*sy + by, C = n*sc + bc.
    let (sy, by, sc, bc) = if frame.full_range() {
        (1.0, 0.0, 1.0, -128.0 * k / max)
    } else {
        (
            max / (219.0 * k),
            -16.0 / 219.0,
            max / (224.0 * k),
            -128.0 / 224.0,
        )
    };
    let scale = [sy, sc, sc];
    let bias = [by, bc, bc];
    let mut rows = [[0.0f32; 4]; 3];
    for (row, coeffs) in rows.iter_mut().zip(m) {
        for j in 0..3 {
            row[j] = coeffs[j] * scale[j];
        }
        row[3] = (0..3).map(|j| coeffs[j] * bias[j]).sum();
    }
    let transfer = match frame.transfer() {
        Transfer::Sdr => 0.0,
        Transfer::Pq => 1.0,
        Transfer::Hlg => 2.0,
    };
    ColorParams {
        rows,
        params: [
            sample_scale,
            if frame.layout() == PlaneLayout::Planar {
                0.0
            } else {
                1.0
            },
            if frame.matrix() == Matrix::Bt2020 {
                1.0
            } else {
                0.0
            },
            transfer,
        ],
        adjust: [0.0, 1.0, 1.0, 0.0],
    }
}

// Copyright (c) 2021 Okko Hakola
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};

use ahash::AHashMap;
use egui::{
    epaint::{Mesh, Primitive},
    ClippedPrimitive, PaintCallbackInfo, Rect, TexturesDelta,
};
use vulkano::{
    buffer::{
        allocator::{SubbufferAllocator, SubbufferAllocatorCreateInfo},
        Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer,
    },
    command_buffer::{
        allocator::StandardCommandBufferAllocator, AutoCommandBufferBuilder, BlitImageInfo,
        CommandBufferInheritanceInfo, CommandBufferUsage, CopyBufferToImageInfo, ImageBlit,
        PrimaryAutoCommandBuffer, PrimaryCommandBufferAbstract, RenderPassBeginInfo,
        SecondaryAutoCommandBuffer, SubpassBeginInfo, SubpassContents,
    },
    descriptor_set::{
        allocator::StandardDescriptorSetAllocator, layout::DescriptorSetLayout,
        PersistentDescriptorSet, WriteDescriptorSet,
    },
    device::Queue,
    format::{Format, NumericFormat},
    image::{
        sampler::{Filter, Sampler, SamplerAddressMode, SamplerCreateInfo, SamplerMipmapMode},
        view::ImageView,
        Image, ImageCreateInfo, ImageLayout, ImageType, ImageUsage, SampleCount,
    },
    memory::allocator::{AllocationCreateInfo, MemoryTypeFilter, StandardMemoryAllocator},
    pipeline::{
        graphics::{
            color_blend::{
                AttachmentBlend, BlendFactor, ColorBlendAttachmentState, ColorBlendState,
            },
            input_assembly::InputAssemblyState,
            multisample::MultisampleState,
            rasterization::RasterizationState,
            vertex_input::{Vertex, VertexDefinition},
            viewport::{Scissor, Viewport, ViewportState},
            GraphicsPipelineCreateInfo,
        },
        layout::PipelineDescriptorSetLayoutCreateInfo,
        DynamicState, GraphicsPipeline, Pipeline, PipelineBindPoint, PipelineLayout,
        PipelineShaderStageCreateInfo,
    },
    render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass},
    sync::GpuFuture,
    DeviceSize,
};

use crate::utils::Allocators;

const VERTICES_PER_QUAD: DeviceSize = 4;
const VERTEX_BUFFER_SIZE: DeviceSize = 1024 * 1024 * VERTICES_PER_QUAD;
const INDEX_BUFFER_SIZE: DeviceSize = 1024 * 1024 * 2;

/// Should match vertex definition of egui
#[repr(C)]
#[derive(BufferContents, Vertex)]
pub struct EguiVertex {
    #[format(R32G32_SFLOAT)]
    pub position: [f32; 2],
    #[format(R32G32_SFLOAT)]
    pub tex_coords: [f32; 2],
    #[format(R8G8B8A8_UNORM)]
    pub color: [u8; 4],
}

pub struct Renderer {
    gfx_queue: Arc<Queue>,
    render_pass: Option<Arc<RenderPass>>,
    is_overlay: bool,
    output_in_linear_colorspace: bool,

    #[allow(unused)]
    format: vulkano::format::Format,
    font_sampler: Arc<Sampler>,

    allocators: Allocators,
    vertex_buffer_pool: SubbufferAllocator,
    index_buffer_pool: SubbufferAllocator,
    pipeline: Arc<GraphicsPipeline>,
    subpass: Subpass,

    texture_desc_sets: AHashMap<egui::TextureId, Arc<PersistentDescriptorSet>>,
    texture_images: AHashMap<egui::TextureId, Arc<ImageView>>,
    next_native_tex_id: u64,
}

impl Renderer {
    pub fn new_with_subpass(
        gfx_queue: Arc<Queue>,
        final_output_format: Format,
        subpass: Subpass,
    ) -> Renderer {
        Self::new_internal(gfx_queue, final_output_format, subpass, None, false)
    }

    /// Creates a new [Renderer] which is responsible for rendering egui with its own renderpass
    /// See examples
    pub fn new_with_render_pass(
        gfx_queue: Arc<Queue>,
        final_output_format: Format,
        is_overlay: bool,
        samples: SampleCount,
    ) -> Renderer {
        // Create Gui render pass with just depth and final color
        let render_pass = if is_overlay {
            vulkano::single_pass_renderpass!(gfx_queue.device().clone(),
                attachments: {
                    final_color: {
                        format: final_output_format,
                        samples: samples,
                        load_op: Load,
                        store_op: Store,
                    }
                },
                pass: {
                        color: [final_color],
                        depth_stencil: {}
                }
            )
            .unwrap()
        } else {
            vulkano::single_pass_renderpass!(gfx_queue.device().clone(),
                attachments: {
                    final_color: {
                        format: final_output_format,
                        samples: samples,
                        load_op: Clear,
                        store_op: Store,
                    }
                },
                pass: {
                        color: [final_color],
                        depth_stencil: {}
                }
            )
            .unwrap()
        };
        let subpass = Subpass::from(render_pass.clone(), 0).unwrap();
        Self::new_internal(gfx_queue, final_output_format, subpass, Some(render_pass), is_overlay)
    }

    fn new_internal(
        gfx_queue: Arc<Queue>,
        final_output_format: Format,
        subpass: Subpass,
        render_pass: Option<Arc<RenderPass>>,
        is_overlay: bool,
    ) -> Renderer {
        let output_in_linear_colorspace =
            // final_output_format.type_color().unwrap() == NumericType::SRGB;
            final_output_format.numeric_format_color().unwrap() == NumericFormat::SRGB;
        let allocators = Allocators::new_default(gfx_queue.device());
        let (vertex_buffer_pool, index_buffer_pool) = Self::create_buffers(&allocators.memory);
        let pipeline = Self::create_pipeline(gfx_queue.clone(), subpass.clone());
        let font_sampler = Sampler::new(gfx_queue.device().clone(), SamplerCreateInfo {
            mag_filter: Filter::Linear,
            min_filter: Filter::Linear,
            address_mode: [SamplerAddressMode::ClampToEdge; 3],
            mipmap_mode: SamplerMipmapMode::Linear,
            ..Default::default()
        })
        .unwrap();
        Renderer {
            gfx_queue,
            format: final_output_format,
            render_pass,
            vertex_buffer_pool,
            index_buffer_pool,
            pipeline,
            subpass,
            texture_desc_sets: AHashMap::default(),
            texture_images: AHashMap::default(),
            next_native_tex_id: 0,
            is_overlay,
            output_in_linear_colorspace,
            font_sampler,
            allocators,
        }
    }

    pub fn has_renderpass(&self) -> bool {
        self.render_pass.is_some()
    }

    fn create_buffers(
        allocator: &Arc<StandardMemoryAllocator>,
    ) -> (SubbufferAllocator, SubbufferAllocator) {
        // Create vertex and index buffers
        let vertex_buffer_pool =
            SubbufferAllocator::new(allocator.clone(), SubbufferAllocatorCreateInfo {
                arena_size: VERTEX_BUFFER_SIZE,
                buffer_usage: BufferUsage::VERTEX_BUFFER,
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                    | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            });

        let index_buffer_pool =
            SubbufferAllocator::new(allocator.clone(), SubbufferAllocatorCreateInfo {
                arena_size: INDEX_BUFFER_SIZE,
                buffer_usage: BufferUsage::INDEX_BUFFER,
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                    | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            });

        (vertex_buffer_pool, index_buffer_pool)
    }

    fn create_pipeline(gfx_queue: Arc<Queue>, subpass: Subpass) -> Arc<GraphicsPipeline> {
        let vs = vs::load(gfx_queue.device().clone())
            .expect("failed to create shader module")
            .entry_point("main")
            .unwrap();
        let fs = fs::load(gfx_queue.device().clone())
            .expect("failed to create shader module")
            .entry_point("main")
            .unwrap();

        let mut blend = AttachmentBlend::alpha();
        blend.src_color_blend_factor = BlendFactor::One;
        blend.src_alpha_blend_factor = BlendFactor::OneMinusDstAlpha;
        blend.dst_alpha_blend_factor = BlendFactor::One;
        let blend_state = ColorBlendState {
            attachments: vec![ColorBlendAttachmentState {
                blend: Some(blend),
                ..Default::default()
            }],
            ..ColorBlendState::default()
        };

        let vertex_input_state =
            Some(EguiVertex::per_vertex().definition(&vs.info().input_interface).unwrap());

        let stages =
            [PipelineShaderStageCreateInfo::new(vs), PipelineShaderStageCreateInfo::new(fs)];

        let layout = PipelineLayout::new(
            gfx_queue.device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(gfx_queue.device().clone())
                .unwrap(),
        )
        .unwrap();

        GraphicsPipeline::new(gfx_queue.device().clone(), None, GraphicsPipelineCreateInfo {
            stages: stages.into_iter().collect(),
            vertex_input_state,
            input_assembly_state: Some(InputAssemblyState::default()),
            viewport_state: Some(ViewportState::default()),
            rasterization_state: Some(RasterizationState::default()),
            multisample_state: Some(MultisampleState {
                rasterization_samples: subpass.num_samples().unwrap_or(SampleCount::Sample1),
                ..Default::default()
            }),
            color_blend_state: Some(blend_state),
            dynamic_state: [DynamicState::Viewport, DynamicState::Scissor].into_iter().collect(),
            subpass: Some(subpass.into()),
            ..GraphicsPipelineCreateInfo::layout(layout)
        })
        .unwrap()
    }

    /// Creates a descriptor set for images
    fn sampled_image_desc_set(
        &self,
        layout: &Arc<DescriptorSetLayout>,
        image: Arc<ImageView>,
        sampler: Arc<Sampler>,
    ) -> Arc<PersistentDescriptorSet> {
        PersistentDescriptorSet::new(
            &self.allocators.descriptor_set,
            layout.clone(),
            [WriteDescriptorSet::image_view_sampler(0, image, sampler)],
            [],
        )
        .unwrap()
    }

    /// Registers a user texture. User texture needs to be unregistered when it is no longer needed
    pub fn register_image(
        &mut self,
        image: Arc<ImageView>,
        sampler_create_info: SamplerCreateInfo,
    ) -> egui::TextureId {
        let layout = self.pipeline.layout().set_layouts().first().unwrap();
        let sampler = Sampler::new(self.gfx_queue.device().clone(), sampler_create_info).unwrap();
        let desc_set = self.sampled_image_desc_set(layout, image.clone(), sampler);
        let id = egui::TextureId::User(self.next_native_tex_id);
        self.next_native_tex_id += 1;
        self.texture_desc_sets.insert(id, desc_set);
        self.texture_images.insert(id, image);
        id
    }

    /// Unregister user texture.
    pub fn unregister_image(&mut self, texture_id: egui::TextureId) {
        self.texture_desc_sets.remove(&texture_id);
        self.texture_images.remove(&texture_id);
    }

    fn update_texture(&mut self, texture_id: egui::TextureId, delta: &egui::epaint::ImageDelta) {
        // Extract pixel data from egui
        let data: Vec<u8> = match &delta.image {
            egui::ImageData::Color(image) => {
                assert_eq!(
                    image.width() * image.height(),
                    image.pixels.len(),
                    "Mismatch between texture size and texel count"
                );
                image.pixels.iter().flat_map(|color| color.to_array()).collect()
            }
            egui::ImageData::Font(image) => {
                image.srgba_pixels(None).flat_map(|color| color.to_array()).collect()
            }
        };
        // Create buffer to be copied to the image
        let texture_data_buffer = Buffer::from_iter(
            self.allocators.memory.clone(),
            BufferCreateInfo { usage: BufferUsage::TRANSFER_SRC, ..Default::default() },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                    | MemoryTypeFilter::HOST_RANDOM_ACCESS,
                ..Default::default()
            },
            data,
        )
        .unwrap();
        // Create image
        let img = {
            let extent = [delta.image.width() as u32, delta.image.height() as u32, 1];
            Image::new(
                self.allocators.memory.clone(),
                ImageCreateInfo {
                    image_type: ImageType::Dim2d,
                    format: Format::R8G8B8A8_SRGB,
                    extent,
                    usage: ImageUsage::TRANSFER_DST
                        | ImageUsage::TRANSFER_SRC
                        | ImageUsage::SAMPLED,
                    // initial_layout: ImageLayout::ShaderReadOnlyOptimal,
                    // TODO(aedm): miez
                    initial_layout: ImageLayout::Undefined,
                    ..Default::default()
                },
                AllocationCreateInfo::default(),
            )
            .unwrap()
        };

        // Create command buffer builder
        let mut cbb = AutoCommandBufferBuilder::primary(
            &self.allocators.command_buffer,
            self.gfx_queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();

        // Copy buffer to image
        cbb.copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(
            texture_data_buffer,
            img.clone(),
        ))
        .unwrap();

        let font_image = ImageView::new_default(img).unwrap();

        // Blit texture data to existing image if delta pos exists (e.g. font changed)
        if let Some(pos) = delta.pos {
            if let Some(existing_image) = self.texture_images.get(&texture_id) {
                let src_dims = font_image.image().extent();
                let top_left = [pos[0] as u32, pos[1] as u32, 0];
                let bottom_right = [pos[0] as u32 + src_dims[0], pos[1] as u32 + src_dims[1], 1];

                cbb.blit_image(BlitImageInfo {
                    src_image_layout: ImageLayout::General,
                    dst_image_layout: ImageLayout::General,
                    regions: [ImageBlit {
                        src_subresource: font_image.image().subresource_layers(),
                        src_offsets: [[0, 0, 0], src_dims],
                        dst_subresource: existing_image.image().subresource_layers(),
                        dst_offsets: [top_left, bottom_right],
                        ..Default::default()
                    }]
                    .into(),
                    filter: Filter::Nearest,
                    ..BlitImageInfo::images(
                        font_image.image().clone(),
                        existing_image.image().clone(),
                    )
                })
                .unwrap();
            }
            // Otherwise save the newly created image
        } else {
            let layout = self.pipeline.layout().set_layouts().first().unwrap();
            let font_desc_set =
                self.sampled_image_desc_set(layout, font_image.clone(), self.font_sampler.clone());
            self.texture_desc_sets.insert(texture_id, font_desc_set);
            self.texture_images.insert(texture_id, font_image);
        }
        // Execute command buffer
        let command_buffer = cbb.build().unwrap();
        let finished = command_buffer.execute(self.gfx_queue.clone()).unwrap();
        let _fut = finished.then_signal_fence_and_flush().unwrap();
    }

    fn get_rect_scissor(
        &self,
        scale_factor: f32,
        framebuffer_dimensions: [u32; 2],
        rect: Rect,
    ) -> Scissor {
        let min = rect.min;
        let min = egui::Pos2 { x: min.x * scale_factor, y: min.y * scale_factor };
        let min = egui::Pos2 {
            x: min.x.clamp(0.0, framebuffer_dimensions[0] as f32),
            y: min.y.clamp(0.0, framebuffer_dimensions[1] as f32),
        };
        let max = rect.max;
        let max = egui::Pos2 { x: max.x * scale_factor, y: max.y * scale_factor };
        let max = egui::Pos2 {
            x: max.x.clamp(min.x, framebuffer_dimensions[0] as f32),
            y: max.y.clamp(min.y, framebuffer_dimensions[1] as f32),
        };
        Scissor {
            offset: [min.x.round() as u32, min.y.round() as u32],
            extent: [(max.x.round() - min.x) as u32, (max.y.round() - min.y) as u32],
        }
    }

    fn create_subbuffers(&self, mesh: &Mesh) -> (Subbuffer<[EguiVertex]>, Subbuffer<[u32]>) {
        // Copy vertices to buffer
        let v_slice = &mesh.vertices;

        let vertex_chunk =
            self.vertex_buffer_pool.allocate_slice::<EguiVertex>(v_slice.len() as u64).unwrap();
        {
            let mut vertex_write = vertex_chunk.write().unwrap();
            for (i, v) in v_slice.iter().enumerate() {
                vertex_write[i] = EguiVertex {
                    position: [v.pos.x, v.pos.y],
                    tex_coords: [v.uv.x, v.uv.y],
                    color: v.color.to_array(),
                };
            }
        }

        // Copy indices to buffer
        let i_slice = &mesh.indices;
        let index_chunk =
            self.index_buffer_pool.allocate_slice::<u32>(i_slice.len() as u64).unwrap();
        {
            let mut index_write = index_chunk.write().unwrap();
            for (i, v) in i_slice.iter().enumerate() {
                index_write[i] = *v;
            }
        }

        (vertex_chunk, index_chunk)
    }

    fn create_secondary_command_buffer_builder(
        &self,
    ) -> AutoCommandBufferBuilder<SecondaryAutoCommandBuffer> {
        AutoCommandBufferBuilder::secondary(
            &self.allocators.command_buffer,
            self.gfx_queue.queue_family_index(),
            CommandBufferUsage::MultipleSubmit,
            CommandBufferInheritanceInfo {
                render_pass: Some(self.subpass.clone().into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    // Starts the rendering pipeline and returns [`AutoCommandBufferBuilder`] for drawing
    fn start(
        &mut self,
        final_image: Arc<ImageView>,
    ) -> (AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, [u32; 2]) {
        // Get dimensions
        let img_dims = final_image.image().extent();
        // Create framebuffer (must be in same order as render pass description in `new`
        let framebuffer = Framebuffer::new(
            self.render_pass
                .as_ref()
                .expect(
                    "No renderpass on this renderer (created with subpass), use 'draw_subpass' \
                     instead",
                )
                .clone(),
            FramebufferCreateInfo { attachments: vec![final_image], ..Default::default() },
        )
        .unwrap();
        let mut command_buffer_builder = AutoCommandBufferBuilder::primary(
            &self.allocators.command_buffer,
            self.gfx_queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();
        // Add clear values here for attachments and begin render pass
        command_buffer_builder
            .begin_render_pass(
                RenderPassBeginInfo {
                    clear_values: vec![if !self.is_overlay { Some([0.0; 4].into()) } else { None }],
                    ..RenderPassBeginInfo::framebuffer(framebuffer)
                },
                SubpassBeginInfo {
                    contents: SubpassContents::SecondaryCommandBuffers,
                    ..SubpassBeginInfo::default()
                },
            )
            .unwrap();
        (command_buffer_builder, [img_dims[0], img_dims[1]])
    }

    /// Executes our draw commands on the final image and returns a `GpuFuture` to wait on
    pub fn draw_on_image<F>(
        &mut self,
        clipped_meshes: &[ClippedPrimitive],
        textures_delta: &TexturesDelta,
        scale_factor: f32,
        before_future: F,
        final_image: Arc<ImageView>,
    ) -> Box<dyn GpuFuture>
    where
        F: GpuFuture + 'static,
    {
        for (id, image_delta) in &textures_delta.set {
            self.update_texture(*id, image_delta);
        }

        let (mut command_buffer_builder, framebuffer_dimensions) = self.start(final_image);
        let mut builder = self.create_secondary_command_buffer_builder();
        self.draw_egui(scale_factor, clipped_meshes, framebuffer_dimensions, &mut builder);
        // Execute draw commands
        let command_buffer = builder.build().unwrap();
        command_buffer_builder.execute_commands(command_buffer).unwrap();
        let done_future = self.finish(command_buffer_builder, Box::new(before_future));

        for &id in &textures_delta.free {
            self.unregister_image(id);
        }

        done_future
    }

    // Finishes the rendering pipeline
    fn finish(
        &self,
        mut command_buffer_builder: AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        before_main_cb_future: Box<dyn GpuFuture>,
    ) -> Box<dyn GpuFuture> {
        // We end render pass
        command_buffer_builder.end_render_pass(Default::default()).unwrap();
        // Then execute our whole command buffer
        let command_buffer = command_buffer_builder.build().unwrap();
        let after_main_cb =
            before_main_cb_future.then_execute(self.gfx_queue.clone(), command_buffer).unwrap();
        let future =
            after_main_cb.then_signal_fence_and_flush().expect("Failed to signal fence and flush");
        // Return our future
        Box::new(future)
    }

    pub fn draw_on_subpass_image(
        &mut self,
        clipped_meshes: &[ClippedPrimitive],
        textures_delta: &TexturesDelta,
        scale_factor: f32,
        framebuffer_dimensions: [u32; 2],
    ) -> Arc<SecondaryAutoCommandBuffer> {
        for (id, image_delta) in &textures_delta.set {
            self.update_texture(*id, image_delta);
        }
        let mut builder = self.create_secondary_command_buffer_builder();
        self.draw_egui(scale_factor, clipped_meshes, framebuffer_dimensions, &mut builder);
        let buffer = builder.build().unwrap();
        for &id in &textures_delta.free {
            self.unregister_image(id);
        }
        buffer
    }

    fn draw_egui(
        &mut self,
        scale_factor: f32,
        clipped_meshes: &[ClippedPrimitive],
        framebuffer_dimensions: [u32; 2],
        builder: &mut AutoCommandBufferBuilder<SecondaryAutoCommandBuffer>,
    ) {
        let push_constants = vs::PushConstants {
            screen_size: [
                framebuffer_dimensions[0] as f32 / scale_factor,
                framebuffer_dimensions[1] as f32 / scale_factor,
            ],
            output_in_linear_colorspace: self.output_in_linear_colorspace.into(),
        };

        for ClippedPrimitive { clip_rect, primitive } in clipped_meshes {
            match primitive {
                Primitive::Mesh(mesh) => {
                    // Nothing to draw if we don't have vertices & indices
                    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                        continue;
                    }
                    if self.texture_desc_sets.get(&mesh.texture_id).is_none() {
                        eprintln!("This texture no longer exists {:?}", mesh.texture_id);
                        continue;
                    }
                    let (vertices, indices) = self.create_subbuffers(mesh);
                    let desc_set = self.texture_desc_sets.get(&mesh.texture_id).unwrap();

                    builder
                        .bind_pipeline_graphics(self.pipeline.clone())
                        .unwrap()
                        .set_viewport(
                            0,
                            [Viewport {
                                offset: [0.0, 0.0],
                                extent: [
                                    framebuffer_dimensions[0] as f32,
                                    framebuffer_dimensions[1] as f32,
                                ],
                                depth_range: 0.0..=1.0,
                            }]
                            .into_iter()
                            .collect(),
                        )
                        .unwrap()
                        .set_scissor(
                            0,
                            [self.get_rect_scissor(
                                scale_factor,
                                framebuffer_dimensions,
                                *clip_rect,
                            )]
                            .into_iter()
                            .collect(),
                        )
                        .unwrap()
                        .bind_descriptor_sets(
                            PipelineBindPoint::Graphics,
                            self.pipeline.layout().clone(),
                            0,
                            desc_set.clone(),
                        )
                        .unwrap()
                        .push_constants(self.pipeline.layout().clone(), 0, push_constants)
                        .unwrap()
                        .bind_vertex_buffers(0, vertices.clone())
                        .unwrap()
                        .bind_index_buffer(indices.clone())
                        .unwrap()
                        .draw_indexed(indices.len() as u32, 1, 0, 0, 0)
                        .unwrap();
                }
                Primitive::Callback(callback) => {
                    if callback.rect.is_positive() {
                        let rect_min_x = scale_factor * callback.rect.min.x;
                        let rect_min_y = scale_factor * callback.rect.min.y;
                        let rect_max_x = scale_factor * callback.rect.max.x;
                        let rect_max_y = scale_factor * callback.rect.max.y;

                        let rect_min_x = rect_min_x.round();
                        let rect_min_y = rect_min_y.round();
                        let rect_max_x = rect_max_x.round();
                        let rect_max_y = rect_max_y.round();

                        builder
                            .set_viewport(
                                0,
                                [Viewport {
                                    offset: [rect_min_x, rect_min_y],
                                    extent: [rect_max_x - rect_min_x, rect_max_y - rect_min_y],
                                    depth_range: 0.0..=1.0,
                                }]
                                .into_iter()
                                .collect(),
                            )
                            .unwrap()
                            .set_scissor(
                                0,
                                [self.get_rect_scissor(
                                    scale_factor,
                                    framebuffer_dimensions,
                                    *clip_rect,
                                )]
                                .into_iter()
                                .collect(),
                            )
                            .unwrap();

                        let info = egui::PaintCallbackInfo {
                            viewport: callback.rect,
                            clip_rect: *clip_rect,
                            pixels_per_point: scale_factor,
                            screen_size_px: framebuffer_dimensions,
                        };

                        if let Some(callback) = callback.callback.downcast_ref::<CallbackFn>() {
                            (callback.f)(info, &mut CallbackContext {
                                builder,
                                resources: self.render_resources(),
                            });
                        } else {
                            println!(
                                "Warning: Unsupported render callback. Expected \
                                 egui_winit_vulkano::CallbackFn"
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn render_resources(&self) -> RenderResources {
        RenderResources {
            queue: self.queue(),
            subpass: self.subpass.clone(),
            memory_allocator: self.allocators.memory.clone(),
            descriptor_set_allocator: &self.allocators.descriptor_set,
            command_buffer_allocator: &self.allocators.command_buffer,
        }
    }

    pub fn queue(&self) -> Arc<Queue> {
        self.gfx_queue.clone()
    }

    pub fn allocators(&self) -> &Allocators {
        &self.allocators
    }
}

/// A set of objects used to perform custom rendering in a `PaintCallback`. It
/// includes [`RenderResources`] for constructing a subpass pipeline and a secondary
/// command buffer for pushing render commands onto it.
///
/// # Example
///
/// See the `triangle` demo source for a detailed usage example.
pub struct CallbackContext<'a> {
    pub builder: &'a mut AutoCommandBufferBuilder<SecondaryAutoCommandBuffer>,
    pub resources: RenderResources<'a>,
}

/// A set of resources used to construct the render pipeline. These can be reused
/// to create additional pipelines and buffers to be rendered in a `PaintCallback`.
///
/// # Example
///
/// See the `triangle` demo source for a detailed usage example.
#[derive(Clone)]
pub struct RenderResources<'a> {
    pub memory_allocator: Arc<StandardMemoryAllocator>,
    pub descriptor_set_allocator: &'a StandardDescriptorSetAllocator,
    pub command_buffer_allocator: &'a StandardCommandBufferAllocator,
    pub queue: Arc<Queue>,
    pub subpass: Subpass,
}

pub type CallbackFnDef = dyn Fn(PaintCallbackInfo, &mut CallbackContext) + Sync + Send;

/// A callback function that can be used to compose an [`epaint::PaintCallback`] for
/// custom rendering with [`vulkano`].
///
/// The callback is passed an [`egui::PaintCallbackInfo`] and a [`CallbackContext`] which
/// can be used to construct Vulkano graphics pipelines and buffers.
///
/// # Example
///
/// See the `triangle` demo source for a detailed usage example.
pub struct CallbackFn {
    pub(crate) f: Box<CallbackFnDef>,
}

impl CallbackFn {
    pub fn new<F: Fn(PaintCallbackInfo, &mut CallbackContext) + Sync + Send + 'static>(
        callback: F,
    ) -> Self {
        let f = Box::new(callback);
        CallbackFn { f }
    }
}

mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        src: "
#version 450

layout(location = 0) in vec2 position;
layout(location = 1) in vec2 tex_coords;
layout(location = 2) in vec4 color;

layout(location = 0) out vec4 v_color;
layout(location = 1) out vec2 v_tex_coords;

layout(push_constant) uniform PushConstants {
    vec2 screen_size;
    int output_in_linear_colorspace;
} push_constants;

void main() {
    gl_Position = vec4(
        2.0 * position.x / push_constants.screen_size.x - 1.0,
        2.0 * position.y / push_constants.screen_size.y - 1.0,
        0.0, 1.0
    );
    v_color = color;
    v_tex_coords = tex_coords;
}"
    }
}

// Similar to https://github.com/ArjunNair/egui_sdl2_gl/blob/main/src/painter.rs
mod fs {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: "
#version 450

layout(location = 0) in vec4 v_color;
layout(location = 1) in vec2 v_tex_coords;

layout(location = 0) out vec4 f_color;

layout(binding = 0, set = 0) uniform sampler2D font_texture;

layout(push_constant) uniform PushConstants {
    vec2 screen_size;
    int output_in_linear_colorspace;
} push_constants;

// 0-1 sRGB  from  0-1 linear
vec3 srgb_from_linear(vec3 linear) {
    bvec3 cutoff = lessThan(linear, vec3(0.0031308));
    vec3 lower = linear * vec3(12.92);
    vec3 higher = vec3(1.055) * pow(linear, vec3(1./2.4)) - vec3(0.055);
    return mix(higher, lower, vec3(cutoff));
}

// 0-1 sRGBA  from  0-1 linear
vec4 srgba_from_linear(vec4 linear) {
    return vec4(srgb_from_linear(linear.rgb), linear.a);
}

// 0-1 linear  from  0-1 sRGB
vec3 linear_from_srgb(vec3 srgb) {
    bvec3 cutoff = lessThan(srgb, vec3(0.04045));
    vec3 lower = srgb / vec3(12.92);
    vec3 higher = pow((srgb + vec3(0.055) / vec3(1.055)), vec3(2.4));
    return mix(higher, lower, vec3(cutoff));
}

// 0-1 linear  from  0-1 sRGB
vec4 linear_from_srgba(vec4 srgb) {
    return vec4(linear_from_srgb(srgb.rgb), srgb.a);
}

void main() {
    // ALL calculations should be done in gamma space, this includes texture * color and blending
    vec4 texture_color = srgba_from_linear(texture(font_texture, v_tex_coords));
    vec4 color = v_color * texture_color;

    // If output_in_linear_colorspace is true, we are rendering into an sRGB image, for which we'll convert to linear color space.
    // **This will break blending** as it will be performed in linear color space instead of sRGB like egui expects.
    if (push_constants.output_in_linear_colorspace == 1) {
        color = linear_from_srgba(color);
    }
    f_color = color;
}"
    }
}

use crate::{
    backend::{self, image::*, presentation::blit_image_to_swapchain, shader::*, RenderBackend},
    render_passes::SdfRasterBricks,
    rg,
    rg::CompiledRenderGraph,
    rg::RenderGraph,
    rg::RenderGraphExecutionParams,
    transient_resource_cache::TransientResourceCache,
};
use crate::{
    chunky_list::TempList, dynamic_constants::*, pipeline_cache::*,
    state_tracker::LocalImageStateTracker, viewport::ViewConstants, FrameState,
};
use ash::{version::DeviceV1_0, vk};
use backend::{
    barrier::record_image_barrier,
    barrier::ImageBarrier,
    buffer::{Buffer, BufferDesc},
    device::{CommandBuffer, Device},
};
use byte_slice_cast::AsByteSlice;
use glam::Vec2;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::{collections::HashMap, sync::Arc};
use turbosloth::*;
use winit::VirtualKeyCode;

pub const SDF_DIM: u32 = 256;

#[repr(C)]
#[derive(Copy, Clone)]
struct FrameConstants {
    view_constants: ViewConstants,
    mouse: [f32; 4],
    frame_idx: u32,
}

#[allow(dead_code)]
pub struct Renderer {
    backend: RenderBackend,
    pipeline_cache: PipelineCache,
    transient_resource_cache: TransientResourceCache,
    dynamic_constants: DynamicConstants,
    frame_descriptor_set: vk::DescriptorSet,
    frame_idx: u32,

    present_shader: ComputePipeline,
    depth_img: Image,

    raster_simple_render_pass: Arc<RenderPass>,
    raster_simple: RasterPipelineHandle,

    sdf_img: Arc<Image>,
    gen_empty_sdf: ComputePipelineHandle,
    sdf_raymarch_gbuffer: ComputePipelineHandle,
    edit_sdf: ComputePipelineHandle,
    cube_index_buffer: Arc<Buffer>,

    compiled_rg: Option<CompiledRenderGraph>,
    rg_output_tex: Option<rg::ExportedHandle<Image>>,
}

lazy_static::lazy_static! {
    static ref FRAME_CONSTANTS_LAYOUT: HashMap<u32, rspirv_reflect::DescriptorInfo> = [(
        0,
        rspirv_reflect::DescriptorInfo {
            ty: rspirv_reflect::DescriptorType::UNIFORM_BUFFER,
            is_bindless: false,
            stages: rspirv_reflect::ShaderStageFlags(
                (vk::ShaderStageFlags::COMPUTE
                    | vk::ShaderStageFlags::ALL_GRAPHICS
                    | vk::ShaderStageFlags::RAYGEN_KHR)
                    .as_raw(),
            ),
            name: Default::default(),
        },
    )]
    .iter()
    .cloned()
    .collect();
}

pub enum DescriptorSetBinding {
    Image(vk::DescriptorImageInfo),
    Buffer(vk::DescriptorBufferInfo),
}

pub mod view {
    use super::*;

    pub fn image_rw(view: vk::ImageView) -> DescriptorSetBinding {
        DescriptorSetBinding::Image(
            vk::DescriptorImageInfo::builder()
                .image_layout(vk::ImageLayout::GENERAL)
                .image_view(view)
                .build(),
        )
    }

    pub fn image(view: vk::ImageView) -> DescriptorSetBinding {
        DescriptorSetBinding::Image(
            vk::DescriptorImageInfo::builder()
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image_view(view)
                .build(),
        )
    }

    pub fn buffer_rw(buffer: &Buffer) -> DescriptorSetBinding {
        DescriptorSetBinding::Buffer(
            vk::DescriptorBufferInfo::builder()
                .buffer(buffer.raw)
                .range(vk::WHOLE_SIZE)
                .build(),
        )
    }

    pub fn buffer(buffer: &Buffer) -> DescriptorSetBinding {
        DescriptorSetBinding::Buffer(
            vk::DescriptorBufferInfo::builder()
                .buffer(buffer.raw)
                .range(vk::WHOLE_SIZE)
                .build(),
        )
    }
}

impl Renderer {
    pub fn new(backend: RenderBackend, output_dims: [u32; 2]) -> anyhow::Result<Self> {
        let present_shader = backend::presentation::create_present_compute_shader(&*backend.device);

        let lazy_cache = LazyCache::create();
        let mut pipeline_cache = PipelineCache::new(&lazy_cache);

        let dynamic_constants = DynamicConstants::new({
            let buffer_info = vk::BufferCreateInfo {
                // Allocate twice the size for even and odd frames
                size: (DYNAMIC_CONSTANTS_SIZE_BYTES * 2) as u64,
                usage: vk::BufferUsageFlags::UNIFORM_BUFFER,
                sharing_mode: vk::SharingMode::EXCLUSIVE,
                ..Default::default()
            };

            let buffer_mem_info = vk_mem::AllocationCreateInfo {
                usage: vk_mem::MemoryUsage::CpuToGpu,
                flags: vk_mem::AllocationCreateFlags::MAPPED,
                ..Default::default()
            };

            let (buffer, allocation, allocation_info) = backend
                .device
                .global_allocator
                .create_buffer(&buffer_info, &buffer_mem_info)
                .expect("vma::create_buffer");

            Buffer {
                raw: buffer,
                desc: BufferDesc {
                    size: buffer_info.size as _,
                    usage: buffer_info.usage,
                },
                allocation,
                allocation_info,
            }
        });

        let frame_descriptor_set =
            Self::create_frame_descriptor_set(&backend, &dynamic_constants.buffer);

        let depth_img = backend.device.create_image(
            ImageDesc::new_2d(vk::Format::D24_UNORM_S8_UINT, output_dims).usage(
                vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST,
            ),
            None,
        )?;

        let raster_simple_render_pass = create_render_pass(
            &*backend.device,
            RenderPassDesc {
                color_attachments: &[RenderPassAttachmentDesc::new(
                    vk::Format::R16G16B16A16_SFLOAT,
                )
                .garbage_input()],
                depth_attachment: Some(RenderPassAttachmentDesc::new(
                    vk::Format::D24_UNORM_S8_UINT,
                )),
            },
        )?;

        let raster_simple = pipeline_cache.register_raster(
            &[
                RasterPipelineShader {
                    code: "/assets/shaders/raster_simple_vs.hlsl",
                    desc: RasterShaderDesc::builder(RasterStage::Vertex)
                        .build()
                        .unwrap(),
                },
                RasterPipelineShader {
                    code: "/assets/shaders/raster_simple_ps.hlsl",
                    desc: RasterShaderDesc::builder(RasterStage::Pixel)
                        .build()
                        .unwrap(),
                },
            ],
            &RasterPipelineDesc::builder()
                .render_pass(raster_simple_render_pass.clone())
                .face_cull(true)
                .build()
                .unwrap(),
        );

        let sdf_img = backend.device.create_image(
            ImageDesc::new_3d(vk::Format::R16_SFLOAT, [SDF_DIM, SDF_DIM, SDF_DIM])
                .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED),
            None,
        )?;

        let sdf_pipeline_desc = ComputePipelineDesc::builder().descriptor_set_opts(&[(
            2,
            DescriptorSetLayoutOpts::builder().replace(FRAME_CONSTANTS_LAYOUT.clone()),
        )]);

        let gen_empty_sdf = pipeline_cache.register_compute(
            "/assets/shaders/sdf/gen_empty_sdf.hlsl",
            &sdf_pipeline_desc.clone().build().unwrap(),
        );
        let edit_sdf = pipeline_cache.register_compute(
            "/assets/shaders/sdf/edit_sdf.hlsl",
            &sdf_pipeline_desc.clone().build().unwrap(),
        );

        let sdf_raymarch_gbuffer = pipeline_cache.register_compute(
            "/assets/shaders/sdf/sdf_raymarch_gbuffer.hlsl",
            &sdf_pipeline_desc.clone().build().unwrap(),
        );

        let cube_indices = cube_indices();
        let cube_index_buffer = backend.device.create_buffer(
            BufferDesc {
                size: cube_indices.len() * 4,
                usage: vk::BufferUsageFlags::INDEX_BUFFER,
            },
            Some((&cube_indices).as_byte_slice()),
        )?;

        Ok(Renderer {
            backend,
            dynamic_constants,
            frame_descriptor_set,
            frame_idx: !0,
            pipeline_cache: pipeline_cache,
            transient_resource_cache: Default::default(),
            present_shader,

            depth_img,
            raster_simple_render_pass,
            raster_simple,

            sdf_img: Arc::new(sdf_img),
            gen_empty_sdf,
            sdf_raymarch_gbuffer,
            edit_sdf,
            cube_index_buffer: Arc::new(cube_index_buffer),

            compiled_rg: None,
            rg_output_tex: None,
        })
    }

    pub fn draw_frame(&mut self, frame_state: &FrameState) {
        self.dynamic_constants.advance_frame();
        self.frame_idx = self.frame_idx.overflowing_add(1).0;

        let width = frame_state.window_cfg.width;
        let height = frame_state.window_cfg.height;

        let frame_constants_offset = self.dynamic_constants.push(FrameConstants {
            view_constants: ViewConstants::builder(frame_state.camera_matrices, width, height)
                .build(),
            mouse: gen_shader_mouse_state(&frame_state),
            frame_idx: self.frame_idx,
        });

        // Note: this can be done at the end of the frame, not at the start.
        // The image can be acquired just in time for a blit into it,
        // after all the other rendering commands have been recorded.
        let swapchain_image = self
            .backend
            .swapchain
            .acquire_next_image()
            .ok()
            .expect("swapchain image");

        let current_frame = self.backend.device.current_frame();
        let cb = &current_frame.command_buffer;
        let device = &*self.backend.device;
        let raw_device = &device.raw;

        let mut sdf_img_tracker = LocalImageStateTracker::new(
            self.sdf_img.raw,
            vk::ImageAspectFlags::COLOR,
            if self.frame_idx == 0 {
                vk_sync::AccessType::Nothing
            } else {
                vk_sync::AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer
            },
            cb.raw,
            device,
        );

        unsafe {
            raw_device
                .begin_command_buffer(
                    cb.raw,
                    &vk::CommandBufferBeginInfo::builder()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();

            sdf_img_tracker.transition(vk_sync::AccessType::ComputeShaderWrite);

            // TODO: move to render graph

            // Edit the SDF
            {
                let shader = self.pipeline_cache.get_compute(if self.frame_idx == 0 {
                    // Clear if this is the first frame
                    self.gen_empty_sdf
                } else {
                    self.edit_sdf
                });

                bind_pipeline(&*self.backend.device, cb, &*shader);
                bind_descriptor_set(
                    &*self.backend.device,
                    cb,
                    &*shader,
                    0,
                    &[view::image_rw(
                        self.sdf_img.view(device, &ImageViewDesc::default()),
                    )],
                );
                self.bind_frame_constants(cb, &*shader, frame_constants_offset);

                raw_device.cmd_dispatch(cb.raw, SDF_DIM / 4, SDF_DIM / 4, SDF_DIM / 4);
            }

            /*sdf_img_tracker
            .transition(vk_sync::AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer);*/

            if let Some((rg, rg_output_img)) =
                self.compiled_rg.take().zip(self.rg_output_tex.take())
            {
                let retired_rg = rg.execute(
                    RenderGraphExecutionParams {
                        device: &self.backend.device,
                        pipeline_cache: &mut self.pipeline_cache,
                        frame_descriptor_set: self.frame_descriptor_set,
                        frame_constants_offset,
                    },
                    &mut self.transient_resource_cache,
                    &mut self.dynamic_constants,
                    cb,
                );

                /*output_img_tracker.transition(
                    vk_sync::AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
                );*/

                let (rg_output_img, rg_output_access_type) = retired_rg.get_image(rg_output_img);

                record_image_barrier(
                    device,
                    cb.raw,
                    ImageBarrier::new(
                        rg_output_img.raw,
                        rg_output_access_type,
                        vk_sync::AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer,
                        vk::ImageAspectFlags::COLOR,
                    ),
                );

                blit_image_to_swapchain(
                    &*self.backend.device,
                    cb,
                    &swapchain_image,
                    rg_output_img.view(device, &ImageViewDesc::default()),
                    &self.present_shader,
                );

                retired_rg.release_resources(&mut self.transient_resource_cache);
            }

            raw_device.end_command_buffer(cb.raw).unwrap();
        }

        self.dynamic_constants
            .flush(&self.backend.device.global_allocator);

        let submit_info = vk::SubmitInfo::builder()
            .wait_semaphores(std::slice::from_ref(&swapchain_image.acquire_semaphore))
            .wait_dst_stage_mask(&[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT])
            .command_buffers(std::slice::from_ref(&cb.raw));

        unsafe {
            raw_device
                .reset_fences(std::slice::from_ref(&cb.submit_done_fence))
                .expect("reset_fences");

            raw_device
                .queue_submit(
                    self.backend.device.universal_queue.raw,
                    &[submit_info.build()],
                    cb.submit_done_fence,
                )
                .expect("queue submit failed.");
        }

        self.backend.swapchain.present_image(swapchain_image, &[]);
        self.backend.device.finish_frame(current_frame);
    }

    fn create_frame_descriptor_set(
        backend: &RenderBackend,
        dynamic_constants: &Buffer,
    ) -> vk::DescriptorSet {
        let device = &backend.device.raw;
        let descriptor_set_layout = unsafe {
            device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::builder()
                        .bindings(&[vk::DescriptorSetLayoutBinding::builder()
                            .descriptor_count(1)
                            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                            .stage_flags(
                                vk::ShaderStageFlags::COMPUTE
                                    | vk::ShaderStageFlags::ALL_GRAPHICS
                                    | vk::ShaderStageFlags::RAYGEN_KHR,
                            )
                            .binding(0)
                            .build()])
                        .build(),
                    None,
                )
                .unwrap()
        };

        let descriptor_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC,
            descriptor_count: 1,
        }];

        let descriptor_pool_info = vk::DescriptorPoolCreateInfo::builder()
            .pool_sizes(&descriptor_sizes)
            .max_sets(1);

        let descriptor_pool = unsafe {
            device
                .create_descriptor_pool(&descriptor_pool_info, None)
                .unwrap()
        };

        let set = unsafe {
            device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::builder()
                        .descriptor_pool(descriptor_pool)
                        .set_layouts(std::slice::from_ref(&descriptor_set_layout))
                        .build(),
                )
                .unwrap()[0]
        };

        {
            let buffer_info = vk::DescriptorBufferInfo::builder()
                .buffer(dynamic_constants.raw)
                .range(16384)
                .build();

            let write_descriptor_set = vk::WriteDescriptorSet::builder()
                .dst_set(set)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                .buffer_info(std::slice::from_ref(&buffer_info))
                .build();

            unsafe {
                device.update_descriptor_sets(std::slice::from_ref(&write_descriptor_set), &[])
            };
        }

        set
    }

    pub fn bind_frame_constants(
        &self,
        cb: &CommandBuffer,
        pipeline: &impl std::ops::Deref<Target = ShaderPipelineCommon>,
        frame_constants_offset: u32,
    ) {
        if pipeline
            .set_layout_info
            .get(2)
            .map(|set| !set.is_empty())
            .unwrap_or_default()
        {
            unsafe {
                self.backend.device.raw.cmd_bind_descriptor_sets(
                    cb.raw,
                    pipeline.pipeline_bind_point,
                    pipeline.pipeline_layout,
                    2,
                    &[self.frame_descriptor_set],
                    &[frame_constants_offset],
                );
            }
        }
    }

    fn prepare_render_graph(&mut self, rg: &mut RenderGraph, frame_state: &FrameState) {
        let sdf_img = rg.import_image(
            self.sdf_img.clone(),
            vk_sync::AccessType::ComputeShaderWrite,
        );
        let cube_index_buffer = rg.import_buffer(
            self.cube_index_buffer.clone(),
            vk_sync::AccessType::TransferWrite,
        );

        let mut depth_img = crate::render_passes::create_image(
            rg,
            ImageDesc::new_2d(vk::Format::D24_UNORM_S8_UINT, frame_state.window_cfg.dims()),
        );
        crate::render_passes::clear_depth(rg, &mut depth_img);

        let sdf_raster_bricks: SdfRasterBricks =
            crate::render_passes::calculate_sdf_bricks_meta(rg, &sdf_img);

        /*let mut tex = crate::render_passes::raymarch_sdf(
            rg,
            &sdf_img,
            ImageDesc::new_2d(
                vk::Format::R16G16B16A16_SFLOAT,
                frame_state.window_cfg.dims(),
            ),
        );*/
        let mut tex = crate::render_passes::create_image(
            rg,
            ImageDesc::new_2d(
                vk::Format::R16G16B16A16_SFLOAT,
                frame_state.window_cfg.dims(),
            ),
        );
        crate::render_passes::clear_color(rg, &mut tex, [0.1, 0.2, 0.5, 1.0]);

        crate::render_passes::raster_sdf(
            rg,
            self.raster_simple_render_pass.clone(),
            &mut depth_img,
            &mut tex,
            crate::render_passes::RasterSdfData {
                sdf_img: &sdf_img,
                brick_inst_buffer: &sdf_raster_bricks.brick_inst_buffer,
                brick_meta_buffer: &sdf_raster_bricks.brick_meta_buffer,
                cube_index_buffer: &cube_index_buffer,
            },
        );

        let tex = crate::render_passes::blur(rg, &tex);
        self.rg_output_tex = Some(rg.export_image(tex, vk::ImageUsageFlags::SAMPLED));
    }

    pub fn prepare_frame(&mut self, frame_state: &FrameState) -> anyhow::Result<()> {
        let mut rg = RenderGraph::new(Some(FRAME_CONSTANTS_LAYOUT.clone()));

        self.prepare_render_graph(&mut rg, frame_state);

        self.compiled_rg = Some(rg.compile(&mut self.pipeline_cache));
        self.pipeline_cache.prepare_frame(&self.backend.device)?;

        Ok(())
    }
}

fn gen_shader_mouse_state(frame_state: &FrameState) -> [f32; 4] {
    let pos = frame_state.input.mouse.pos
        / Vec2::new(
            frame_state.window_cfg.width as f32,
            frame_state.window_cfg.height as f32,
        );

    [
        pos.x(),
        pos.y(),
        if (frame_state.input.mouse.button_mask & 1) != 0 {
            1.0
        } else {
            0.0
        },
        if frame_state.input.keys.is_down(VirtualKeyCode::LShift) {
            -1.0
        } else {
            1.0
        },
    ]
}

// Vertices: bits 0, 1, 2, map to +/- X, Y, Z
fn cube_indices() -> Vec<u32> {
    let mut res = Vec::with_capacity(6 * 2 * 3);

    for (ndim, dim0, dim1) in [(1, 2, 4), (2, 4, 1), (4, 1, 2)].iter().copied() {
        for (nbit, dim0, dim1) in [(0, dim1, dim0), (ndim, dim0, dim1)].iter().copied() {
            res.push(nbit);
            res.push(nbit + dim0);
            res.push(nbit + dim1);

            res.push(nbit + dim1);
            res.push(nbit + dim0);
            res.push(nbit + dim0 + dim1);
        }
    }

    res
}

pub fn bind_pipeline(
    device: &Device,
    cb: &CommandBuffer,
    shader: &impl std::ops::Deref<Target = ShaderPipelineCommon>,
) {
    unsafe {
        device
            .raw
            .cmd_bind_pipeline(cb.raw, shader.pipeline_bind_point, shader.pipeline);
    }
}

pub fn bind_descriptor_set(
    device: &Device,
    cb: &CommandBuffer,
    pipeline: &impl std::ops::Deref<Target = ShaderPipelineCommon>,
    set_index: u32,
    bindings: &[DescriptorSetBinding],
) {
    let shader_set_info = if let Some(info) = pipeline.set_layout_info.get(set_index as usize) {
        info
    } else {
        println!(
            "bind_descriptor_set: set index {} does not exist",
            set_index
        );
        return;
    };

    let image_info = TempList::new();
    let buffer_info = TempList::new();

    let raw_device = &device.raw;

    let descriptor_pool = {
        let descriptor_pool_create_info = vk::DescriptorPoolCreateInfo::builder()
            .max_sets(1)
            .pool_sizes(&pipeline.descriptor_pool_sizes);

        unsafe { raw_device.create_descriptor_pool(&descriptor_pool_create_info, None) }.unwrap()
    };
    device.defer_release(descriptor_pool);

    let descriptor_set = {
        let descriptor_set_allocate_info = vk::DescriptorSetAllocateInfo::builder()
            .descriptor_pool(descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &pipeline.descriptor_set_layouts[set_index as usize],
            ));

        unsafe { raw_device.allocate_descriptor_sets(&descriptor_set_allocate_info) }.unwrap()[0]
    };

    unsafe {
        let descriptor_writes: Vec<vk::WriteDescriptorSet> = bindings
            .iter()
            .enumerate()
            .filter(|(binding_idx, _)| shader_set_info.contains_key(&(*binding_idx as u32)))
            .map(|(binding_idx, binding)| {
                let write = vk::WriteDescriptorSet::builder()
                    .dst_set(descriptor_set)
                    .dst_binding(binding_idx as _)
                    .dst_array_element(0);

                match binding {
                    DescriptorSetBinding::Image(image) => write
                        .descriptor_type(match image.image_layout {
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => {
                                vk::DescriptorType::SAMPLED_IMAGE
                            }
                            vk::ImageLayout::GENERAL => vk::DescriptorType::STORAGE_IMAGE,
                            _ => unimplemented!("{:?}", image.image_layout),
                        })
                        .image_info(std::slice::from_ref(image_info.add(*image)))
                        .build(),
                    DescriptorSetBinding::Buffer(buffer) => write
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(std::slice::from_ref(buffer_info.add(*buffer)))
                        .build(),
                }
            })
            .collect();

        device.raw.update_descriptor_sets(&descriptor_writes, &[]);

        device.raw.cmd_bind_descriptor_sets(
            cb.raw,
            pipeline.pipeline_bind_point,
            pipeline.pipeline_layout,
            set_index,
            &[descriptor_set],
            &[],
        );
    }
}

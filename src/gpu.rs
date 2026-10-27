//! The Vulkan side: one full-screen draw per frame, into a swapchain whose
//! format and colour space follow the mode, with HDR metadata in the HDR modes.

use std::ffi::CStr;
use std::sync::Arc;

use ash::{ext, khr, vk};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use crate::{Mode, Options};

const FRAMES_IN_FLIGHT: usize = 2;
const VERT_SPV: &[u8] = include_bytes!("../shaders/fullscreen.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../shaders/neon.frag.spv");

/// The brightest patch in the reference strip, which HDR metadata has to cover.
const STRIP_PEAK: f32 = 4000.0;

const COLOR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
	aspect_mask: vk::ImageAspectFlags::COLOR,
	base_mip_level: 0,
	level_count: 1,
	base_array_layer: 0,
	layer_count: 1,
};

/// Matches the shader's push constants.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
	time: f32,
	mode: u32,
	peak: f32,
	load: u32,
	resolution: [f32; 2],
}

struct Frame {
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	acquired: vk::Semaphore,
}

pub struct Gpu {
	window: Arc<Window>,
	/// Keeps libvulkan loaded.
	_entry: ash::Entry,
	instance: ash::Instance,
	surface_fn: khr::surface::Instance,
	surface: vk::SurfaceKHR,
	physical: vk::PhysicalDevice,
	device_name: String,
	device: ash::Device,
	queue: vk::Queue,
	swapchain_fn: khr::swapchain::Device,
	hdr_fn: Option<ext::hdr_metadata::Device>,
	pool: vk::CommandPool,
	frames: Vec<Frame>,
	frame: usize,
	layout: vk::PipelineLayout,
	vert: vk::ShaderModule,
	frag: vk::ShaderModule,
	pipeline: vk::Pipeline,
	pipeline_format: vk::Format,
	swapchain: vk::SwapchainKHR,
	images: Vec<vk::Image>,
	views: Vec<vk::ImageView>,
	/// One per swapchain image: presentation waits on the image's own.
	rendered: Vec<vk::Semaphore>,
	extent: vk::Extent2D,
	mode: Mode,
	peak: f32,
	load: u32,
	vsync: bool,
	/// The swapchain needs recreating before the next frame.
	stale: bool,
}

fn vk_err(what: &str) -> impl Fn(vk::Result) -> String + '_ {
	move |e| format!("{what}: {e}")
}

fn load_vulkan() -> Result<ash::Entry, String> {
	// SAFETY: loads the system's Vulkan loader, which stays loaded as long as
	// the returned entry.
	let entry = unsafe { ash::Entry::load() };
	// Homebrew's loader isn't on the default library path.
	#[cfg(target_os = "macos")]
	let entry = entry.or_else(|e| {
		[
			"/opt/homebrew/lib/libvulkan.1.dylib",
			"/usr/local/lib/libvulkan.1.dylib",
		]
		.into_iter()
		// SAFETY: as above.
		.find_map(|path| unsafe { ash::Entry::load_from(path) }.ok())
		.ok_or(e)
	});
	entry.map_err(|e| format!("no Vulkan loader: {e}"))
}

fn has_extension(list: &[vk::ExtensionProperties], name: &CStr) -> bool {
	list.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

impl Gpu {
	pub fn new(window: Arc<Window>, options: &Options) -> Result<Self, String> {
		let display = window
			.display_handle()
			.map_err(|e| format!("display handle: {e}"))?
			.as_raw();
		let handle = window
			.window_handle()
			.map_err(|e| format!("window handle: {e}"))?
			.as_raw();

		// SAFETY: every call gets valid handles created here. If one fails
		// partway, what was made leaks; the caller exits.
		unsafe {
			let entry = load_vulkan()?;

			let available = entry
				.enumerate_instance_extension_properties(None)
				.map_err(vk_err("instance extensions"))?;
			let mut extensions = ash_window::enumerate_required_extensions(display)
				.map_err(vk_err("surface extensions"))?
				.to_vec();
			// HDR10 and scRGB colour spaces need this; without it only SDR works.
			if has_extension(&available, ext::swapchain_colorspace::NAME) {
				extensions.push(ext::swapchain_colorspace::NAME.as_ptr());
			}
			let mut flags = vk::InstanceCreateFlags::empty();
			if has_extension(&available, khr::portability_enumeration::NAME) {
				extensions.push(khr::portability_enumeration::NAME.as_ptr());
				flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
			}
			let app = vk::ApplicationInfo::default()
				.application_name(c"hdrneon")
				.api_version(vk::API_VERSION_1_3);
			let instance = entry
				.create_instance(
					&vk::InstanceCreateInfo::default()
						.application_info(&app)
						.enabled_extension_names(&extensions)
						.flags(flags),
					None,
				)
				.map_err(vk_err("create instance"))?;

			let surface_fn = khr::surface::Instance::new(&entry, &instance);
			let surface = ash_window::create_surface(&entry, &instance, display, handle, None)
				.map_err(vk_err("create surface"))?;

			// A Vulkan 1.3 device with a queue that draws and presents; a
			// discrete GPU if there is one.
			let mut best = None;
			for physical in instance.enumerate_physical_devices().map_err(vk_err("devices"))? {
				let props = instance.get_physical_device_properties(physical);
				if props.api_version < vk::API_VERSION_1_3 {
					continue;
				}
				let families = instance.get_physical_device_queue_family_properties(physical);
				let family = (0..families.len() as u32).find(|&i| {
					families[i as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS)
						&& surface_fn
							.get_physical_device_surface_support(physical, i, surface)
							.unwrap_or(false)
				});
				if let Some(family) = family {
					let discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
					if best.is_none() || discrete {
						best = Some((physical, family, props));
					}
					if discrete {
						break;
					}
				}
			}
			let (physical, family, props) = best.ok_or("no Vulkan 1.3 device can draw to this window")?;
			let device_name = props
				.device_name_as_c_str()
				.unwrap_or(c"?")
				.to_string_lossy()
				.into_owned();

			let available = instance
				.enumerate_device_extension_properties(physical)
				.map_err(vk_err("device extensions"))?;
			let mut extensions = vec![khr::swapchain::NAME.as_ptr()];
			let hdr_metadata = has_extension(&available, ext::hdr_metadata::NAME);
			if hdr_metadata {
				extensions.push(ext::hdr_metadata::NAME.as_ptr());
			}
			if has_extension(&available, khr::portability_subset::NAME) {
				extensions.push(khr::portability_subset::NAME.as_ptr());
			}
			let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
				.dynamic_rendering(true)
				.synchronization2(true);
			let priorities = [1.0];
			let queues = [vk::DeviceQueueCreateInfo::default()
				.queue_family_index(family)
				.queue_priorities(&priorities)];
			let device = instance
				.create_device(
					physical,
					&vk::DeviceCreateInfo::default()
						.queue_create_infos(&queues)
						.enabled_extension_names(&extensions)
						.push_next(&mut features13),
					None,
				)
				.map_err(vk_err("create device"))?;
			let queue = device.get_device_queue(family, 0);
			let swapchain_fn = khr::swapchain::Device::new(&instance, &device);
			let hdr_fn = hdr_metadata.then(|| ext::hdr_metadata::Device::new(&instance, &device));

			let pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default()
						.queue_family_index(family)
						.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
					None,
				)
				.map_err(vk_err("command pool"))?;
			let buffers = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(pool)
						.level(vk::CommandBufferLevel::PRIMARY)
						.command_buffer_count(FRAMES_IN_FLIGHT as u32),
				)
				.map_err(vk_err("command buffers"))?;
			let mut frames = Vec::new();
			for command_buffer in buffers {
				frames.push(Frame {
					command_buffer,
					fence: device
						.create_fence(
							&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
							None,
						)
						.map_err(vk_err("fence"))?,
					acquired: device
						.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
						.map_err(vk_err("semaphore"))?,
				});
			}

			let push = [vk::PushConstantRange::default()
				.stage_flags(vk::ShaderStageFlags::FRAGMENT)
				.size(size_of::<Params>() as u32)];
			let layout = device
				.create_pipeline_layout(
					&vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push),
					None,
				)
				.map_err(vk_err("pipeline layout"))?;
			let module = |spv: &[u8]| {
				let code = ash::util::read_spv(&mut std::io::Cursor::new(spv)).map_err(|e| e.to_string())?;
				device
					.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
					.map_err(vk_err("shader module"))
			};
			let vert = module(VERT_SPV)?;
			let frag = module(FRAG_SPV)?;

			Ok(Self {
				window,
				_entry: entry,
				instance,
				surface_fn,
				surface,
				physical,
				device_name,
				device,
				queue,
				swapchain_fn,
				hdr_fn,
				pool,
				frames,
				frame: 0,
				layout,
				vert,
				frag,
				pipeline: vk::Pipeline::null(),
				pipeline_format: vk::Format::UNDEFINED,
				swapchain: vk::SwapchainKHR::null(),
				images: Vec::new(),
				views: Vec::new(),
				rendered: Vec::new(),
				extent: vk::Extent2D::default(),
				mode: options.mode,
				peak: options.peak,
				load: options.load,
				vsync: options.vsync,
				stale: true,
			})
		}
	}

	pub fn device_name(&self) -> &str {
		&self.device_name
	}

	pub fn window(&self) -> &Window {
		&self.window
	}

	pub fn mode(&self) -> Mode {
		self.mode
	}

	pub fn print_formats(&self) {
		// SAFETY: valid physical device and surface.
		let formats = unsafe {
			self.surface_fn
				.get_physical_device_surface_formats(self.physical, self.surface)
				.unwrap_or_default()
		};
		println!("{}:", self.device_name);
		for f in formats {
			println!("  {:?} {:?}", f.format, f.color_space);
		}
	}

	pub fn resized(&mut self) {
		self.stale = true;
	}

	/// Present in `mode` from the next frame. On an error the mode is unchanged.
	pub fn set_mode(&mut self, mode: Mode) -> Result<(), String> {
		self.create_swapchain(mode)
	}

	fn pick_format(&self, mode: Mode) -> Result<vk::SurfaceFormatKHR, String> {
		use vk::{ColorSpaceKHR as Cs, Format as F};
		// SAFETY: valid physical device and surface.
		let offered = unsafe {
			self.surface_fn
				.get_physical_device_surface_formats(self.physical, self.surface)
		}
		.map_err(vk_err("surface formats"))?;
		let wanted: &[(F, Cs)] = match mode {
			Mode::Sdr => &[
				(F::B8G8R8A8_UNORM, Cs::SRGB_NONLINEAR),
				(F::R8G8B8A8_UNORM, Cs::SRGB_NONLINEAR),
				(F::A2B10G10R10_UNORM_PACK32, Cs::SRGB_NONLINEAR),
			],
			Mode::Hdr10 => &[
				(F::A2B10G10R10_UNORM_PACK32, Cs::HDR10_ST2084_EXT),
				(F::A2R10G10B10_UNORM_PACK32, Cs::HDR10_ST2084_EXT),
			],
			Mode::Scrgb => &[(F::R16G16B16A16_SFLOAT, Cs::EXTENDED_SRGB_LINEAR_EXT)],
		};
		wanted
			.iter()
			.find_map(|&(format, cs)| {
				offered
					.iter()
					.find(|o| o.format == format && o.color_space == cs)
					.copied()
			})
			.ok_or_else(|| {
				format!(
					"no {} surface format, is hdr on? (--list-formats shows what's there)",
					mode.name()
				)
			})
	}

	fn create_swapchain(&mut self, mode: Mode) -> Result<(), String> {
		let format = self.pick_format(mode)?;
		self.mode = mode;
		// SAFETY: the device is idle before anything the old swapchain uses is
		// destroyed; all handles are this device's.
		unsafe {
			let caps = self
				.surface_fn
				.get_physical_device_surface_capabilities(self.physical, self.surface)
				.map_err(vk_err("surface capabilities"))?;
			let size = self.window.inner_size();
			let extent = if caps.current_extent.width != u32::MAX {
				caps.current_extent
			} else {
				vk::Extent2D {
					width: size
						.width
						.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
					height: size
						.height
						.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
				}
			};
			if extent.width == 0 || extent.height == 0 {
				// Minimised; try again on the next frame.
				self.stale = true;
				return Ok(());
			}
			let modes = self
				.surface_fn
				.get_physical_device_surface_present_modes(self.physical, self.surface)
				.map_err(vk_err("present modes"))?;
			let present_mode = if self.vsync {
				vk::PresentModeKHR::FIFO
			} else {
				[vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE]
					.into_iter()
					.find(|m| modes.contains(m))
					.unwrap_or(vk::PresentModeKHR::FIFO)
			};
			let mut count = caps.min_image_count + 1;
			if caps.max_image_count > 0 {
				count = count.min(caps.max_image_count);
			}
			let alpha = [
				vk::CompositeAlphaFlagsKHR::OPAQUE,
				vk::CompositeAlphaFlagsKHR::INHERIT,
				vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
				vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
			]
			.into_iter()
			.find(|a| caps.supported_composite_alpha.contains(*a))
			.unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);

			self.device.device_wait_idle().map_err(vk_err("wait idle"))?;
			let old = self.swapchain;
			let swapchain = self
				.swapchain_fn
				.create_swapchain(
					&vk::SwapchainCreateInfoKHR::default()
						.surface(self.surface)
						.min_image_count(count)
						.image_format(format.format)
						.image_color_space(format.color_space)
						.image_extent(extent)
						.image_array_layers(1)
						.image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
						.image_sharing_mode(vk::SharingMode::EXCLUSIVE)
						.pre_transform(caps.current_transform)
						.composite_alpha(alpha)
						.present_mode(present_mode)
						.clipped(true)
						.old_swapchain(old),
					None,
				)
				.map_err(vk_err("create swapchain"))?;
			self.destroy_swapchain_resources();
			self.swapchain_fn.destroy_swapchain(old, None);
			self.swapchain = swapchain;
			self.images = self
				.swapchain_fn
				.get_swapchain_images(swapchain)
				.map_err(vk_err("swapchain images"))?;
			for &image in &self.images {
				let view = self
					.device
					.create_image_view(
						&vk::ImageViewCreateInfo::default()
							.image(image)
							.view_type(vk::ImageViewType::TYPE_2D)
							.format(format.format)
							.subresource_range(COLOR),
						None,
					)
					.map_err(vk_err("image view"))?;
				self.views.push(view);
				let semaphore = self
					.device
					.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
					.map_err(vk_err("semaphore"))?;
				self.rendered.push(semaphore);
			}
			if format.format != self.pipeline_format {
				self.device.destroy_pipeline(self.pipeline, None);
				self.pipeline = self.create_pipeline(format.format)?;
				self.pipeline_format = format.format;
			}
			// MoltenVK passes this on to macOS as HDR10 EDR metadata with an
			// optical output scale of 1, which macOS also applies to linear
			// content: scRGB's 1.0 would show as 1 nit. Without it, macOS takes
			// 1.0 as SDR white.
			let send_metadata = match self.mode {
				Mode::Sdr => false,
				Mode::Hdr10 => true,
				Mode::Scrgb => !cfg!(target_os = "macos"),
			};
			if send_metadata && let Some(hdr_fn) = &self.hdr_fn {
				let xy = |x, y| vk::XYColorEXT { x, y };
				// BT.2020 primaries, D65 white.
				let metadata = vk::HdrMetadataEXT::default()
					.display_primary_red(xy(0.708, 0.292))
					.display_primary_green(xy(0.170, 0.797))
					.display_primary_blue(xy(0.131, 0.046))
					.white_point(xy(0.3127, 0.3290))
					.max_luminance(self.peak.max(STRIP_PEAK))
					.min_luminance(0.001)
					.max_content_light_level(self.peak.max(STRIP_PEAK))
					// 0: unknown.
					.max_frame_average_light_level(0.0);
				hdr_fn.set_hdr_metadata(&[swapchain], &[metadata]);
			}
			self.extent = extent;
			self.stale = false;
		}
		Ok(())
	}

	/// # Safety
	///
	/// The device must be idle.
	unsafe fn destroy_swapchain_resources(&mut self) {
		unsafe {
			for view in self.views.drain(..) {
				self.device.destroy_image_view(view, None);
			}
			for semaphore in self.rendered.drain(..) {
				self.device.destroy_semaphore(semaphore, None);
			}
		}
		self.images.clear();
	}

	fn create_pipeline(&self, format: vk::Format) -> Result<vk::Pipeline, String> {
		let stages = [
			vk::PipelineShaderStageCreateInfo::default()
				.stage(vk::ShaderStageFlags::VERTEX)
				.module(self.vert)
				.name(c"main"),
			vk::PipelineShaderStageCreateInfo::default()
				.stage(vk::ShaderStageFlags::FRAGMENT)
				.module(self.frag)
				.name(c"main"),
		];
		let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
		let input_assembly =
			vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
		let viewport = vk::PipelineViewportStateCreateInfo::default()
			.viewport_count(1)
			.scissor_count(1);
		let raster = vk::PipelineRasterizationStateCreateInfo::default()
			.polygon_mode(vk::PolygonMode::FILL)
			.cull_mode(vk::CullModeFlags::NONE)
			.line_width(1.0);
		let multisample =
			vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
		let attachments =
			[vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
		let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
		let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
		let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
		let formats = [format];
		let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
		let info = vk::GraphicsPipelineCreateInfo::default()
			.stages(&stages)
			.vertex_input_state(&vertex_input)
			.input_assembly_state(&input_assembly)
			.viewport_state(&viewport)
			.rasterization_state(&raster)
			.multisample_state(&multisample)
			.color_blend_state(&blend)
			.dynamic_state(&dynamic)
			.layout(self.layout)
			.push_next(&mut rendering);
		// SAFETY: every referenced object is this device's and alive.
		unsafe {
			self.device
				.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
		}
		.map(|p| p[0])
		.map_err(|(_, e)| format!("create pipeline: {e}"))
	}

	pub fn draw(&mut self, time: f32) -> Result<(), String> {
		if self.stale {
			self.create_swapchain(self.mode)?;
			if self.stale {
				return Ok(());
			}
		}
		let frame = &self.frames[self.frame];
		// SAFETY: the frame's fence guards its command buffer and semaphore; the
		// image's `rendered` semaphore is only reused after its present.
		unsafe {
			self.device
				.wait_for_fences(&[frame.fence], true, u64::MAX)
				.map_err(vk_err("wait for frame"))?;
			let index =
				match self
					.swapchain_fn
					.acquire_next_image(self.swapchain, u64::MAX, frame.acquired, vk::Fence::null())
				{
					Ok((index, _)) => index,
					Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
						self.stale = true;
						return Ok(());
					},
					Err(e) => return Err(format!("acquire: {e}")),
				};
			let (image, view, rendered) = (
				self.images[index as usize],
				self.views[index as usize],
				self.rendered[index as usize],
			);
			self.device
				.reset_fences(&[frame.fence])
				.map_err(vk_err("reset fence"))?;

			let cmd = frame.command_buffer;
			self.device
				.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
				.map_err(vk_err("reset command buffer"))?;
			self.device
				.begin_command_buffer(
					cmd,
					&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
				)
				.map_err(vk_err("begin command buffer"))?;

			let to_attachment = vk::ImageMemoryBarrier2::default()
				.src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
				.old_layout(vk::ImageLayout::UNDEFINED)
				.new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.image(image)
				.subresource_range(COLOR);
			self.device.cmd_pipeline_barrier2(
				cmd,
				&vk::DependencyInfo::default().image_memory_barriers(&[to_attachment]),
			);

			let attachments = [vk::RenderingAttachmentInfo::default()
				.image_view(view)
				.image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.load_op(vk::AttachmentLoadOp::DONT_CARE)
				.store_op(vk::AttachmentStoreOp::STORE)];
			let area = vk::Rect2D {
				offset: vk::Offset2D::default(),
				extent: self.extent,
			};
			self.device.cmd_begin_rendering(
				cmd,
				&vk::RenderingInfo::default()
					.render_area(area)
					.layer_count(1)
					.color_attachments(&attachments),
			);
			self.device
				.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
			self.device.cmd_set_viewport(
				cmd,
				0,
				&[vk::Viewport {
					x: 0.0,
					y: 0.0,
					width: self.extent.width as f32,
					height: self.extent.height as f32,
					min_depth: 0.0,
					max_depth: 1.0,
				}],
			);
			self.device.cmd_set_scissor(cmd, 0, &[area]);
			let params = Params {
				time,
				mode: match self.mode {
					Mode::Sdr => 0,
					Mode::Hdr10 => 1,
					Mode::Scrgb => 2,
				},
				peak: self.peak,
				load: self.load,
				resolution: [self.extent.width as f32, self.extent.height as f32],
			};
			let bytes = std::slice::from_raw_parts((&raw const params).cast::<u8>(), size_of::<Params>());
			self.device
				.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::FRAGMENT, 0, bytes);
			self.device.cmd_draw(cmd, 3, 1, 0, 0);
			self.device.cmd_end_rendering(cmd);

			let to_present = vk::ImageMemoryBarrier2::default()
				.src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
				.dst_stage_mask(vk::PipelineStageFlags2::NONE)
				.old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
				.image(image)
				.subresource_range(COLOR);
			self.device
				.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[to_present]));
			self.device
				.end_command_buffer(cmd)
				.map_err(vk_err("end command buffer"))?;

			let waits = [vk::SemaphoreSubmitInfo::default()
				.semaphore(frame.acquired)
				.stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
			let signals = [vk::SemaphoreSubmitInfo::default()
				.semaphore(rendered)
				.stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
			let buffers = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
			self.device
				.queue_submit2(
					self.queue,
					&[vk::SubmitInfo2::default()
						.wait_semaphore_infos(&waits)
						.command_buffer_infos(&buffers)
						.signal_semaphore_infos(&signals)],
					frame.fence,
				)
				.map_err(vk_err("submit"))?;

			let swapchains = [self.swapchain];
			let indices = [index];
			let wait = [rendered];
			match self.swapchain_fn.queue_present(
				self.queue,
				&vk::PresentInfoKHR::default()
					.wait_semaphores(&wait)
					.swapchains(&swapchains)
					.image_indices(&indices),
			) {
				Ok(false) => {},
				Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.stale = true,
				Err(e) => return Err(format!("present: {e}")),
			}
		}
		self.frame = (self.frame + 1) % FRAMES_IN_FLIGHT;
		Ok(())
	}
}

impl Drop for Gpu {
	fn drop(&mut self) {
		// SAFETY: nothing is in flight after the wait, and each object is
		// destroyed once, children before their parents.
		unsafe {
			let _ = self.device.device_wait_idle();
			self.destroy_swapchain_resources();
			self.swapchain_fn.destroy_swapchain(self.swapchain, None);
			self.device.destroy_pipeline(self.pipeline, None);
			self.device.destroy_shader_module(self.vert, None);
			self.device.destroy_shader_module(self.frag, None);
			self.device.destroy_pipeline_layout(self.layout, None);
			for frame in &self.frames {
				self.device.destroy_fence(frame.fence, None);
				self.device.destroy_semaphore(frame.acquired, None);
			}
			self.device.destroy_command_pool(self.pool, None);
			self.device.destroy_device(None);
			self.surface_fn.destroy_surface(self.surface, None);
			self.instance.destroy_instance(None);
		}
	}
}

//! OpenXR instance + Vulkan device chosen by the runtime (XR_KHR_vulkan_enable2)
//! + session. The runtime picks the GPU; we never enumerate devices ourselves.

use anyhow::{Context, bail, ensure};
use ash::vk::{self, Handle};
use openxr as xr;
use std::ffi::c_void;

pub const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

pub struct XrContext {
    pub xr: xr::Instance,
    pub system: xr::SystemId,
    pub system_name: String,
    pub views: Vec<xr::ViewConfigurationView>,
    pub blend_mode: xr::EnvironmentBlendMode,
    pub vk_entry: ash::Entry,
    pub vk: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue_family: u32,
    pub queue: vk::Queue,
    pub session: xr::Session<xr::Vulkan>,
    pub frame_waiter: xr::FrameWaiter,
    pub frame_stream: xr::FrameStream<xr::Vulkan>,
}

impl XrContext {
    pub fn new() -> anyhow::Result<Self> {
        let entry = super::runtime::entry()?;
        let available = entry.enumerate_extensions()?;
        ensure!(
            available.khr_vulkan_enable2,
            "OpenXR runtime lacks XR_KHR_vulkan_enable2"
        );
        let mut extensions = xr::ExtensionSet::default();
        extensions.khr_vulkan_enable2 = true;
        // Native Steam Frame controller bindings (otherwise SteamVR remaps Index ones).
        const FRAME_CONTROLLER: &str = "XR_VALVE_frame_controller_interaction";
        // The crate keeps these names NUL-terminated (they go to the runtime as C strings).
        let name = |e: &[u8]| e.strip_suffix(b"\0").unwrap_or(e).to_vec();
        if available
            .other
            .iter()
            .any(|e| name(e) == FRAME_CONTROLLER.as_bytes())
        {
            extensions
                .other
                .push(format!("{FRAME_CONTROLLER}\0").into_bytes());
            eprintln!("OpenXR: enabled {FRAME_CONTROLLER}");
        } else {
            let names: Vec<String> = available
                .other
                .iter()
                .map(|e| String::from_utf8_lossy(&name(e)).into_owned())
                .collect();
            eprintln!(
                "OpenXR: runtime does not offer {FRAME_CONTROLLER} (offers {})",
                names.join(", ")
            );
        }
        let xr = entry.create_instance(
            &xr::ApplicationInfo {
                application_name: "Just Video",
                application_version: 1,
                engine_name: "just-video",
                engine_version: 1,
                api_version: xr::Version::new(1, 0, 0),
            },
            &extensions,
            &[],
        )?;
        let system = xr
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .context("No headset available to OpenXR (is it awake and tracking?)")?;
        let system_name = xr.system_properties(system)?.system_name;
        let views = xr.enumerate_view_configuration_views(system, VIEW_TYPE)?;
        ensure!(views.len() == 2, "Expected a stereo view configuration");
        let blend_mode = xr.enumerate_environment_blend_modes(system, VIEW_TYPE)?[0];

        // Required before creating the Vulkan instance/device.
        let requirements = xr.graphics_requirements::<xr::Vulkan>(system)?;
        let vk_target = vk::make_api_version(0, 1, 1, 0);
        let xr_min = requirements.min_api_version_supported;
        ensure!(
            vk::make_api_version(0, xr_min.major().into(), xr_min.minor().into(), 0) <= vk_target,
            "Runtime needs Vulkan {xr_min} or newer"
        );

        let vk_entry = unsafe { ash::Entry::load() }.context("Load Vulkan")?;
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"Just Video")
            .api_version(vk_target);
        let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let get_instance_proc_addr: xr::sys::platform::VkGetInstanceProcAddr =
            unsafe { std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr) };
        let vk_raw = unsafe {
            xr.create_vulkan_instance(
                system,
                get_instance_proc_addr,
                &instance_info as *const _ as *const _,
            )?
        }
        .map_err(|e| anyhow::anyhow!("vkCreateInstance: {:?}", vk::Result::from_raw(e)))?;
        let vk = unsafe {
            ash::Instance::load(vk_entry.static_fn(), vk::Instance::from_raw(vk_raw as u64))
        };
        let physical_device =
            vk::PhysicalDevice::from_raw(
                unsafe { xr.vulkan_graphics_device(system, vk_raw)? } as u64
            );
        let queue_family =
            unsafe { vk.get_physical_device_queue_family_properties(physical_device) }
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .context("GPU has no graphics queue")? as u32;
        let priorities = [1.0];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_info);
        let device_raw = unsafe {
            xr.create_vulkan_device(
                system,
                get_instance_proc_addr,
                physical_device.as_raw() as *const c_void,
                &device_info as *const _ as *const _,
            )?
        }
        .map_err(|e| anyhow::anyhow!("vkCreateDevice: {:?}", vk::Result::from_raw(e)))?;
        let device =
            unsafe { ash::Device::load(vk.fp_v1_0(), vk::Device::from_raw(device_raw as u64)) };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let (session, frame_waiter, frame_stream) = unsafe {
            xr.create_session::<xr::Vulkan>(
                system,
                &xr::vulkan::SessionCreateInfo {
                    instance: vk_raw,
                    physical_device: physical_device.as_raw() as *const c_void,
                    device: device_raw,
                    queue_family_index: queue_family,
                    queue_index: 0,
                },
            )
        }
        .context("Create OpenXR session")?;
        Ok(Self {
            xr,
            system,
            system_name,
            views,
            blend_mode,
            vk_entry,
            vk,
            physical_device,
            device,
            queue_family,
            queue,
            session,
            frame_waiter,
            frame_stream,
        })
    }

    pub fn gpu_name(&self) -> String {
        let props = unsafe { self.vk.get_physical_device_properties(self.physical_device) };
        props
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn swapchain_formats(&self) -> anyhow::Result<Vec<vk::Format>> {
        Ok(self
            .session
            .enumerate_swapchain_formats()?
            .into_iter()
            .map(|f| vk::Format::from_raw(f as i32))
            .collect())
    }
}

/// Picks an sRGB color format so the compositor sees correctly encoded output.
pub fn choose_color_format(formats: &[vk::Format]) -> anyhow::Result<vk::Format> {
    for wanted in [vk::Format::R8G8B8A8_SRGB, vk::Format::B8G8R8A8_SRGB] {
        if formats.contains(&wanted) {
            return Ok(wanted);
        }
    }
    bail!("Runtime offers no sRGB swapchain format: {formats:?}")
}

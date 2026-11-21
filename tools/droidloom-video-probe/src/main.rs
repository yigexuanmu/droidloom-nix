//! Probe the video decode capabilities the Vulkan device exposes.
//!
//! Runs against whatever driver the Vulkan loader picks: the host's own
//! driver, or Droidloom's forwarding driver through `VK_DRIVER_FILES` with a
//! `droidloom-gpu-server` beside it. Prints the video extensions, the video
//! decode queue families, and the H.264/HEVC/AV1 capabilities and DPB
//! formats a Codec2 decoder builds its sessions from. Every video structure
//! comes from the generated protocol crate, so the ABI here is the ABI the
//! wire carries.
//!
//! The codec-operation and profile constants are frozen Vulkan registry
//! values (registry bit positions, not offset-derived numbers); the
//! structures and structure types it does emit are imported instead.

use std::ffi::{CStr, CString};

use ash::vk;
use droidloom_gpu_protocol::generated::vk::{
    PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR, PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR,
    VkExtensionProperties, VkOffset2D, VkPhysicalDeviceVideoFormatInfoKHR, VkVideoCapabilitiesKHR,
    VkVideoComponentBitDepthFlagsKHR, VkVideoDecodeAV1CapabilitiesKHR, VkVideoDecodeAV1ProfileInfoKHR,
    VkVideoDecodeCapabilitiesKHR, VkVideoDecodeH264CapabilitiesKHR, VkVideoDecodeH264ProfileInfoKHR,
    VkVideoDecodeH265CapabilitiesKHR, VkVideoDecodeH265ProfileInfoKHR, VkVideoFormatPropertiesKHR,
    VkVideoProfileInfoKHR, VkVideoProfileListInfoKHR, VK_STRUCTURE_TYPE_VIDEO_CAPABILITIES_KHR,
    VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR,
    VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_CAPABILITIES_KHR, VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_PROFILE_INFO_KHR,
    VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR, VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_CAPABILITIES_KHR,
    VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PROFILE_INFO_KHR, VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_CAPABILITIES_KHR,
    VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_PROFILE_INFO_KHR, VK_STRUCTURE_TYPE_VIDEO_PROFILE_INFO_KHR,
};

/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` (registry bitpos 0).
const CODEC_H264: i32 = 0x1;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR` (registry bitpos 1).
const CODEC_H265: i32 = 0x2;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR` (registry bitpos 2).
const CODEC_AV1: i32 = 0x4;
/// `VK_VIDEO_CHROMA_SUBSAMPLING_420_KHR`.
const CHROMA_420: u32 = 0x2;
/// `VK_VIDEO_COMPONENT_BIT_DEPTH_8_KHR`.
const BIT_DEPTH_8: VkVideoComponentBitDepthFlagsKHR = 0x1;
/// `VK_VIDEO_DECODE_H264_PICTURE_LAYOUT_PROGRESSIVE_KHR`.
const LAYOUT_PROGRESSIVE: i32 = 0;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_DPB_BIT_KHR` (registry bitpos 12).
const USAGE_DECODE_DPB: u32 = 1 << 12;
/// H.264 `STD_VIDEO_H264_PROFILE_IDC_HIGH` (video.xml).
const H264_PROFILE_HIGH: i32 = 100;
/// H.265 `STD_VIDEO_H265_PROFILE_IDC_MAIN` (video.xml).
const H265_PROFILE_MAIN: i32 = 1;
/// `VK_QUEUE_VIDEO_DECODE_BIT_KHR`.
const QUEUE_VIDEO_DECODE: u32 = 0x20;

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-video-probe: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let entry = unsafe { ash::Entry::load() }.map_err(|error| error.to_string())?;
    let instance = unsafe {
        entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(
                    &vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0)),
                ),
                None,
            )
            .map_err(|error| error.to_string())?
    };
    // The first device is whatever the loader ordered (a software ICD has no
    // video); pick the vendor the host actually drives.
    let devices = unsafe { instance.enumerate_physical_devices() }.map_err(|error| error.to_string())?;
    let physical = devices
        .into_iter()
        .find(|device| {
            let properties = unsafe { instance.get_physical_device_properties(*device) };
            properties.vendor_id == 0x10DE
                || properties
                    .device_name_as_c_str()
                    .is_ok_and(|found| found.to_string_lossy().contains("NVIDIA"))
        })
        .ok_or("no NVIDIA device among the loader's devices")?;
    let properties = unsafe { instance.get_physical_device_properties(physical) };
    let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy().into_owned();
    eprintln!(
        "device: {name} (Vulkan {}.{})",
        vk::api_version_major(properties.api_version),
        vk::api_version_minor(properties.api_version)
    );

    let extensions =
        unsafe { instance.enumerate_device_extension_properties(physical) }.map_err(|error| error.to_string())?;
    let mut video = Vec::new();
    for extension in &extensions {
        let found = extension
            .extension_name_as_c_str()
            .map(|found| found.to_string_lossy().into_owned())
            .unwrap_or_default();
        if found.contains("video") {
            println!("extension {found} (spec {})", extension.spec_version);
            video.push(found);
        }
    }
    let supported = |name: &str| video.iter().any(|found| found == name);
    if !supported("VK_KHR_video_queue") {
        println!("VK_KHR_video_queue is missing; a Codec2 decoder stays in software");
        return Ok(());
    }

    let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    let mut decode_family = None;
    for (index, family) in families.iter().enumerate() {
        if family.queue_flags.as_raw() & QUEUE_VIDEO_DECODE != 0 {
            println!("queue family {index}: {} video decode queue(s)", family.queue_count);
            decode_family.get_or_insert(index as u32);
        }
    }
    let Some(decode_family) = decode_family else {
        println!("no queue family carries the video decode bit despite the extension; refusing");
        return Ok(());
    };

    // A device with the video extensions enabled activates the loader's
    // functions for them (the pre-device trampoline reports
    // VK_ERROR_INITIALIZATION_FAILED otherwise); a decoder does exactly this
    // before querying capabilities.
    let want: Vec<CString> = video
        .iter()
        .filter(|found| found.starts_with("VK_KHR_video"))
        .filter_map(|found| CString::new(found.clone()).ok())
        .collect();
    let names: Vec<*const core::ffi::c_char> = want.iter().map(|name| name.as_ptr()).collect();
    let device = unsafe {
        instance
            .create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&[vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(decode_family)
                        .queue_priorities(&[1.0])])
                    .enabled_extension_names(&names),
                None,
            )
            .map_err(|error| error.to_string())?
    };
    let load = |name: &str| -> Result<*const core::ffi::c_void, String> {
        let cname = CString::new(name).map_err(|error| error.to_string())?;
        let pointer = unsafe {
            (entry.static_fn().get_instance_proc_addr)(instance.handle(), cname.as_ptr())
        }
        .ok_or(format!("{name} not exposed by the driver"))?;
        Ok(pointer as *const core::ffi::c_void)
    };
    let capabilities_fn: PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR =
        unsafe { std::mem::transmute(load("vkGetPhysicalDeviceVideoCapabilitiesKHR")?) };
    let formats_fn: PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR =
        unsafe { std::mem::transmute(load("vkGetPhysicalDeviceVideoFormatPropertiesKHR")?) };

    probe_h264(capabilities_fn, formats_fn, physical, decode_family, |name| supported(name));
    probe_h265(capabilities_fn, formats_fn, physical, decode_family, |name| supported(name));
    probe_av1(capabilities_fn, formats_fn, physical, decode_family, |name| supported(name));
    Ok(())
}

/// One video profile with a codec-specific structure chained behind it. The
/// chain borrows the caller's structure, which must outlive the query.
fn profile<C: Sized>(codec: i32, chain: &C) -> VkVideoProfileInfoKHR {
    VkVideoProfileInfoKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_PROFILE_INFO_KHR,
        pNext: (chain as *const C).cast(),
        videoCodecOperation: codec,
        chromaSubsampling: CHROMA_420,
        lumaBitDepth: BIT_DEPTH_8,
        chromaBitDepth: BIT_DEPTH_8,
    }
}

/// Fill the capability chain for one codec; `chain` carries the required
/// `VkVideoDecodeCapabilitiesKHR` followed by the codec-specific structure.
fn query_capabilities(
    function: PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR,
    physical: vk::PhysicalDevice,
    profile: &VkVideoProfileInfoKHR,
    chain: *mut core::ffi::c_void,
) -> Result<VkVideoCapabilitiesKHR, String> {
    let mut capabilities = VkVideoCapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_CAPABILITIES_KHR,
        pNext: chain,
        flags: 0,
        minBitstreamBufferOffsetAlignment: 0,
        minBitstreamBufferSizeAlignment: 0,
        pictureAccessGranularity: VkExtent2D { width: 0, height: 0 },
        minCodedExtent: VkExtent2D { width: 0, height: 0 },
        maxCodedExtent: VkExtent2D { width: 0, height: 0 },
        maxDpbSlots: 0,
        maxActiveReferencePictures: 0,
        stdHeaderVersion: VkExtensionProperties { extensionName: [0; 256], specVersion: 0 },
    };
    let result = unsafe { function(vk::Handle::as_raw(physical) as usize, profile, &mut capabilities) };
    if result != 0 {
        return Err(format!("capabilities query failed with VkResult {result}"));
    }
    Ok(capabilities)
}

/// The DPB formats a session and its reference pictures allocate from.
fn query_formats(
    function: PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR,
    physical: vk::PhysicalDevice,
    profile: &VkVideoProfileInfoKHR,
    decode_family: u32,
) -> Result<(), String> {
    // This registry revision wraps the profile in a profile list for the
    // format query.
    let list = VkVideoProfileListInfoKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR,
        pNext: std::ptr::null(),
        profileCount: 1,
        pProfiles: profile,
    };
    let info = VkPhysicalDeviceVideoFormatInfoKHR {
        sType: 1_000_023_014, // VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_FORMAT_INFO_KHR
        pNext: (&list as *const VkVideoProfileListInfoKHR).cast(),
        imageUsage: USAGE_DECODE_DPB,
    };
    let mut count = 0_u32;
    let result = unsafe { function(vk::Handle::as_raw(physical) as usize, &info, &mut count, std::ptr::null_mut()) };
    if result != 0 && result != 2 {
        // VK_INCOMPLETE (2) alongside a zero count still means "ask again".
        return Err(format!("format count query failed with VkResult {result}"));
    }
    if count == 0 {
        println!("      dpb formats: none for the decode DPB usage");
        return Ok(());
    }
    let mut properties = vec![
        VkVideoFormatPropertiesKHR {
            sType: 1_000_023_015, // VK_STRUCTURE_TYPE_VIDEO_FORMAT_PROPERTIES_KHR
            pNext: std::ptr::null_mut(),
            format: 0,
            componentMapping: VkComponentMapping { r: 0, g: 0, b: 0, a: 0 },
            imageCreateFlags: 0,
            imageType: 0,
            imageTiling: 0,
            imageUsageFlags: 0,
        };
        count as usize
    ];
    let result = unsafe { function(vk::Handle::as_raw(physical) as usize, &info, &mut count, properties.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("format query failed with VkResult {result}"));
    }
    properties.truncate(count as usize);
    for format in &properties {
        println!(
            "      dpb format {} tiling {} usage {:#x}, queue family {decode_family}",
            format.format, format.imageTiling, format.imageUsageFlags,
        );
    }
    Ok(())
}

fn probe_h264(
    capabilities_fn: PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR,
    formats_fn: PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR,
    physical: vk::PhysicalDevice,
    decode_family: u32,
    supported: impl Fn(&str) -> bool,
) {
    if !supported("VK_KHR_video_decode_h264") {
        return;
    }
    let h264 = VkVideoDecodeH264ProfileInfoKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PROFILE_INFO_KHR,
        pNext: std::ptr::null(),
        stdProfileIdc: H264_PROFILE_HIGH,
        pictureLayout: LAYOUT_PROGRESSIVE,
    };
    let profile = profile(CODEC_H264, &h264);
    let mut codec = VkVideoDecodeH264CapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_CAPABILITIES_KHR,
        pNext: std::ptr::null_mut(),
        maxLevelIdc: 0,
        fieldOffsetGranularity: VkOffset2D { x: 0, y: 0 },
    };
    let mut decode_caps = VkVideoDecodeCapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR,
        pNext: (&mut codec as *mut VkVideoDecodeH264CapabilitiesKHR).cast(),
        flags: Default::default(),
    };
    let Ok(capabilities) =
        query_capabilities(capabilities_fn, physical, &profile, &mut decode_caps as *mut _ as *mut core::ffi::c_void)
    else {
        return;
    };
    println!(
        "h264: dpb {} slots, {} active references, max coded {}x{}, std header version {}, max level {}, field granularity {}x{}",
        capabilities.maxDpbSlots,
        capabilities.maxActiveReferencePictures,
        capabilities.maxCodedExtent.width,
        capabilities.maxCodedExtent.height,
        capabilities.stdHeaderVersion.specVersion,
        codec.maxLevelIdc,
        codec.fieldOffsetGranularity.x,
        codec.fieldOffsetGranularity.y,
    );
    if let Err(error) = query_formats(formats_fn, physical, &profile, decode_family) {
        println!("      h264 formats: {error}");
    }
}

fn probe_h265(
    capabilities_fn: PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR,
    formats_fn: PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR,
    physical: vk::PhysicalDevice,
    decode_family: u32,
    supported: impl Fn(&str) -> bool,
) {
    if !supported("VK_KHR_video_decode_h265") {
        return;
    }
    let h265 = VkVideoDecodeH265ProfileInfoKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_PROFILE_INFO_KHR,
        pNext: std::ptr::null(),
        stdProfileIdc: H265_PROFILE_MAIN,
    };
    let profile = profile(CODEC_H265, &h265);
    let mut codec = VkVideoDecodeH265CapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_CAPABILITIES_KHR,
        pNext: std::ptr::null_mut(),
        maxLevelIdc: 0,
    };
    let mut decode_caps = VkVideoDecodeCapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR,
        pNext: (&mut codec as *mut VkVideoDecodeH265CapabilitiesKHR).cast(),
        flags: Default::default(),
    };
    let Ok(capabilities) =
        query_capabilities(capabilities_fn, physical, &profile, &mut decode_caps as *mut _ as *mut core::ffi::c_void)
    else {
        return;
    };
    println!(
        "h265: dpb {} slots, {} active references, max level {}, std header version {}",
        capabilities.maxDpbSlots,
        capabilities.maxActiveReferencePictures,
        codec.maxLevelIdc,
        capabilities.stdHeaderVersion.specVersion,
    );
    if let Err(error) = query_formats(formats_fn, physical, &profile, decode_family) {
        println!("      h265 formats: {error}");
    }
}

fn probe_av1(
    capabilities_fn: PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR,
    formats_fn: PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR,
    physical: vk::PhysicalDevice,
    decode_family: u32,
    supported: impl Fn(&str) -> bool,
) {
    if !supported("VK_KHR_video_decode_av1") {
        return;
    }
    let av1 = VkVideoDecodeAV1ProfileInfoKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_PROFILE_INFO_KHR,
        pNext: std::ptr::null(),
        stdProfile: 0, // StdVideoAV1Profile Main
        filmGrainSupport: 0,
    };
    let profile = profile(CODEC_AV1, &av1);
    let mut codec = VkVideoDecodeAV1CapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_CAPABILITIES_KHR,
        pNext: std::ptr::null_mut(),
        maxLevel: 0,
    };
    let mut decode_caps = VkVideoDecodeCapabilitiesKHR {
        sType: VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR,
        pNext: (&mut codec as *mut VkVideoDecodeAV1CapabilitiesKHR).cast(),
        flags: Default::default(),
    };
    let Ok(capabilities) =
        query_capabilities(capabilities_fn, physical, &profile, &mut decode_caps as *mut _ as *mut core::ffi::c_void)
    else {
        return;
    };
    println!(
        "av1: dpb {} slots, {} active references, max level {}, std header version {}",
        capabilities.maxDpbSlots,
        capabilities.maxActiveReferencePictures,
        codec.maxLevel,
        capabilities.stdHeaderVersion.specVersion,
    );
    if let Err(error) = query_formats(formats_fn, physical, &profile, decode_family) {
        println!("      av1 formats: {error}");
    }
}

use droidloom_gpu_protocol::generated::vk::{VkComponentMapping, VkExtent2D};

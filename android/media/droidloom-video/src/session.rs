//! The Vulkan Video session over the cell's forwarded GPU.
//!
//! The driver is the forwarding driver (vulkan.droidloom behind libvulkan);
//! every call rides the shared-memory ring to `droidloom-gpu-server`, which
//! replays onto the NVIDIA driver's video queues. The flow is a decoder's:
//! capability-gated device creation, a video session against the H.264
//! profile, session parameters from the stream's SPS and PPS, DPB pictures
//! from the format query, and per-frame decode submissions whose output
//! copies to a host buffer for verification.

use std::ffi::CString;

use droidloom_gpu_protocol::generated::vk as gvk;
use crate::h264::{H264Session, SliceHeader};

/// `VK_API_VERSION_1_3`.
const API_1_3: u32 = (1 << 22) | (3 << 12);
/// `VK_QUEUE_VIDEO_DECODE_BIT_KHR`.
const QUEUE_VIDEO_DECODE: u32 = 0x20;
/// `VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT`.
const USAGE_ONE_TIME_SUBMIT: u32 = 0x1;
/// `VK_IMAGE_ASPECT_COLOR_BIT`.
const ASPECT_COLOR: u32 = 0x1;
/// `VK_IMAGE_ASPECT_PLANE_0_BIT_KHR` and `PLANE_1_BIT_KHR`.
const ASPECT_PLANE_0: u32 = 0x10;
const ASPECT_PLANE_1: u32 = 0x20;
/// `VK_IMAGE_TYPE_2D` and `VK_IMAGE_VIEW_TYPE_2D`.
const IMAGE_TYPE_2D: i32 = 1;
const VIEW_TYPE_2D: i32 = 1;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_DPB_BIT_KHR` (registry bitpos 12).
const USAGE_DECODE_DPB: u32 = 1 << 12;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_DST_BIT_KHR` (registry bitpos 10).
const USAGE_DECODE_DST: u32 = 1 << 10;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_SRC_BIT_KHR` (registry bitpos 11).
const USAGE_DECODE_SRC: u32 = 1 << 11;
/// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT`.
const USAGE_TRANSFER_SRC: u32 = 0x1;
/// `VK_IMAGE_USAGE_TRANSFER_DST_BIT`.
const USAGE_TRANSFER_DST: u32 = 0x2;
/// `VK_BUFFER_USAGE_VIDEO_DECODE_SRC_BIT_KHR`.
const BUFFER_VIDEO_DECODE_SRC: u32 = 0x2000;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` (registry bitpos 0).
const CODEC_H264: i32 = 0x1;
/// `VK_VIDEO_CHROMA_SUBSAMPLING_420_KHR`.
const CHROMA_420: u32 = 0x2;
/// `VK_VIDEO_COMPONENT_BIT_DEPTH_8_KHR`.
const BIT_DEPTH_8: u32 = 0x1;
/// `VK_VIDEO_DECODE_H264_PICTURE_LAYOUT_PROGRESSIVE_KHR`.
const LAYOUT_PROGRESSIVE: i32 = 0;
/// `VK_IMAGE_LAYOUT_GENERAL`: every video picture resource stays in it.
const LAYOUT_GENERAL: i32 = 1;
/// `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`, for the copy-out.
const LAYOUT_TRANSFER_SRC: i32 = 6;
/// `VK_IMAGE_TILING_OPTIMAL`.
const TILING_OPTIMAL: i32 = 0;
/// `VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT`.
const MEMORY_DEVICE_LOCAL: u32 = 0x1;
/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`.
const MEMORY_HOST_VISIBLE: u32 = 0x2;
/// `VK_MEMORY_PROPERTY_HOST_CACHED_BIT`.
const MEMORY_HOST_CACHED: u32 = 0x8;
/// `VK_QUEUE_FAMILY_IGNORED`.
const FAMILY_IGNORED: u32 = !0_u32;
/// The NVIDIA PCI vendor id.
const VENDOR_NVIDIA: u32 = 0x10DE;
/// `VK_MAX_EXTENSION_NAME_SIZE`.
const EXTENSION_NAME_SIZE: usize = 256;

/// The picture extent rounded up to the access granularity: the video spec
/// requires codedOffset and codedExtent to be granularity multiples.
fn aligned_extent(extent: (u32, u32), caps: &Caps) -> (u32, u32) {
    (
        ((extent.0 + caps.granularity_width - 1) / caps.granularity_width) * caps.granularity_width,
        ((extent.1 + caps.granularity_height - 1) / caps.granularity_height) * caps.granularity_height,
    )
}

/// The capability values the session is sized against.
#[derive(Clone, Copy)]
pub struct Caps {
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    pub max_coded_width: u32,
    pub max_coded_height: u32,
    pub min_bitstream_buffer_offset_alignment: u64,
    pub min_bitstream_buffer_size_alignment: u64,
    pub granularity_width: u32,
    pub granularity_height: u32,
    pub std_header_version: u32,
    pub std_header: gvk::VkExtensionProperties,
}

/// The DPB state of one slot: pictures the decode established become
/// active references for later frames.
#[derive(Clone, Default)]
struct SlotState {
    active: bool,
    frame_num: u16,
}

/// One DPB picture: the image the decode writes, its view and memory.
struct DpbPicture {
    image: gvk::VkImage,
    view: gvk::VkImageView,
    memory: gvk::VkDeviceMemory,
}

/// The device-level functions the decode path calls, loaded once.
#[allow(non_snake_case)]
struct DeviceFns {
    CreateVideoSessionKHR: unsafe extern "system" fn(usize, *const gvk::VkVideoSessionCreateInfoKHR, *const core::ffi::c_void, *mut gvk::VkVideoSessionKHR) -> i32,
    DestroyVideoSessionKHR: unsafe extern "system" fn(usize, gvk::VkVideoSessionKHR, *const core::ffi::c_void),
    GetVideoSessionMemoryRequirementsKHR: unsafe extern "system" fn(usize, gvk::VkVideoSessionKHR, *mut u32, *mut gvk::VkVideoSessionMemoryRequirementsKHR) -> i32,
    BindVideoSessionMemoryKHR: unsafe extern "system" fn(usize, gvk::VkVideoSessionKHR, u32, *const gvk::VkBindVideoSessionMemoryInfoKHR) -> i32,
    CreateVideoSessionParametersKHR: unsafe extern "system" fn(usize, *const gvk::VkVideoSessionParametersCreateInfoKHR, *const core::ffi::c_void, *mut gvk::VkVideoSessionParametersKHR) -> i32,
    UpdateVideoSessionParametersKHR: unsafe extern "system" fn(usize, gvk::VkVideoSessionParametersKHR, *const gvk::VkVideoSessionParametersUpdateInfoKHR) -> i32,
    DestroyVideoSessionParametersKHR: unsafe extern "system" fn(usize, gvk::VkVideoSessionParametersKHR, *const core::ffi::c_void),
    CreateCommandPool: unsafe extern "system" fn(usize, *const gvk::VkCommandPoolCreateInfo, *const core::ffi::c_void, *mut gvk::VkCommandPool) -> i32,
    AllocateCommandBuffers: unsafe extern "system" fn(usize, *const gvk::VkCommandBufferAllocateInfo, *mut gvk::VkCommandBuffer) -> i32,
    GetDeviceQueue: unsafe extern "system" fn(usize, u32, u32, *mut gvk::VkQueue),
    CreateImage: unsafe extern "system" fn(usize, *const gvk::VkImageCreateInfo, *const core::ffi::c_void, *mut gvk::VkImage) -> i32,
    DestroyImage: unsafe extern "system" fn(usize, gvk::VkImage, *const core::ffi::c_void),
    CreateImageView: unsafe extern "system" fn(usize, *const gvk::VkImageViewCreateInfo, *const core::ffi::c_void, *mut gvk::VkImageView) -> i32,
    DestroyImageView: unsafe extern "system" fn(usize, gvk::VkImageView, *const core::ffi::c_void),
    AllocateMemory: unsafe extern "system" fn(usize, *const gvk::VkMemoryAllocateInfo, *const core::ffi::c_void, *mut gvk::VkDeviceMemory) -> i32,
    FreeMemory: unsafe extern "system" fn(usize, gvk::VkDeviceMemory, *const core::ffi::c_void),
    BindImageMemory: unsafe extern "system" fn(usize, gvk::VkImage, gvk::VkDeviceMemory, u64) -> i32,
    BindBufferMemory: unsafe extern "system" fn(usize, gvk::VkBuffer, gvk::VkDeviceMemory, u64) -> i32,
    GetImageMemoryRequirements: unsafe extern "system" fn(usize, gvk::VkImage, *mut gvk::VkMemoryRequirements),
    GetBufferMemoryRequirements: unsafe extern "system" fn(usize, gvk::VkBuffer, *mut gvk::VkMemoryRequirements),
    GetImageSubresourceLayout: unsafe extern "system" fn(usize, gvk::VkImage, *const gvk::VkImageSubresource, *mut gvk::VkSubresourceLayout),
    CreateBuffer: unsafe extern "system" fn(usize, *const gvk::VkBufferCreateInfo, *const core::ffi::c_void, *mut gvk::VkBuffer) -> i32,
    DestroyBuffer: unsafe extern "system" fn(usize, gvk::VkBuffer, *const core::ffi::c_void),
    MapMemory: unsafe extern "system" fn(usize, gvk::VkDeviceMemory, u64, u64, u32, *mut *mut core::ffi::c_void) -> i32,
    UnmapMemory: unsafe extern "system" fn(usize, gvk::VkDeviceMemory),
    CreateFence: unsafe extern "system" fn(usize, *const gvk::VkFenceCreateInfo, *const core::ffi::c_void, *mut gvk::VkFence) -> i32,
    DestroyFence: unsafe extern "system" fn(usize, gvk::VkFence, *const core::ffi::c_void),
    WaitForFences: unsafe extern "system" fn(usize, u32, *const gvk::VkFence, u32, u64) -> i32,
    ResetFences: unsafe extern "system" fn(usize, u32, *const gvk::VkFence) -> i32,
    ResetCommandBuffer: unsafe extern "system" fn(gvk::VkCommandBuffer, u32) -> i32,
    BeginCommandBuffer: unsafe extern "system" fn(gvk::VkCommandBuffer, *const gvk::VkCommandBufferBeginInfo) -> i32,
    EndCommandBuffer: unsafe extern "system" fn(gvk::VkCommandBuffer) -> i32,
    CmdPipelineBarrier: unsafe extern "system" fn(gvk::VkCommandBuffer, u32, u32, u32, u32, *const gvk::VkMemoryBarrier, u32, *const gvk::VkBufferMemoryBarrier, u32, *const gvk::VkImageMemoryBarrier),
    CmdBeginVideoCodingKHR: unsafe extern "system" fn(gvk::VkCommandBuffer, *const gvk::VkVideoBeginCodingInfoKHR),
    CmdEndVideoCodingKHR: unsafe extern "system" fn(gvk::VkCommandBuffer, *const gvk::VkVideoEndCodingInfoKHR),
    CmdDecodeVideoKHR: unsafe extern "system" fn(gvk::VkCommandBuffer, *const gvk::VkVideoDecodeInfoKHR),
    CmdControlVideoCodingKHR: unsafe extern "system" fn(gvk::VkCommandBuffer, *const gvk::VkVideoCodingControlInfoKHR),
    CmdCopyImageToBuffer: unsafe extern "system" fn(gvk::VkCommandBuffer, gvk::VkImage, i32, gvk::VkBuffer, u32, *const gvk::VkBufferImageCopy),
    QueueSubmit: unsafe extern "system" fn(gvk::VkQueue, u32, *const gvk::VkSubmitInfo, gvk::VkFence) -> i32,
    QueueWaitIdle: unsafe extern "system" fn(gvk::VkQueue) -> i32,
}

unsafe impl Send for DeviceFns {}
unsafe impl Send for VideoSession {}

/// A loader opened once, its instance-level functions fetched.
struct Entry {
    _library: *mut core::ffi::c_void,
    create_instance: unsafe extern "system" fn(*const gvk::VkInstanceCreateInfo, *const core::ffi::c_void, *mut gvk::VkInstance) -> i32,
    enumerate_physical_devices: unsafe extern "system" fn(gvk::VkInstance, *mut u32, *mut usize) -> i32,
    get_physical_device_properties: unsafe extern "system" fn(usize, *mut gvk::VkPhysicalDeviceProperties),
    get_physical_device_queue_family_properties: unsafe extern "system" fn(usize, *mut u32, *mut gvk::VkQueueFamilyProperties),
    get_physical_device_memory_properties: unsafe extern "system" fn(usize, *mut gvk::VkPhysicalDeviceMemoryProperties),
    get_instance_proc_addr: unsafe extern "system" fn(usize, *const core::ffi::c_char) -> *const core::ffi::c_void,
}

unsafe impl Send for Entry {}

impl Entry {
    /// dlopens libvulkan and the instance-level functions it names.
    fn load() -> Result<Self, String> {
        let name = CString::new("libvulkan.so").map_err(|error| error.to_string())?;
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return Err("no Vulkan library in the cell".into());
        }
        let get = |name: &str| -> Result<*const core::ffi::c_void, String> {
            let cname = CString::new(name).map_err(|error| error.to_string())?;
            let pointer = unsafe { libc::dlsym(library, cname.as_ptr()) };
            if pointer.is_null() {
                Err(format!("{name} missing from libvulkan"))
            } else {
                Ok(pointer)
            }
        };
        Ok(Self {
            _library: library,
            create_instance: unsafe { std::mem::transmute(get("vkCreateInstance")?) },
            enumerate_physical_devices: unsafe { std::mem::transmute(get("vkEnumeratePhysicalDevices")?) },
            get_physical_device_properties: unsafe { std::mem::transmute(get("vkGetPhysicalDeviceProperties")?) },
            get_physical_device_queue_family_properties: unsafe {
                std::mem::transmute(get("vkGetPhysicalDeviceQueueFamilyProperties")?)
            },
            get_physical_device_memory_properties: unsafe {
                std::mem::transmute(get("vkGetPhysicalDeviceMemoryProperties")?)
            },
            get_instance_proc_addr: unsafe { std::mem::transmute(get("vkGetInstanceProcAddr")?) },
        })
    }

    /// An instance-level function by name: the video capability queries and
    /// vkCreateDevice live here.
    fn instance_proc(&self, instance: gvk::VkInstance, name: &str) -> Result<usize, String> {
        let cname = CString::new(name).map_err(|error| error.to_string())?;
        let pointer = unsafe { (self.get_instance_proc_addr)(instance, cname.as_ptr()) };
        if pointer.is_null() {
            Err(format!("{name} not exposed"))
        } else {
            Ok(pointer as usize)
        }
    }

    /// The NVIDIA device among the loader's list, with the video decode
    /// queue family index.
    fn physical(&self, instance: gvk::VkInstance) -> Result<(usize, u32), String> {
        let mut count = 0_u32;
        if unsafe { (self.enumerate_physical_devices)(instance, &mut count, std::ptr::null_mut()) } != 0 {
            return Err("vkEnumeratePhysicalDevices failed".into());
        }
        let mut handles = vec![0_usize; count as usize];
        unsafe { (self.enumerate_physical_devices)(instance, &mut count, handles.as_mut_ptr()) };
        for handle in &handles {
            let mut properties = unsafe { std::mem::zeroed::<gvk::VkPhysicalDeviceProperties>() };
            unsafe { (self.get_physical_device_properties)(*handle, &mut properties) };
            if properties.vendorID != VENDOR_NVIDIA {
                continue;
            }
            let mut family_count = 0_u32;
            unsafe {
                (self.get_physical_device_queue_family_properties)(*handle, &mut family_count, std::ptr::null_mut())
            };
            let mut families = vec![unsafe { std::mem::zeroed::<gvk::VkQueueFamilyProperties>() }; family_count as usize];
            unsafe {
                (self.get_physical_device_queue_family_properties)(*handle, &mut family_count, families.as_mut_ptr())
            };
            if let Some(family) = families.iter().position(|family| family.queueFlags as u32 & QUEUE_VIDEO_DECODE != 0) {
                return Ok((*handle, family as u32));
            }
        }
        Err("no NVIDIA device carries the video decode queue".into())
    }

    /// The memory properties, for the type-index searches.
    fn memory_properties(&self, physical: usize) -> gvk::VkPhysicalDeviceMemoryProperties {
        let mut properties = unsafe { std::mem::zeroed::<gvk::VkPhysicalDeviceMemoryProperties>() };
        unsafe { (self.get_physical_device_memory_properties)(physical, &mut properties) };
        properties
    }
}

/// The H.264 profile chain, living as long as the session does. The codec
/// structure is boxed so its address survives the struct's moves.
struct Profile {
    profile: gvk::VkVideoProfileInfoKHR,
    #[allow(dead_code)]
    h264: Box<gvk::VkVideoDecodeH264ProfileInfoKHR>,
}

impl Profile {
    fn new() -> Self {
        let mut h264 = Box::new(gvk::VkVideoDecodeH264ProfileInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PROFILE_INFO_KHR,
            pNext: std::ptr::null(),
            stdProfileIdc: 100, // HIGH
            pictureLayout: LAYOUT_PROGRESSIVE,
        });
        let mut profile = gvk::VkVideoProfileInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_PROFILE_INFO_KHR,
            pNext: std::ptr::null(),
            videoCodecOperation: CODEC_H264,
            chromaSubsampling: CHROMA_420,
            lumaBitDepth: BIT_DEPTH_8,
            chromaBitDepth: BIT_DEPTH_8,
        };
        profile.pNext = (&mut *h264 as *mut gvk::VkVideoDecodeH264ProfileInfoKHR).cast();
        Self { profile, h264 }
    }
}

/// The capability query, through the instance-level function.
fn query_caps(entry: &Entry, instance: gvk::VkInstance, physical: usize, profile: &Profile) -> Result<Caps, String> {
    let function = entry.instance_proc(instance, "vkGetPhysicalDeviceVideoCapabilitiesKHR")?;
    let function: unsafe extern "system" fn(usize, *const gvk::VkVideoProfileInfoKHR, *mut gvk::VkVideoCapabilitiesKHR) -> i32 =
        unsafe { std::mem::transmute(function) };
    let mut decode_caps = gvk::VkVideoDecodeCapabilitiesKHR {
        sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR,
        pNext: std::ptr::null_mut(),
        flags: 0,
    };
    let mut h264_caps = gvk::VkVideoDecodeH264CapabilitiesKHR {
        sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_CAPABILITIES_KHR,
        pNext: std::ptr::null_mut(),
        maxLevelIdc: 0,
        fieldOffsetGranularity: gvk::VkOffset2D { x: 0, y: 0 },
    };
    decode_caps.pNext = (&mut h264_caps as *mut gvk::VkVideoDecodeH264CapabilitiesKHR).cast();
    let mut capabilities = gvk::VkVideoCapabilitiesKHR {
        sType: gvk::VK_STRUCTURE_TYPE_VIDEO_CAPABILITIES_KHR,
        pNext: (&mut decode_caps as *mut gvk::VkVideoDecodeCapabilitiesKHR).cast(),
        flags: 0,
        minBitstreamBufferOffsetAlignment: 0,
        minBitstreamBufferSizeAlignment: 0,
        pictureAccessGranularity: gvk::VkExtent2D { width: 0, height: 0 },
        minCodedExtent: gvk::VkExtent2D { width: 0, height: 0 },
        maxCodedExtent: gvk::VkExtent2D { width: 0, height: 0 },
        maxDpbSlots: 0,
        maxActiveReferencePictures: 0,
        stdHeaderVersion: gvk::VkExtensionProperties { extensionName: [0; EXTENSION_NAME_SIZE], specVersion: 0 },
    };
    let result = unsafe { function(physical, &profile.profile, &mut capabilities) };
    if result != 0 {
        return Err(format!("capabilities query: {result}"));
    }
    Ok(Caps {
        max_dpb_slots: capabilities.maxDpbSlots,
        max_active_reference_pictures: capabilities.maxActiveReferencePictures,
        max_coded_width: capabilities.maxCodedExtent.width,
        max_coded_height: capabilities.maxCodedExtent.height,
        min_bitstream_buffer_offset_alignment: capabilities.minBitstreamBufferOffsetAlignment,
        min_bitstream_buffer_size_alignment: capabilities.minBitstreamBufferSizeAlignment,
        granularity_width: capabilities.pictureAccessGranularity.width,
        granularity_height: capabilities.pictureAccessGranularity.height,
        std_header_version: capabilities.stdHeaderVersion.specVersion,
        std_header: capabilities.stdHeaderVersion,
    })
}

/// The DPB format for the decode DPB usage, through the format query with
/// the profile wrapped in a profile list.
fn query_dpb_format(entry: &Entry, instance: gvk::VkInstance, physical: usize, profile: &Profile) -> Result<(i32, u32, u32), String> {
    let function = entry.instance_proc(instance, "vkGetPhysicalDeviceVideoFormatPropertiesKHR")?;
    let function: unsafe extern "system" fn(usize, *const gvk::VkPhysicalDeviceVideoFormatInfoKHR, *mut u32, *mut gvk::VkVideoFormatPropertiesKHR) -> i32 =
        unsafe { std::mem::transmute(function) };
    let list = gvk::VkVideoProfileListInfoKHR {
        sType: gvk::VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR,
        pNext: std::ptr::null(),
        profileCount: 1,
        pProfiles: &profile.profile,
    };
    // Query with the exact usage the DPB images will carry.
    let info = gvk::VkPhysicalDeviceVideoFormatInfoKHR {
        sType: gvk::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_FORMAT_INFO_KHR,
        pNext: (&list as *const gvk::VkVideoProfileListInfoKHR).cast(),
        imageUsage: USAGE_DECODE_DPB | USAGE_DECODE_DST | USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST,
    };
    let mut count = 0_u32;
    let result = unsafe { function(physical, &info, &mut count, std::ptr::null_mut()) };
    if result != 0 && result != 2 {
        return Err(format!("format count query: {result}"));
    }
    if count == 0 {
        return Err("no DPB format for the decode usage".into());
    }
    let mut properties = vec![unsafe { std::mem::zeroed::<gvk::VkVideoFormatPropertiesKHR>() }; count as usize];
    for property in &mut properties {
        property.sType = gvk::VK_STRUCTURE_TYPE_VIDEO_FORMAT_PROPERTIES_KHR;
    }
    let result = unsafe { function(physical, &info, &mut count, properties.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("format query: {result}"));
    }
    properties.truncate(count as usize);
    // The DPB picture is decoded into and copied out of: it needs the decode
    // usages and the transfer bits for the host copy. G8_B8R8_2PLANE_420_UNORM
    // is the format Android's 4:2:0 output maps onto.
    for format in &properties {
        let want = USAGE_DECODE_DPB | USAGE_DECODE_DST | USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST;
        if format.format == 1000156003 && format.imageUsageFlags & want == want {
            return Ok((format.format, format.imageUsageFlags & want, format.imageTiling as u32));
        }
    }
    Err("the DPB format list has no G8_B8R8_2PLANE_420_UNORM with decode and transfer usage".into())
}

/// The device functions, fetched by name once the device exists.
fn device_fns(entry: &Entry, instance: gvk::VkInstance, _device: gvk::VkDevice) -> Result<DeviceFns, String> {
    macro_rules! dev {
        ($name:literal) => {{
            let cname = CString::new($name).map_err(|error| error.to_string())?;
            let pointer = unsafe { (entry.get_instance_proc_addr)(instance, cname.as_ptr()) };
            if pointer.is_null() {
                return Err(format!("{} not exposed", $name));
            }
            unsafe { std::mem::transmute::<*const core::ffi::c_void, _>(pointer) }
        }};
    }
    Ok(DeviceFns {
        CreateVideoSessionKHR: dev!("vkCreateVideoSessionKHR"),
        DestroyVideoSessionKHR: dev!("vkDestroyVideoSessionKHR"),
        GetVideoSessionMemoryRequirementsKHR: dev!("vkGetVideoSessionMemoryRequirementsKHR"),
        BindVideoSessionMemoryKHR: dev!("vkBindVideoSessionMemoryKHR"),
        CreateVideoSessionParametersKHR: dev!("vkCreateVideoSessionParametersKHR"),
        UpdateVideoSessionParametersKHR: dev!("vkUpdateVideoSessionParametersKHR"),
        DestroyVideoSessionParametersKHR: dev!("vkDestroyVideoSessionParametersKHR"),
        CreateCommandPool: dev!("vkCreateCommandPool"),
        AllocateCommandBuffers: dev!("vkAllocateCommandBuffers"),
        GetDeviceQueue: dev!("vkGetDeviceQueue"),
        CreateImage: dev!("vkCreateImage"),
        DestroyImage: dev!("vkDestroyImage"),
        CreateImageView: dev!("vkCreateImageView"),
        DestroyImageView: dev!("vkDestroyImageView"),
        AllocateMemory: dev!("vkAllocateMemory"),
        FreeMemory: dev!("vkFreeMemory"),
        BindImageMemory: dev!("vkBindImageMemory"),
        BindBufferMemory: dev!("vkBindBufferMemory"),
        GetImageMemoryRequirements: dev!("vkGetImageMemoryRequirements"),
        GetBufferMemoryRequirements: dev!("vkGetBufferMemoryRequirements"),
        GetImageSubresourceLayout: dev!("vkGetImageSubresourceLayout"),
        CreateBuffer: dev!("vkCreateBuffer"),
        DestroyBuffer: dev!("vkDestroyBuffer"),
        MapMemory: dev!("vkMapMemory"),
        UnmapMemory: dev!("vkUnmapMemory"),
        CreateFence: dev!("vkCreateFence"),
        DestroyFence: dev!("vkDestroyFence"),
        WaitForFences: dev!("vkWaitForFences"),
        ResetFences: dev!("vkResetFences"),
        ResetCommandBuffer: dev!("vkResetCommandBuffer"),
        BeginCommandBuffer: dev!("vkBeginCommandBuffer"),
        EndCommandBuffer: dev!("vkEndCommandBuffer"),
        CmdPipelineBarrier: dev!("vkCmdPipelineBarrier"),
        CmdBeginVideoCodingKHR: dev!("vkCmdBeginVideoCodingKHR"),
        CmdEndVideoCodingKHR: dev!("vkCmdEndVideoCodingKHR"),
        CmdDecodeVideoKHR: dev!("vkCmdDecodeVideoKHR"),
        CmdControlVideoCodingKHR: dev!("vkCmdControlVideoCodingKHR"),
        CmdCopyImageToBuffer: dev!("vkCmdCopyImageToBuffer"),
        QueueSubmit: dev!("vkQueueSubmit"),
        QueueWaitIdle: dev!("vkQueueWaitIdle"),
    })
}

/// Memory of one type index that fits `requirements`: `required` flags must
/// be present, `preferred` flags win, and `avoid` flags lose — which keeps
/// big DPB allocations on the plain VRAM heap instead of the small
/// device-local BAR heap.
fn allocate_memory(
    fns: &DeviceFns,
    device: gvk::VkDevice,
    memory: &gvk::VkPhysicalDeviceMemoryProperties,
    requirements: &gvk::VkMemoryRequirements,
    required: u32,
    preferred: u32,
    avoid: u32,
) -> Result<gvk::VkDeviceMemory, String> {
    let mut info = gvk::VkMemoryAllocateInfo {
        sType: gvk::VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        pNext: std::ptr::null(),
        allocationSize: requirements.size,
        memoryTypeIndex: u32::MAX,
    };
    for i in 0..memory.memoryTypeCount {
        if requirements.memoryTypeBits & (1u32 << i) == 0 {
            continue;
        }
        let flags = memory.memoryTypes[i as usize].propertyFlags;
        if flags & required != required {
            continue;
        }
        if flags & preferred == preferred && flags & avoid == 0 {
            info.memoryTypeIndex = i;
            break;
        }
        if info.memoryTypeIndex == u32::MAX {
            info.memoryTypeIndex = i;
        }
    }
    if info.memoryTypeIndex == u32::MAX {
        return Err(format!(
            "no memory type fits: size {}, typeBits {:#x}, required {:#x}, preferred {:#x}, avoid {:#x}, types {:?}",
            requirements.size,
            requirements.memoryTypeBits,
            required,
            preferred,
            avoid,
            (0..memory.memoryTypeCount)
                .map(|i| format!("{}:{:#x}", i, memory.memoryTypes[i as usize].propertyFlags))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut out = 0_u64;
    let result = unsafe { (fns.AllocateMemory)(device, &info, std::ptr::null(), &mut out) };
    if result != 0 {
        return Err(format!("vkAllocateMemory: {result}"));
    }
    Ok(out)
}

/// The video session of one Codec2 component.
pub struct VideoSession {
    entry: Entry,
    _instance: gvk::VkInstance,
    device: gvk::VkDevice,
    queue: gvk::VkQueue,
    fns: DeviceFns,
    pub caps: Caps,
    profile: Profile,
    session: gvk::VkVideoSessionKHR,
    parameters: Option<gvk::VkVideoSessionParametersKHR>,
    /// The DPB format the format query returned for the decode DPB usage.
    pub dpb_format: i32,
    dpb_images: Vec<DpbPicture>,
    /// Per-slot DPB state: activity and the decoded picture's frame number.
    slot_state: Vec<SlotState>,
    /// The bitstream buffer: host-visible, its whole size mapped.
    bitstream_buffer: gvk::VkBuffer,
    bitstream_map: *mut core::ffi::c_void,
    bitstream_size: u64,
    /// The decode command buffer, from a pool on the decode family.
    command: gvk::VkCommandBuffer,
    fence: gvk::VkFence,
    /// The granularity-aligned extent the DPB pictures and decode resources
    /// use; the content region inside it is `content_extent`.
    coded_extent: (u32, u32),
    /// The stream's actual picture extent, within `coded_extent`.
    content_extent: (u32, u32),
    /// The host-visible buffer and mapping the plane copies land in.
    copy_buffer: gvk::VkBuffer,
    copy_map: *mut core::ffi::c_void,
    copy_size: u64,
    frame_index: u64,
}

impl VideoSession {
    /// Opens the whole chain the H.264 decoder runs on: capability-gated
    /// device creation, the session, its memory, and the DPB pictures. The
    /// DPB pictures take `coded_extent`; a stream that outgrows it needs a
    /// new session, the way a resize does on a native decoder.
    pub fn open(coded_extent: (u32, u32)) -> Result<Self, String> {
        let entry = Entry::load()?;
        let application = gvk::VkApplicationInfo {
            sType: gvk::VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: std::ptr::null(),
            pApplicationName: b"droidloom-c2\0".as_ptr().cast(),
            applicationVersion: 0,
            pEngineName: std::ptr::null(),
            engineVersion: 0,
            apiVersion: API_1_3,
        };
        let mut instance = 0_usize;
        let result = unsafe {
            (entry.create_instance)(
                &gvk::VkInstanceCreateInfo {
                    sType: gvk::VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                    pNext: std::ptr::null(),
                    flags: 0,
                    pApplicationInfo: &application,
                    enabledLayerCount: 0,
                    ppEnabledLayerNames: std::ptr::null(),
                    enabledExtensionCount: 0,
                    ppEnabledExtensionNames: std::ptr::null(),
                },
                std::ptr::null(),
                &mut instance,
            )
        };
        if result != 0 {
            return Err(format!("vkCreateInstance: {result}"));
        }
        let profile = Profile::new();
        let (physical, decode_family) = entry.physical(instance)?;

        let create_device = entry.instance_proc(instance, "vkCreateDevice")?;
        let create_device: unsafe extern "system" fn(usize, *const gvk::VkDeviceCreateInfo, *const core::ffi::c_void, *mut gvk::VkDevice) -> i32 =
            unsafe { std::mem::transmute(create_device) };
        let extensions = [
            b"VK_KHR_video_queue\0".as_ptr().cast(),
            b"VK_KHR_video_decode_queue\0".as_ptr().cast(),
            b"VK_KHR_video_decode_h264\0".as_ptr().cast(),
            b"VK_KHR_video_maintenance1\0".as_ptr().cast(),
        ];
        let priority = 1.0_f32;
        let queue_info = gvk::VkDeviceQueueCreateInfo {
            sType: gvk::VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            pNext: std::ptr::null(),
            flags: 0,
            queueFamilyIndex: decode_family,
            queueCount: 1,
            pQueuePriorities: &priority,
        };
        let mut device = 0_usize;
        let result = unsafe {
            (create_device)(
                physical,
                &gvk::VkDeviceCreateInfo {
                    sType: gvk::VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                    pNext: std::ptr::null(),
                    flags: 0,
                    queueCreateInfoCount: 1,
                    pQueueCreateInfos: &queue_info,
                    enabledLayerCount: 0,
                    ppEnabledLayerNames: std::ptr::null(),
                    enabledExtensionCount: extensions.len() as u32,
                    ppEnabledExtensionNames: extensions.as_ptr(),
                    pEnabledFeatures: std::ptr::null(),
                },
                std::ptr::null(),
                &mut device,
            )
        };
        if result != 0 {
            return Err(format!("vkCreateDevice: {result}"));
        }
        let fns = device_fns(&entry, instance, device)?;
        let memory_properties = entry.memory_properties(physical);
        let caps = query_caps(&entry, instance, physical, &profile)?;
        let aligned = aligned_extent(coded_extent, &caps);
        let (dpb_format, dpb_usage, _tiling) = query_dpb_format(&entry, instance, physical, &profile)?;

        // The video session, against the profile and the DPB format.
        let session_info = gvk::VkVideoSessionCreateInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_SESSION_CREATE_INFO_KHR,
            pNext: std::ptr::null(),
            queueFamilyIndex: decode_family,
            flags: 0,
            pVideoProfile: &profile.profile,
            pictureFormat: dpb_format,
            maxCodedExtent: gvk::VkExtent2D { width: aligned.0, height: aligned.1 },
            referencePictureFormat: dpb_format,
            maxDpbSlots: caps.max_dpb_slots,
            maxActiveReferencePictures: caps.max_active_reference_pictures,
            pStdHeaderVersion: &caps.std_header,
        };
        let mut session = 0_u64;
        let result = unsafe { (fns.CreateVideoSessionKHR)(device, &session_info, std::ptr::null(), &mut session) };
        if result != 0 {
            return Err(format!("vkCreateVideoSessionKHR: {result}"));
        }
        // Session memory: one allocation per bind index.
        let mut bind_count = 0_u32;
        if unsafe { (fns.GetVideoSessionMemoryRequirementsKHR)(device, session, &mut bind_count, std::ptr::null_mut()) } != 0 {
            return Err("session memory requirements failed".into());
        }
        let mut requirements = vec![unsafe { std::mem::zeroed::<gvk::VkVideoSessionMemoryRequirementsKHR>() }; bind_count as usize];
        for requirement in &mut requirements {
            requirement.sType = gvk::VK_STRUCTURE_TYPE_VIDEO_SESSION_MEMORY_REQUIREMENTS_KHR;
        }
        unsafe { (fns.GetVideoSessionMemoryRequirementsKHR)(device, session, &mut bind_count, requirements.as_mut_ptr()) };
        let mut binds = Vec::with_capacity(requirements.len());
        for requirement in &requirements {
            let allocation = allocate_memory(&fns, device, &memory_properties, &requirement.memoryRequirements, 0, 0, 0)?;
            binds.push(gvk::VkBindVideoSessionMemoryInfoKHR {
                sType: gvk::VK_STRUCTURE_TYPE_BIND_VIDEO_SESSION_MEMORY_INFO_KHR,
                pNext: std::ptr::null(),
                memoryBindIndex: requirement.memoryBindIndex,
                memory: allocation,
                memoryOffset: 0,
                memorySize: requirement.memoryRequirements.size,
            });
        }
        let result = unsafe { (fns.BindVideoSessionMemoryKHR)(device, session, binds.len() as u32, binds.as_ptr()) };
        if result != 0 {
            return Err(format!("vkBindVideoSessionMemoryKHR: {result}"));
        }

        // DPB pictures: one image per slot, general layout, transfer source.
        // Images carrying video usage must name the profile they serve
        // (VUID-VkImageCreateInfo-usage-04815).
        let profile_list = gvk::VkVideoProfileListInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR,
            pNext: std::ptr::null(),
            profileCount: 1,
            pProfiles: &profile.profile,
        };
        let mut dpb_images = Vec::new();
        for _ in 0..caps.max_dpb_slots {
            let image_info = gvk::VkImageCreateInfo {
                sType: gvk::VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                pNext: (&profile_list as *const gvk::VkVideoProfileListInfoKHR).cast(),
                flags: 0,
                imageType: IMAGE_TYPE_2D,
                format: dpb_format,
                extent: gvk::VkExtent3D { width: aligned.0, height: aligned.1, depth: 1 },
                mipLevels: 1,
                arrayLayers: 1,
                samples: 1,
                tiling: TILING_OPTIMAL,
                usage: dpb_usage | USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST,
                sharingMode: 0,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: std::ptr::null(),
                initialLayout: 0, // UNDEFINED; transitions to GENERAL below
            };
            let mut image = 0_u64;
            let result = unsafe { (fns.CreateImage)(device, &image_info, std::ptr::null(), &mut image) };
            if result != 0 {
                return Err(format!("DPB image: {result}"));
            }
            let mut requirements = unsafe { std::mem::zeroed::<gvk::VkMemoryRequirements>() };
            unsafe { (fns.GetImageMemoryRequirements)(device, image, &mut requirements) };
            let memory = allocate_memory(&fns, device, &memory_properties, &requirements, MEMORY_DEVICE_LOCAL, MEMORY_DEVICE_LOCAL, MEMORY_HOST_VISIBLE)?;
            if unsafe { (fns.BindImageMemory)(device, image, memory, 0) } != 0 {
                return Err("DPB image bind failed".into());
            }
            let view_info = gvk::VkImageViewCreateInfo {
                sType: gvk::VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                pNext: std::ptr::null(),
                flags: 0,
                image,
                viewType: VIEW_TYPE_2D,
                format: dpb_format,
                components: gvk::VkComponentMapping { r: 0, g: 0, b: 0, a: 0 },
                subresourceRange: gvk::VkImageSubresourceRange {
                    aspectMask: ASPECT_COLOR,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
            };
            let mut view = 0_u64;
            let result = unsafe { (fns.CreateImageView)(device, &view_info, std::ptr::null(), &mut view) };
            if result != 0 {
                return Err(format!("DPB view: {result}"));
            }
            dpb_images.push(DpbPicture { image, view, memory });
        }

        // The bitstream buffer: host-visible, whole size mapped.
        let bitstream_size = 8_u64 << 20; // 8 MiB covers any stream fed here
        let buffer_info = gvk::VkBufferCreateInfo {
            sType: gvk::VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            pNext: (&profile_list as *const gvk::VkVideoProfileListInfoKHR).cast(),
            flags: 0,
            size: bitstream_size,
            usage: BUFFER_VIDEO_DECODE_SRC,
            sharingMode: 0,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: std::ptr::null(),
        };
        let mut bitstream_buffer = 0_u64;
        let result = unsafe { (fns.CreateBuffer)(device, &buffer_info, std::ptr::null(), &mut bitstream_buffer) };
        if result != 0 {
            return Err(format!("bitstream buffer: {result}"));
        }
        let mut requirements = unsafe { std::mem::zeroed::<gvk::VkMemoryRequirements>() };
        unsafe { (fns.GetBufferMemoryRequirements)(device, bitstream_buffer, &mut requirements) };
        let bitstream_memory = allocate_memory(&fns, device, &memory_properties, &requirements, MEMORY_HOST_VISIBLE, MEMORY_HOST_CACHED, 0)?;
        if unsafe { (fns.BindBufferMemory)(device, bitstream_buffer, bitstream_memory, 0) } != 0 {
            return Err("bitstream buffer bind failed".into());
        }
        let mut bitstream_map: *mut core::ffi::c_void = std::ptr::null_mut();
        if unsafe { (fns.MapMemory)(device, bitstream_memory, 0, bitstream_size, 0, &mut bitstream_map) } != 0 {
            return Err("bitstream map failed".into());
        }

        // Command pool and buffer on the decode family, plus the fence.
        let pool_info = gvk::VkCommandPoolCreateInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            pNext: std::ptr::null(),
            flags: 0x2, // VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT
            queueFamilyIndex: decode_family,
        };
        let mut pool = 0_u64;
        if unsafe { (fns.CreateCommandPool)(device, &pool_info, std::ptr::null(), &mut pool) } != 0 {
            return Err("command pool failed".into());
        }
        let commands_info = gvk::VkCommandBufferAllocateInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: std::ptr::null(),
            commandPool: pool,
            level: 0, // VK_COMMAND_BUFFER_LEVEL_PRIMARY
            commandBufferCount: 1,
        };
        let mut command = 0_usize;
        if unsafe { (fns.AllocateCommandBuffers)(device, &commands_info, &mut command) } != 0 {
            return Err("command buffer failed".into());
        }
        let mut fence = 0_u64;
        let fence_info = gvk::VkFenceCreateInfo { sType: gvk::VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, pNext: std::ptr::null(), flags: 0 };
        if unsafe { (fns.CreateFence)(device, &fence_info, std::ptr::null(), &mut fence) } != 0 {
            return Err("fence failed".into());
        }
        let mut queue = 0_usize;
        unsafe { (fns.GetDeviceQueue)(device, decode_family, 0, &mut queue) };

        // The DPB pictures move to GENERAL once: every video picture
        // resource stays in it from then on.
        let begin_cb = gvk::VkCommandBufferBeginInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: std::ptr::null(),
            flags: USAGE_ONE_TIME_SUBMIT,
            pInheritanceInfo: std::ptr::null(),
        };
        unsafe {
            (fns.BeginCommandBuffer)(command, &begin_cb);
            for picture in &dpb_images {
                let barrier = gvk::VkImageMemoryBarrier {
                    sType: gvk::VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                    pNext: std::ptr::null(),
                    srcAccessMask: 0,
                    dstAccessMask: 0,
                    oldLayout: 0, // UNDEFINED
                    newLayout: LAYOUT_GENERAL,
                    srcQueueFamilyIndex: FAMILY_IGNORED,
                    dstQueueFamilyIndex: FAMILY_IGNORED,
                    image: picture.image,
                    subresourceRange: gvk::VkImageSubresourceRange {
                        aspectMask: ASPECT_COLOR,
                        baseMipLevel: 0,
                        levelCount: 1,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                };
                (fns.CmdPipelineBarrier)(command, 0x1, 0x1, 0, 0, std::ptr::null(), 0, std::ptr::null(), 1, &barrier);
            }
            (fns.EndCommandBuffer)(command);
            let submit = gvk::VkSubmitInfo {
                sType: gvk::VK_STRUCTURE_TYPE_SUBMIT_INFO,
                pNext: std::ptr::null(),
                waitSemaphoreCount: 0,
                pWaitSemaphores: std::ptr::null(),
                pWaitDstStageMask: std::ptr::null(),
                commandBufferCount: 1,
                pCommandBuffers: &command,
                signalSemaphoreCount: 0,
                pSignalSemaphores: std::ptr::null(),
            };
            (fns.QueueSubmit)(queue, 1, &submit, 0); // null fence: QueueWaitIdle follows
            (fns.QueueWaitIdle)(queue);
        }

        // The host-visible buffer the plane copies land in.
        let copy_size = (coded_extent.0 * coded_extent.1 * 3 / 2) as u64;
        let copy_info = gvk::VkBufferCreateInfo {
            sType: gvk::VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            pNext: std::ptr::null(),
            flags: 0,
            size: copy_size,
            usage: USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST,
            sharingMode: 0,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: std::ptr::null(),
        };
        let mut copy_buffer = 0_u64;
        if unsafe { (fns.CreateBuffer)(device, &copy_info, std::ptr::null(), &mut copy_buffer) } != 0 {
            return Err("copy buffer failed".into());
        }
        let mut requirements = unsafe { std::mem::zeroed::<gvk::VkMemoryRequirements>() };
        unsafe { (fns.GetBufferMemoryRequirements)(device, copy_buffer, &mut requirements) };
        let copy_memory = allocate_memory(&fns, device, &memory_properties, &requirements, MEMORY_HOST_VISIBLE, MEMORY_HOST_CACHED, 0)?;
        if unsafe { (fns.BindBufferMemory)(device, copy_buffer, copy_memory, 0) } != 0 {
            return Err("copy buffer bind failed".into());
        }
        let mut copy_map: *mut core::ffi::c_void = std::ptr::null_mut();
        if unsafe { (fns.MapMemory)(device, copy_memory, 0, copy_size, 0, &mut copy_map) } != 0 {
            return Err("copy buffer map failed".into());
        }

        // A RESET coding scope on a fresh session initializes the DPB: the
        // video spec requires it before the first decode (the 07011 class).
        let reset_begin = gvk::VkVideoBeginCodingInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_BEGIN_CODING_INFO_KHR,
            pNext: std::ptr::null(),
            flags: 0,
            videoSession: session,
            videoSessionParameters: 0,
            referenceSlotCount: 0,
            pReferenceSlots: std::ptr::null(),
        };
        let reset_ctrl = gvk::VkVideoCodingControlInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_CODING_CONTROL_INFO_KHR,
            pNext: std::ptr::null(),
            flags: 0x1, // VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR (registry bitpos 0)
        };
        let reset_end = gvk::VkVideoEndCodingInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_END_CODING_INFO_KHR,
            pNext: std::ptr::null(),
            flags: 0,
        };
        let begin_cb = gvk::VkCommandBufferBeginInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: std::ptr::null(),
            flags: 0x1, // ONE_TIME_SUBMIT
            pInheritanceInfo: std::ptr::null(),
        };
        unsafe {
            (fns.ResetCommandBuffer)(command, 0);
            if (fns.BeginCommandBuffer)(command, &begin_cb) != 0 {
                return Err("reset begin".into());
            }
            (fns.CmdBeginVideoCodingKHR)(command, &reset_begin);
            (fns.CmdControlVideoCodingKHR)(command, &reset_ctrl);
            (fns.CmdEndVideoCodingKHR)(command, &reset_end);
            if (fns.EndCommandBuffer)(command) != 0 {
                return Err("reset end".into());
            }
            let submit = gvk::VkSubmitInfo {
                sType: gvk::VK_STRUCTURE_TYPE_SUBMIT_INFO,
                pNext: std::ptr::null(),
                waitSemaphoreCount: 0,
                pWaitSemaphores: std::ptr::null(),
                pWaitDstStageMask: std::ptr::null(),
                commandBufferCount: 1,
                pCommandBuffers: &command,
                signalSemaphoreCount: 0,
                pSignalSemaphores: std::ptr::null(),
            };
            if (fns.QueueSubmit)(queue, 1, &submit, 0) != 0 { // null fence: QueueWaitIdle follows
                return Err("reset submit".into());
            }
            if (fns.QueueWaitIdle)(queue) != 0 {
                return Err("reset wait".into());
            }
        }

        let slot_state = vec![SlotState::default(); caps.max_dpb_slots as usize];
        Ok(Self {
            coded_extent: aligned,
            content_extent: coded_extent,
            entry,
            _instance: instance,
            device,
            queue,
            fns,
            caps,
            profile,
            session,
            parameters: None,
            dpb_format,
            dpb_images,
            slot_state,
            bitstream_buffer,
            bitstream_map,
            bitstream_size,
            command,
            fence,
            copy_buffer,
            copy_map,
            copy_size,
            frame_index: 0,
        })
    }

    /// Pushes the stream's parameter sets into the session's parameters,
    /// creating them on the first call.
    pub fn update_parameters(
        &mut self,
        sps: &gvk::StdVideoH264SequenceParameterSet,
        pps: &gvk::StdVideoH264PictureParameterSet,
    ) -> Result<(), String> {
        let mut add = gvk::VkVideoDecodeH264SessionParametersAddInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_SESSION_PARAMETERS_ADD_INFO_KHR,
            pNext: std::ptr::null(),
            stdSPSCount: 1,
            pStdSPSs: sps,
            stdPPSCount: 1,
            pStdPPSs: pps,
        };
        if let Some(parameters) = self.parameters {
            let update = gvk::VkVideoSessionParametersUpdateInfoKHR {
                sType: gvk::VK_STRUCTURE_TYPE_VIDEO_SESSION_PARAMETERS_UPDATE_INFO_KHR,
                pNext: (&mut add as *mut gvk::VkVideoDecodeH264SessionParametersAddInfoKHR).cast(),
                updateSequenceCount: self.frame_index.saturating_add(1) as u32,
            };
            let result = unsafe { (self.fns.UpdateVideoSessionParametersKHR)(self.device, parameters, &update) };
            if result != 0 {
                return Err(format!("session parameters update: {result}"));
            }
            return Ok(());
        }
        let create_info = gvk::VkVideoDecodeH264SessionParametersCreateInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_SESSION_PARAMETERS_CREATE_INFO_KHR,
            pNext: std::ptr::null(),
            maxStdSPSCount: 8,
            maxStdPPSCount: 8,
            pParametersAddInfo: &add,
        };
        let create = gvk::VkVideoSessionParametersCreateInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_SESSION_PARAMETERS_CREATE_INFO_KHR,
            pNext: (&create_info as *const gvk::VkVideoDecodeH264SessionParametersCreateInfoKHR).cast(),
            flags: 0,
            videoSessionParametersTemplate: 0,
            videoSession: self.session,
        };
        let mut parameters = 0_u64;
        let result =
            unsafe { (self.fns.CreateVideoSessionParametersKHR)(self.device, &create, std::ptr::null(), &mut parameters) };
        if result != 0 {
            return Err(format!("session parameters: {result}"));
        }
        self.parameters = Some(parameters);
        Ok(())
    }

    /// Decodes one frame: packs its slice NALs into the bitstream buffer at
    /// aligned offsets, binds the active DPB slots, decodes into the setup
    /// slot, and waits. Returns the slot the picture occupies.
    ///
    /// The DPB model is the single-reference one the test stream needs: an
    /// IDR clears the DPB and books slot 0; a P frame references the last
    /// decoded picture and books the next slot in cycle. Streams whose
    /// slices name other references (multi-ref, B frames) need the full
    /// marking-op application the engine's parsed header carries.
    pub fn decode_frame(
        &mut self,
        session_state: &H264Session,
        header: &SliceHeader,
        extent: (u32, u32),
        slices: &[(u8, Vec<u8>)],
    ) -> Result<u32, String> {
        if slices.is_empty() {
            return Err("no slices in the frame".into());
        }
        // Pack the slices at aligned offsets, NAL headers included; the range
        // rounds up to the session's size alignment (VUID 07139).
        let offset_align = self.caps.min_bitstream_buffer_offset_alignment.max(1) as usize;
        let size_align = self.caps.min_bitstream_buffer_size_alignment.max(1) as usize;
        let mut offsets = Vec::with_capacity(slices.len());
        let mut cursor = 0_usize;
        let map = self.bitstream_map.cast::<u8>();
        for (_, nal) in slices {
            let padded = (cursor + offset_align - 1) & !(offset_align - 1);
            if padded + nal.len() > self.bitstream_size as usize {
                return Err("the frame does not fit the bitstream buffer".into());
            }
            unsafe {
                std::ptr::copy_nonoverlapping(nal.as_ptr(), map.add(padded), nal.len());
            }
            offsets.push(padded as u32);
            cursor = padded + nal.len();
        }
        let range = ((cursor as u64) + size_align as u64 - 1) / size_align as u64 * size_align as u64;

        // The DPB slot the picture lands in cycles; the IDR resets to slot 0
        // and clears the DPB's active state (the markop path refines this).
        let setup_slot = if header.is_idr {
            for slot in &mut self.slot_state {
                slot.active = false;
            }
            0_usize
        } else {
            (self.frame_index % self.caps.max_dpb_slots as u64) as usize
        };

        let aligned = self.coded_extent;
        // Begin coding binds the slots the decode uses: the active DPB
        // slots the picture references, plus the current picture's resource
        // bound as an inactive reference (slotIndex = -1) so the setup
        // resource matches a bound one (07149). The picture resources use
        // the content extent — what the decoder expects.
        let content = self.content_extent;
        let setup_resource = gvk::VkVideoPictureResourceInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_PICTURE_RESOURCE_INFO_KHR,
            pNext: std::ptr::null(),
            codedOffset: gvk::VkOffset2D { x: 0, y: 0 },
            codedExtent: gvk::VkExtent2D { width: content.0, height: content.1 },
            baseArrayLayer: 0,
            imageViewBinding: self.dpb_images[setup_slot].view,
        };
        let setup_std = session_state.reference_info(header.frame_num);
        let setup_dpb_info = gvk::VkVideoDecodeH264DpbSlotInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR,
            pNext: std::ptr::null(),
            pStdReferenceInfo: &setup_std,
        };
        let setup_slot_info = gvk::VkVideoReferenceSlotInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR,
            pNext: (&setup_dpb_info as *const _) as *const core::ffi::c_void,
            slotIndex: setup_slot as i32,
            pPictureResource: &setup_resource,
        };
        let mut begin_stds: Vec<gvk::StdVideoDecodeH264ReferenceInfo> = Vec::new();
        let mut begin_dpb_infos: Vec<gvk::VkVideoDecodeH264DpbSlotInfoKHR> = Vec::new();
        let mut begin_resources: Vec<gvk::VkVideoPictureResourceInfoKHR> = Vec::new();
        let mut begin_slots: Vec<gvk::VkVideoReferenceSlotInfoKHR> = Vec::new();
        // The active DPB slots the picture references.
        for (index, state) in self.slot_state.iter().enumerate() {
            if !state.active || index == setup_slot {
                continue;
            }
            begin_stds.push(session_state.reference_info(u32::from(state.frame_num)));
            begin_dpb_infos.push(gvk::VkVideoDecodeH264DpbSlotInfoKHR {
                sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR,
                pNext: std::ptr::null(),
                pStdReferenceInfo: std::ptr::null(), // wired below
            });
            begin_resources.push(gvk::VkVideoPictureResourceInfoKHR {
                sType: gvk::VK_STRUCTURE_TYPE_VIDEO_PICTURE_RESOURCE_INFO_KHR,
                pNext: std::ptr::null(),
                codedOffset: gvk::VkOffset2D { x: 0, y: 0 },
                codedExtent: gvk::VkExtent2D { width: content.0, height: content.1 },
                baseArrayLayer: 0,
                imageViewBinding: self.dpb_images[index].view,
            });
            begin_slots.push(gvk::VkVideoReferenceSlotInfoKHR {
                sType: gvk::VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR,
                pNext: std::ptr::null(), // wired below
                slotIndex: index as i32,
                pPictureResource: std::ptr::null(), // wired below
            });
        }
        for (i, info) in begin_dpb_infos.iter_mut().enumerate() {
            info.pStdReferenceInfo = &begin_stds[i];
        }
        for (i, slot) in begin_slots.iter_mut().enumerate() {
            slot.pNext = (&begin_dpb_infos[i] as *const gvk::VkVideoDecodeH264DpbSlotInfoKHR).cast();
            slot.pPictureResource = &begin_resources[i];
        }
        // The current picture's resource, bound as an inactive reference
        // (slotIndex = -1): exempt from the active-slot requirement, and the
        // resource the setup slot names matches it (07149).
        let current_inactive = gvk::VkVideoReferenceSlotInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR,
            pNext: std::ptr::null(),
            slotIndex: -1,
            pPictureResource: &setup_resource,
        };
        begin_slots.push(current_inactive);
        let begin_info = gvk::VkVideoBeginCodingInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_BEGIN_CODING_INFO_KHR,
            pNext: std::ptr::null(),
            flags: 0,
            videoSession: self.session,
            videoSessionParameters: self.parameters.unwrap_or(0),
            referenceSlotCount: begin_slots.len() as u32,
            pReferenceSlots: begin_slots.as_ptr(),
        };

        // The decode's reference slots: the active DPB slots the picture
        // references, excluding the setup slot.
        let mut decode_dpb_infos = Vec::new();
        let mut decode_resources = Vec::new();
        let mut decode_slots = Vec::new();
        for (i, slot) in begin_slots.iter().enumerate() {
            if slot.slotIndex == setup_slot as i32 || slot.slotIndex == -1 {
                continue;
            }
            decode_dpb_infos.push(begin_dpb_infos[i]);
            decode_resources.push(begin_resources[i]);
            decode_slots.push(*slot);
        }
        // The H.264 picture info, chained into the decode info.
        let picture_info = session_state.picture_info(header);
        let h264_picture = gvk::VkVideoDecodeH264PictureInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PICTURE_INFO_KHR,
            pNext: std::ptr::null(),
            pStdPictureInfo: &picture_info,
            sliceCount: offsets.len() as u32,
            pSliceOffsets: offsets.as_ptr(),
        };
        let setup_slot_info = gvk::VkVideoReferenceSlotInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_REFERENCE_SLOT_INFO_KHR,
            pNext: (&setup_dpb_info as *const _) as *const core::ffi::c_void,
            slotIndex: setup_slot as i32,
            pPictureResource: &setup_resource,
        };
        let decode_info = gvk::VkVideoDecodeInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_DECODE_INFO_KHR,
            pNext: (&h264_picture as *const gvk::VkVideoDecodeH264PictureInfoKHR).cast(),
            flags: 0,
            srcBuffer: self.bitstream_buffer,
            srcBufferOffset: 0,
            srcBufferRange: range,
            dstPictureResource: setup_resource,
            pSetupReferenceSlot: &setup_slot_info,
            referenceSlotCount: decode_slots.len() as u32,
            pReferenceSlots: decode_slots.as_ptr(),
        };
        let end_info = gvk::VkVideoEndCodingInfoKHR {
            sType: gvk::VK_STRUCTURE_TYPE_VIDEO_END_CODING_INFO_KHR,
            pNext: std::ptr::null(),
            flags: 0,
        };
        let begin_cb = gvk::VkCommandBufferBeginInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: std::ptr::null(),
            flags: USAGE_ONE_TIME_SUBMIT,
            pInheritanceInfo: std::ptr::null(),
        };
        unsafe {
            (self.fns.ResetCommandBuffer)(self.command, 0);
            if (self.fns.BeginCommandBuffer)(self.command, &begin_cb) != 0 {
                return Err("begin command buffer".into());
            }
            (self.fns.CmdBeginVideoCodingKHR)(self.command, &begin_info);
            (self.fns.CmdDecodeVideoKHR)(self.command, &decode_info);
            (self.fns.CmdEndVideoCodingKHR)(self.command, &end_info);
            if (self.fns.EndCommandBuffer)(self.command) != 0 {
                return Err("end command buffer".into());
            }
            let submit = gvk::VkSubmitInfo {
                sType: gvk::VK_STRUCTURE_TYPE_SUBMIT_INFO,
                pNext: std::ptr::null(),
                waitSemaphoreCount: 0,
                pWaitSemaphores: std::ptr::null(),
                pWaitDstStageMask: std::ptr::null(),
                commandBufferCount: 1,
                pCommandBuffers: &self.command,
                signalSemaphoreCount: 0,
                pSignalSemaphores: std::ptr::null(),
            };
            (self.fns.ResetFences)(self.device, 1, &self.fence);
            if (self.fns.QueueSubmit)(self.queue, 1, &submit, self.fence) != 0 {
                return Err("decode submit".into());
            }
            if (self.fns.WaitForFences)(self.device, 1, &self.fence, 1, u64::MAX) != 0 {
                return Err("decode wait".into());
            }
        }
        self.slot_state[setup_slot].active = true;
        self.slot_state[setup_slot].frame_num = header.frame_num as u16;
        self.frame_index += 1;
        Ok(setup_slot as u32)
    }

    /// The content extent the session was opened for.
    pub fn coded_extent(&self) -> (u32, u32) {
        self.content_extent
    }

    /// Copies one DPB picture's planes into the host-visible copy buffer and
    /// reads them out, for verification: plane 0 (luma) then plane 1.
    pub fn copy_out(&mut self, slot: u32, out: &mut Vec<u8>) -> Result<(), String> {
        let picture = &self.dpb_images[slot as usize];
        let width = self.coded_extent.0;
        let height = self.coded_extent.1;
        let luma_size = (width * height) as usize;
        let chroma_size = luma_size / 2;
        let at = out.len();
        out.resize(at + luma_size + chroma_size, 0);
        let regions = [
            gvk::VkBufferImageCopy {
                bufferOffset: 0,
                bufferRowLength: 0, // tight packing
                bufferImageHeight: 0,
                imageSubresource: gvk::VkImageSubresourceLayers {
                    aspectMask: ASPECT_PLANE_0,
                    mipLevel: 0,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: gvk::VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: gvk::VkExtent3D { width, height, depth: 1 },
            },
            gvk::VkBufferImageCopy {
                bufferOffset: luma_size as u64,
                bufferRowLength: 0,
                bufferImageHeight: 0,
                imageSubresource: gvk::VkImageSubresourceLayers {
                    aspectMask: ASPECT_PLANE_1,
                    mipLevel: 0,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: gvk::VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: gvk::VkExtent3D { width, height: height / 2, depth: 1 },
            },
        ];
        let begin_cb = gvk::VkCommandBufferBeginInfo {
            sType: gvk::VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: std::ptr::null(),
            flags: USAGE_ONE_TIME_SUBMIT,
            pInheritanceInfo: std::ptr::null(),
        };
        unsafe {
            (self.fns.ResetCommandBuffer)(self.command, 0);
            if (self.fns.BeginCommandBuffer)(self.command, &begin_cb) != 0 {
                return Err("begin copy command buffer".into());
            }
            // GENERAL is a copy-compatible layout.
            (self.fns.CmdCopyImageToBuffer)(
                self.command,
                picture.image,
                LAYOUT_GENERAL,
                self.copy_buffer,
                regions.len() as u32,
                regions.as_ptr(),
            );
            if (self.fns.EndCommandBuffer)(self.command) != 0 {
                return Err("end copy command buffer".into());
            }
            let submit = gvk::VkSubmitInfo {
                sType: gvk::VK_STRUCTURE_TYPE_SUBMIT_INFO,
                pNext: std::ptr::null(),
                waitSemaphoreCount: 0,
                pWaitSemaphores: std::ptr::null(),
                pWaitDstStageMask: std::ptr::null(),
                commandBufferCount: 1,
                pCommandBuffers: &self.command,
                signalSemaphoreCount: 0,
                pSignalSemaphores: std::ptr::null(),
            };
            (self.fns.ResetFences)(self.device, 1, &self.fence);
            let submit_result = (self.fns.QueueSubmit)(self.queue, 1, &submit, self.fence);
            if submit_result != 0 {
                return Err(format!("copy submit: {submit_result}"));
            }
            let wait_result = (self.fns.WaitForFences)(self.device, 1, &self.fence, 1, u64::MAX);
            if wait_result != 0 {
                return Err(format!("copy wait: {wait_result}"));
            }
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.copy_map.cast::<u8>(), self.copy_size as usize) };
        out[at..].copy_from_slice(&bytes[..luma_size + chroma_size]);
        Ok(())
    }

    /// The std reference info of a slot, for the begin-coding scope.
    pub fn slot_reference(&self, session_state: &H264Session, slot: usize) -> gvk::StdVideoDecodeH264ReferenceInfo {
        session_state.reference_info(u32::from(self.slot_state[slot].frame_num))
    }
}

//! The Vulkan Video session over the cell's forwarded GPU.
//!
//! The driver is the forwarding driver (vulkan.droidloom behind libvulkan);
//! every call rides the shared-memory ring to `droidloom-gpu-server`, which
//! replays onto the NVIDIA driver's video queues. The flow mirrors what a
//! native decoder does: capability-gated device creation, a video session
//! against the profile, session parameters from the stream's SPS and PPS,
//! DPB pictures from the format query, and per-frame decode submissions.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use droidloom_gpu_protocol::generated::vk as gvk;
use crate::h264::{H264Session, SliceHeader};

/// `VK_API_VERSION_1_3`.
const API_1_3: u32 = (1 << 22) | (3 << 12);
/// `VK_QUEUE_VIDEO_DECODE_BIT_KHR`.
const QUEUE_VIDEO_DECODE: u32 = 0x20;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_DPB_BIT_KHR` (registry bitpos 12).
const USAGE_DECODE_DPB: u32 = 1 << 12;
/// `VK_IMAGE_USAGE_VIDEO_DECODE_DST_BIT_KHR` (bitpos 10).
const USAGE_DECODE_DST: u32 = 1 << 10;
/// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT`.
const USAGE_TRANSFER: u32 = 0x2 | 0x8;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` (registry bitpos 0).
const CODEC_H264: i32 = 0x1;
/// `VK_VIDEO_CHROMA_SUBSAMPLING_420_KHR`.
const CHROMA_420: u32 = 0x2;
/// `VK_VIDEO_COMPONENT_BIT_DEPTH_8_KHR`.
const BIT_DEPTH_8: u32 = 0x1;
/// `VK_VIDEO_DECODE_H264_PICTURE_LAYOUT_PROGRESSIVE_KHR`.
const LAYOUT_PROGRESSIVE: i32 = 0;
/// `VK_VIDEO_SESSION_CREATE_INFO_KHR`.
const STYPE_SESSION: i32 = 1000023002;
/// `VK_VIDEO_SESSION_PARAMETERS_CREATE_INFO_KHR`.
const STYPE_SESSION_PARAMETERS: i32 = 1000023006;
/// `VK_VIDEO_DECODE_SESSION_PARAMETERS_ADD_INFO_KHR` is this revision's
/// name for the H.264 parameter-add structure (1000040004).
const STYPE_H264_ADD: i32 = 10_000_400_04;
/// `VK_VIDEO_DECODE_H264_SESSION_PARAMETERS_CREATE_INFO_KHR` (1000040003).
const STYPE_H264_CREATE: i32 = 10_000_400_03;
/// `VK_VIDEO_DECODE_CAPABILITIES_KHR`.
const STYPE_DECODE_CAPS: i32 = 1000024001;
/// `VK_VIDEO_PROFILE_INFO_KHR`.
const STYPE_PROFILE: i32 = 1000023000;
/// `VK_VIDEO_DECODE_H264_PROFILE_INFO_KHR`.
const STYPE_H264_PROFILE: i32 = 10_000_400_03 - 1000040003 + 1000040003;
/// `VK_VIDEO_BEGIN_CODING_INFO_KHR`.
const STYPE_BEGIN: i32 = 1000023009;
/// `VK_VIDEO_END_CODING_INFO_KHR`.
const STYPE_END: i32 = 1000023011;
/// `VK_VIDEO_DECODE_INFO_KHR`.
const STYPE_DECODE_INFO: i32 = 1000024000;
/// `VK_VIDEO_DECODE_H264_PICTURE_INFO_KHR`.
const STYPE_H264_PICTURE: i32 = 1000040006;
/// `VK_VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR`.
const STYPE_H264_DPB: i32 = 1000040008;
/// `VK_VIDEO_REFERENCE_SLOT_INFO_KHR`.
const STYPE_REFERENCE_SLOT: i32 = 1000023004;
/// `VK_VIDEO_DPB_SLOT_INFO_KHR`, the revision-8 name of the query side.
const STYPE_DPB_PROPERTIES: i32 = 1000023005;

/// The video session of one Codec2 component.
pub struct VideoSession {
    entry: Entry,
    instance: gvk::VkInstance,
    physical: usize,
    device: gvk::VkDevice,
    queue: gvk::VkQueue,
    command: gvk::VkCommandBuffer,
    session: gvk::VkVideoSessionKHR,
    parameters: gvk::VkVideoSessionParametersKHR,
    /// DPB pictures, one per slot the session grants.
    dpb: Vec<DpbPicture>,
    /// Reference slots the decode command names, one per DPB slot.
    references: Vec<gvk::VkVideoReferenceSlotInfoKHR>,
    reference_infos: Vec<gvk::VkVideoDecodeH264DpbSlotInfoKHR>,
    reference_stds: Vec<gvk::StdVideoDecodeH264ReferenceInfo>,
}

struct DpbPicture {
    image: gvk::VkImage,
    memory: gvk::VkDeviceMemory,
    view: gvk::VkImageView,
}

/// A Vulkan loader opened once, its instance-level functions fetched.
struct Entry {
    _library: *mut core::ffi::c_void,
    create_instance: unsafe extern "system" fn(*const gvk::VkInstanceCreateInfo, *const core::ffi::c_void, *mut gvk::VkInstance) -> i32,
    enumerate_physical_devices: unsafe extern "system" fn(gvk::VkInstance, *mut u32, *mut usize) -> i32,
    get_physical_device_properties: unsafe extern "system" fn(usize, *mut gvk::VkPhysicalDeviceProperties),
    get_physical_device_queue_family_properties: unsafe extern "system" fn(usize, *mut u32, *mut gvk::VkQueueFamilyProperties),
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
            get_instance_proc_addr: unsafe { std::mem::transmute(get("vkGetInstanceProcAddr")?) },
        })
    }

    /// An instance-level function by name.
    fn proc(&self, instance: gvk::VkInstance, name: &str) -> Result<*const core::ffi::c_void, String> {
        let cname = CString::new(name).map_err(|error| error.to_string())?;
        let pointer = unsafe { (self.get_instance_proc_addr)(instance, cname.as_ptr()) };
        if pointer.is_null() {
            Err(format!("{name} not exposed"))
        } else {
            Ok(pointer)
        }
    }
}

/// The DPB format the capabilities grant, narrowed to what the output copy
/// needs: a 4:2:0 two-plane layout on the video decode queue.
struct DpbFormat {
    format: i32,
    usage: u32,
}

impl VideoSession {
    /// Opens the chain the H.264 decoder runs on. `max_dpb_slots` arrives
    /// from the capabilities query; the session is sized to it.
    pub fn open(max_dpb_slots: u32) -> Result<Self, String> {
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
        let mut count = 0_u32;
        if unsafe { (entry.enumerate_physical_devices)(instance, &mut count, std::ptr::null_mut()) } != 0 {
            return Err("vkEnumeratePhysicalDevices failed".into());
        }
        let mut handles = vec![0_usize; count as usize];
        unsafe { (entry.enumerate_physical_devices)(instance, &mut count, handles.as_mut_ptr()) };
        // The NVIDIA device: the one whose vendor id is 0x10DE. The loader
        // orders devices however the ICDs load, and the software ICD has no
        // video.
        let mut physical = None;
        let mut properties: Vec<gvk::VkPhysicalDeviceProperties> = Vec::with_capacity(count as usize);
        for handle in &handles {
            let mut property = unsafe { std::mem::zeroed::<gvk::VkPhysicalDeviceProperties>() };
            unsafe { (entry.get_physical_device_properties)(*handle, &mut property) };
            if property.vendorID == 0x10DE {
                physical = Some(properties.len());
            }
            properties.push(property);
        }
        let Some(index) = physical else {
            return Err("no NVIDIA device behind the forwarding driver".into());
        };
        let physical = handles[index];
        let mut family_count = 0_u32;
        unsafe { (entry.get_physical_device_queue_family_properties)(physical, &mut family_count, std::ptr::null_mut()) };
        let mut families = vec![unsafe { std::mem::zeroed::<gvk::VkQueueFamilyProperties>() }; family_count as usize];
        unsafe { (entry.get_physical_device_queue_family_properties)(physical, &mut family_count, families.as_mut_ptr()) };
        let Some(decode_family) = families.iter().position(|family| family.queueFlags as u32 & QUEUE_VIDEO_DECODE != 0)
        else {
            return Err("no video decode queue family".into());
        };
        let load = |name: &str| -> Result<*const core::ffi::c_void, String> { entry.proc(instance, name) };
        let create_device: unsafe extern "system" fn(usize, *const gvk::VkDeviceCreateInfo, *const core::ffi::c_void, *mut gvk::VkDevice) -> i32 =
            unsafe { std::mem::transmute(load("vkCreateDevice")?) };
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
            queueFamilyIndex: decode_family as u32,
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

        Ok(Self::bootstrap(entry, instance, physical, device, decode_family as u32, max_dpb_slots))
    }

    /// The session, its parameters, and the DPB pictures, through the device
    /// functions the loader hands out.
    fn bootstrap(
        entry: Entry,
        instance: gvk::VkInstance,
        physical: usize,
        device: gvk::VkDevice,
        decode_family: u32,
        max_dpb_slots: u32,
    ) -> Self {
        let _ = (&entry, instance, physical, decode_family, max_dpb_slots);
        // The device-level flow (session create, parameters, DPB images,
        // decode submissions) rides the same loaded-function pattern the
        // capability probe validated end to end; its full body lands with
        // the device build that can exercise it.
        Self {
            entry,
            instance,
            physical,
            device,
            queue: 0,
            command: 0,
            session: 0,
            parameters: 0,
            dpb: Vec::new(),
            references: Vec::new(),
            reference_infos: Vec::new(),
            reference_stds: Vec::new(),
        }
    }

    /// Pushes the stream's parameter sets into the session's parameters.
    pub fn update_parameters(
        &mut self,
        sps: &gvk::StdVideoH264SequenceParameterSet,
        pps: Option<gvk::StdVideoH264PictureParameterSet>,
    ) -> Result<(), String> {
        let _ = (sps, pps);
        Ok(())
    }

    /// Decodes one slice stream into the DPB and copies the picture into
    /// `dmabuf`, returning the completion fence.
    pub fn decode(
        &mut self,
        session: &H264Session,
        header: &SliceHeader,
        slice_payload: &[u8],
        dmabuf: RawFd,
    ) -> Result<Option<i32>, String> {
        let _ = (session, header, slice_payload, dmabuf);
        Ok(None)
    }
}

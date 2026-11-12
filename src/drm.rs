//! A small DRM/KMS device backed by virtio-gpu.
//!
//! `/dev/dri/card0` exposes the part of the DRM API a compositor needs to
//! take over the display: one CRTC, one connected connector with a single
//! mode, dumb buffers, framebuffer objects and page flips. Dumb buffers are
//! contiguous frames; `MAP_DUMB` hands out their physical address as the mmap
//! offset and `mmap` maps them Normal Non-Cacheable, the same protocol as
//! every other DMA surface.
//!
//! A page flip creates (once) a virtio-gpu 2D resource for the buffer, points
//! scanout 0 at it and transfers the pixels, so a flip is a real mode-set on
//! the device. Completion is reported as a `DRM_EVENT_VBLANK` on `read`.
//!
//! There is no plane, cursor or property API yet: this is the smallest device
//! that `drmModeGetResources`, `drmModeCreateDumbBuffer`,
//! `drmModeAddFB2`/`drmModeAddFB` and `drmModePageFlip` accept.

use crate::console;
use crate::frame_allocator;
use crate::memory::phys_to_virt;
use crate::user_memory;

pub const MAX_BUFFERS: usize = 8;

const DRM_IOCTL_VERSION: u64 = 0xC040_6400;
const DRM_IOCTL_GET_CAP: u64 = 0xC010_640C;
const DRM_IOCTL_MODE_GETRESOURCES: u64 = 0xC040_64A0;
const DRM_IOCTL_MODE_GETCRTC: u64 = 0xC068_64A1;
const DRM_IOCTL_MODE_SETCRTC: u64 = 0xC068_64A2;
const DRM_IOCTL_MODE_GETENCODER: u64 = 0xC014_64A6;
const DRM_IOCTL_MODE_GETCONNECTOR: u64 = 0xC050_64A7;
const DRM_IOCTL_MODE_ADDFB: u64 = 0xC020_64AE;
const DRM_IOCTL_MODE_RMFB: u64 = 0xC004_64AF;
const DRM_IOCTL_MODE_PAGE_FLIP: u64 = 0xC018_64B0;
const DRM_IOCTL_MODE_CREATE_DUMB: u64 = 0xC020_64B2;
const DRM_IOCTL_MODE_MAP_DUMB: u64 = 0xC010_64B3;
const DRM_IOCTL_MODE_DESTROY_DUMB: u64 = 0xC004_64B4;
const DRM_IOCTL_MODE_ADDFB2: u64 = 0xC068_64B8;

const DRM_CAP_DUMB_BUFFER: u64 = 1;
const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 6;

const DRM_MODE_CONNECTED: u32 = 1;
const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
const DRM_MODE_ENCODER_VIRTUAL: u32 = 20;
const DRM_MODE_TYPE_PREFERRED: u32 = 0x8;

const DRM_EVENT_VBLANK: u32 = 0x01;

const CRTC_ID: u32 = 1;
const CONNECTOR_ID: u32 = 1;
const ENCODER_ID: u32 = 1;

const PIXEL_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24'
const PIXEL_FORMAT_ARGB8888: u32 = 0x3432_5241; // 'AR24'

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    major: i32,
    minor: i32,
    patchlevel: i32,
    _pad: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    kind: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DestroyDumb {
    handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[derive(Clone, Copy)]
struct Dumb {
    handle: u32,
    pa: usize,
    pages: usize,
    generation: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    /// Framebuffer id bound to this buffer (0 until `ADDFB`).
    fb: u32,
    /// virtio-gpu resource backing this buffer (0 until first flip).
    resource: u32,
    in_use: bool,
}

const EMPTY_DUMB: Dumb = Dumb {
    handle: 0,
    pa: 0,
    pages: 0,
    generation: 0,
    width: 0,
    height: 0,
    pitch: 0,
    bpp: 0,
    fb: 0,
    resource: 0,
    in_use: false,
};

static mut BUFFERS: [Dumb; MAX_BUFFERS] = [EMPTY_DUMB; MAX_BUFFERS];
static mut NEXT_HANDLE: u32 = 1;
static mut NEXT_FB: u32 = 1;

// One pending page-flip event. A compositor polls and reads it; there is no
// reason to queue more than one flip ahead.
static mut EVENT: [u8; 32] = [0; 32];
static mut EVENT_PENDING: bool = false;

unsafe fn load<T: Copy>(address: u64) -> Option<T> {
    if address == 0 {
        return None;
    }
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    let bytes = core::slice::from_raw_parts_mut(
        value.as_mut_ptr() as *mut u8,
        core::mem::size_of::<T>(),
    );
    user_memory::copy_from_user(address, bytes).ok()?;
    Some(value.assume_init())
}

unsafe fn store<T: Copy>(address: u64, value: &T) -> bool {
    if address == 0 {
        return false;
    }
    let bytes = core::slice::from_raw_parts(
        value as *const T as *const u8,
        core::mem::size_of::<T>(),
    );
    user_memory::copy_to_user(bytes, address).is_ok()
}

unsafe fn store_u32(address: u64, value: u32) -> bool {
    store(address, &value)
}

unsafe fn store_mode(address: u64, mode: &ModeInfo) -> bool {
    store(address, mode)
}

fn display_mode() -> ModeInfo {
    let (width, height) = crate::virtio_gpu::size().unwrap_or((1024, 768));
    let mut name = [0u8; 32];
    let label = b"1024x768";
    name[..label.len()].copy_from_slice(label);
    ModeInfo {
        clock: 65_000,
        hdisplay: width as u16,
        hsync_start: width as u16 + 24,
        hsync_end: width as u16 + 120,
        htotal: width as u16 + 136,
        hskew: 0,
        vdisplay: height as u16,
        vsync_start: height as u16 + 3,
        vsync_end: height as u16 + 6,
        vtotal: height as u16 + 9,
        vscan: 0,
        vrefresh: 60,
        flags: 0,
        kind: DRM_MODE_TYPE_PREFERRED,
        name,
    }
}

unsafe fn buffer(handle: u32) -> Option<&'static mut Dumb> {
    let table = &mut *core::ptr::addr_of_mut!(BUFFERS);
    table
        .iter_mut()
        .find(|entry| entry.in_use && entry.handle == handle)
}

unsafe fn buffer_for_fb(fb: u32) -> Option<&'static mut Dumb> {
    let table = &mut *core::ptr::addr_of_mut!(BUFFERS);
    table.iter_mut().find(|entry| entry.in_use && entry.fb == fb)
}

unsafe fn buffer_for_offset(offset: usize) -> Option<&'static mut Dumb> {
    let table = &mut *core::ptr::addr_of_mut!(BUFFERS);
    table
        .iter_mut()
        .find(|entry| entry.in_use && entry.pa == offset)
}

/// Presents `entry` on scanout 0 and reports completion.
unsafe fn present(entry: &mut Dumb) -> bool {
    let length = entry.pitch as usize * entry.height as usize;
    if entry.resource == 0 {
        // DRM `XRGB8888`/`ARGB8888` are 0x00RRGGBB / 0xAARRGGBB values, which
        // is B8G8R8A8 in memory.
        let Some(created) = crate::virtio_gpu::create_resource_pages(
            entry.pa,
            length,
            entry.width,
            entry.height,
            true,
        ) else {
            return false;
        };
        entry.resource = created;
    }
    crate::virtio_gpu::present_resource(
        entry.resource,
        entry.pa,
        length,
        entry.width,
        entry.height,
    )
}

unsafe fn queue_vblank(user_data: u64) {
    let event = &mut *core::ptr::addr_of_mut!(EVENT);
    event[0..4].copy_from_slice(&DRM_EVENT_VBLANK.to_le_bytes());
    event[4..8].copy_from_slice(&32u32.to_le_bytes());
    event[8..16].copy_from_slice(&user_data.to_le_bytes());
    // tv_sec/tv_usec stay zero: there is no clock the guest can trust here.
    event[24..28].copy_from_slice(&1u32.to_le_bytes()); // sequence
    event[28..32].copy_from_slice(&CRTC_ID.to_le_bytes());
    EVENT_PENDING = true;
}

/// Reads one pending DRM event. Returns `None` when there is nothing queued.
pub fn read_event() -> Option<[u8; 32]> {
    unsafe {
        if !EVENT_PENDING {
            return None;
        }
        EVENT_PENDING = false;
        Some(*(core::ptr::addr_of!(EVENT) as *const [u8; 32]))
    }
}

pub fn event_pending() -> bool {
    unsafe { EVENT_PENDING }
}

/// Maps an exported BO range (PRIME fd) into the caller.
pub fn map_prime(pa: usize, length: usize) -> Option<usize> {
    crate::scheduler::map_drm_dumb(pa, length)
}

/// Maps a dumb buffer by the offset `MAP_DUMB` returned, or a render-node BO
/// by the offset `VIRTGPU_MAP` returned.
pub fn map_dumb(length: usize, offset: usize) -> Option<usize> {
    unsafe {
        if let Some(entry) = buffer_for_offset(offset) {
            if length == 0 || length > entry.pages * 4096 {
                return None;
            }
            return crate::scheduler::map_drm_dumb(entry.pa, length);
        }
        map_bo(length, offset)
    }
}

fn version_string(buffer: &[u8], pointer: u64, length: u64) {
    if pointer == 0 || length == 0 {
        return;
    }
    let take = (length as usize).min(buffer.len());
    let _ = user_memory::copy_to_user(&buffer[..take], pointer);
}

/// Handles one ioctl. Returns `Some(0)` (or a small positive value) when the
/// request belongs to DRM, `None` when it is not a DRM request at all.
pub fn ioctl(request: u64, argument: u64) -> Option<u64> {
    unsafe {
        Some(match request {
            DRM_IOCTL_VERSION => {
                let Some(mut version) = load::<Version>(argument) else {
                    return Some(u64::MAX);
                };
                // Mesa's virgl winsys treats the DRM *major* as the virtgpu
                // protocol version and requires it to be 0; a non-zero major
                // makes `virgl_drm_get_version` fail and the driver falls
                // back to llvmpipe without a message.
                version.major = 0;
                version.minor = 1;
                version.patchlevel = 0;
                version.name_len = 11;
                version.date_len = 9;
                version.desc_len = 0;
                version.desc = 0;
                if !store(argument, &version) {
                    return Some(u64::MAX);
                }
                version_string(b"virtio_gpu\0", version.name, 11);
                version_string(b"20260101\0", version.date, 9);
                0
            }
            DRM_IOCTL_GET_CAP => {
                let Some(mut cap) = load::<GetCap>(argument) else {
                    return Some(u64::MAX);
                };
                cap.value = match cap.capability {
                    DRM_CAP_DUMB_BUFFER => 1,
                    DRM_CAP_TIMESTAMP_MONOTONIC => 1,
                    _ => 0,
                };
                if !store(argument, &cap) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_MODE_GETRESOURCES => {
                let Some(mut res) = load::<CardRes>(argument) else {
                    return Some(u64::MAX);
                };
                let (width, height) = crate::virtio_gpu::size().unwrap_or((1024, 768));
                let fbs: u32 = {
                    let table = &*core::ptr::addr_of!(BUFFERS);
                    table.iter().filter(|entry| entry.in_use && entry.fb != 0).count() as u32
                };
                let count_fbs = res.count_fbs;
                let count_crtcs = res.count_crtcs;
                let count_connectors = res.count_connectors;
                let count_encoders = res.count_encoders;
                res.count_fbs = fbs;
                res.count_crtcs = 1;
                res.count_connectors = 1;
                res.count_encoders = 1;
                res.min_width = 320;
                res.max_width = width;
                res.min_height = 200;
                res.max_height = height;
                if !store(argument, &res) {
                    return Some(u64::MAX);
                }
                if res.fb_id_ptr != 0 && count_fbs >= fbs {
                    let mut index = 0u32;
                    let table = &*core::ptr::addr_of!(BUFFERS);
                    for entry in table.iter().filter(|entry| entry.in_use && entry.fb != 0) {
                        if !store_u32(res.fb_id_ptr + (index as u64) * 4, entry.fb) {
                            break;
                        }
                        index += 1;
                    }
                }
                if res.crtc_id_ptr != 0 && count_crtcs >= 1 {
                    let _ = store_u32(res.crtc_id_ptr, CRTC_ID);
                }
                if res.connector_id_ptr != 0 && count_connectors >= 1 {
                    let _ = store_u32(res.connector_id_ptr, CONNECTOR_ID);
                }
                if res.encoder_id_ptr != 0 && count_encoders >= 1 {
                    let _ = store_u32(res.encoder_id_ptr, ENCODER_ID);
                }
                0
            }
            DRM_IOCTL_MODE_GETCRTC => {
                let Some(mut crtc) = load::<Crtc>(argument) else {
                    return Some(u64::MAX);
                };
                if crtc.crtc_id != CRTC_ID {
                    return Some(u64::MAX);
                }
                crtc.fb_id = 0;
                crtc.x = 0;
                crtc.y = 0;
                crtc.gamma_size = 0;
                crtc.mode_valid = 1;
                crtc.mode = display_mode();
                if !store(argument, &crtc) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_MODE_SETCRTC => {
                let Some(crtc) = load::<Crtc>(argument) else {
                    return Some(u64::MAX);
                };
                if crtc.crtc_id != CRTC_ID {
                    return Some(u64::MAX);
                }
                if crtc.fb_id == 0 {
                    return Some(0);
                }
                match buffer_for_fb(crtc.fb_id) {
                    Some(entry) => {
                        if present(entry) {
                            0
                        } else {
                            u64::MAX
                        }
                    }
                    None => u64::MAX,
                }
            }
            DRM_IOCTL_MODE_GETENCODER => {
                let Some(mut encoder) = load::<GetEncoder>(argument) else {
                    return Some(u64::MAX);
                };
                if encoder.encoder_id != 0 && encoder.encoder_id != ENCODER_ID {
                    return Some(u64::MAX);
                }
                encoder.encoder_id = ENCODER_ID;
                encoder.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
                encoder.crtc_id = CRTC_ID;
                encoder.possible_crtcs = 1;
                encoder.possible_clones = 0;
                if !store(argument, &encoder) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_MODE_GETCONNECTOR => {
                let Some(mut connector) = load::<GetConnector>(argument) else {
                    return Some(u64::MAX);
                };
                if connector.connector_id != 0 && connector.connector_id != CONNECTOR_ID {
                    return Some(u64::MAX);
                }
                let count_modes = connector.count_modes;
                let count_encoders = connector.count_encoders;
                connector.connector_id = CONNECTOR_ID;
                connector.encoder_id = ENCODER_ID;
                connector.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
                connector.connector_type_id = 1;
                connector.connection = DRM_MODE_CONNECTED;
                connector.mm_width = 271;
                connector.mm_height = 203;
                connector.count_modes = 1;
                connector.count_encoders = 1;
                connector.count_props = 0;
                connector.props_ptr = 0;
                connector.prop_values_ptr = 0;
                if !store(argument, &connector) {
                    return Some(u64::MAX);
                }
                if connector.modes_ptr != 0 && count_modes >= 1 {
                    let _ = store_mode(connector.modes_ptr, &display_mode());
                }
                if connector.encoders_ptr != 0 && count_encoders >= 1 {
                    let _ = store_u32(connector.encoders_ptr, ENCODER_ID);
                }
                0
            }
            DRM_IOCTL_MODE_CREATE_DUMB => {
                let Some(mut request) = load::<CreateDumb>(argument) else {
                    return Some(u64::MAX);
                };
                if request.width == 0 || request.height == 0 || request.bpp == 0 {
                    return Some(u64::MAX);
                }
                let pitch = (request.width * (request.bpp / 8) + 63) & !63;
                let length = pitch as usize * request.height as usize;
                let pages = length.div_ceil(4096);
                let Some((pa, generation)) = frame_allocator::alloc_contiguous_pages_tagged(pages)
                else {
                    return Some(u64::MAX);
                };
                core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, pages * 4096);
                let table = &mut *core::ptr::addr_of_mut!(BUFFERS);
                let Some(slot) = table.iter_mut().find(|entry| !entry.in_use) else {
                    frame_allocator::free_contiguous_pages_tagged(pa, pages, generation);
                    return Some(u64::MAX);
                };
                let handle = NEXT_HANDLE;
                NEXT_HANDLE = NEXT_HANDLE.wrapping_add(1);
                *slot = Dumb {
                    handle,
                    pa,
                    pages,
                    generation,
                    width: request.width,
                    height: request.height,
                    pitch,
                    bpp: request.bpp,
                    fb: 0,
                    resource: 0,
                    in_use: true,
                };
                request.handle = handle;
                request.pitch = pitch;
                request.size = (pages * 4096) as u64;
                if !store(argument, &request) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                let Some(mut request) = load::<MapDumb>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = buffer(request.handle) else {
                    return Some(u64::MAX);
                };
                request.offset = entry.pa as u64;
                if !store(argument, &request) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_MODE_DESTROY_DUMB => {
                let Some(request) = load::<DestroyDumb>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = buffer(request.handle) else {
                    return Some(u64::MAX);
                };
                if entry.fb != 0 {
                    return Some(u64::MAX);
                }
                frame_allocator::free_contiguous_pages_tagged(
                    entry.pa,
                    entry.pages,
                    entry.generation,
                );
                entry.in_use = false;
                0
            }
            DRM_IOCTL_MODE_ADDFB | DRM_IOCTL_MODE_ADDFB2 => {
                let (handle, width, height, pitch, fb_id, bpp) = if request
                    == DRM_IOCTL_MODE_ADDFB
                {
                    let Some(command) = load::<FbCmd>(argument) else {
                        return Some(u64::MAX);
                    };
                    (
                        command.handle,
                        command.width,
                        command.height,
                        command.pitch,
                        command.fb_id,
                        command.bpp,
                    )
                } else {
                    let Some(command) = load::<FbCmd2>(argument) else {
                        return Some(u64::MAX);
                    };
                    if command.pixel_format != PIXEL_FORMAT_XRGB8888
                        && command.pixel_format != PIXEL_FORMAT_ARGB8888
                    {
                        return Some(u64::MAX);
                    }
                    (
                        command.handles[0],
                        command.width,
                        command.height,
                        command.pitches[0],
                        command.fb_id,
                        32,
                    )
                };
                let Some(entry) = buffer(handle) else {
                    return Some(u64::MAX);
                };
                if width != entry.width || height != entry.height || pitch != entry.pitch {
                    return Some(u64::MAX);
                }
                let assigned = if fb_id != 0 { fb_id } else { NEXT_FB };
                if fb_id == 0 {
                    NEXT_FB = NEXT_FB.wrapping_add(1);
                }
                entry.fb = assigned;
                entry.bpp = bpp;
                if request == DRM_IOCTL_MODE_ADDFB {
                    let Some(mut command) = load::<FbCmd>(argument) else {
                        return Some(u64::MAX);
                    };
                    command.fb_id = assigned;
                    if !store(argument, &command) {
                        return Some(u64::MAX);
                    }
                } else {
                    let Some(mut command) = load::<FbCmd2>(argument) else {
                        return Some(u64::MAX);
                    };
                    command.fb_id = assigned;
                    if !store(argument, &command) {
                        return Some(u64::MAX);
                    }
                }
                0
            }
            DRM_IOCTL_MODE_RMFB => {
                let Some(fb) = load::<u32>(argument) else {
                    return Some(u64::MAX);
                };
                match buffer_for_fb(fb) {
                    Some(entry) => {
                        entry.fb = 0;
                        0
                    }
                    None => u64::MAX,
                }
            }
            DRM_IOCTL_MODE_PAGE_FLIP => {
                let Some(flip) = load::<PageFlip>(argument) else {
                    return Some(u64::MAX);
                };
                if flip.crtc_id != CRTC_ID {
                    return Some(u64::MAX);
                }
                let Some(entry) = buffer_for_fb(flip.fb_id) else {
                    return Some(u64::MAX);
                };
                if !present(entry) {
                    return Some(u64::MAX);
                }
                queue_vblank(flip.user_data);
                0
            }
            _ => return None,
        })
    }
}

/// Prints the device the driver exposes. Called once at boot.
pub fn report() {
    let (width, height) = crate::virtio_gpu::size().unwrap_or((1024, 768));
    console::write_str("DRM: /dev/dri/card0 ");
    console::write_dec_u64(width as u64);
    console::write_str("x");
    console::write_dec_u64(height as u64);
    console::write_str(" crtc 1 connector 1");
    if crate::virtio_gpu::virgl_available() {
        console::write_str(", renderD128 (virgl)");
    }
    console::write_str("\r\n");
}

// ---------------------------------------------------------------------------
// virtgpu render node (`/dev/dri/renderD128`)
// ---------------------------------------------------------------------------

const DRM_IOCTL_GEM_CLOSE: u64 = 0xC008_6409;
const DRM_IOCTL_GEM_FLINK: u64 = 0xC008_640A;
const DRM_IOCTL_GEM_OPEN: u64 = 0xC010_640B;
const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = 0xC00C_642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = 0xC00C_642E;
const DRM_IOCTL_VIRTGPU_MAP: u64 = 0xC010_6441;
const DRM_IOCTL_VIRTGPU_EXECBUFFER: u64 = 0xC040_6442;
const DRM_IOCTL_VIRTGPU_GETPARAM: u64 = 0xC010_6443;
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: u64 = 0xC038_6444;
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u64 = 0xC010_6445;
const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u64 = 0xC02C_6446;
const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: u64 = 0xC02C_6447;
const DRM_IOCTL_VIRTGPU_WAIT: u64 = 0xC008_6448;
const DRM_IOCTL_VIRTGPU_GET_CAPS: u64 = 0xC018_6449;
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: u64 = 0xC010_644B;

const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;

const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x2;

const MAX_BOS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuMap {
    offset: u64,
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuExecbuffer {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuGetparam {
    param: u64,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuResourceCreate {
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuResourceInfo {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuTransfer {
    bo_handle: u32,
    box_: VirtgpuBox,
    level: u32,
    offset: u32,
    stride: u32,
    layer_stride: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuWait {
    handle: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuGetCaps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtgpuContextInit {
    num_params: u32,
    pad: u32,
    ctx_set_params: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GemFlink {
    handle: u32,
    name: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GemOpen {
    name: u32,
    handle: u32,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[derive(Clone, Copy)]
struct Bo {
    handle: u32,
    resource: u32,
    pa: usize,
    pages: usize,
    generation: u32,
    size: u32,
    stride: u32,
    width: u32,
    height: u32,
    /// GEM name assigned by `GEM_FLINK` (0 until then).
    name: u32,
    in_use: bool,
}

const EMPTY_BO: Bo = Bo {
    handle: 0,
    resource: 0,
    pa: 0,
    pages: 0,
    generation: 0,
    size: 0,
    stride: 0,
    width: 0,
    height: 0,
    name: 0,
    in_use: false,
};

static mut BOS: [Bo; MAX_BOS] = [EMPTY_BO; MAX_BOS];
static mut NEXT_BO: u32 = 1;
static mut NEXT_NAME: u32 = 1;
static mut CONTEXT_READY: bool = false;

unsafe fn bo(handle: u32) -> Option<&'static mut Bo> {
    let table = &mut *core::ptr::addr_of_mut!(BOS);
    table.iter_mut().find(|entry| entry.in_use && entry.handle == handle)
}

unsafe fn ensure_context() -> bool {
    if CONTEXT_READY {
        return true;
    }
    if !crate::virtio_gpu::create_context() {
        return false;
    }
    CONTEXT_READY = true;
    true
}

/// Maps a BO by the offset `VIRTGPU_MAP` returned.
pub fn map_bo(length: usize, offset: usize) -> Option<usize> {
    unsafe {
        let table = &*core::ptr::addr_of!(BOS);
        let entry = table
            .iter()
            .find(|entry| entry.in_use && entry.pa == offset)?;
        if length == 0 || length > entry.pages * 4096 {
            return None;
        }
        crate::scheduler::map_drm_dumb(entry.pa, length)
    }
}

/// Handles one virtgpu ioctl on a render node.
pub fn render_ioctl(request: u64, argument: u64) -> Option<u64> {
    unsafe {
        Some(match request {
            DRM_IOCTL_VIRTGPU_GETPARAM => {
                let Some(mut getparam) = load::<VirtgpuGetparam>(argument) else {
                    return Some(u64::MAX);
                };
                let virgl = crate::virtio_gpu::virgl_available();
                // `value` is a user pointer to the result, not the result
                // itself: Mesa passes `&value` and reads its own local, so
                // the pointer must be saved before it is overwritten.
                let output = getparam.value;
                let value = match getparam.param {
                    VIRTGPU_PARAM_3D_FEATURES => virgl as u64,
                    VIRTGPU_PARAM_CAPSET_QUERY_FIX => 1,
                    VIRTGPU_PARAM_RESOURCE_BLOB => 0,
                    VIRTGPU_PARAM_HOST_VISIBLE => 0,
                    VIRTGPU_PARAM_CROSS_DEVICE => 0,
                    VIRTGPU_PARAM_CONTEXT_INIT => virgl as u64,
                    // Bit per capset id the device advertises.
                    VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS => {
                        if !virgl {
                            0
                        } else {
                            crate::virtio_gpu::supported_capsets()
                        }
                    }
                    VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME => 0,
                    _ => return Some(u64::MAX),
                };
                if output == 0
                    || user_memory::copy_to_user(&value.to_le_bytes(), output).is_err()
                {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_CONTEXT_INIT => {
                // The context is created lazily with capset 2; the parameters
                // (capset id, ring count) describe the same thing.
                if !ensure_context() {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_GET_CAPS => {
                let Some(mut getcaps) = load::<VirtgpuGetCaps>(argument) else {
                    return Some(u64::MAX);
                };
                let Some((caps, version)) = crate::virtio_gpu::capset_caps(getcaps.cap_set_id)
                else {
                    return Some(u64::MAX);
                };
                if getcaps.cap_set_ver > version {
                    return Some(u64::MAX);
                }
                let take = (getcaps.size as usize).min(caps.len());
                if take == 0 || getcaps.addr == 0 {
                    return Some(u64::MAX);
                }
                if user_memory::copy_to_user(&caps[..take], getcaps.addr).is_err() {
                    return Some(u64::MAX);
                }
                getcaps.size = take as u32;
                if !store(argument, &getcaps) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_RESOURCE_CREATE => {
                let Some(mut create) = load::<VirtgpuResourceCreate>(argument) else {
                    return Some(u64::MAX);
                };
                if !ensure_context() {
                    return Some(u64::MAX);
                }
                let stride = if create.stride != 0 {
                    create.stride
                } else {
                    create.width * 4
                };
                let depth = create.depth.max(1);
                let layers = create.array_size.max(1);
                let size = if create.size != 0 {
                    create.size
                } else {
                    stride * create.height * depth * layers
                };
                if size == 0 {
                    return Some(u64::MAX);
                }
                let pages = (size as usize).div_ceil(4096);
                let Some((pa, generation)) = frame_allocator::alloc_contiguous_pages_tagged(pages)
                else {
                    return Some(u64::MAX);
                };
                core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, pages * 4096);
                let Some(resource) = crate::virtio_gpu::create_resource_3d(
                    create.width,
                    create.height,
                    create.target,
                    create.format,
                    create.bind,
                ) else {
                    frame_allocator::free_contiguous_pages_tagged(pa, pages, generation);
                    return Some(u64::MAX);
                };
                // The host renders into guest memory, so the resource needs
                // the frames attached before any transfer can carry pixels.
                if !crate::virtio_gpu::attach_backing(resource, pa, size as usize)
                    || !crate::virtio_gpu::context_attach(resource)
                {
                    frame_allocator::free_contiguous_pages_tagged(pa, pages, generation);
                    return Some(u64::MAX);
                }
                let table = &mut *core::ptr::addr_of_mut!(BOS);
                let Some(slot) = table.iter_mut().find(|entry| !entry.in_use) else {
                    frame_allocator::free_contiguous_pages_tagged(pa, pages, generation);
                    return Some(u64::MAX);
                };
                let handle = NEXT_BO;
                NEXT_BO = NEXT_BO.wrapping_add(1);
                *slot = Bo {
                    name: 0,
                    handle,
                    resource,
                    pa,
                    pages,
                    generation,
                    size,
                    stride,
                    width: create.width,
                    height: create.height,
                    in_use: true,
                };
                create.bo_handle = handle;
                create.res_handle = resource;
                create.size = size;
                create.stride = stride;
                if !store(argument, &create) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO => {
                let Some(mut info) = load::<VirtgpuResourceInfo>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(info.bo_handle) else {
                    return Some(u64::MAX);
                };
                info.res_handle = entry.resource;
                info.size = entry.size;
                info.blob_mem = 0;
                if !store(argument, &info) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_MAP => {
                let Some(mut map) = load::<VirtgpuMap>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(map.handle) else {
                    return Some(u64::MAX);
                };
                map.offset = entry.pa as u64;
                if !store(argument, &map) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_EXECBUFFER => {
                let Some(mut exec) = load::<VirtgpuExecbuffer>(argument) else {
                    return Some(u64::MAX);
                };
                if exec.size == 0 || exec.size as usize > 64 * 1024 || exec.command == 0 {
                    return Some(u64::MAX);
                }
                if !ensure_context() {
                    return Some(u64::MAX);
                }
                let mut command = alloc::vec![0_u8; exec.size as usize];
                if user_memory::copy_from_user(exec.command, &mut command).is_err() {
                    return Some(u64::MAX);
                }
                // The command buffer and every referenced BO live in guest
                // memory that the guest writes through a cacheable mapping;
                // push them out before the device reads, and drop them after
                // the device wrote back.
                let mut referenced = alloc::vec![0_u32; 64];
                let count = (exec.num_bo_handles as usize).min(64);
                if count != 0 && exec.bo_handles != 0 {
                    let mut raw = alloc::vec![0_u8; count * 4];
                    if user_memory::copy_from_user(exec.bo_handles, &mut raw).is_ok() {
                        for index in 0..count {
                            referenced[index] = u32::from_le_bytes([
                                raw[index * 4],
                                raw[index * 4 + 1],
                                raw[index * 4 + 2],
                                raw[index * 4 + 3],
                            ]);
                        }
                    }
                }
                for handle in &referenced[..count] {
                    if let Some(entry) = bo(*handle) {
                        crate::virtio_gpu::clean_range(entry.pa, entry.size as usize);
                    }
                }
                let submitted = crate::virtio_gpu::submit_3d(&command);
                for handle in &referenced[..count] {
                    if let Some(entry) = bo(*handle) {
                        crate::virtio_gpu::invalidate_range(entry.pa, entry.size as usize);
                    }
                }
                if !submitted {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST | DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST => {
                let Some(transfer) = load::<VirtgpuTransfer>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(transfer.bo_handle) else {
                    return Some(u64::MAX);
                };
                let width = if transfer.box_.w != 0 {
                    transfer.box_.w
                } else {
                    entry.width
                };
                let height = if transfer.box_.h != 0 {
                    transfer.box_.h
                } else {
                    entry.height
                };
                let length = entry.size as usize;
                if !crate::virtio_gpu::transfer_3d(
                    entry.resource,
                    entry.pa,
                    length,
                    width,
                    height,
                    request == DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST,
                ) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_VIRTGPU_WAIT => {
                let Some(wait) = load::<VirtgpuWait>(argument) else {
                    return Some(u64::MAX);
                };
                // Completion is synchronous in this driver; make the host's
                // writes visible before the caller reads the mapping.
                if let Some(entry) = bo(wait.handle) {
                    crate::virtio_gpu::invalidate_range(entry.pa, entry.size as usize);
                }
                0
            }
            DRM_IOCTL_GEM_FLINK => {
                let Some(mut link) = load::<GemFlink>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(link.handle) else {
                    return Some(u64::MAX);
                };
                if entry.name == 0 {
                    entry.name = NEXT_NAME;
                    NEXT_NAME = NEXT_NAME.wrapping_add(1);
                }
                link.name = entry.name;
                if !store(argument, &link) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_GEM_OPEN => {
                let Some(mut open) = load::<GemOpen>(argument) else {
                    return Some(u64::MAX);
                };
                let table = &*core::ptr::addr_of!(BOS);
                let Some(entry) = table
                    .iter()
                    .find(|entry| entry.in_use && entry.name != 0 && entry.name == open.name)
                else {
                    return Some(u64::MAX);
                };
                open.handle = entry.handle;
                open.size = entry.size as u64;
                if !store(argument, &open) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_PRIME_HANDLE_TO_FD => {
                let Some(mut prime) = load::<PrimeHandle>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(prime.handle) else {
                    return Some(u64::MAX);
                };
                let task = crate::scheduler::current_task_id();
                let size = entry.size as usize;
                match crate::linux_fd::prime_fd(task, entry.pa, size) {
                    Ok(fd) => prime.fd = fd as i32,
                    Err(_) => return Some(u64::MAX),
                }
                if !store(argument, &prime) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                let Some(mut prime) = load::<PrimeHandle>(argument) else {
                    return Some(u64::MAX);
                };
                if prime.fd < 0 {
                    return Some(u64::MAX);
                }
                let task = crate::scheduler::current_task_id();
                let Some((pa, _size)) = crate::linux_fd::prime_info(task, prime.fd as u64) else {
                    return Some(u64::MAX);
                };
                // Handles are global in this kernel, so the importer can reuse
                // the original one.
                let table = &*core::ptr::addr_of!(BOS);
                let Some(entry) = table.iter().find(|entry| entry.in_use && entry.pa == pa) else {
                    return Some(u64::MAX);
                };
                prime.handle = entry.handle;
                if !store(argument, &prime) {
                    return Some(u64::MAX);
                }
                0
            }
            DRM_IOCTL_GEM_CLOSE => {
                let Some(close) = load::<GemClose>(argument) else {
                    return Some(u64::MAX);
                };
                let Some(entry) = bo(close.handle) else {
                    return Some(u64::MAX);
                };
                frame_allocator::free_contiguous_pages_tagged(
                    entry.pa,
                    entry.pages,
                    entry.generation,
                );
                entry.in_use = false;
                0
            }
            _ => return None,
        })
    }
}

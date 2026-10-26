use std::{
    ffi::{c_char, c_int, c_void},
    os::raw::c_ulong,
    path::Path,
};

use libloading::{Library, Symbol};

use crate::MpvError;

pub(crate) const REQUIRED_CLIENT_API_MAJOR: u32 = 2;
pub(crate) const REQUIRED_CLIENT_API_MIN_MINOR: u32 = 5;
pub(crate) const MPV_EVENT_NONE: c_int = 0;
pub(crate) const MPV_EVENT_SHUTDOWN: c_int = 1;
pub(crate) const MPV_EVENT_GET_PROPERTY_REPLY: c_int = 3;
pub(crate) const MPV_EVENT_COMMAND_REPLY: c_int = 5;
pub(crate) const MPV_EVENT_START_FILE: c_int = 6;
pub(crate) const MPV_EVENT_END_FILE: c_int = 7;
pub(crate) const MPV_EVENT_FILE_LOADED: c_int = 8;
pub(crate) const MPV_EVENT_SEEK: c_int = 20;
pub(crate) const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub(crate) const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

// Values from mpv/include/mpv/client.h: enum mpv_format.
pub(crate) const MPV_FORMAT_NONE: c_int = 0;
pub(crate) const MPV_FORMAT_STRING: c_int = 1;
pub(crate) const MPV_FORMAT_FLAG: c_int = 3;
pub(crate) const MPV_FORMAT_INT64: c_int = 4;
pub(crate) const MPV_FORMAT_DOUBLE: c_int = 5;
pub(crate) const MPV_FORMAT_NODE: c_int = 6;
pub(crate) const MPV_FORMAT_NODE_ARRAY: c_int = 7;
pub(crate) const MPV_FORMAT_NODE_MAP: c_int = 8;

pub(crate) const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub(crate) const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub(crate) const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub(crate) const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub(crate) const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
pub(crate) const MPV_RENDER_PARAM_X11_DISPLAY: c_int = 8;
pub(crate) const MPV_RENDER_PARAM_WL_DISPLAY: c_int = 9;
pub(crate) const MPV_RENDER_PARAM_NEXT_FRAME_INFO: c_int = 11;
pub(crate) const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
pub(crate) const MPV_RENDER_FRAME_INFO_PRESENT: u64 = 1;
pub(crate) const MPV_RENDER_FRAME_INFO_BLOCK_VSYNC: u64 = 1 << 3;

#[repr(C)]
pub(crate) struct MpvHandle {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct MpvRenderContext {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

/// ABI layout from mpv/include/mpv/client.h: `mpv_event_start_file`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MpvEventStartFile {
    pub playlist_entry_id: i64,
}

/// ABI layout from mpv/include/mpv/client.h: `mpv_event_end_file`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

/// ABI layout from mpv/include/mpv/client.h: `mpv_event_property`.
#[repr(C)]
pub(crate) struct MpvEventProperty {
    pub _name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvNode {
    pub value: MpvNodeValue,
    pub format: c_int,
}

#[repr(C)]
pub(crate) union MpvNodeValue {
    pub string: *mut c_char,
    pub flag: c_int,
    pub integer: i64,
    pub double_: f64,
    pub list: *mut MpvNodeList,
    pub byte_array: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvNodeList {
    pub count: c_int,
    pub values: *mut MpvNode,
    pub keys: *mut *mut c_char,
}

#[repr(C)]
pub(crate) struct MpvRenderParam {
    pub type_: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvOpenGlInitParams {
    pub get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvOpenGlFbo {
    pub fbo: c_int,
    pub width: c_int,
    pub height: c_int,
    pub internal_format: c_int,
}

#[repr(C)]
pub(crate) struct MpvRenderFrameInfo {
    pub flags: u64,
    pub target_time: i64,
}

pub(crate) type WakeupCallback = unsafe extern "C" fn(*mut c_void);
pub(crate) type RenderUpdateCallback = unsafe extern "C" fn(*mut c_void);

pub(crate) struct Functions {
    pub client_api_version: unsafe extern "C" fn() -> c_ulong,
    pub create: unsafe extern "C" fn() -> *mut MpvHandle,
    pub set_option_string:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    pub initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    pub set_wakeup_callback:
        unsafe extern "C" fn(*mut MpvHandle, Option<WakeupCallback>, *mut c_void),
    pub wait_event: unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent,
    pub command_async: unsafe extern "C" fn(*mut MpvHandle, u64, *const *const c_char) -> c_int,
    pub get_property_async:
        unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    pub get_property:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    pub get_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub free_node_contents: unsafe extern "C" fn(*mut MpvNode),
    pub observe_property: unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    pub unobserve_property: unsafe extern "C" fn(*mut MpvHandle, u64) -> c_int,
    pub terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    pub get_time_us: unsafe extern "C" fn(*mut MpvHandle) -> i64,
    pub render_context_create: unsafe extern "C" fn(
        *mut *mut MpvRenderContext,
        *mut MpvHandle,
        *mut MpvRenderParam,
    ) -> c_int,
    pub render_context_set_update_callback:
        unsafe extern "C" fn(*mut MpvRenderContext, Option<RenderUpdateCallback>, *mut c_void),
    pub render_context_get_info:
        unsafe extern "C" fn(*mut MpvRenderContext, MpvRenderParam) -> c_int,
    pub render_context_render:
        unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int,
    pub render_context_report_swap: unsafe extern "C" fn(*mut MpvRenderContext),
    pub render_context_free: unsafe extern "C" fn(*mut MpvRenderContext),
}

pub(crate) struct Api {
    // The library must outlive every copied function pointer and mpv object.
    _library: Library,
    pub functions: Functions,
    pub client_api_version: c_ulong,
}

impl Api {
    pub fn load(path: &Path) -> Result<Self, MpvError> {
        // SAFETY: Symbols are resolved below with the ABI signatures from
        // mpv/include/mpv/client.h, render.h, and render_gl.h. The Library is
        // retained in Api for at least as long as those pointers are used.
        let library = unsafe { Library::new(path) }.map_err(|error| MpvError::LibraryOpen {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let bytes = concat!($name, "\0").as_bytes();
                // SAFETY: Each requested name and function type is part of
                // libmpv's exported C client/render API.
                let loaded: Symbol<'_, $ty> =
                    unsafe { library.get(bytes) }.map_err(|error| MpvError::MissingSymbol {
                        path: path.to_path_buf(),
                        symbol: $name,
                        detail: error.to_string(),
                    })?;
                *loaded
            }};
        }

        let functions = Functions {
            client_api_version: symbol!(
                "mpv_client_api_version",
                unsafe extern "C" fn() -> c_ulong
            ),
            create: symbol!("mpv_create", unsafe extern "C" fn() -> *mut MpvHandle),
            set_option_string: symbol!(
                "mpv_set_option_string",
                unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int
            ),
            initialize: symbol!(
                "mpv_initialize",
                unsafe extern "C" fn(*mut MpvHandle) -> c_int
            ),
            set_wakeup_callback: symbol!(
                "mpv_set_wakeup_callback",
                unsafe extern "C" fn(*mut MpvHandle, Option<WakeupCallback>, *mut c_void)
            ),
            wait_event: symbol!(
                "mpv_wait_event",
                unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent
            ),
            command_async: symbol!(
                "mpv_command_async",
                unsafe extern "C" fn(*mut MpvHandle, u64, *const *const c_char) -> c_int
            ),
            get_property_async: symbol!(
                "mpv_get_property_async",
                unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int
            ),
            get_property: symbol!(
                "mpv_get_property",
                unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int
            ),
            get_property_string: symbol!(
                "mpv_get_property_string",
                unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char
            ),
            free: symbol!("mpv_free", unsafe extern "C" fn(*mut c_void)),
            free_node_contents: symbol!(
                "mpv_free_node_contents",
                unsafe extern "C" fn(*mut MpvNode)
            ),
            observe_property: symbol!(
                "mpv_observe_property",
                unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int
            ),
            unobserve_property: symbol!(
                "mpv_unobserve_property",
                unsafe extern "C" fn(*mut MpvHandle, u64) -> c_int
            ),
            terminate_destroy: symbol!(
                "mpv_terminate_destroy",
                unsafe extern "C" fn(*mut MpvHandle)
            ),
            get_time_us: symbol!(
                "mpv_get_time_us",
                unsafe extern "C" fn(*mut MpvHandle) -> i64
            ),
            render_context_create: symbol!(
                "mpv_render_context_create",
                unsafe extern "C" fn(
                    *mut *mut MpvRenderContext,
                    *mut MpvHandle,
                    *mut MpvRenderParam,
                ) -> c_int
            ),
            render_context_set_update_callback: symbol!(
                "mpv_render_context_set_update_callback",
                unsafe extern "C" fn(
                    *mut MpvRenderContext,
                    Option<RenderUpdateCallback>,
                    *mut c_void,
                )
            ),
            render_context_get_info: symbol!(
                "mpv_render_context_get_info",
                unsafe extern "C" fn(*mut MpvRenderContext, MpvRenderParam) -> c_int
            ),
            render_context_render: symbol!(
                "mpv_render_context_render",
                unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int
            ),
            render_context_report_swap: symbol!(
                "mpv_render_context_report_swap",
                unsafe extern "C" fn(*mut MpvRenderContext)
            ),
            render_context_free: symbol!(
                "mpv_render_context_free",
                unsafe extern "C" fn(*mut MpvRenderContext)
            ),
        };
        // SAFETY: client_api_version has no arguments and is exported by the
        // already-loaded library. No mpv handle is required for this query.
        let client_api_version = unsafe { (functions.client_api_version)() };
        #[allow(clippy::useless_conversion)] // c_ulong is u32 on Windows, u64 on Linux and macOS.
        let version = u64::from(client_api_version);
        let major = ((version >> 16) & 0xffff) as u32;
        let minor = (version & 0xffff) as u32;
        if major != REQUIRED_CLIENT_API_MAJOR || minor < REQUIRED_CLIENT_API_MIN_MINOR {
            return Err(MpvError::UnsupportedClientApi {
                found: version,
                required_major: REQUIRED_CLIENT_API_MAJOR,
                required_min_minor: REQUIRED_CLIENT_API_MIN_MINOR,
            });
        }

        Ok(Self {
            _library: library,
            functions,
            client_api_version,
        })
    }
}

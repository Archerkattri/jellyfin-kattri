use std::{
    ffi::{CStr, c_void},
    ptr,
};

use crate::ffi::{
    Api, MPV_FORMAT_NODE, MPV_FORMAT_NODE_ARRAY, MPV_FORMAT_NODE_MAP, MPV_FORMAT_NONE, MpvHandle,
    MpvNode, MpvNodeList, MpvNodeValue,
};

const MAX_PROPERTY_BYTES: usize = 16 * 1024;
const MAX_MAP_KEY_BYTES: usize = 128;
const MAX_LIST_ENTRIES: usize = 2048;
const MAX_LIST_DEPTH: usize = 8;
const MAX_TOTAL_STRING_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityUnavailable {
    PropertyNotAvailable,
    NullResult,
    InvalidUtf8,
    MissingTerminator,
    LimitExceeded,
    MalformedNode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityValue<T> {
    Available(T),
    Unavailable(CapabilityUnavailable),
}

/// A runtime snapshot, not a guarantee that every listed codec can play in
/// real time. Missing libmpv properties remain explicitly unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub mpv_version: CapabilityValue<String>,
    pub ffmpeg_version: CapabilityValue<String>,
    pub decoders: CapabilityValue<Vec<String>>,
    pub demuxers: CapabilityValue<Vec<String>>,
    pub configured_hwdec: CapabilityValue<String>,
    pub active_hwdec: CapabilityValue<String>,
    pub hwdec_interop: CapabilityValue<String>,
    pub audio_devices: CapabilityValue<Vec<String>>,
}

impl CapabilitySnapshot {
    /// Run only on the libmpv owner thread. The pointers returned by libmpv
    /// are copied or freed before this function returns.
    pub(crate) fn probe(api: &Api, handle: *mut MpvHandle) -> Self {
        // SAFETY: The worker calls this after initialization and serializes it
        // with every other client API call. Property names are static NUL
        // terminated strings from mpv's documented property list.
        unsafe {
            Self {
                mpv_version: read_string(api, handle, c"mpv-version"),
                ffmpeg_version: read_string(api, handle, c"ffmpeg-version"),
                decoders: read_node_strings(api, handle, c"decoder-list", "codec"),
                demuxers: read_node_strings(api, handle, c"demuxer-lavf-list", "name"),
                configured_hwdec: read_string(api, handle, c"hwdec"),
                active_hwdec: read_string(api, handle, c"hwdec-current"),
                hwdec_interop: read_string(api, handle, c"hwdec-interop"),
                audio_devices: read_node_strings(api, handle, c"audio-device-list", "name"),
            }
        }
    }
}

pub(crate) unsafe fn read_string(
    api: &Api,
    handle: *mut MpvHandle,
    property: &CStr,
) -> CapabilityValue<String> {
    // SAFETY: The handle is live and owner-thread-confined; libmpv returns a
    // heap string owned by the caller, released with mpv_free.
    let value = unsafe { (api.functions.get_property_string)(handle, property.as_ptr()) };
    if value.is_null() {
        return CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable);
    }
    let result = unsafe { copy_bounded_c_string(value.cast_const()) };
    // SAFETY: This pointer was returned by mpv_get_property_string.
    unsafe { (api.functions.free)(value.cast::<c_void>()) };
    match result {
        Ok(value) => CapabilityValue::Available(value),
        Err(reason) => CapabilityValue::Unavailable(reason),
    }
}

unsafe fn read_node_strings(
    api: &Api,
    handle: *mut MpvHandle,
    property: &CStr,
    identifier_key: &str,
) -> CapabilityValue<Vec<String>> {
    let mut node = MpvNode {
        value: MpvNodeValue {
            byte_array: ptr::null_mut(),
        },
        format: MPV_FORMAT_NONE,
    };
    // SAFETY: The destination is a correctly laid out, initialized mpv_node;
    // the call executes on the initialized handle's owner thread.
    let code = unsafe {
        (api.functions.get_property)(
            handle,
            property.as_ptr(),
            MPV_FORMAT_NODE,
            (&mut node as *mut MpvNode).cast::<c_void>(),
        )
    };
    if code < 0 {
        return CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable);
    }
    // SAFETY: A successful NODE read owns all nested allocations until they
    // are released by mpv_free_node_contents; traversal is bounded below.
    let result = unsafe { collect_node_strings(&node, identifier_key) };
    // SAFETY: node contents came from the successful mpv_get_property call.
    unsafe { (api.functions.free_node_contents)(&mut node) };
    match result {
        Ok(values) => CapabilityValue::Available(values),
        Err(reason) => CapabilityValue::Unavailable(reason),
    }
}

unsafe fn copy_bounded_c_string(
    value: *const std::ffi::c_char,
) -> Result<String, CapabilityUnavailable> {
    unsafe { copy_bounded_c_string_with_limit(value, MAX_PROPERTY_BYTES) }
}

unsafe fn copy_bounded_c_string_with_limit(
    value: *const std::ffi::c_char,
    max_bytes: usize,
) -> Result<String, CapabilityUnavailable> {
    if value.is_null() {
        return Err(CapabilityUnavailable::NullResult);
    }
    let mut length = 0usize;
    while length <= max_bytes {
        // SAFETY: The libmpv-owned property string is NUL-terminated; the
        // explicit bound prevents unbounded scanning if it is malformed.
        if unsafe { value.add(length).read() } == 0 {
            // SAFETY: The range through this terminator was checked above.
            let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length) };
            return std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| CapabilityUnavailable::InvalidUtf8);
        }
        length += 1;
    }
    Err(CapabilityUnavailable::LimitExceeded)
}

unsafe fn collect_node_strings(
    node: &MpvNode,
    identifier_key: &str,
) -> Result<Vec<String>, CapabilityUnavailable> {
    if !matches!(node.format, MPV_FORMAT_NODE_ARRAY | MPV_FORMAT_NODE_MAP) {
        return Err(CapabilityUnavailable::MalformedNode);
    }
    let mut output = Vec::new();
    let mut total_bytes = 0;
    unsafe { visit_collection(node, identifier_key, 0, &mut output, &mut total_bytes)? };
    Ok(output)
}

unsafe fn visit_collection(
    node: &MpvNode,
    identifier_key: &str,
    depth: usize,
    output: &mut Vec<String>,
    total_bytes: &mut usize,
) -> Result<(), CapabilityUnavailable> {
    if depth > MAX_LIST_DEPTH || output.len() > MAX_LIST_ENTRIES {
        return Err(CapabilityUnavailable::LimitExceeded);
    }
    // libmpv places an mpv_node_list pointer in the union for compound nodes.
    let list = unsafe { node.value.list };
    if list.is_null() {
        return Err(CapabilityUnavailable::MalformedNode);
    }
    let list = unsafe { &*list };
    match node.format {
        MPV_FORMAT_NODE_ARRAY => unsafe {
            visit_array(list, identifier_key, depth + 1, output, total_bytes)?
        },
        MPV_FORMAT_NODE_MAP => unsafe {
            visit_map(list, identifier_key, depth + 1, output, total_bytes)?
        },
        _ => return Err(CapabilityUnavailable::MalformedNode),
    }
    Ok(())
}

unsafe fn visit_array(
    list: &MpvNodeList,
    identifier_key: &str,
    depth: usize,
    output: &mut Vec<String>,
    total_bytes: &mut usize,
) -> Result<(), CapabilityUnavailable> {
    validate_list_count_and_values(list)?;
    for index in 0..list.count as usize {
        // SAFETY: positive count and non-null array were validated; indices
        // stay strictly below libmpv's reported array length.
        let child = unsafe { &*list.values.add(index) };
        match child.format {
            1 => {
                // Keep support for properties represented as arrays of plain
                // strings, while maps are handled using their named field.
                let value = unsafe { copy_bounded_c_string(child.value.string.cast_const()) }?;
                push_identifier(value, output, total_bytes)?;
            }
            MPV_FORMAT_NODE_ARRAY | MPV_FORMAT_NODE_MAP => unsafe {
                visit_collection(child, identifier_key, depth, output, total_bytes)?
            },
            // Ignore non-string scalar entries; they are not identifiers.
            _ => {}
        }
        if output.len() > MAX_LIST_ENTRIES {
            return Err(CapabilityUnavailable::LimitExceeded);
        }
    }
    Ok(())
}

unsafe fn visit_map(
    list: &MpvNodeList,
    identifier_key: &str,
    _depth: usize,
    output: &mut Vec<String>,
    total_bytes: &mut usize,
) -> Result<(), CapabilityUnavailable> {
    validate_list_count_and_values(list)?;
    if list.count > 0 && list.keys.is_null() {
        return Err(CapabilityUnavailable::MalformedNode);
    }
    for index in 0..list.count as usize {
        // SAFETY: count, values, and keys are validated and index is in range.
        let key_ptr = unsafe { *list.keys.add(index) };
        let key =
            unsafe { copy_bounded_c_string_with_limit(key_ptr.cast_const(), MAX_MAP_KEY_BYTES)? };
        if key != identifier_key {
            continue;
        }
        // SAFETY: validated list storage and in-range index.
        let value = unsafe { &*list.values.add(index) };
        if value.format != 1 {
            return Err(CapabilityUnavailable::MalformedNode);
        }
        let value = unsafe { copy_bounded_c_string(value.value.string.cast_const()) }?;
        push_identifier(value, output, total_bytes)?;
    }
    Ok(())
}

fn validate_list_count_and_values(list: &MpvNodeList) -> Result<(), CapabilityUnavailable> {
    if list.count < 0 || list.count as usize > MAX_LIST_ENTRIES {
        return Err(CapabilityUnavailable::LimitExceeded);
    }
    if list.count > 0 && list.values.is_null() {
        return Err(CapabilityUnavailable::MalformedNode);
    }
    Ok(())
}

fn push_identifier(
    value: String,
    output: &mut Vec<String>,
    total_bytes: &mut usize,
) -> Result<(), CapabilityUnavailable> {
    *total_bytes = total_bytes.saturating_add(value.len());
    if *total_bytes > MAX_TOTAL_STRING_BYTES || output.len() >= MAX_LIST_ENTRIES {
        return Err(CapabilityUnavailable::LimitExceeded);
    }
    output.push(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_capabilities_are_not_collapsed_to_false() {
        let value: CapabilityValue<String> =
            CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable);
        assert_eq!(
            value,
            CapabilityValue::Unavailable(CapabilityUnavailable::PropertyNotAvailable)
        );
    }

    #[test]
    fn bounded_string_reader_rejects_missing_terminator() {
        let value = vec![b'x' as std::ffi::c_char; MAX_PROPERTY_BYTES + 1];
        // Synthetic boundary fixture: this tests only the reader limit and is
        // not presented as captured output from a libmpv runtime.
        let result = unsafe { copy_bounded_c_string(value.as_ptr()) };
        assert_eq!(result, Err(CapabilityUnavailable::LimitExceeded));
    }

    #[test]
    fn node_reader_rejects_a_non_node_root_format() {
        let node = MpvNode {
            value: MpvNodeValue { integer: 4 },
            format: 4,
        };
        // Synthetic ABI-boundary fixture; no runtime capability is inferred.
        let result = unsafe { collect_node_strings(&node, "codec") };
        assert_eq!(result, Err(CapabilityUnavailable::MalformedNode));
    }

    #[test]
    fn decoder_map_extracts_only_the_codec_identifier() {
        let fixture = map_fixture(&[
            ("codec", "h264"),
            ("driver", "h264_qsv"),
            ("description", "H.264 decoder description"),
        ]);
        // Synthetic mpv_node fixture based on the documented decoder-list
        // shape; it is not captured output from a runtime.
        let result = unsafe { collect_node_strings(&fixture.node, "codec") };
        assert_eq!(result, Ok(vec!["h264".to_owned()]));
        drop(fixture);
    }

    #[test]
    fn named_map_extracts_only_name_and_plain_string_arrays_are_supported() {
        let fixture = map_fixture(&[
            ("name", "alsa/default"),
            ("description", "Human-readable device description"),
        ]);
        // Synthetic device-map fixture; only the option-facing name is kept.
        let result = unsafe { collect_node_strings(&fixture.node, "name") };
        assert_eq!(result, Ok(vec!["alsa/default".to_owned()]));
        drop(fixture);

        let strings = ["matroska", "mov"]
            .map(|value| std::ffi::CString::new(value).expect("static fixture has no NUL"));
        let mut values = strings
            .iter()
            .map(|value| MpvNode {
                value: MpvNodeValue {
                    string: value.as_ptr().cast_mut(),
                },
                format: 1,
            })
            .collect::<Vec<_>>();
        let mut list = Box::new(MpvNodeList {
            count: values.len() as i32,
            values: values.as_mut_ptr(),
            keys: ptr::null_mut(),
        });
        let node = MpvNode {
            value: MpvNodeValue { list: &mut *list },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        // Synthetic demuxer-list fixture covering the documented name-only
        // representation, which can be read as a simple string array.
        let result = unsafe { collect_node_strings(&node, "name") };
        assert_eq!(result, Ok(vec!["matroska".to_owned(), "mov".to_owned()]));
        drop((strings, values, list));
    }

    #[test]
    fn map_key_parsing_is_bounded() {
        let key = "k".repeat(MAX_MAP_KEY_BYTES + 1);
        let fixture = map_fixture(&[(&key, "value")]);
        let result = unsafe { collect_node_strings(&fixture.node, "codec") };
        assert_eq!(result, Err(CapabilityUnavailable::LimitExceeded));
        drop(fixture);
    }

    struct MapFixture {
        _key_strings: Vec<std::ffi::CString>,
        _value_strings: Vec<std::ffi::CString>,
        _keys: Vec<*mut std::ffi::c_char>,
        _values: Vec<MpvNode>,
        _list: Box<MpvNodeList>,
        node: MpvNode,
    }

    fn map_fixture(fields: &[(&str, &str)]) -> MapFixture {
        let key_strings = fields
            .iter()
            .map(|(key, _)| std::ffi::CString::new(*key).expect("fixture key has no NUL"))
            .collect::<Vec<_>>();
        let value_strings = fields
            .iter()
            .map(|(_, value)| std::ffi::CString::new(*value).expect("fixture value has no NUL"))
            .collect::<Vec<_>>();
        let mut keys = key_strings
            .iter()
            .map(|key| key.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let mut values = value_strings
            .iter()
            .map(|value| MpvNode {
                value: MpvNodeValue {
                    string: value.as_ptr().cast_mut(),
                },
                format: 1,
            })
            .collect::<Vec<_>>();
        let mut list = Box::new(MpvNodeList {
            count: values.len() as i32,
            values: values.as_mut_ptr(),
            keys: keys.as_mut_ptr(),
        });
        let node = MpvNode {
            value: MpvNodeValue { list: &mut *list },
            format: MPV_FORMAT_NODE_MAP,
        };
        MapFixture {
            _key_strings: key_strings,
            _value_strings: value_strings,
            _keys: keys,
            _values: values,
            _list: list,
            node,
        }
    }
}

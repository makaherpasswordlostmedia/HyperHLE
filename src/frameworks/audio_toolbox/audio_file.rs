/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `AudioFile.h` (Audio File Services)

use crate::abi::{CallFromHost, GuestFunction};
use crate::audio; // Keep this module namespaced to avoid confusion
use crate::dyld::{export_c_func, FunctionExports};
use crate::frameworks::carbon_core::{eofErr, OSStatus};
use crate::frameworks::core_audio_types::{debug_fourcc, fourcc, AudioStreamBasicDescription};
use crate::frameworks::core_foundation::cf_url::CFURLRef;
use crate::frameworks::foundation::ns_url::to_rust_path;
use crate::mem::{guest_size_of, GuestUSize, MutPtr, MutVoidPtr, SafeRead};
use crate::Environment;
use std::collections::HashMap;

#[derive(Default)]
pub struct State {
    pub audio_files: HashMap<AudioFileID, AudioFileHostObject>,
    pub audio_file_streams: HashMap<AudioFileStreamID, AudioFileStreamHostObject>,
}
impl State {
    pub fn get(framework_state: &mut crate::frameworks::State) -> &mut Self {
        &mut framework_state.audio_toolbox.audio_file
    }
}

pub struct AudioFileHostObject {
    pub audio_file: audio::AudioFile,
}

#[repr(C, packed)]
pub struct OpaqueAudioFileID {
    _filler: u8,
}
unsafe impl SafeRead for OpaqueAudioFileID {}

pub type AudioFileID = MutPtr<OpaqueAudioFileID>;

#[repr(C, packed)]
pub struct OpaqueAudioFileStreamID {
    _filler: u8,
}
unsafe impl SafeRead for OpaqueAudioFileStreamID {}

pub type AudioFileStreamID = MutPtr<OpaqueAudioFileStreamID>;

/// Host-side state for `AudioFileStream*`. Since our underlying `audio::AudioFile`
/// parser needs the whole file up-front, we buffer every byte handed to us via
/// `AudioFileStreamParseBytes()` and only actually parse once we can. This is not
/// "real" incremental/streaming parsing, but it lets apps that use the
/// AudioFileStream API (rather than AudioFile) still get working audio playback.
pub struct AudioFileStreamHostObject {
    client_data: MutVoidPtr,
    property_listener_proc: GuestFunction,
    packets_proc: GuestFunction,
    #[allow(dead_code)]
    file_type_hint: AudioFileTypeID,
    /// All bytes seen so far via `AudioFileStreamParseBytes()`.
    buffer: Vec<u8>,
    /// Set once we've successfully parsed `buffer` and informed the client
    /// of the data format / that packets can be produced.
    parsed_audio_file: Option<audio::AudioFile>,
    /// How many bytes of `buffer` we've already reported as packets.
    bytes_delivered: usize,
}

#[repr(C, packed)]
struct AudioFilePacketTableInfo {
    number_valid_frames: i64,
    priming_frames: i32,
    remainder_frames: i32,
}
unsafe impl SafeRead for AudioFilePacketTableInfo {}

#[allow(dead_code)]
const kAudioFileFileNotFoundError: OSStatus = -43;
pub const kAudioFileBadPropertySizeError: OSStatus = fourcc(b"!siz") as _;
const kAudioFileUnsupportedPropertyError: OSStatus = fourcc(b"pty?") as _;
const kAudioFileUnsupportedFileTypeError: OSStatus = fourcc(b"typ?") as _;
const kAudioFileUnspecifiedError: OSStatus = fourcc(b"wht?") as _;

type AudioFilePermissions = i8;
pub const kAudioFileReadPermission: AudioFilePermissions = 1;

/// Usually a FourCC.
type AudioFileTypeID = u32;
const kAudioFileCAFType: AudioFileTypeID = fourcc(b"caff");
const kAUdioFileAIFFType: AudioFileTypeID = fourcc(b"AIFF");

/// Usually a FourCC.
type AudioFilePropertyID = u32;
pub const kAudioFilePropertyFileFormat: AudioFilePropertyID = fourcc(b"ffmt");
pub const kAudioFilePropertyDataFormat: AudioFilePropertyID = fourcc(b"dfmt");
const kAudioFilePropertyAudioDataByteCount: AudioFilePropertyID = fourcc(b"bcnt");
const kAudioFilePropertyAudioDataPacketCount: AudioFilePropertyID = fourcc(b"pcnt");
pub const kAudioFilePropertyPacketSizeUpperBound: AudioFilePropertyID = fourcc(b"pkub");
const kAudioFilePropertyMagicCookieData: AudioFilePropertyID = fourcc(b"mgic");
const kAudioFilePropertyChannelLayout: AudioFilePropertyID = fourcc(b"cmap");
const kAudioFilePropertyEstimatedDuration: AudioFilePropertyID = fourcc(b"edur");
const kAudioFilePropertyPacketTableInfo: AudioFilePropertyID = fourcc(b"pnfo");

/// Usually a FourCC. These are `AudioFileStreamPropertyID`s, distinct from
/// (but overlapping in spirit with) `AudioFilePropertyID`s.
type AudioFileStreamPropertyID = u32;
const kAudioFileStreamProperty_ReadyToProducePackets: AudioFileStreamPropertyID =
    fourcc(b"redy");
const kAudioFileStreamProperty_DataFormat: AudioFileStreamPropertyID = fourcc(b"dfmt");
const kAudioFileStreamProperty_FileFormat: AudioFileStreamPropertyID = fourcc(b"ffmt");
const kAudioFileStreamProperty_MagicCookieData: AudioFileStreamPropertyID = fourcc(b"mgic");
const kAudioFileStreamProperty_MaximumPacketSize: AudioFileStreamPropertyID = fourcc(b"pkub");
const kAudioFileStreamProperty_AudioDataByteCount: AudioFileStreamPropertyID = fourcc(b"bcnt");
const kAudioFileStreamProperty_AudioDataPacketCount: AudioFileStreamPropertyID = fourcc(b"pcnt");

/// Flags for the `AudioFileStream_PropertyListenerProc` callback.
#[allow(dead_code)]
const kAudioFileStreamPropertyFlag_PropertyIsCached: u32 = 1;

/// Bit flags passed to the packets callback. We only ever pass 0 (no discontinuity).
#[allow(dead_code)]
const kAudioFileStreamParseFlag_Discontinuity: u32 = 1;

pub fn AudioFileOpenURL(
    env: &mut Environment,
    in_file_ref: CFURLRef,
    in_permissions: AudioFilePermissions,
    in_file_type_hint: AudioFileTypeID,
    out_audio_file: MutPtr<AudioFileID>,
) -> OSStatus {
    return_if_null!(in_file_ref);

    assert!(in_permissions == kAudioFileReadPermission); // writing TODO

    // The hint is optional and is supposed to only be used for certain file
    // formats that can't be uniquely identified, which we don't support so far.
    // Hints for well-known types are ignored as well.
    match in_file_type_hint {
        0 => {}
        kAudioFileCAFType => {
            log!("Ignoring 'caff' file type hint for AudioFileOpenURL()");
        }
        kAUdioFileAIFFType => {
            log!("Ignoring 'AIFF' file type hint for AudioFileOpenURL()");
        }
        _ => unimplemented!(),
    }

    let path = to_rust_path(env, in_file_ref);
    let audio_file = match audio::AudioFile::open_for_reading(path, &env.fs) {
        Ok(audio_file) => audio_file,
        Err(error) => {
            log!(
                "Warning: AudioFileOpenURL() for path {:?} failed",
                in_file_ref
            );
            return match error {
                audio::AudioFileOpenError::FileDecodeError => kAudioFileUnsupportedFileTypeError,
                _ => kAudioFileUnspecifiedError,
            };
        }
    };

    let host_object = AudioFileHostObject { audio_file };

    let guest_audio_file = env.mem.alloc_and_write(OpaqueAudioFileID { _filler: 0 });
    State::get(&mut env.framework_state)
        .audio_files
        .insert(guest_audio_file, host_object);

    env.mem.write(out_audio_file, guest_audio_file);

    log_dbg!(
        "AudioFileOpenURL() opened path {:?}, new audio file handle: {:?}",
        in_file_ref,
        guest_audio_file
    );

    0 // success
}

pub fn AudioFileOpenWithCallbacks(
    env: &mut Environment,
    client_data: MutVoidPtr,
    // typedef OSStatus (*AudioFile_ReadProc)
    //      (void *inClientData,
    //       SInt64 inPosition,
    //       UInt32 requestCount,
    //       void *buffer,
    //       UInt32 *actualCount);
    read_callback: GuestFunction,
    // typedef OSStatus (*AudioFile_WriteProc)
    //      (void *inClientData,
    //       SInt64 inPosition,
    //       UInt32 requestCount,
    //       const void *buffer,
    //       UInt32 *actualCount);
    _write_callback: GuestFunction,
    // typedef SInt64 (*AudioFile_GetSizeProc)(void *inClientData);
    getsize_callback: GuestFunction,
    // typedef OSStatus (*AudioFile_SetSizeProc)
    //      (void *inClientData, SInt64 inSize);
    _setsize_callback: GuestFunction,
    in_file_type_hint: AudioFileTypeID,
    out_audio_file: MutPtr<AudioFileID>,
) -> OSStatus {
    if _write_callback.to_ptr().is_null() || _setsize_callback.to_ptr().is_null() {
        log_dbg!("AudioFileOpenWithCallbacks() called with (unsupported) write({:?})/set_size({:?}) callbacks!",
            _write_callback,
            _setsize_callback);
    }
    // The hint is optional and is supposed to only be used for certain file
    // formats that can't be uniquely identified, which we don't support so far.
    if in_file_type_hint != 0 {
        log!("Ignoring file type hint for AudioFileOpenWithCallbacks()");
    }

    // TODO: We're just reading in the whole file at once and parsing it here,
    // this should change when streaming parsing is implemented.
    let size: i64 = getsize_callback.call_from_host(env, (client_data,));
    let size: u32 = size.try_into().unwrap();

    assert!(
        size != 0,
        "0 byte size of file for AudioFileOpenWithCallbacks(), likely bad!"
    );

    let data_ptr: MutPtr<u8> = env.mem.alloc(size).cast();
    let bytes_read_ptr: MutPtr<u32> = env.mem.alloc(guest_size_of::<u32>()).cast();

    env.mem.write(bytes_read_ptr, 0);
    log_dbg!(
        "AudioFileOpenWithCallbacks() calling read: {:?}",
        (client_data, 0_i64, size, data_ptr, bytes_read_ptr)
    );
    let status: OSStatus =
        read_callback.call_from_host(env, (client_data, 0_i64, size, data_ptr, bytes_read_ptr));
    if status != 0 {
        log!(
            "AudioFileOpenWithCallbacks() failed read, returning {}",
            fourcc(&status.to_le_bytes())
        );

        return status;
    }

    assert!(
        env.mem.read(bytes_read_ptr) == size,
        "Bytes read != size for AudioFileOpenWithCallbacks(), likely bad!"
    );

    let data_vec = env
        .mem
        .bytes_at(data_ptr, env.mem.read(bytes_read_ptr))
        .to_vec();

    let Ok(guest_audio_file) = guest_audio_file_read_from_vec(env, data_vec) else {
        log!("Warning: AudioFileOpenWithCallbacks() failed parse");
        return kAudioFileUnsupportedFileTypeError;
    };

    env.mem.write(out_audio_file, guest_audio_file);

    log_dbg!(
        "AudioFileOpenWithCallbacks() opened, new audio file handle: {:?}",
        guest_audio_file
    );

    0 // success
}

pub(super) fn property_size(property_id: AudioFilePropertyID) -> GuestUSize {
    match property_id {
        kAudioFilePropertyFileFormat => guest_size_of::<u32>(),
        kAudioFilePropertyDataFormat => guest_size_of::<AudioStreamBasicDescription>(),
        kAudioFilePropertyAudioDataByteCount => guest_size_of::<u64>(),
        kAudioFilePropertyAudioDataPacketCount => guest_size_of::<u64>(),
        kAudioFilePropertyPacketSizeUpperBound => guest_size_of::<u32>(),
        kAudioFilePropertyEstimatedDuration => guest_size_of::<f64>(),
        kAudioFilePropertyPacketTableInfo => guest_size_of::<AudioFilePacketTableInfo>(),
        _ => unimplemented!("Unimplemented property ID: {}", debug_fourcc(property_id)),
    }
}

fn AudioFileGetPropertyInfo(
    env: &mut Environment,
    in_audio_file: AudioFileID,
    in_property_id: AudioFilePropertyID,
    out_data_size: MutPtr<u32>,
    is_writable: MutPtr<u32>,
) -> OSStatus {
    return_if_null!(in_audio_file);

    if in_property_id == kAudioFilePropertyMagicCookieData
        || in_property_id == kAudioFilePropertyChannelLayout
    {
        // Our currently supported formats probably don't use these properties.
        // Not sure if this is correct, but it skips some code we don't want to
        // run in Touch & Go.
        if !out_data_size.is_null() {
            env.mem.write(out_data_size, 0);
        }
        if !is_writable.is_null() {
            env.mem.write(is_writable, 0);
        }
        return kAudioFileUnsupportedPropertyError;
    }
    if !out_data_size.is_null() {
        env.mem.write(out_data_size, property_size(in_property_id));
    }
    if !is_writable.is_null() {
        env.mem.write(is_writable, 0); // TODO: probably not always correct
    }
    0 // success
}

pub fn AudioFileGetProperty(
    env: &mut Environment,
    in_audio_file: AudioFileID,
    in_property_id: AudioFilePropertyID,
    io_data_size: MutPtr<u32>,
    out_property_data: MutVoidPtr,
) -> OSStatus {
    return_if_null!(in_audio_file);

    let required_size = property_size(in_property_id);
    if env.mem.read(io_data_size) != required_size {
        log!(
            "Warning: AudioFileGetProperty({}) failed, {} != {}",
            debug_fourcc(in_property_id),
            env.mem.read(io_data_size),
            required_size
        );
        return kAudioFileBadPropertySizeError;
    }

    let host_object = State::get(&mut env.framework_state)
        .audio_files
        .get_mut(&in_audio_file)
        .unwrap();

    match in_property_id {
        kAudioFilePropertyFileFormat => {
            let bundle_id = env.bundle.bundle_identifier();
            if bundle_id.starts_with("com.ea.mirrorsedge.bv")
                || bundle_id.starts_with("com.ea.mirrorsedge.inc")
            {
                log!("Applying game-specific hack for Mirror's Edge: returning WAVE for kAudioFilePropertyFileFormat in AudioFileGetProperty()");
                env.mem.write(out_property_data.cast(), fourcc(b"WAVE"));
            } else {
                todo!()
            }
        }
        kAudioFilePropertyDataFormat => {
            let desc = AudioStreamBasicDescription::from_audio_description(
                host_object.audio_file.audio_description(),
            );
            env.mem.write(out_property_data.cast(), desc);
        }
        kAudioFilePropertyAudioDataByteCount => {
            let byte_count: u64 = host_object.audio_file.byte_count();
            env.mem.write(out_property_data.cast(), byte_count);
        }
        kAudioFilePropertyAudioDataPacketCount => {
            let packet_count: u64 = host_object.audio_file.packet_count();
            env.mem.write(out_property_data.cast(), packet_count);
        }
        kAudioFilePropertyPacketSizeUpperBound => {
            let packet_size_upper_bound: u32 = host_object.audio_file.packet_size_upper_bound();
            env.mem
                .write(out_property_data.cast(), packet_size_upper_bound);
        }
        kAudioFilePropertyEstimatedDuration => {
            let estimated_duration = host_object.audio_file.estimated_duration();
            env.mem.write(out_property_data.cast(), estimated_duration);
        }
        kAudioFilePropertyPacketTableInfo => {
            log!("TODO: AudioFileGetProperty({:?}, kAudioFilePropertyPacketTableInfo, {:?}, {:?}) -> kAudioFileUnsupportedPropertyError", in_audio_file, io_data_size, out_property_data);
            return kAudioFileUnsupportedPropertyError;
        }
        _ => unreachable!(),
    }

    0 // success
}

pub fn AudioFileReadBytes(
    env: &mut Environment,
    in_audio_file: AudioFileID,
    _in_use_cache: bool,
    in_starting_byte: i64,
    io_num_bytes: MutPtr<u32>,
    out_buffer: MutVoidPtr,
) -> OSStatus {
    return_if_null!(in_audio_file);

    let host_object = State::get(&mut env.framework_state)
        .audio_files
        .get_mut(&in_audio_file)
        .unwrap();

    let bytes_to_read = env.mem.read(io_num_bytes);
    let buffer_slice = env.mem.bytes_at_mut(out_buffer.cast(), bytes_to_read);

    let bytes_read = host_object
        .audio_file
        .read_bytes(in_starting_byte.try_into().unwrap(), buffer_slice)
        .unwrap(); // TODO: handle seek error?
    env.mem.write(io_num_bytes, bytes_read.try_into().unwrap());

    if bytes_read < bytes_to_read as usize {
        eofErr
    } else {
        0 // success
    }
}

fn AudioFileReadPacketData(
    env: &mut Environment,
    in_audio_file: AudioFileID,
    in_use_cache: bool,
    out_num_bytes: MutPtr<u32>,
    out_packet_descriptions: MutVoidPtr, // unimplemented
    in_starting_packet: i64,
    io_num_packets: MutPtr<u32>,
    out_buffer: MutVoidPtr,
) -> OSStatus {
    // TODO: real VBR support
    AudioFileReadPackets(
        env,
        in_audio_file,
        in_use_cache,
        out_num_bytes,
        out_packet_descriptions,
        in_starting_packet,
        io_num_packets,
        out_buffer,
    )
}

pub fn AudioFileReadPackets(
    env: &mut Environment,
    in_audio_file: AudioFileID,
    in_use_cache: bool,
    out_num_bytes: MutPtr<u32>,
    out_packet_descriptions: MutVoidPtr, // unimplemented
    in_starting_packet: i64,
    io_num_packets: MutPtr<u32>,
    out_buffer: MutVoidPtr,
) -> OSStatus {
    return_if_null!(in_audio_file);

    // Variable-size packets are not implemented currently. When they are,
    // this parameter should be a `MutPtr<AudioStreamPacketDescription>`.
    if !out_packet_descriptions.is_null() {
        log!("Warning: ignoring non-null out_packet_descriptions in AudioFileReadPackets()");
    }

    let host_object = State::get(&mut env.framework_state)
        .audio_files
        .get_mut(&in_audio_file)
        .unwrap();
    let packet_size = host_object.audio_file.packet_size_fixed();

    let packets_to_read = env.mem.read(io_num_packets);

    let starting_byte = i64::from(packet_size)
        .checked_mul(in_starting_packet)
        .unwrap();
    let bytes_to_read = packets_to_read.checked_mul(packet_size).unwrap();

    env.mem.write(out_num_bytes, bytes_to_read);
    let res = AudioFileReadBytes(
        env,
        in_audio_file,
        in_use_cache,
        starting_byte,
        out_num_bytes,
        out_buffer,
    );

    let bytes_read = env.mem.read(out_num_bytes);
    let packets_read = bytes_read / packet_size;
    env.mem.write(io_num_packets, packets_read);

    res
}

pub fn AudioFileClose(env: &mut Environment, in_audio_file: AudioFileID) -> OSStatus {
    return_if_null!(in_audio_file);

    let Some(_host_object) = State::get(&mut env.framework_state)
        .audio_files
        .remove(&in_audio_file)
    else {
        log!(
            "Bad AudioFileClose for {:?} (likely double close), ignoring!",
            in_audio_file
        );
        return kAudioFileUnspecifiedError;
    };
    env.mem.free(in_audio_file.cast());
    log_dbg!(
        "AudioFileClose() destroyed audio file handle: {:?}",
        in_audio_file
    );
    0 // success
}

fn AudioFileStreamOpen(
    env: &mut Environment,
    in_client_data: MutVoidPtr,
    // typedef void (*AudioFileStream_PropertyListenerProc)(
    //     void *inClientData,
    //     AudioFileStreamID inAudioFileStream,
    //     AudioFileStreamPropertyID inPropertyID,
    //     UInt32 *ioFlags);
    in_property_listener_proc: GuestFunction,
    // typedef void (*AudioFileStream_PacketsProc)(
    //     void *inClientData,
    //     UInt32 inNumberBytes,
    //     UInt32 inNumberPackets,
    //     const void *inInputData,
    //     AudioStreamPacketDescription *inPacketDescriptions);
    in_packets_proc: GuestFunction,
    in_file_type_hint: AudioFileTypeID,
    out_audio_file_stream: MutPtr<AudioFileStreamID>,
) -> OSStatus {
    let host_object = AudioFileStreamHostObject {
        client_data: in_client_data,
        property_listener_proc: in_property_listener_proc,
        packets_proc: in_packets_proc,
        file_type_hint: in_file_type_hint,
        buffer: Vec::new(),
        parsed_audio_file: None,
        bytes_delivered: 0,
    };

    let guest_stream = env
        .mem
        .alloc_and_write(OpaqueAudioFileStreamID { _filler: 0 });
    State::get(&mut env.framework_state)
        .audio_file_streams
        .insert(guest_stream, host_object);

    env.mem.write(out_audio_file_stream, guest_stream);

    log_dbg!(
        "AudioFileStreamOpen() new audio file stream handle: {:?}",
        guest_stream
    );

    0 // success
}

/// Tries to (re-)parse everything buffered so far, and if that succeeds for
/// the first time, fires off the property-listener callback(s) that tell the
/// client the data format is known and it's ready to produce packets.
/// Returns `true` if `parsed_audio_file` is populated after this call.
fn try_parse_buffered_stream(
    env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
) -> bool {
    let host_object = State::get(&mut env.framework_state)
        .audio_file_streams
        .get_mut(&in_audio_file_stream)
        .unwrap();

    if host_object.parsed_audio_file.is_some() {
        return true;
    }

    // Cheap to clone since this only actually runs (successfully) once; on
    // failure we just keep buffering and try again next call.
    let buffer_clone = host_object.buffer.clone();
    let Ok(audio_file) = audio::AudioFile::read_from_vec(buffer_clone) else {
        return false;
    };

    let host_object = State::get(&mut env.framework_state)
        .audio_file_streams
        .get_mut(&in_audio_file_stream)
        .unwrap();
    host_object.parsed_audio_file = Some(audio_file);

    let client_data = host_object.client_data;
    let property_listener_proc = host_object.property_listener_proc;

    let ready_flags_ptr: MutPtr<u32> = env.mem.alloc(guest_size_of::<u32>()).cast();
    env.mem.write(ready_flags_ptr, 0);

    // Tell the client the data format is available.
    let () = property_listener_proc.call_from_host(
        env,
        (
            client_data,
            in_audio_file_stream,
            kAudioFileStreamProperty_DataFormat,
            ready_flags_ptr,
        ),
    );

    // Tell the client it can now start producing packets.
    env.mem.write(ready_flags_ptr, 0);
    let () = property_listener_proc.call_from_host(
        env,
        (
            client_data,
            in_audio_file_stream,
            kAudioFileStreamProperty_ReadyToProducePackets,
            ready_flags_ptr,
        ),
    );

    env.mem.free(ready_flags_ptr.cast());

    true
}

fn AudioFileStreamParseBytes(
    env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
    in_data_byte_size: u32,
    in_data: MutVoidPtr,
    _in_discontinuity: bool,
) -> OSStatus {
    return_if_null!(in_audio_file_stream);

    let new_bytes = env
        .mem
        .bytes_at(in_data.cast(), in_data_byte_size)
        .to_vec();

    let host_object = State::get(&mut env.framework_state)
        .audio_file_streams
        .get_mut(&in_audio_file_stream)
        .unwrap();
    host_object.buffer.extend_from_slice(&new_bytes);

    if !try_parse_buffered_stream(env, in_audio_file_stream) {
        // Not enough data yet to figure out the format; that's fine, wait
        // for more bytes on a future call.
        return 0;
    }

    // We now have a fully parsed file. Hand over any packets we haven't
    // delivered to the client yet, in one go (not truly incremental, but
    // the client-visible behaviour -- packets proc getting called with
    // audio data -- ends up correct).
    let host_object = State::get(&mut env.framework_state)
        .audio_file_streams
        .get_mut(&in_audio_file_stream)
        .unwrap();

    let audio_file = host_object.parsed_audio_file.as_ref().unwrap();
    let packet_size = audio_file.packet_size_fixed();
    if packet_size == 0 {
        return 0;
    }

    let total_bytes = host_object.buffer.len();
    let available_bytes = total_bytes - (total_bytes % packet_size as usize);
    let undelivered_bytes = available_bytes.saturating_sub(host_object.bytes_delivered);
    if undelivered_bytes == 0 {
        return 0;
    }

    let start = host_object.bytes_delivered;
    let chunk = host_object.buffer[start..start + undelivered_bytes].to_vec();
    let num_packets = (undelivered_bytes as u32) / packet_size;

    host_object.bytes_delivered += undelivered_bytes;

    let client_data = host_object.client_data;
    let packets_proc = host_object.packets_proc;

    let chunk_ptr: MutPtr<u8> = env.mem.alloc(undelivered_bytes as GuestUSize).cast();
    env.mem.bytes_at_mut(chunk_ptr, undelivered_bytes as GuestUSize)
        .copy_from_slice(&chunk);

    let () = packets_proc.call_from_host(
        env,
        (
            client_data,
            undelivered_bytes as u32,
            num_packets,
            chunk_ptr,
            MutVoidPtr::null(), // packet descriptions: unimplemented (fixed-size packets only)
        ),
    );

    env.mem.free(chunk_ptr.cast());

    0 // success
}

fn AudioFileStreamGetPropertyInfo(
    env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
    in_property_id: AudioFileStreamPropertyID,
    out_property_data_size: MutPtr<u32>,
    is_writable: MutPtr<u32>,
) -> OSStatus {
    return_if_null!(in_audio_file_stream);

    let size = match in_property_id {
        kAudioFileStreamProperty_DataFormat => guest_size_of::<AudioStreamBasicDescription>(),
        kAudioFileStreamProperty_FileFormat => guest_size_of::<u32>(),
        kAudioFileStreamProperty_MaximumPacketSize => guest_size_of::<u32>(),
        kAudioFileStreamProperty_AudioDataByteCount => guest_size_of::<u64>(),
        kAudioFileStreamProperty_AudioDataPacketCount => guest_size_of::<u64>(),
        kAudioFileStreamProperty_MagicCookieData => {
            if !out_property_data_size.is_null() {
                env.mem.write(out_property_data_size, 0);
            }
            if !is_writable.is_null() {
                env.mem.write(is_writable, 0);
            }
            return kAudioFileUnsupportedPropertyError;
        }
        _ => {
            log!(
                "Warning: AudioFileStreamGetPropertyInfo() unsupported property {}",
                debug_fourcc(in_property_id)
            );
            return kAudioFileUnsupportedPropertyError;
        }
    };

    if !out_property_data_size.is_null() {
        env.mem.write(out_property_data_size, size);
    }
    if !is_writable.is_null() {
        env.mem.write(is_writable, 0);
    }
    0 // success
}

fn AudioFileStreamGetProperty(
    env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
    in_property_id: AudioFileStreamPropertyID,
    io_property_data_size: MutPtr<u32>,
    out_property_data: MutVoidPtr,
) -> OSStatus {
    return_if_null!(in_audio_file_stream);

    let host_object = State::get(&mut env.framework_state)
        .audio_file_streams
        .get(&in_audio_file_stream)
        .unwrap();

    let Some(audio_file) = host_object.parsed_audio_file.as_ref() else {
        log!("Warning: AudioFileStreamGetProperty() called before format is known");
        return kAudioFileUnspecifiedError;
    };

    match in_property_id {
        kAudioFileStreamProperty_DataFormat => {
            let desc =
                AudioStreamBasicDescription::from_audio_description(audio_file.audio_description());
            env.mem.write(
                io_property_data_size,
                guest_size_of::<AudioStreamBasicDescription>(),
            );
            env.mem.write(out_property_data.cast(), desc);
        }
        kAudioFileStreamProperty_MaximumPacketSize => {
            let packet_size_upper_bound: u32 = audio_file.packet_size_upper_bound();
            env.mem.write(io_property_data_size, guest_size_of::<u32>());
            env.mem
                .write(out_property_data.cast(), packet_size_upper_bound);
        }
        kAudioFileStreamProperty_AudioDataByteCount => {
            let byte_count: u64 = audio_file.byte_count();
            env.mem.write(io_property_data_size, guest_size_of::<u64>());
            env.mem.write(out_property_data.cast(), byte_count);
        }
        kAudioFileStreamProperty_AudioDataPacketCount => {
            let packet_count: u64 = audio_file.packet_count();
            env.mem.write(io_property_data_size, guest_size_of::<u64>());
            env.mem.write(out_property_data.cast(), packet_count);
        }
        _ => {
            log!(
                "Warning: AudioFileStreamGetProperty() unsupported property {}",
                debug_fourcc(in_property_id)
            );
            return kAudioFileUnsupportedPropertyError;
        }
    }

    0 // success
}

fn AudioFileStreamSeek(
    _env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
    _in_packet_offset: i64,
    _out_data_byte_offset: MutPtr<i64>,
    _out_flags: MutPtr<u32>,
) -> OSStatus {
    return_if_null!(in_audio_file_stream);
    // We don't support real seeking within the (conceptually still-streaming)
    // data. Report failure so the client falls back to sequential reading.
    log!("TODO: AudioFileStreamSeek() not really supported, returning kAudioFileUnspecifiedError!");
    kAudioFileUnspecifiedError
}

fn AudioFileStreamClose(
    env: &mut Environment,
    in_audio_file_stream: AudioFileStreamID,
) -> OSStatus {
    return_if_null!(in_audio_file_stream);

    let Some(_host_object) = State::get(&mut env.framework_state)
        .audio_file_streams
        .remove(&in_audio_file_stream)
    else {
        log!(
            "Bad AudioFileStreamClose for {:?} (likely double close), ignoring!",
            in_audio_file_stream
        );
        return kAudioFileUnspecifiedError;
    };
    env.mem.free(in_audio_file_stream.cast());
    log_dbg!(
        "AudioFileStreamClose() destroyed audio file stream handle: {:?}",
        in_audio_file_stream
    );
    0 // success
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(AudioFileOpenURL(_, _, _, _)),
    export_c_func!(AudioFileGetPropertyInfo(_, _, _, _)),
    export_c_func!(AudioFileGetProperty(_, _, _, _)),
    export_c_func!(AudioFileReadBytes(_, _, _, _, _)),
    export_c_func!(AudioFileReadPackets(_, _, _, _, _, _, _)),
    export_c_func!(AudioFileReadPacketData(_, _, _, _, _, _, _)),
    export_c_func!(AudioFileOpenWithCallbacks(_, _, _, _, _, _, _)),
    export_c_func!(AudioFileClose(_)),
    export_c_func!(AudioFileStreamOpen(_, _, _, _, _)),
    export_c_func!(AudioFileStreamParseBytes(_, _, _, _)),
    export_c_func!(AudioFileStreamGetPropertyInfo(_, _, _, _)),
    export_c_func!(AudioFileStreamGetProperty(_, _, _, _)),
    export_c_func!(AudioFileStreamSeek(_, _, _, _)),
    export_c_func!(AudioFileStreamClose(_)),
];

/// Helper function. Used by `AudioFileOpenWithCallbacks()` function and
/// `[AVAudioPlayer initWithData:error:]` method.
pub(crate) fn guest_audio_file_read_from_vec(
    env: &mut Environment,
    data_vec: Vec<u8>,
) -> Result<AudioFileID, audio::AudioFileOpenError> {
    let audio_file = audio::AudioFile::read_from_vec(data_vec)?;
    let guest_audio_file = env.mem.alloc_and_write(OpaqueAudioFileID { _filler: 0 });

    let host_object = AudioFileHostObject { audio_file };

    State::get(&mut env.framework_state)
        .audio_files
        .insert(guest_audio_file, host_object);

    Ok(guest_audio_file)
}

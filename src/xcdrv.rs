//! Minimal wrapper over `/dev/vixs/xcodedrv` — just enough to pull MPEG-TS out.
//!
//! Every ioctl number and request size below was recovered twice, from
//! independent directions, and the two agreed:
//!
//! * kernel side — `xcdrv.ko` is an unstripped relocatable ELF whose dispatch
//!   is a `.data` array of named function pointers (`cmd_check_dispatch`), so
//!   `slot = (reloc_offset - symbol_value) / 4` gives the number outright, and
//!   each handler stores its `copy_from_user` length as an immediate at
//!   `+0x8b8`.  See `tools/analysis/xcdrv_ioctl_table.py` and
//!   `tools/analysis/xcdrv_req_sizes.py`.
//! * user side — `libvixs.so` issues the ioctls with literal immediates and
//!   builds the request on its own stack, which pins the field order.  See
//!   `evidence/device/analysis/TS_EGRESS.md`.
//!
//! Field *names* come from the vendor's own debug format strings, not from
//! guesswork.  Anything still unverified is called out in a comment.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use crate::sys::{self, c_int};

pub const DEV_PATH: &str = "/dev/vixs/xcodedrv";

// --- ioctl numbers -------------------------------------------------------
// The driver masks the command with `uxth`, so only the low 16 bits select a
// handler; the upper halfword is a sub-command id the handler can read back.

pub const SBUF_CREATE_READ: c_int = 0x37; // xc_sbuf_create_read_func    (8 B)
pub const SBUF_DESTROY_READ: c_int = 0x38; // xc_sbuf_destroy_read_func  (4 B)
pub const SBUF_GET_READ: c_int = 0x3B; // xc_sbuf_get_read_func         (40 B)
/// `xcode_buffer_cancel_get_event @libvixs 0x86c34` builds `{handle, flag}`
/// and issues `movs r1, #0x3f`. `abs:remuxif_task @0x2b84b8` calls it with the
/// *reader* handle and flag 1 before tearing a host connection down.
pub const SBUF_CANCEL_GET: c_int = 0x3F;
pub const SBUF_COMMIT_READ: c_int = 0x3C; // xc_sbuf_commit_read_func   (16 B)
pub const SBUF_INFO: c_int = 0x36; // xc_sbuf_info_func            (sub-table)
pub const MEM_MAP: c_int = 0x7A; // xc_mem_map_func                   (12 B)
pub const MEM_UNMAP: c_int = 0x7B; // xc_mem_unmap_func                 (8 B)
pub const GET_GRAPH: c_int = 0x86; // xc_get_graph_func                (32 B)
/// `xc_temperature_func @xcdrv 0x241b4` declares 16 bytes; `libvixs`'s
/// `xcode_get_temperature @0x7b09c` issues it with `movs r1, #0x64`.
pub const GET_TEMPERATURE: c_int = 0x64;
const MMR_BASE: c_int = 0x19; // returns the open-time userspace MMR map (4 B)
const TSI_CONFIG: c_int = 0x50; // xc_tsi_config_func                  (40 B)
const TSI_STATUS: c_int = 0x51; // xc_tsi_status_func                  (36 B)

const SDEV_CREATE: c_int = 0x28;
const SDEV_DESTROY: c_int = 0x29;
const SBUF_CREATE: c_int = 0x32;
const SBUF_DESTROY: c_int = 0x34;
const SBUF_SET_READY: c_int = 0x0001_0035;
const SBUF_FORCE_RESET: c_int = 0x0003_0035;
const PIPELINE_CREATE: c_int = 0x46;
const PIPELINE_DESTROY: c_int = 0x47;
const PIPELINE_STOP: c_int = 0x8f;
const SDEV_CONNECT_INPUT: c_int = 0x0007_002a;
const SDEV_CONNECT_OUTPUT: c_int = 0x0008_002a;
const SDEV_DISCONNECT: c_int = 0x0009_002a;
// Device class 11 (REMUX) control block, `class * 100 + index`. Every code
// below is a measured `movw r1, #imm` inside the matching `libvixs.so`
// wrapper, not a table-position inference:
//   0x44c xcode_mini_device_remux_config          @libvixs 0x7ff0c
//   0x44d xcode_mini_device_remux_add_program     @0x7fe18
//   0x44e xcode_mini_device_remux_remove_program  @0x7fd94  (`#0x44e` @0x7fdd4)
//   0x44f xcode_mini_device_remux_add_video       @0x7fca0  (`#0x44f` @0x7fd2c)
//   0x450 xcode_mini_device_remux_add_audio       @0x7fbac  (`#0x450` @0x7fc38)
//   0x454 xcode_mini_device_remux_remove_stream   @0x7f884  (`#0x454` @0x7f8d0)
const REMUX_CONFIG: c_int = 0x044c_002a;
const REMUX_ADD_PROGRAM: c_int = 0x044d_002a;
const REMUX_REMOVE_PROGRAM: c_int = 0x044e_002a;
const REMUX_ADD_VIDEO: c_int = 0x044f_002a;
const REMUX_ADD_AUDIO: c_int = 0x0450_002a;
const REMUX_REMOVE_STREAM: c_int = 0x0454_002a;
//   0x456 xc_mini_device_remux_config_video        @libxcodeapi 0xe6c8 (`#0x456` @0xe70e)
//   0x459 xc_mini_device_remux_set_copy_permission @libxcodeapi 0xea6c (`movt r1,#0x459`)
const REMUX_CONFIG_VIDEO: c_int = 0x0456_002a;
const REMUX_SET_COPY_PERMISSION: c_int = 0x0459_002a;
/// `xcode_pipeline_get_time @libvixs 0x7cf34` builds `{pipeline, lo, hi}` and
/// issues `movs r1, #0x48`; the last two words are the reply.
const PIPELINE_GET_TIME: c_int = 0x48;
/// `xcode_mini_device_get_status @libvixs 0x85bbc` builds `{device, status}`
/// and issues info sub-command 1 (`movs r1, #1` @0x85bf8) on ioctl 0x2B.
const SDEV_GET_STATUS: c_int = 0x0001_002b;
const SDEV_SET_PIPELINE: c_int = 0x000c_002a;
const SDEV_RUN: c_int = 0x0001_002a;
const SDEV_STOP: c_int = 0x0003_002a;
const SDEV_CONFIG_TSI: c_int = 0x01f4_002a;
const DEMUX_ADD_PID_FILTER: c_int = 0x01f5_002a;
const DEMUX_REMOVE_PID_FILTER: c_int = 0x01f6_002a;
const DEMUX_PID_FILTER_INFO: c_int = 0x01f5_002b;
const AV_SYNC_SET_SYNC_MODE: c_int = 0x0259_002a;
const TS_REC_CONFIG: c_int = 0x0640_002a;

/// Sub-command of `SBUF_INFO`, encoded as `(sub << 16) | 0x36`.
pub const SBUF_INFO_STATUS: c_int = 0x01;

const fn sub(cmd: c_int, sub: c_int) -> c_int {
    (sub << 16) | cmd
}

fn log_raw_words(label: &str, words: &[u32]) {
    eprintln!(
        "xcdrv: t_ms={} {label} raw words={words:08x?}",
        sys::unix_time_ms()
    );
}

/// Request emitted by the product's only `xc_mini_device_config_tsi` call.
///
/// `abs` calls the public `xc_mini_device_config_tsi` with `(m_port=0,
/// asi_port=0)`.  `libxcodeapi.so` appends a fifth argument of `-1`; the
/// `libvixs.so` wrapper only sets bit 31 when that fifth argument is *not* -1.
/// Therefore the product request is the direct logical-route form below, not
/// the ASI/crossbar-selector form with bit 31.  Its logical m_port value must
/// not be equated with the separately configured physical TSI port.
const fn product_tsi_request(device: u32) -> [u32; 4] {
    [device, 0, 0, 0]
}

/// TSI attachment used by `ts_input_t::detect_stream` for a TS_RECODER.
/// Its fifth `xc_mini_device_config_tsi` argument is the literal `-1`, so the
/// wrapper does not set bit 31 and the ARC device consumes its direct logical
/// TSI route.  This is the same request form used by the product DEMUX caller;
/// a bit-31/TSD request requires an explicit nonnegative fifth argument, which
/// no product caller supplies here.
const fn ts_recorder_tsi_request(device: u32, port: u32) -> [u32; 4] {
    [device, 0, port, 0]
}

/// Physical TSI setup issued by `abs` before it creates its MPEG-TS demux.
///
/// `abs:0x2874e8` builds `tsd_configure_t {enable=1, parallel=0, packet=0,
/// use_sync=0, negative_edge=1, active_high=1, keep_null=1}` and calls
/// `setup_tsi_port`. The caller's spro/system discriminator maps system 0 to
/// physical TSI0 and system 2 (BS/CS 2K) to physical TSI1 before this helper.
/// The negative-edge input word comes from the product's CPLD-version test.
/// Following the seven input fields through
/// `libxcodeapi.so:0x1003c`, `libvixs.so:0x8e00c`, and finally
/// `xcode_tsi_config@0x7bba8` produces these ten fields:
/// `{port,packet,parallel,enable,use_sync,disable_auto_reset,active_high,
/// bpc_sel,use_neg_edge,drop_null}`.
const fn product_mpeg_ts_tsi_request(port: u32) -> [u32; 10] {
    product_physical_tsi_request(port, ME_PACKET_188, false)
}

/// `me_packet` enum, word 1 of the 40-byte physical TSI request.
/// `tsconfig:0x8d36-0x8d54` maps the `-z` argument to it and
/// `libvixs.so:0x8e012` + the `.rodata:0x127460` table `{1,2,3}` carry it into
/// the driver: `0 = 188`, `1 = 192`, `2 = 204`, `3 = 130`.
pub const ME_PACKET_188: u32 = 0;
pub const ME_PACKET_192: u32 = 1;
pub const ME_PACKET_204: u32 = 2;
pub const ME_PACKET_130: u32 = 3;

/// The product request with only `me_packet` substituted.
const fn product_tsi_request_with_packet(port: u32, me_packet: u32) -> [u32; 10] {
    product_physical_tsi_request(port, me_packet, false)
}

/// The product physical TSI request with the two fields Gate 5D/5E vary.
///
/// Word 4 is `m_use_sync_bit` — "use the external TS sync signal". The product
/// sets it to 0 for every system (`abs:Dxd_SetTsConfig`), but the product only
/// ever runs ISDB-S3/TLV on this SKU, where packet framing happens downstream in
/// the TLV parser. Two production MN884433 designs (LG 50PJ350-SA to Saturn 7,
/// LG 32LE5510 to BCM3556) route the demodulator's packet-sync pin into the
/// SoC's TS sync input for conventional MPEG-TS, so the value is unvalidated for
/// MPEG-TS on this board.
///
/// Everything else — serial, source enable, VALID active-high, BPC, falling
/// edge, keep null — stays exactly as `abs:Dxd_SetTsConfig` sets it.
const fn product_physical_tsi_request(port: u32, me_packet: u32, use_sync: bool) -> [u32; 10] {
    [port, me_packet, 0, 1, use_sync as u32, 0, 1, 0, 1, 0]
}

/// Physical TSI setup implied by the SDK `ts_input_t` that owns TS_RECODER.
///
/// Both `ts_input_t::open` (`libvixs.so:0xd1a6c`) and `set_active`
/// (`0xd1ab8`) call `xcode_io_t::setup_tsi_port(hardware_port, &cfg)` using
/// the seven-word configuration stored at object offset `0x36c`.  The
/// constructor explicitly writes `{1,?,0,0,1,0,1}`: the second word
/// (`data_mode`) is not initialized by that constructor.  No shipped product
/// binary instantiates this class, so its runtime value cannot be recovered
/// from a product call site.  We choose zero (serial), which is independently
/// the vendor `tsconfig` default.  Following `{1,0,0,0,1,0,1}` through
/// `xcode_io_t::setup_tsi_port@0x8e00c` gives this request.
///
/// Word 6 (`use_sync_signal`) is not ambiguous: the constructor explicitly
/// sets the corresponding configuration word to zero.  The product DEMUX
/// request above sets it to one and must not be described as the same
/// lifecycle.
const fn ts_recorder_physical_tsi_request(port: u32) -> [u32; 10] {
    [port, 0, 0, 1, 0, 0, 0, 0, 1, 0]
}

/// Product DEMUX shape from `abs:Dxd_CreateRoute`.
const fn mpeg_ts_demux_create_request() -> [u32; 6] {
    [0x0c, 0, 32, 0, 0, 0]
}

/// AV_SYNC clock owner created by `abs:Dxd_InitSproInfo` for every route.
///
/// `xc_sdev_create_func` copies 24 bytes, and the product call at
/// `abs:0x28646c` supplies `{type=7, input=0, output=0, control=16}`.
const fn av_sync_create_request() -> [u32; 6] {
    [7, 0, 0, 16, 0, 0]
}

/// Live synchronization mode used immediately before the product DEMUX is
/// created. `libvixs.so:xc_mini_device_av_sync_set_sync_mode@0x83c20`
/// constructs this exact 32-byte request; `Dxd_CreateRoute` passes mode 1.
const fn av_sync_live_mode_request(av_sync: u32) -> [u32; 8] {
    [av_sync, 1, 1, 0, 0, 0, 0, 0]
}

/// Host-readable full-transport-stream ring for the normal MPEG-TS DEMUX.
///
/// SYSTEM/TS is accepted by the ARC `get_pid_filter_output_type` switch and
/// maps to output type 0.  For RING buffers `m_size` is a count of units, not
/// bytes: the measured 0x6000 units * 188 bytes produces a 0x468000-byte ring.
/// Passing 0x468000 here asks for 868,614,144 bytes and returns XCE_NO_MORE.
const fn demux_ts_buffer_create_request() -> [u32; 9] {
    [3, 1, 2, 1, 0x1000, 0x6000, 188, 3, 0]
}

/// Product PAT/PMT section ring from `Dxd_AllocateSecFilter`.
///
/// `abs:0x288f8c..0x288fc6` creates RING/DATA/PSI_SECTION with one reader,
/// 0x1000 alignment, unit size 1 and flags 3.  The size is selected from a
/// product table; 0x40000 is independently used by the shipped section path
/// and is a bounded diagnostic allocation.
const fn demux_pat_section_buffer_create_request() -> [u32; 9] {
    [3, 2, 14, 1, 0x1000, 0x40000, 1, 3, 0]
}

/// Whole-multiplex PID filter recovered from ARC700 firmware.
///
/// `tsd_add_pid_param_check` accepts PID -1 only for filter type 0/1, and
/// `get_pid_filter_output_type` accepts PID -1 only when the output media is
/// SYSTEM.  With SYSTEM/TS it returns output type 0.  Filter type 1 is the
/// product-observed normal PID-filter value.
///
/// The kernel handler first copies 24 bytes, then unconditionally probes a
/// 36-byte form for filter types other than 3.  `libvixs.so` supplies the
/// output media attribute as a 12-byte extension: `{size, type, subtype}`.
/// This is not optional for SYSTEM/TS.  The ARC `get_pid_filter_output_type`
/// reads type/subtype at request offsets 0x1c/0x20; zeroing the extension
/// makes it reject the request as XCE_INVAL_PARAM (-1023).
///
/// `m_to_replace = -2` is also mandatory for an add.  The ARC ioctl
/// dispatcher compares request offset 0x10 with -2: only that sentinel calls
/// `tsd_add_pid_filter`; every other accepted value, including -1, calls
/// `tsd_replace_pid_filter`.  The product's normal add path also supplies -2.
/// Its seventh API argument is 1000 and libxcodeapi/libvixs pass that value
/// unchanged into request word 5; do not normalize it to boolean 1.
const fn demux_pid_request(device: u32, pid: u32, output: u32) -> [u32; 9] {
    [
        device,
        pid,
        1,        // normal PID filter
        output,
        u32::MAX - 1, // m_to_replace = -2: select tsd_add_pid_filter
        1000,     // product add-PID wait value, passed through unchanged
        12, // sizeof(s_media_attr) supplied by the libvixs base wrapper
        1,  // SYSTEM
        2,  // TS
    ]
}

/// Read-only DEMUX PID-filter query used by the public
/// `xcode_mini_device_pid_filter_info` wrapper.
///
/// libvixs builds `{m_device, m_pid, 0, 0}` and returns filter type and the
/// output connection in the last two words.  Driver class-5 info selector 501
/// marshals the same 16-byte in/out object.
const fn demux_pid_filter_info_request(device: u32, pid: u32) -> [u32; 4] {
    [device, pid, 0, 0]
}

/// Product section-filter request used for PAT/PMT and other PSI.
///
/// `abs:0x2896da..0x289726` passes filter type 2 and the media extension
/// `{12, DATA(2), PSI_SECTION(14)}` to the same normal add dispatcher.
const fn demux_pat_section_request(device: u32, output: u32) -> [u32; 9] {
    [
        device,
        0, // PAT PID
        2, // section filter
        output,
        u32::MAX - 1,
        1000,
        12,
        2,  // DATA
        14, // PSI_SECTION
    ]
}

const fn demux_remove_pid_request(device: u32, pid: u32) -> [u32; 3] {
    [device, 1, pid]
}

// --- REMUX (me_type 0x0E) --------------------------------------------------
//
// The product builds exactly this device for every recording:
// `adsb:remux_xcif_open_remux @0x421f8` creates it, configures it, creates
// TSOUT/TSINFO and connects both outputs; `remux_xcif_connect_avesbuff
// @0x42528` connects the *live* video/audio ES rings as inputs; and
// `remux_xcif_add_stream @0x42b5c` declares the program and streams. The
// output is the plaintext 192-byte TTS that was captured in
// a captured REMUX TSOUT ring.

/// `adsb:0x42248..0x42260`: `xc_mini_device_create(xcode, 0x0E, 13, 2, 1,
/// &handle, "TSREMUX")`. The driver copies 24 bytes and `libvixs` initialises
/// only 20, so the sixth word is zeroed here.
const fn remux_create_request() -> [u32; 6] {
    [0x0e, 13, 2, 1, 0, 0]
}

/// `adsb:0x42294`: `xc_mini_device_remux_config_ext(xcode, remux, 3, 0x10, 0,
/// 1, 100, 0)`. `libxcodeapi:0xe660` memsets 0x1c bytes and stores the seven
/// arguments in order, so the request is exactly this.
///
/// `me_format = 3` is a measured value whose *name* is unknown; it is the
/// setting that produced the captured 192-byte TTS, so it is copied verbatim
/// rather than guessed at.
/// `m_stream_id` becomes the transport_stream_id of the PAT the REMUX
/// generates. The product passes 1 because a recording is played back
/// standalone; we pass the real TLV stream id so the emitted TS identifies
/// itself the way a Mirakurun-style client expects.
const fn remux_config_request(device: u32, stream_id: u32) -> [u32; 7] {
    [device, 3, 0x10, 0, stream_id, 100, 0]
}

/// `adsb:0x422e2`: `xc_buffer_create(xcode, 4 LINEAR_RING, 1 SYSTEM, 0, 1,
/// 0x1000, 0x468000, 1, 1, 1, &tsout, "TSOUT")`, i.e. `m_bf_flags = 3`.
///
/// Unlike the RING buffers used by the DEMUX path, a LINEAR_RING takes its
/// size in bytes; 0x468000 is a compile-time immediate at `adsb:0x422ae`.
/// Flag bit 1 is what puts the allocation inside the host-visible XCode
/// aperture: the product's own flags=1 ES rings land below 0x1F000000 and
/// cannot be mapped, while every flags=3 buffer in the live graph can.
const fn remux_tsout_buffer_request() -> [u32; 9] {
    [4, 1, 0, 1, 0x1000, 0x468000, 1, 3, 0]
}

/// `adsb:0x4232c`: the metadata side channel created alongside TSOUT. The
/// REMUX is created with `m_max_output = 2` and the product connects both
/// outputs before running, so this is created and connected even though
/// nothing here reads it.
const fn remux_tsinfo_buffer_request() -> [u32; 9] {
    [4, 2, 0x10, 1, 0x1000, 0x20000, 1, 3, 0]
}

/// `libxcodeapi:0xe768` memsets 0x2c bytes, stores device, `*program_number`,
/// `*pcr_pid`, `*pmt_pid`, `*pcr_interval` and the `flag` argument in order,
/// then `strd`s the 64-bit ATS base into the two words that follow.
const fn remux_add_program_request(
    device: u32,
    program: u32,
    pcr_pid: u32,
    pmt_pid: u32,
    pcr_interval: u32,
    ats_base: u64,
) -> [u32; 11] {
    [
        device,
        program,
        pcr_pid,
        pmt_pid,
        pcr_interval,
        1, // flag: `adsb:0x42bbc` passes 1
        ats_base as u32,
        (ats_base >> 32) as u32,
        0,
        0,
        0,
    ]
}

/// `adsb:0x42be6..0x42c0e`: `config_video(xcode, remux, video_conn, 0,
/// 0x02A4C200, 0, 0, 0, 0, 0x029F6300)`. `libxcodeapi:0xe6c8` memsets 0x24
/// bytes and stores the nine values in argument order. The two literals are
/// 44,352,000 and 44,064,000 — the product's max and target rates for the
/// remultiplexed stream; their field names are not in either library.
const fn remux_config_video_request(device: u32, connection: u32) -> [u32; 9] {
    [device, connection, 0, 0x02A4_C200, 0, 0, 0, 0, 0x029F_6300]
}

/// `adsb:0x43e1e` passes the session's copy-control byte, which is zero for
/// every path we replicate.
const fn remux_copy_permission_request(device: u32, ccc: u32) -> [u32; 2] {
    [device, ccc]
}

/// `libvixs:0x7fca0` builds `{m_device, m_connect, m_program_number, m_pid,
/// m_stream_id, m_ext_stream_type, m_pts_interval}` at `sp+4` and issues
/// control code 0x44f. `add_audio` (0x450) has the identical shape.
const fn remux_add_stream_request(
    device: u32,
    connection: u32,
    program: u32,
    pid: u32,
    stream_id: u32,
    ext_stream_type: u32,
) -> [u32; 7] {
    [device, connection, program, pid, stream_id, ext_stream_type, 0]
}

/// `adsb` always pairs `remux_remove_stream(conn, flag)` with a following
/// `disconnect`, never the reverse. Flag 1 is used for video and audio
/// connections (`adsb:0x42e2a`, `0x42e56`); -1 is used for PSI and PES.
const fn remux_remove_stream_request(device: u32, connection: u32, flag: u32) -> [u32; 3] {
    [device, connection, flag]
}

const fn remux_remove_program_request(device: u32, program: u32) -> [u32; 2] {
    [device, program]
}

/// `abs:Dxd_CreateRoute` passes timeout zero to the public run wrapper. The
/// wrapper keeps `m_wait=1` for every timeout except `-1`.
const fn product_demux_run_request(device: u32) -> [u32; 3] {
    [device, 1, 0]
}

const fn force_reset_buffer_request(buffer: u32) -> [u32; 2] {
    [buffer, 1]
}

const fn product_demux_disconnect_request(device: u32, connection: u32) -> [u32; 4] {
    [device, connection, 1, 3000]
}

// --- request structs -----------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct ReqCreateRead {
    handle: u32,     // in:  buffer handle
    out_reader: u32, // out: reader (port) handle used by get/commit
}

/// 40 bytes.  The vendor zeroes the seven output words before the call.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ReqGetRead {
    handle: u32,
    wait: u32,
    timeout: u32,
    mp_addr: u32,  // out: *physical* address of the first piece
    m_size: u32,   // out: total bytes across both pieces
    mp_addr1: u32, // out: second piece, non-zero only on ring wrap
    m_size1: u32,  // out
    mp_desc: u32,  // out
    _pad: [u32; 2],
}

#[repr(C)]
struct ReqCommitRead {
    handle: u32,
    addr: u32,
    size: u32,
    desc: u32,
}

#[repr(C)]
#[derive(Default)]
struct ReqMemMap {
    size: u32,
    phys: u32,
    vaddr: u32, // out
}

/// 8 bytes. Field order is pinned by `libxcodeapi.so:xc_unmap` @0xfa20, which
/// stores its *third* argument at +0x00 and its second at +0x04 — the same
/// shape as `xc_map`'s `{size, phys, vaddr}`. The vendor's success message
/// (`successfully unmaped addr=0x%p`) prints the second argument, which pins
/// it as the address rather than the length.
///
/// Getting this backwards is not a harmless failed ioctl: the handler ends in
/// a munmap, so `{vaddr, size}` asks the kernel to unmap `vaddr` bytes
/// starting at `size`, which tears down the calling process's whole address
/// space. That is what it did before this was checked.
#[repr(C)]
struct ReqMemUnmap {
    size: u32,
    vaddr: u32,
}

/// One read window handed back by `get_read`.
///
/// Two `(addr, size)` pairs exist because the underlying ring can hand back a
/// wrapped region as two contiguous halves.
#[derive(Clone, Copy, Debug, Default)]
pub struct Chunk {
    pub addr: u32,
    pub size: u32,
    pub addr1: u32,
    pub size1: u32,
    pub desc: u32,
}

impl Chunk {
    pub fn total(&self) -> usize {
        self.size as usize
    }

    /// Bytes in the first physical piece. `m_size` is the total and
    /// `m_size1` is the wrapped second piece, exactly as consumed by
    /// `ts_input_t::receiving_thread` in the vendor library.
    pub fn first_size(&self) -> Option<u32> {
        self.size.checked_sub(self.size1)
    }
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

// --- driver handle -------------------------------------------------------

pub struct XcDrv {
    file: File,
}

impl XcDrv {
    pub fn open_path(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(XcDrv { file })
    }

    fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }

    unsafe fn call<T>(&self, request: c_int, arg: &mut T) -> io::Result<c_int> {
        sys::ioctl_ptr(self.fd(), request, arg as *mut T as *mut c_void)
    }

    /// Read one XCode6 MMR word through the mapping installed by
    /// `xcdrv.ko:xcode_open -> map_mmr_addr`.
    ///
    /// Ioctl 0x19 only returns the process-local address already stored in
    /// the driver's client object. The hardware access here is an aligned
    /// volatile load inside the fixed 0x20000-byte MMR window.
    pub fn mmr_read32(&self, offset: u32) -> io::Result<u32> {
        const MMR_SIZE: u32 = 0x2_0000;
        if offset & 3 != 0 || offset > MMR_SIZE - 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MMR offset must be aligned and below {MMR_SIZE:#x}: {offset:#x}"),
            ));
        }
        let mut base = 0u32;
        unsafe { self.call(MMR_BASE, &mut base)? };
        if base == 0 || base & 3 != 0 {
            return Err(io::Error::other(format!(
                "ioctl 0x19 returned invalid MMR mapping {base:#x}"
            )));
        }
        let addr = base
            .checked_add(offset)
            .ok_or_else(|| io::Error::other("MMR userspace address overflow"))?;
        Ok(unsafe { std::ptr::read_volatile(addr as *const u32) })
    }

    /// Create the vendor's TSI-fed TS_RECODER shape exactly.
    pub fn create_ts_recorder(&self) -> io::Result<u32> {
        let mut req = [0x13u32, 1, 1, 0, 0, 0];
        unsafe { self.call(SDEV_CREATE, &mut req)? };
        if req[4] == 0 {
            return Err(io::Error::other("TS_RECODER create returned handle 0"));
        }
        Ok(req[4])
    }

    pub fn create_pipeline(&self) -> io::Result<u32> {
        let mut pipeline = 0u32;
        unsafe { self.call(PIPELINE_CREATE, &mut pipeline)? };
        if pipeline == 0 {
            return Err(io::Error::other("pipeline create returned handle 0"));
        }
        Ok(pipeline)
    }

    pub fn destroy_pipeline(&self, pipeline: u32) -> io::Result<()> {
        unsafe { sys::ioctl_value(self.fd(), PIPELINE_DESTROY, pipeline)? };
        Ok(())
    }

    pub fn stop_pipeline(&self, pipeline: u32) -> io::Result<()> {
        unsafe { sys::ioctl_value(self.fd(), PIPELINE_STOP, pipeline)? };
        Ok(())
    }

    pub fn create_av_sync(&self) -> io::Result<u32> {
        let mut req = av_sync_create_request();
        unsafe { self.call(SDEV_CREATE, &mut req)? };
        if req[4] == 0 {
            return Err(io::Error::other("AV_SYNC create returned handle 0"));
        }
        Ok(req[4])
    }

    pub fn set_av_sync_live_mode(&self, av_sync: u32) -> io::Result<()> {
        let mut req = av_sync_live_mode_request(av_sync);
        unsafe { self.call(AV_SYNC_SET_SYNC_MODE, &mut req)? };
        Ok(())
    }

    pub fn create_mpeg_ts_demux(&self) -> io::Result<u32> {
        let mut req = mpeg_ts_demux_create_request();
        unsafe { self.call(SDEV_CREATE, &mut req)? };
        if req[4] == 0 {
            return Err(io::Error::other("DEMUX create returned handle 0"));
        }
        Ok(req[4])
    }

    pub fn set_pipeline(&self, device: u32, pipeline: u32) -> io::Result<()> {
        let mut req = [device, pipeline];
        unsafe { self.call(SDEV_SET_PIPELINE, &mut req)? };
        Ok(())
    }

    /// RING / SYSTEM / TS, matching `ts_input_t::detect_stream` exactly.
    pub fn create_ts_recorder_buffer(&self) -> io::Result<u32> {
        let mut req = [3u32, 1, 2, 1, 0x1000, 0x4000, 188, 3, 0];
        unsafe { self.call(SBUF_CREATE, &mut req)? };
        if req[8] == 0 {
            return Err(io::Error::other("TS ring create returned handle 0"));
        }
        Ok(req[8])
    }

    pub fn create_demux_ts_buffer(&self) -> io::Result<u32> {
        let mut req = demux_ts_buffer_create_request();
        log_raw_words("full-TS ring create request", &req);
        unsafe { self.call(SBUF_CREATE, &mut req)? };
        log_raw_words("full-TS ring create reply", &req);
        if req[8] == 0 {
            return Err(io::Error::other("DEMUX TS ring create returned handle 0"));
        }
        Ok(req[8])
    }

    pub fn create_demux_pat_section_buffer(&self) -> io::Result<u32> {
        let mut req = demux_pat_section_buffer_create_request();
        log_raw_words("PAT ring create request", &req);
        unsafe { self.call(SBUF_CREATE, &mut req)? };
        log_raw_words("PAT ring create reply", &req);
        if req[8] == 0 {
            return Err(io::Error::other("DEMUX PAT section ring create returned handle 0"));
        }
        Ok(req[8])
    }

    pub fn connect_output(&self, device: u32, buffer: u32) -> io::Result<u32> {
        let mut req = [device, 0, buffer, 0];
        log_raw_words("connect_output request", &req);
        unsafe { self.call(SDEV_CONNECT_OUTPUT, &mut req)? };
        log_raw_words("connect_output reply", &req);
        if req[3] == 0 {
            return Err(io::Error::other("connect_output returned handle 0"));
        }
        Ok(req[3])
    }

    /// Attach an existing buffer to a device as an *input*.
    ///
    /// Sub-command 7 of `0x2A`, request `{m_device, m_bind, m_buffer,
    /// m_connect(OUT)}` (`libvixs:0x8be6c`). Every one of the 33 product call
    /// sites passes `m_bind = 0`; its meaning is unrecovered, so nothing else
    /// is ever sent.
    ///
    /// This consumes one of the buffer's creation-time `m_max_reader` slots —
    /// the same counter host readers use (`xcdrv:port_connect_buffer @0xe75a0`
    /// compares `buf+0xB0` against `buf+0xAC`) — and returns
    /// `XCE_TOO_MANY (-1035)` when the quota is full. Unlike a host reader,
    /// it can be released again: `disconnect` is a supported product path,
    /// while `destroy_reader` is broken on this box.
    pub fn connect_input(&self, device: u32, buffer: u32) -> io::Result<u32> {
        let mut req = [device, 0, buffer, 0];
        log_raw_words("connect_input request", &req);
        unsafe { self.call(SDEV_CONNECT_INPUT, &mut req)? };
        log_raw_words("connect_input reply", &req);
        if req[3] == 0 {
            return Err(io::Error::other("connect_input returned handle 0"));
        }
        Ok(req[3])
    }

    pub fn create_remux(&self) -> io::Result<u32> {
        let mut req = remux_create_request();
        log_raw_words("REMUX create request", &req);
        unsafe { self.call(SDEV_CREATE, &mut req)? };
        log_raw_words("REMUX create reply", &req);
        if req[4] == 0 {
            return Err(io::Error::other("REMUX create returned handle 0"));
        }
        Ok(req[4])
    }

    pub fn config_remux(&self, device: u32, stream_id: u32) -> io::Result<()> {
        let mut req = remux_config_request(device, stream_id);
        log_raw_words("REMUX config request", &req);
        unsafe { self.call(REMUX_CONFIG, &mut req)? };
        Ok(())
    }

    pub fn create_remux_tsout_buffer(&self) -> io::Result<u32> {
        let mut req = remux_tsout_buffer_request();
        log_raw_words("TSOUT create request", &req);
        unsafe { self.call(SBUF_CREATE, &mut req)? };
        log_raw_words("TSOUT create reply", &req);
        if req[8] == 0 {
            return Err(io::Error::other("TSOUT create returned handle 0"));
        }
        Ok(req[8])
    }

    pub fn create_remux_tsinfo_buffer(&self) -> io::Result<u32> {
        let mut req = remux_tsinfo_buffer_request();
        log_raw_words("TSINFO create request", &req);
        unsafe { self.call(SBUF_CREATE, &mut req)? };
        log_raw_words("TSINFO create reply", &req);
        if req[8] == 0 {
            return Err(io::Error::other("TSINFO create returned handle 0"));
        }
        Ok(req[8])
    }

    /// Current pipeline clock, in the units the REMUX stamps TTS arrival
    /// times with. The product reads it immediately before running the remux
    /// and uses the same reference for `add_program`'s ATS base.
    pub fn pipeline_get_time(&self, pipeline: u32) -> io::Result<u64> {
        let mut req = [pipeline, 0, 0];
        unsafe { self.call(PIPELINE_GET_TIME, &mut req)? };
        log_raw_words("pipeline get_time reply", &req);
        Ok(u64::from(req[1]) | (u64::from(req[2]) << 32))
    }

    /// Device state word, as the product's `remuxif_task` polls it.
    pub fn device_status(&self, device: u32) -> io::Result<u32> {
        let mut req = [device, 0];
        unsafe { self.call(SDEV_GET_STATUS, &mut req)? };
        Ok(req[1])
    }

    pub fn remux_set_copy_permission(&self, device: u32, ccc: u32) -> io::Result<()> {
        let mut req = remux_copy_permission_request(device, ccc);
        log_raw_words("REMUX set_copy_permission request", &req);
        unsafe { self.call(REMUX_SET_COPY_PERMISSION, &mut req)? };
        Ok(())
    }

    pub fn remux_config_video(&self, device: u32, connection: u32) -> io::Result<()> {
        let mut req = remux_config_video_request(device, connection);
        log_raw_words("REMUX config_video request", &req);
        unsafe { self.call(REMUX_CONFIG_VIDEO, &mut req)? };
        Ok(())
    }

    pub fn remux_add_program(
        &self,
        device: u32,
        program: u32,
        pcr_pid: u32,
        pmt_pid: u32,
        pcr_interval: u32,
        ats_base: u64,
    ) -> io::Result<()> {
        let mut req =
            remux_add_program_request(device, program, pcr_pid, pmt_pid, pcr_interval, ats_base);
        log_raw_words("REMUX add_program request", &req);
        unsafe { self.call(REMUX_ADD_PROGRAM, &mut req)? };
        Ok(())
    }

    pub fn remux_add_video(
        &self,
        device: u32,
        connection: u32,
        program: u32,
        pid: u32,
        stream_id: u32,
        ext_stream_type: u32,
    ) -> io::Result<()> {
        let mut req =
            remux_add_stream_request(device, connection, program, pid, stream_id, ext_stream_type);
        log_raw_words("REMUX add_video request", &req);
        unsafe { self.call(REMUX_ADD_VIDEO, &mut req)? };
        Ok(())
    }

    pub fn remux_add_audio(
        &self,
        device: u32,
        connection: u32,
        program: u32,
        pid: u32,
        stream_id: u32,
        ext_stream_type: u32,
    ) -> io::Result<()> {
        let mut req =
            remux_add_stream_request(device, connection, program, pid, stream_id, ext_stream_type);
        log_raw_words("REMUX add_audio request", &req);
        unsafe { self.call(REMUX_ADD_AUDIO, &mut req)? };
        Ok(())
    }

    pub fn remux_remove_stream(&self, device: u32, connection: u32, flag: u32) -> io::Result<()> {
        let mut req = remux_remove_stream_request(device, connection, flag);
        log_raw_words("REMUX remove_stream request", &req);
        unsafe { self.call(REMUX_REMOVE_STREAM, &mut req)? };
        Ok(())
    }

    pub fn remux_remove_program(&self, device: u32, program: u32) -> io::Result<()> {
        let mut req = remux_remove_program_request(device, program);
        log_raw_words("REMUX remove_program request", &req);
        unsafe { self.call(REMUX_REMOVE_PROGRAM, &mut req)? };
        Ok(())
    }

    /// Enable and configure the physical port exactly as the product's 2K
    /// MPEG-TS path does, before attaching an ARC device to that input.
    pub fn configure_product_mpeg_ts_tsi(&self, port: u32) -> io::Result<()> {
        self.configure_physical_tsi_packet(port, ME_PACKET_188)
    }

    /// Gate 5D-B1: the same product physical TSI request with a different
    /// `me_packet`. ISDB-S is transmitted as RS(204,188), so a demodulator that
    /// emits the 204-byte codeword would never sync against the 188 setting.
    pub fn configure_physical_tsi_packet(&self, port: u32, me_packet: u32) -> io::Result<()> {
        self.configure_physical_tsi(port, me_packet, false)
    }

    /// Gate 5E2-SYNC: the product request with `m_use_sync_bit` selectable.
    pub fn configure_physical_tsi(
        &self,
        port: u32,
        me_packet: u32,
        use_sync: bool,
    ) -> io::Result<()> {
        let mut req = product_physical_tsi_request(port, me_packet, use_sync);
        log_raw_words("physical TSI config request", &req);
        unsafe { self.call(TSI_CONFIG, &mut req)? };
        Ok(())
    }

    /// Read one physical TSI status without changing its configuration.
    /// `libvixs:xcode_tsi_status_check @0x7be48` returns per-port CRC samples
    /// in words 0..7 and the lock bitmask in word 8.
    pub fn tsi_status(&self, port: u32) -> io::Result<(bool, u8)> {
        if port > 7 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TSI status port must be 0..7",
            ));
        }
        let mut req = [0u32; 9];
        unsafe { self.call(TSI_STATUS, &mut req)? };
        let lock = ((req[8] >> port) & 1) != 0;
        Ok((lock, (req[port as usize] & 0xff) as u8))
    }

    pub fn configure_ts_recorder_physical_tsi(&self, port: u32) -> io::Result<()> {
        let mut req = ts_recorder_physical_tsi_request(port);
        unsafe { self.call(TSI_CONFIG, &mut req)? };
        Ok(())
    }

    pub fn config_tsi_product_route(&self, device: u32) -> io::Result<()> {
        let mut req = product_tsi_request(device);
        log_raw_words("DEMUX direct-route request", &req);
        unsafe { self.call(SDEV_CONFIG_TSI, &mut req)? };
        Ok(())
    }

    pub fn config_ts_recorder_tsi(&self, device: u32, port: u32) -> io::Result<()> {
        let mut req = ts_recorder_tsi_request(device, port);
        unsafe { self.call(SDEV_CONFIG_TSI, &mut req)? };
        Ok(())
    }

    pub fn config_ts_recorder(&self, device: u32) -> io::Result<()> {
        let mut req = [device, 0x0f, 3, 3, 2, 50];
        unsafe { self.call(TS_REC_CONFIG, &mut req)? };
        Ok(())
    }

    pub fn add_demux_pid(&self, device: u32, pid: u32, output: u32) -> io::Result<()> {
        let mut req = demux_pid_request(device, pid, output);
        log_raw_words("DEMUX PID add request", &req);
        unsafe { self.call(DEMUX_ADD_PID_FILTER, &mut req)? };
        Ok(())
    }

    pub fn add_demux_pat_section(&self, device: u32, output: u32) -> io::Result<()> {
        let mut req = demux_pat_section_request(device, output);
        log_raw_words("DEMUX PAT add request", &req);
        unsafe { self.call(DEMUX_ADD_PID_FILTER, &mut req)? };
        Ok(())
    }

    /// Query an installed PID filter without changing the device graph.
    /// Returns `(filter_type, output_connection)`.
    pub fn demux_pid_filter_info(&self, device: u32, pid: u32) -> io::Result<(u32, u32)> {
        let mut req = demux_pid_filter_info_request(device, pid);
        unsafe { self.call(DEMUX_PID_FILTER_INFO, &mut req)? };
        Ok((req[2], req[3]))
    }

    pub fn remove_demux_pid(&self, device: u32, pid: u32) -> io::Result<()> {
        let mut req = demux_remove_pid_request(device, pid);
        unsafe { self.call(DEMUX_REMOVE_PID_FILTER, &mut req)? };
        Ok(())
    }

    /// Run with the SDK helper's historical 2-second synchronous timeout.
    /// The product DEMUX route is different: it passes timeout zero; use
    /// `run_product_demux` for that byte-exact request.
    pub fn run_device(&self, device: u32) -> io::Result<()> {
        let mut req = [device, 1, 2000];
        unsafe { self.call(SDEV_RUN, &mut req)? };
        Ok(())
    }

    /// Run the normal MPEG-TS DEMUX exactly as `Dxd_CreateRoute` does.
    /// `abs:0x28512e` passes timeout 0 to `xc_mini_device_run`; the wrapper
    /// turns that into `[device, wait=1, timeout=0]`.
    pub fn run_product_demux(&self, device: u32) -> io::Result<()> {
        let mut req = product_demux_run_request(device);
        log_raw_words("DEMUX run request", &req);
        unsafe { self.call(SDEV_RUN, &mut req)? };
        Ok(())
    }

    pub fn stop_device(&self, device: u32) -> io::Result<()> {
        let mut req = [device, 1, 2000];
        unsafe { self.call(SDEV_STOP, &mut req)? };
        Ok(())
    }

    pub fn set_buffer_ready(&self, buffer: u32) -> io::Result<()> {
        log_raw_words("buffer set_ready value request", &[buffer]);
        unsafe { sys::ioctl_value(self.fd(), SBUF_SET_READY, buffer)? };
        Ok(())
    }

    /// Flush/reset a buffer before destruction. The product PID-output
    /// teardown calls `xc_buffer_force_reset(fd, buffer, 1)`, whose libvixs
    /// wrapper emits sub-command 3 with the 8-byte request `[buffer, 1]`.
    pub fn force_reset_buffer(&self, buffer: u32) -> io::Result<()> {
        let mut req = force_reset_buffer_request(buffer);
        unsafe { self.call(SBUF_FORCE_RESET, &mut req)? };
        Ok(())
    }

    pub fn disconnect(&self, device: u32, connection: u32) -> io::Result<()> {
        let mut req = [device, connection, 1, 2000];
        unsafe { self.call(SDEV_DISCONNECT, &mut req)? };
        Ok(())
    }

    /// Disconnect a product DEMUX PID output with the 3000 ms timeout used by
    /// `Dxd_DeleteAvPes` at `abs:0x28742e`.
    pub fn disconnect_product_demux(&self, device: u32, connection: u32) -> io::Result<()> {
        let mut req = product_demux_disconnect_request(device, connection);
        unsafe { self.call(SDEV_DISCONNECT, &mut req)? };
        Ok(())
    }

    pub fn destroy_device(&self, device: u32) -> io::Result<()> {
        unsafe { sys::ioctl_value(self.fd(), SDEV_DESTROY, device)? };
        Ok(())
    }

    pub fn destroy_buffer(&self, buffer: u32) -> io::Result<()> {
        unsafe { sys::ioctl_value(self.fd(), SBUF_DESTROY, buffer)? };
        Ok(())
    }

    /// Attach a new read port to an existing buffer.
    ///
    /// This is what `xc_buffer_create_host_connection` does underneath: it
    /// mallocs a 16-byte object holding `{fd, buffer_handle, reader_handle,
    /// is_write}` and fills the third word from this call.  Each reader port
    /// carries its own read position, which is why attaching one to a buffer
    /// another process is already draining is plausible — but that is a
    /// property of the driver we have *not* measured, so callers should treat
    /// a successful attach as permission to read, not as proof of isolation.
    pub fn create_reader(&self, buffer_handle: u32) -> io::Result<u32> {
        let mut req = ReqCreateRead {
            handle: buffer_handle,
            out_reader: 0,
        };
        log_raw_words("reader create request", &[req.handle, req.out_reader]);
        unsafe { self.call(SBUF_CREATE_READ, &mut req)? };
        log_raw_words("reader create reply", &[req.handle, req.out_reader]);
        Ok(req.out_reader)
    }

    /// Release a reader that may be parked inside `get_read`.
    ///
    /// Without this, `destroy_read` returns -1034 and the buffer then refuses
    /// to be destroyed with `XCE_IN_USE(-1040)`, leaking it until the next
    /// power cycle. `abs` does exactly this before closing a host connection.
    pub fn cancel_get_read(&self, reader: u32) -> io::Result<()> {
        let mut req = [reader, 1];
        unsafe { self.call(SBUF_CANCEL_GET, &mut req)? };
        Ok(())
    }

    pub fn destroy_reader(&self, reader: u32) -> io::Result<()> {
        // Unlike create/get/commit, 0x38 is value-passed.  This was confirmed
        // on the device: ioctl(fd, 0x38, reader) succeeds, while &reader is
        // rejected with XCE_IO and leaks the reader until explicit cleanup.
        unsafe { sys::ioctl_value(self.fd(), SBUF_DESTROY_READ, reader)? };
        Ok(())
    }

    /// Claim the next readable window.  `timeout_ms == u32::MAX` (-1) makes the
    /// vendor path drop `wait` to 0, so we mirror that.
    pub fn get_read(&self, reader: u32, wait: bool, timeout_ms: u32) -> io::Result<Chunk> {
        let mut req = ReqGetRead {
            handle: reader,
            wait: if timeout_ms == u32::MAX || !wait { 0 } else { 1 },
            timeout: timeout_ms,
            ..Default::default()
        };
        log_raw_words(
            "host_read request (reader,wait,timeout)",
            &[req.handle, req.wait, req.timeout],
        );
        unsafe { self.call(SBUF_GET_READ, &mut req)? };
        log_raw_words(
            "host_read reply (addr,size,addr1,size1,desc)",
            &[req.mp_addr, req.m_size, req.mp_addr1, req.m_size1, req.mp_desc],
        );
        Ok(Chunk {
            addr: req.mp_addr,
            size: req.m_size,
            addr1: req.mp_addr1,
            size1: req.m_size1,
            desc: req.mp_desc,
        })
    }

    pub fn commit_read(&self, reader: u32, chunk: &Chunk) -> io::Result<()> {
        let mut req = ReqCommitRead {
            handle: reader,
            addr: chunk.addr,
            size: chunk.size,
            desc: chunk.desc,
        };
        unsafe { self.call(SBUF_COMMIT_READ, &mut req)? };
        Ok(())
    }

    /// Map a physical address into this process. `get_read` hands back physical
    /// addresses, so nothing can be read without this.
    pub fn map(&self, phys: u32, size: u32) -> io::Result<u32> {
        let mut req = ReqMemMap {
            size,
            phys,
            vaddr: 0,
        };
        unsafe { self.call(MEM_MAP, &mut req)? };
        if req.vaddr == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("xc_map({phys:#x}, {size}) returned a null mapping"),
            ));
        }
        Ok(req.vaddr)
    }

    pub fn unmap(&self, vaddr: u32, size: u32) -> io::Result<()> {
        let mut req = ReqMemUnmap { size, vaddr };
        unsafe { self.call(MEM_UNMAP, &mut req)? };
        Ok(())
    }

    /// `xc_sbuf_info_get_status_func`, 48 bytes. Words 8 and 9 are the vendor-
    /// named `m_writer` and `m_num_readers`; all words remain raw so diagnostic
    /// captures preserve the complete reply.
    pub fn sbuf_status_raw(&self, handle: u32) -> io::Result<[u32; 12]> {
        let mut req = [0u32; 12];
        req[0] = handle;
        unsafe { self.call(sub(SBUF_INFO, SBUF_INFO_STATUS), &mut req)? };
        log_raw_words("ring status reply", &req);
        Ok(req)
    }

    /// `xc_get_graph_func`, 32 bytes.  The driver keeps a live graph of
    /// devices, ports and buffers (`add_buffer_into_graph` and friends), so
    /// this is the most likely enumeration route.  Semantics unmeasured:
    /// returned raw.
    /// Chip temperature. The reply's field layout is not documented by the
    /// vendor libraries, so all four words are returned raw; the product's own
    /// serial log prints a matching `[UIM][XC_TEMP] [45/2]`.
    pub fn temperature_raw(&self) -> io::Result<[u32; 4]> {
        let mut req = [0u32; 4];
        unsafe { self.call(GET_TEMPERATURE, &mut req)? };
        Ok(req)
    }

    pub fn get_graph_raw(&self, seed: &[u32; 8]) -> io::Result<[u32; 8]> {
        let mut req = *seed;
        unsafe { self.call(GET_GRAPH, &mut req)? };
        Ok(req)
    }
}

// --- mapping cache -------------------------------------------------------

/// Physical→virtual mappings, kept so the steady-state read loop does not pay
/// two extra ioctls per chunk.
///
/// A ring buffer revisits the same physical pages forever, so a handful of
/// entries covers it. Entries are evicted least-recently-used and unmapped.
pub struct MapCache {
    drv: Arc<XcDrv>,
    entries: Vec<(u32, u32, u32)>, // (phys, size, vaddr), most recent last
    cap: usize,
    pub hits: u64,
    pub misses: u64,
}

impl MapCache {
    pub fn new(drv: Arc<XcDrv>, cap: usize) -> Self {
        MapCache {
            drv,
            entries: Vec::with_capacity(cap),
            cap: cap.max(1),
            hits: 0,
            misses: 0,
        }
    }

    /// Virtual address for `(phys, size)`, mapping it if not already cached.
    pub fn get(&mut self, phys: u32, size: u32) -> io::Result<u32> {
        if let Some(i) = self
            .entries
            .iter()
            .position(|&(p, s, _)| p == phys && s == size)
        {
            let e = self.entries.remove(i);
            let v = e.2;
            self.entries.push(e);
            self.hits += 1;
            return Ok(v);
        }
        self.misses += 1;
        let vaddr = self.drv.map(phys, size)?;
        if self.entries.len() >= self.cap {
            let (_, s, v) = self.entries.remove(0);
            let _ = self.drv.unmap(v, s);
        }
        self.entries.push((phys, size, vaddr));
        Ok(vaddr)
    }

    /// Copy `size` bytes at physical `phys` into `out`.
    ///
    /// # Safety
    /// The mapping is driver memory being written by hardware. We only ever
    /// read it, and a torn read costs at most a corrupt TS packet, which the
    /// downstream demuxer already has to tolerate.
    pub fn copy_out(&mut self, phys: u32, size: u32, out: &mut Vec<u8>) -> io::Result<()> {
        if size == 0 {
            return Ok(());
        }
        let vaddr = self.get(phys, size)?;
        let n = size as usize;
        let start = out.len();
        out.reserve(n);
        unsafe {
            std::ptr::copy_nonoverlapping(vaddr as usize as *const u8, out.as_mut_ptr().add(start), n);
            out.set_len(start + n);
        }
        Ok(())
    }
}

impl Drop for MapCache {
    fn drop(&mut self) {
        for &(_, size, vaddr) in &self.entries {
            let _ = self.drv.unmap(vaddr, size);
        }
    }
}

/// Fraction of 188-byte-aligned positions carrying the MPEG-TS sync byte.
///
/// This is the measurement that settles whether the bytes coming out of the
/// driver are plaintext transport stream (handoff item A1). Scrambled or
/// framed-differently data scores near zero; real TS scores 1.0.
pub fn ts_sync_ratio(buf: &[u8]) -> f32 {
    if buf.len() < 188 * 2 {
        return 0.0;
    }
    // Find the first offset that yields a run of sync bytes, then score from it.
    for off in 0..188.min(buf.len()) {
        let mut total = 0u32;
        let mut hits = 0u32;
        let mut i = off;
        while i < buf.len() {
            total += 1;
            if buf[i] == 0x47 {
                hits += 1;
            }
            i += 188;
        }
        if total >= 4 && hits * 4 >= total * 3 {
            return hits as f32 / total as f32;
        }
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_sizes_match_the_driver() {
        // These are the byte counts each handler stores at +0x8b8 before its
        // copy_from_user; a mismatch means the driver reads past our object.
        assert_eq!(std::mem::size_of::<ReqCreateRead>(), 8);
        assert_eq!(std::mem::size_of::<ReqGetRead>(), 40);
        assert_eq!(std::mem::size_of::<ReqCommitRead>(), 16);
        assert_eq!(std::mem::size_of::<ReqMemMap>(), 12);
        assert_eq!(std::mem::size_of::<ReqMemUnmap>(), 8);
        assert_eq!(std::mem::size_of::<[u32; 9]>(), 36); // ioctl 0x51 TSI status
        assert_eq!(std::mem::size_of::<[u32; 10]>(), 40); // ioctl 0x50 TSI config
        assert_eq!(std::mem::size_of_val(&demux_pid_filter_info_request(0, 0)), 16);
    }

    #[test]
    fn sub_command_encoding_matches_the_known_case() {
        // xc_mini_device_remux_set_copy_permission issues 0x0459002A:
        // (global id << 16) | top-level number.
        assert_eq!(sub(0x2A, 0x459), 0x0459002A);
        assert_eq!(sub(SBUF_INFO, SBUF_INFO_STATUS), 0x00010036);
    }

    #[test]
    fn product_tsi_route_is_direct_logical_port_zero() {
        assert_eq!(product_tsi_request(0x0c), [0x0c, 0, 0, 0]);
        assert_eq!(product_tsi_request(0x12), [0x12, 0, 0, 0]);
    }

    #[test]
    fn ts_recorder_uses_the_sdk_direct_tsi_route() {
        assert_eq!(ts_recorder_tsi_request(0x0c, 0), [0x0c, 0, 0, 0]);
        assert_eq!(ts_recorder_tsi_request(0x12, 3), [0x12, 0, 3, 0]);
    }

    /// Gate 5D-B1 changes exactly one word. Everything else must stay byte
    /// identical to the product request that every previous 0-byte gate used.
    /// Gate 5E2-SYNC must move exactly one word, and it must be word 4.
    #[test]
    fn gate_5e2_sync_changes_only_word_4() {
        let baseline = product_physical_tsi_request(0, ME_PACKET_188, false);
        let sync = product_physical_tsi_request(0, ME_PACKET_188, true);
        assert_eq!(baseline[4], 0);
        assert_eq!(sync[4], 1);
        for i in (0..10).filter(|&i| i != 4) {
            assert_eq!(baseline[i], sync[i], "word {i} must not change");
        }
        assert_eq!(baseline, [0, 0, 0, 1, 0, 0, 1, 0, 1, 0]);
        assert_eq!(sync, [0, 0, 0, 1, 1, 0, 1, 0, 1, 0]);
    }

    /// The two Gate 5D/5E knobs must stay independent: enabling sync must not
    /// disturb the packet field and vice versa.
    #[test]
    fn packet_and_sync_fields_are_independent() {
        assert_eq!(product_physical_tsi_request(0, ME_PACKET_204, true), [0, 2, 0, 1, 1, 0, 1, 0, 1, 0]);
        assert_eq!(product_physical_tsi_request(1, ME_PACKET_188, true), [1, 0, 0, 1, 1, 0, 1, 0, 1, 0]);
    }

    #[test]
    fn me_packet_is_the_only_word_gate_5d_b1_changes() {
        let base = product_mpeg_ts_tsi_request(0);
        let b1 = product_physical_tsi_request(0, ME_PACKET_204, false);
        assert_eq!(base[1], ME_PACKET_188);
        assert_eq!(b1[1], ME_PACKET_204);
        for i in (0..10).filter(|&i| i != 1) {
            assert_eq!(base[i], b1[i], "word {i} must not change");
        }
        assert_eq!(b1, [0, 2, 0, 1, 0, 0, 1, 0, 1, 0]);
    }

    /// `tsconfig:0x8d36-0x8d54` inverse map and `libvixs .rodata:0x127460`.
    #[test]
    fn me_packet_enum_matches_the_vendor_encoding() {
        assert_eq!(
            [ME_PACKET_188, ME_PACKET_192, ME_PACKET_204, ME_PACKET_130],
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn product_mpeg_ts_tsi0_config_matches_abs_on_this_model() {
        assert_eq!(
            product_mpeg_ts_tsi_request(0),
            [0, 0, 0, 1, 0, 0, 1, 0, 1, 0]
        );
        assert_eq!(product_mpeg_ts_tsi_request(3)[0], 3);
    }

    #[test]
    fn ts_recorder_physical_tsi_ignores_the_sync_signal() {
        assert_eq!(
            ts_recorder_physical_tsi_request(0),
            [0, 0, 0, 1, 0, 0, 0, 0, 1, 0]
        );
        assert_eq!(ts_recorder_physical_tsi_request(3)[0], 3);
        assert_ne!(
            ts_recorder_physical_tsi_request(0),
            product_mpeg_ts_tsi_request(0)
        );
    }

    #[test]
    fn product_demux_shape_matches_dxd_create_route() {
        assert_eq!(mpeg_ts_demux_create_request(), [0x0c, 0, 32, 0, 0, 0]);
    }

    #[test]
    fn product_av_sync_requests_are_byte_exact() {
        assert_eq!(av_sync_create_request(), [7, 0, 0, 16, 0, 0]);
        assert_eq!(std::mem::size_of_val(&av_sync_create_request()), 24);
        assert_eq!(
            av_sync_live_mode_request(0x12),
            [0x12, 1, 1, 0, 0, 0, 0, 0]
        );
        assert_eq!(std::mem::size_of_val(&av_sync_live_mode_request(0)), 32);
        assert_eq!(PIPELINE_STOP, 0x8f);
    }

    #[test]
    fn demux_full_ts_ring_is_system_ts() {
        assert_eq!(
            demux_ts_buffer_create_request(),
            [3, 1, 2, 1, 0x1000, 0x6000, 188, 3, 0]
        );
        assert_eq!(0x6000u32 * 188, 0x468000);
    }

    #[test]
    fn demux_pat_section_path_matches_product_calls() {
        assert_eq!(
            demux_pat_section_buffer_create_request(),
            [3, 2, 14, 1, 0x1000, 0x40000, 1, 3, 0]
        );
        assert_eq!(
            demux_pat_section_request(0x12, 0x34),
            [0x12, 0, 2, 0x34, u32::MAX - 1, 1000, 12, 2, 14]
        );
        assert_eq!(std::mem::size_of_val(&demux_pat_section_request(0, 0)), 36);
        assert_eq!(demux_pid_filter_info_request(0x12, 0), [0x12, 0, 0, 0]);
        assert_eq!(DEMUX_PID_FILTER_INFO, 0x01f5_002b);
    }

    #[test]
    fn demux_output_all_request_is_bounded_and_byte_exact() {
        assert_eq!(
            demux_pid_request(0x12, u32::MAX, 0x34),
            [0x12, u32::MAX, 1, 0x34, u32::MAX - 1, 1000, 12, 1, 2]
        );
        assert_eq!(
            std::mem::size_of_val(&demux_pid_request(0, u32::MAX, 0)),
            36
        );
        assert_eq!(
            demux_remove_pid_request(0x12, u32::MAX),
            [0x12, 1, u32::MAX]
        );
        assert_eq!(
            demux_pid_request(0x12, 0, 0x34),
            [0x12, 0, 1, 0x34, u32::MAX - 1, 1000, 12, 1, 2]
        );
        assert_eq!(demux_remove_pid_request(0x12, 0), [0x12, 1, 0]);
    }

    #[test]
    fn product_demux_run_and_buffer_teardown_are_byte_exact() {
        assert_eq!(product_demux_run_request(0x12), [0x12, 1, 0]);
        assert_eq!(force_reset_buffer_request(0x34), [0x34, 1]);
        assert_eq!(
            product_demux_disconnect_request(0x12, 0x56),
            [0x12, 0x56, 1, 3000]
        );
        assert_eq!(SBUF_FORCE_RESET, 0x0003_0035);
        assert_eq!(std::mem::size_of_val(&force_reset_buffer_request(0)), 8);
    }

    /// Every value here is a literal in `adsb`'s recording path, so the test
    /// is a transcription check against the disassembly, not a restatement of
    /// the code above.
    #[test]
    fn product_remux_construction_is_byte_exact() {
        // adsb:0x42248..0x42260  create(0x0E, 13 inputs, 2 outputs, 1 control)
        assert_eq!(remux_create_request(), [0x0e, 13, 2, 1, 0, 0]);
        assert_eq!(std::mem::size_of_val(&remux_create_request()), 24);

        // adsb:0x42294  config_ext(3, 0x10, 0, 1, 100, 0)
        assert_eq!(remux_config_request(0x11, 1), [0x11, 3, 0x10, 0, 1, 100, 0]);
        // Ours differs from the product only in the stream id it advertises.
        assert_eq!(
            remux_config_request(0x11, 0xb072),
            [0x11, 3, 0x10, 0, 0xb072, 100, 0]
        );
        assert_eq!(std::mem::size_of_val(&remux_config_request(0, 0)), 28);

        // adsb:0x422e2 TSOUT and 0x4232c TSINFO, both m_bf_flags = 3
        assert_eq!(
            remux_tsout_buffer_request(),
            [4, 1, 0, 1, 0x1000, 0x468000, 1, 3, 0]
        );
        assert_eq!(
            remux_tsinfo_buffer_request(),
            [4, 2, 0x10, 1, 0x1000, 0x20000, 1, 3, 0]
        );
        assert_eq!(std::mem::size_of_val(&remux_tsout_buffer_request()), 36);
    }

    /// `libxcodeapi:0xe768` memsets 44 bytes and fills exactly six words plus
    /// the 64-bit ATS base; `libvixs:0x7fca0` builds a seven-word stream
    /// request. Both sizes are the driver's own declarations.
    #[test]
    fn product_remux_stream_declarations_are_byte_exact() {
        assert_eq!(
            remux_add_program_request(0x11, 1, 0x1001, 0x100, 100, 0x1234_5678_9abc),
            [0x11, 1, 0x1001, 0x100, 100, 1, 0x5678_9abc, 0x1234, 0, 0, 0]
        );
        assert_eq!(
            std::mem::size_of_val(&remux_add_program_request(0, 0, 0, 0, 0, 0)),
            44
        );

        // adsb:0x42be6..0x42c0e, both rate literals in argument order.
        assert_eq!(
            remux_config_video_request(0x11, 0x22),
            [0x11, 0x22, 0, 0x02a4_c200, 0, 0, 0, 0, 0x029f_6300]
        );
        assert_eq!(std::mem::size_of_val(&remux_config_video_request(0, 0)), 36);
        assert_eq!(remux_copy_permission_request(0x11, 0), [0x11, 0]);

        // adsb:0x42c62 video, 0x42cc0 audio.
        assert_eq!(
            remux_add_stream_request(0x11, 0x22, 1, 0x1011, 0xe0, 0x24),
            [0x11, 0x22, 1, 0x1011, 0xe0, 0x24, 0]
        );
        assert_eq!(
            std::mem::size_of_val(&remux_add_stream_request(0, 0, 0, 0, 0, 0)),
            28
        );

        assert_eq!(remux_remove_stream_request(0x11, 0x22, 1), [0x11, 0x22, 1]);
        assert_eq!(remux_remove_program_request(0x11, 1), [0x11, 1]);
    }

    /// Control codes are measured `movw r1, #imm` immediates inside the
    /// matching `libvixs` wrapper; the low halfword must stay 0x2A so the
    /// driver dispatches to `xc_sdev_ctrl_func`.
    #[test]
    fn remux_control_codes_match_libvixs() {
        for (code, expected) in [
            (REMUX_CONFIG, 0x44c),
            (REMUX_ADD_PROGRAM, 0x44d),
            (REMUX_REMOVE_PROGRAM, 0x44e),
            (REMUX_ADD_VIDEO, 0x44f),
            (REMUX_ADD_AUDIO, 0x450),
            (REMUX_REMOVE_STREAM, 0x454),
            (REMUX_CONFIG_VIDEO, 0x456),
            (REMUX_SET_COPY_PERMISSION, 0x459),
        ] {
            assert_eq!(code & 0xFFFF, 0x2a, "{code:#x} must dispatch to sdev_ctrl");
            assert_eq!(code >> 16, expected, "{code:#x} sub-selector");
        }
        // Class 11 * 100 + index, the encoding `xc_sdev_info_func` decodes.
        assert_eq!(REMUX_CONFIG >> 16, 11 * 100);
        assert_eq!(SDEV_CONNECT_INPUT, 0x0007_002a);
    }

    #[test]
    fn sync_ratio_detects_transport_stream() {
        let mut ts = vec![0u8; 188 * 10];
        for i in 0..10 {
            ts[i * 188] = 0x47;
        }
        assert!(ts_sync_ratio(&ts) > 0.99);

        // Offset streams still score, since we search for the alignment.
        let mut off = vec![0xAAu8; 7];
        off.extend_from_slice(&ts);
        assert!(ts_sync_ratio(&off) > 0.9);

        // Scrambled / non-TS data must not score.
        let noise: Vec<u8> = (0..188 * 10).map(|i| (i * 37 % 251) as u8).collect();
        assert!(ts_sync_ratio(&noise) < 0.75);
    }

    #[test]
    fn wrapped_read_size_matches_vendor_semantics() {
        let chunk = Chunk {
            addr: 0x1000,
            size: 1000,
            addr1: 0x2000,
            size1: 200,
            desc: 0x3000,
        };
        assert_eq!(chunk.first_size(), Some(800));
        assert_eq!(chunk.total(), 1000);

        let malformed = Chunk { size: 10, size1: 11, ..Default::default() };
        assert_eq!(malformed.first_size(), None);
    }
}

//! GPU temperature on Windows, read from the AMD display driver's own library.
//!
//! Why this file exists. The thermal gauge was built on `rocm-smi` and
//! `amd-smi`, and on Windows neither of those exists: ROCm is a Linux stack and
//! `amd-smi` is not part of the consumer Adrenalin package. The Linux
//! `thermal_file` hwmon path is absent too. So on a normal Windows box with a
//! Radeon card, which is most of this miner's audience, the whole feature could
//! only ever print
//!
//!   [Thermal] No GPU temperature to report: no exact temperature sensor for
//!             Amd GPU 0; supported: amd-smi, rocm-smi, or thermal_file
//!
//! What Windows does have is `atiadlxx.dll`, the AMD Display Library. It ships
//! with every Adrenalin driver and lives in `System32`, so it is loaded here at
//! runtime with `LoadLibrary`: no build dependency, no import library, nothing
//! bundled, and on a machine without an AMD driver the load simply fails and
//! this module reports nothing at all.
//!
//! Measured on the machine this was written for, an RX 9070 XT (RDNA 4, Navi 48)
//! on driver 32.0.31035.1003, while it was mining:
//!
//!   ADL2_Main_Control_Create           ~10 ms, once
//!   ADL2_New_QueryPMLogData_Get        0.2 to 1.1 ms per read
//!   sensors: edge 60 C, memory 68 C, hotspot 84 C, fan 1032 rpm,
//!            activity 99%, gfx clock 3302 MHz, gfx voltage 1123 mV,
//!            board power 256 W
//!   ADL2_OverdriveN_Temperature_Get    ADL_ERR_NOT_SUPPORTED (-8)
//!   ADL2_Overdrive6_Temperature_Get    ADL_ERR_NOT_SUPPORTED (-8)
//!
//! So on current hardware PMLog is the only one of the three that answers, and
//! it answers cheaply. The Overdrive entry points are deliberately not used.
//!
//! About the sensor indices. They come from AMD's published `ADLSensorType`
//! enum, and the live dump above is what confirms them rather than a comment
//! claiming they are right: at index 1 a 3302 MHz core clock, at 2 a 2505 MHz
//! memory clock, at 14 a 1032 rpm fan, at 15 a fan percentage, at 19 a 99% load
//! on a card at full tilt, at 21 a 1123 mV core voltage. Every one of those
//! landed exactly where the enum says it should, which is what makes the three
//! temperature indices next to them trustworthy. A sensor whose `supported`
//! flag is clear, or whose value is outside a plausible range, is dropped by the
//! caller rather than reported.
//!
//! Board power (index 73) was pinned down the same way, by dumping every
//! non-zero slot at three load levels on this card:
//!
//!   state                       idx 19  idx 1      idx 73   idx 58
//!   idle desktop                2-7%    58-137MHz    46 W      5
//!   x16rs_gate, 3 work groups   99%     3383 MHz    120 W      5
//!   x16rs_gate, 48 work groups  99%     3312 MHz    256 W      5
//!
//! Index 73 is the only slot that separates those three states in watts, and its
//! neighbours in the enum land where the enum says: index 40 reads 4 and index
//! 41 reads 16 on a card in a PCIe gen 4 x16 slot, which is `ADL_PMLOG_BUS_SPEED`
//! and `ADL_PMLOG_BUS_LANES` exactly. Index 58 was the other candidate and it is
//! flat at 5 through all three states, so it is the throttler percentage the
//! enum says it is, not a power. The numbers are whole watts, not milliwatts and
//! not hundredths: 256 at full tilt is what a 304 W-rated RX 9070 XT actually
//! draws on this workload, and a hundredths reading would have been 2.56 W.
//!
//! What index 73 measures is TOTAL BOARD power, the figure on the electricity
//! bill, not the GPU die alone. `ADL_PMLOG_ASIC_POWER` (23) and
//! `ADL_PMLOG_GFX_POWER` (30) are the die-only figures and neither is supported
//! on this card and driver: both read `supported = 0`. Nothing here silently
//! falls back to them, because die-only power is tens of watts below board power
//! and quoting one as the other would understate the operator's cost. Where
//! board power is absent, this module reports no power at all.

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}

// ADL allocates through a caller-supplied callback. It hands the pointer back
// to us and we own it, so this must be the same allocator the C runtime frees
// with; `std::alloc` with its size-and-align contract is not, because ADL frees
// nothing and we free with `free`.
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

unsafe extern "C" fn adl_malloc(size: c_int) -> *mut c_void {
    if size <= 0 {
        return std::ptr::null_mut();
    }
    unsafe { malloc(size as usize) }
}

const ADL_OK: c_int = 0;
const ADL_MAX_PATH: usize = 256;
const PMLOG_SENSOR_COUNT: usize = 256;

/// `ADLSensorType` indices for the three temperatures `rocm-smi --showtemp`
/// prints on Linux, so the number this module reports means the same thing on
/// both platforms.
const PMLOG_TEMPERATURE_EDGE: usize = 8;
const PMLOG_TEMPERATURE_MEM: usize = 9;
const PMLOG_TEMPERATURE_HOTSPOT: usize = 27;

/// `ADL_PMLOG_BOARD_POWER`: whole watts drawn by the whole graphics board, the
/// 12V rails and the memory and the VRM losses included. Deliberately not
/// `ADL_PMLOG_ASIC_POWER` (23) or `ADL_PMLOG_GFX_POWER` (30), which are the die
/// alone and read tens of watts lower. See the module header.
const PMLOG_BOARD_POWER: usize = 73;

/// `ADL_PMLOG_CLK_GFXCLK`: shader clock in MHz. Confirmed in the same dump the
/// module header quotes: 58-137 MHz on an idle desktop against 3383 MHz under
/// the miner, which is the only slot that moves that way.
const PMLOG_GFX_CLOCK: usize = 1;

/// `AdapterInfo` from `adl_structures.h`. The last five fields are Windows-only
/// in the header and this file is Windows-only, so all of them are present. The
/// struct is passed by us and filled by ADL, so its size must match exactly:
/// 1572 bytes, asserted below and confirmed against the library at runtime by
/// the fact that the bus and device numbers come back correct.
#[repr(C)]
#[derive(Clone, Copy)]
struct AdapterInfo {
    size: c_int,
    adapter_index: c_int,
    udid: [c_char; ADL_MAX_PATH],
    bus_number: c_int,
    device_number: c_int,
    function_number: c_int,
    vendor_id: c_int,
    adapter_name: [c_char; ADL_MAX_PATH],
    display_name: [c_char; ADL_MAX_PATH],
    present: c_int,
    exist: c_int,
    driver_path: [c_char; ADL_MAX_PATH],
    driver_path_ext: [c_char; ADL_MAX_PATH],
    pnp_string: [c_char; ADL_MAX_PATH],
    os_display_index: c_int,
}

const _: () = assert!(std::mem::size_of::<AdapterInfo>() == 1572);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SingleSensorData {
    supported: c_int,
    value: c_int,
}

#[repr(C)]
struct PmLogDataOutput {
    size: c_int,
    sensors: [SingleSensorData; PMLOG_SENSOR_COUNT],
}

type FnMainControlCreate = unsafe extern "C" fn(
    unsafe extern "C" fn(c_int) -> *mut c_void,
    c_int,
    *mut *mut c_void,
) -> c_int;
type FnNumberOfAdapters = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type FnAdapterInfoGet = unsafe extern "C" fn(*mut c_void, *mut AdapterInfo, c_int) -> c_int;
type FnQueryPmLogData = unsafe extern "C" fn(*mut c_void, c_int, *mut PmLogDataOutput) -> c_int;

struct Adl {
    context: *mut c_void,
    adapter_info_get: FnAdapterInfoGet,
    number_of_adapters: FnNumberOfAdapters,
    query_pmlog: FnQueryPmLogData,
}

// The handles are process-lifetime and every use goes through the `Mutex`
// below; ADL's own context is not documented as thread safe, which is exactly
// why nothing here is reachable without holding that lock.
unsafe impl Send for Adl {}

/// One AMD GPU as ADL sees it, and the temperature it answered with.
#[derive(Clone, Debug, PartialEq)]
pub struct AdlGpuTemp {
    pub adapter_index: i32,
    /// PCI bus, device and function. Several ADL adapters share one physical
    /// card (one per display output), so this is what identifies the card.
    pub pci: (i32, i32, i32),
    pub name: String,
    pub temp_c: f32,
}

fn plausible_temp(value: c_int) -> Option<f32> {
    // Same window the rest of the thermal code uses. A GPU below zero or above
    // 120 C is a sensor answering with something that is not a temperature.
    let value = value as f32;
    (value > 0.0 && value < 120.0).then_some(value)
}

/// The widest window a graphics board's total draw can honestly fall in.
///
/// One definition, in `efficiency`, shared with the `nvidia-smi` power path so
/// that a driver reading and a command reading are judged by the same rule and
/// cannot drift apart. Kept here as a name because this module's callers are
/// about ADL slots, not about the panel's economics.
pub(crate) fn plausible_board_power_w(value: f32) -> Option<f32> {
    crate::efficiency::plausible_board_power_w(value)
}

/// A shader clock a GPU can really be running at.
///
/// The floor is 1 MHz rather than something comfortable because this card idles
/// at 58 MHz and a deep-idle reading is still a reading; only an exact zero,
/// which is what an unsupported slot holds, is refused. The ceiling is far above
/// any shipping part, so it excludes a slot holding kHz or a different quantity
/// entirely rather than excluding a fast card.
pub(crate) fn plausible_gfx_clock_mhz(value: f32) -> Option<f32> {
    (value.is_finite() && value > 0.0 && value < 10_000.0).then_some(value)
}

impl Adl {
    fn load() -> Option<Mutex<Adl>> {
        // SAFETY: every pointer below is either checked for null before use or
        // handed straight back to the library that produced it.
        unsafe {
            let name = CString::new("atiadlxx.dll").ok()?;
            let library = LoadLibraryA(name.as_ptr());
            if library.is_null() {
                return None;
            }
            // The library is intentionally never freed: the context created
            // below outlives every read, and unloading a driver library while
            // its worker threads run is how a shutdown crash is bought.
            let symbol = |symbol_name: &str| -> Option<*mut c_void> {
                let symbol_name = CString::new(symbol_name).ok()?;
                let address = GetProcAddress(library, symbol_name.as_ptr());
                (!address.is_null()).then_some(address)
            };

            let create: FnMainControlCreate =
                std::mem::transmute(symbol("ADL2_Main_Control_Create")?);
            let number_of_adapters: FnNumberOfAdapters =
                std::mem::transmute(symbol("ADL2_Adapter_NumberOfAdapters_Get")?);
            let adapter_info_get: FnAdapterInfoGet =
                std::mem::transmute(symbol("ADL2_Adapter_AdapterInfo_Get")?);
            let query_pmlog: FnQueryPmLogData =
                std::mem::transmute(symbol("ADL2_New_QueryPMLogData_Get")?);

            let mut context: *mut c_void = std::ptr::null_mut();
            // Second argument 1: enumerate connected adapters only.
            if create(adl_malloc, 1, &mut context) != ADL_OK || context.is_null() {
                return None;
            }
            Some(Mutex::new(Adl {
                context,
                adapter_info_get,
                number_of_adapters,
                query_pmlog,
            }))
        }
    }

    fn adapters(&self) -> Vec<AdapterInfo> {
        // SAFETY: the buffer is sized from the count ADL just reported, and the
        // byte length passed is the one it fills.
        unsafe {
            let mut count: c_int = 0;
            if (self.number_of_adapters)(self.context, &mut count) != ADL_OK || count <= 0 {
                return Vec::new();
            }
            let count = count as usize;
            let mut infos: Vec<AdapterInfo> = vec![std::mem::zeroed(); count];
            let bytes = count * std::mem::size_of::<AdapterInfo>();
            let Ok(bytes) = c_int::try_from(bytes) else {
                return Vec::new();
            };
            if (self.adapter_info_get)(self.context, infos.as_mut_ptr(), bytes) != ADL_OK {
                return Vec::new();
            }
            infos
        }
    }

    /// One PMLog query. Every reader below goes through this, so a caller that
    /// wants several sensors reads them from ONE driver call and therefore from
    /// one instant, instead of stitching together readings taken milliseconds
    /// apart while the clocks move.
    fn query(&self, adapter_index: i32) -> Option<PmLogDataOutput> {
        // SAFETY: the output struct is fully owned here and ADL only writes into
        // it. A bad adapter index is refused by the library with a non-zero
        // return (measured: -5 for an index that does not exist, -8 for an
        // integrated GPU that has no such sensor), never with a stale value.
        unsafe {
            let mut out: PmLogDataOutput = std::mem::zeroed();
            if (self.query_pmlog)(self.context, adapter_index as c_int, &mut out) != ADL_OK {
                return None;
            }
            Some(out)
        }
    }

    fn temperature(&self, adapter_index: i32) -> Option<f32> {
        self.query(adapter_index)?.temperature()
    }

    /// Total board power in watts, or `None` where this card does not measure it.
    ///
    /// One sensor, not a maximum over several: unlike temperature, where the
    /// hottest of the three is the one that matters, there is exactly one number
    /// that is the board's draw, and reducing over candidates would silently
    /// promote a die-only figure whenever the board figure went missing.
    fn board_power_w(&self, adapter_index: i32) -> Option<f32> {
        self.query(adapter_index)?.board_power_w()
    }
}

impl PmLogDataOutput {
    fn slot(&self, index: usize) -> Option<c_int> {
        let slot = self.sensors[index];
        (slot.supported != 0).then_some(slot.value)
    }

    fn temperature(&self) -> Option<f32> {
        [
            PMLOG_TEMPERATURE_EDGE,
            PMLOG_TEMPERATURE_MEM,
            PMLOG_TEMPERATURE_HOTSPOT,
        ]
        .into_iter()
        .filter_map(|index| self.slot(index))
        .filter_map(plausible_temp)
        .reduce(f32::max)
    }

    fn board_power_w(&self) -> Option<f32> {
        plausible_board_power_w(self.slot(PMLOG_BOARD_POWER)? as f32)
    }

    fn gfx_clock_mhz(&self) -> Option<f32> {
        plausible_gfx_clock_mhz(self.slot(PMLOG_GFX_CLOCK)? as f32)
    }
}

/// One instant of a card's telemetry, all of it from a single driver query.
///
/// Every field is optional on purpose. A slot this driver does not support is
/// absent, never zero: a zero clock or a zero draw would be read downstream as a
/// measurement of an idle card rather than as the absence of a sensor.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AdlSample {
    pub temp_c: Option<f32>,
    pub board_power_w: Option<f32>,
    pub gfx_clock_mhz: Option<f32>,
}

fn adl() -> Option<&'static Mutex<Adl>> {
    static ADL: OnceLock<Option<Mutex<Adl>>> = OnceLock::new();
    ADL.get_or_init(Adl::load).as_ref()
}

fn c_string(bytes: &[c_char]) -> String {
    let raw: Vec<u8> = bytes.iter().map(|byte| *byte as u8).collect();
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).trim().to_string()
}

/// Every distinct AMD card that answers with a temperature right now.
///
/// Distinct means by PCI address: ADL lists one adapter per display output, so
/// a single card shows up several times. Cards that do not answer are left out
/// rather than reported as zero, which is what keeps an integrated GPU (it
/// returns ADL_ERR_NOT_SUPPORTED) from being offered as a mining sensor.
pub fn reporting_gpus() -> Vec<AdlGpuTemp> {
    let Some(adl) = adl() else {
        return Vec::new();
    };
    let adl = adl.lock().unwrap_or_else(|error| error.into_inner());
    let mut found: Vec<AdlGpuTemp> = Vec::new();
    for info in adl.adapters() {
        let pci = (info.bus_number, info.device_number, info.function_number);
        if found.iter().any(|gpu| gpu.pci == pci) {
            continue;
        }
        if let Some(temp_c) = adl.temperature(info.adapter_index) {
            found.push(AdlGpuTemp {
                adapter_index: info.adapter_index,
                pci,
                name: c_string(&info.adapter_name),
                temp_c,
            });
        }
    }
    found
}

/// Read one adapter that `reporting_gpus` already bound to.
pub fn temperature_c(adapter_index: i32) -> Option<f32> {
    let adl = adl()?;
    let adl = adl.lock().unwrap_or_else(|error| error.into_inner());
    adl.temperature(adapter_index)
}

/// Total board power in watts for one adapter that `reporting_gpus` bound to.
///
/// `None` on a card whose driver does not publish it, which is the whole point:
/// a caller that gets nothing here has to say so and fall back to its configured
/// estimate, rather than publish a zero that reads as a card drawing no power.
pub fn board_power_w(adapter_index: i32) -> Option<f32> {
    let adl = adl()?;
    let adl = adl.lock().unwrap_or_else(|error| error.into_inner());
    adl.board_power_w(adapter_index)
}

/// Temperature, board power and shader clock for one adapter, from a single
/// driver query.
///
/// The auto-tuner needs all three at once to decide whether the card has settled
/// into a steady state. Reading them one call at a time would cost three driver
/// round trips per sample and would compare a temperature to a clock taken at a
/// different instant, which is exactly the thing a settling test must not do.
pub fn sample(adapter_index: i32) -> Option<AdlSample> {
    let adl = adl()?;
    let adl = adl.lock().unwrap_or_else(|error| error.into_inner());
    let out = adl.query(adapter_index)?;
    Some(AdlSample {
        temp_c: out.temperature(),
        board_power_w: out.board_power_w(),
        gfx_clock_mhz: out.gfx_clock_mhz(),
    })
}

/// Why there is no ADL temperature, in words an operator can act on.
///
/// Only called on the failure path, and it says what was actually observed
/// rather than guessing: the driver library missing, or present but with more
/// than one card answering, which is the case this module refuses to resolve.
pub fn unavailable_reason(gpu_index: u32) -> String {
    if adl().is_none() {
        return "the AMD display driver library (atiadlxx.dll) did not load; \
                install or repair the AMD driver"
            .to_string();
    }
    let reporting = reporting_gpus();
    match reporting.len() {
        0 => "the AMD display driver library loaded but no card answered with a temperature"
            .to_string(),
        // Reachable only if the one card stopped answering between binding and
        // this call, since one card is exactly the case that does bind.
        1 => format!(
            "the AMD display driver stopped reporting a temperature for {}",
            reporting[0].name
        ),
        _ => format!(
            "the AMD display driver reports {} cards ({}) and cannot say which one is OpenCL \
             device {}; set thermal_file, or run one card per worker",
            reporting.len(),
            reporting
                .iter()
                .map(|gpu| gpu.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            gpu_index
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on any Windows machine. On one without an AMD driver every call
    /// must come back empty instead of panicking or blocking, because that is
    /// the path a CPU-only or NVIDIA miner takes on every start.
    #[test]
    fn a_machine_without_the_amd_driver_reports_nothing_and_does_not_panic() {
        let gpus = reporting_gpus();
        for gpu in &gpus {
            assert!(
                gpu.temp_c > 0.0 && gpu.temp_c < 120.0,
                "a reported temperature must be a plausible one, got {}",
                gpu.temp_c
            );
            assert!(!gpu.name.is_empty(), "a bound card must be named");
        }
        // Two consecutive enumerations must agree on how many cards answer:
        // the count is what decides whether a temperature may be attributed to
        // a GPU at all.
        assert_eq!(gpus.len(), reporting_gpus().len());
        assert!(!unavailable_reason(0).is_empty());
    }

    #[test]
    fn an_adapter_index_that_cannot_exist_yields_no_temperature() {
        // ADL answers a bad index with an error code, and the wrapper must turn
        // that into "no reading" rather than into the zero in the output buffer.
        assert_eq!(temperature_c(i32::MAX), None);
        assert_eq!(temperature_c(-1), None);
    }

    #[test]
    fn an_adapter_index_that_cannot_exist_yields_no_power() {
        // Same rule as the temperature above, and it matters more here: the zero
        // sitting in an unread output buffer is a perfectly formatted watt value
        // that would go straight into an operator's cost figure.
        assert_eq!(board_power_w(i32::MAX), None);
        assert_eq!(board_power_w(-1), None);
    }

    #[test]
    fn a_card_that_answers_at_all_answers_with_a_board_power_in_range() {
        // On a machine with no AMD driver this loop is empty and the test is
        // trivially true, which is the same shape the temperature test uses.
        // On the AMD box it is the real assertion: whatever the card reports has
        // to survive the plausibility window rather than be published raw.
        for gpu in reporting_gpus() {
            if let Some(watts) = board_power_w(gpu.adapter_index) {
                assert!(
                    watts > 0.0 && watts < 1000.0,
                    "{} reported {watts} W, which is not a board draw",
                    gpu.name
                );
            }
        }
    }

    #[test]
    fn implausible_sensor_values_are_not_board_power() {
        // Zero is what an unsupported slot holds, so it must never become a
        // reading; a board drawing nothing is not a state a running card is in.
        assert_eq!(plausible_board_power_w(0.0), None);
        assert_eq!(plausible_board_power_w(-5.0), None);
        assert_eq!(plausible_board_power_w(1000.0), None);
        assert_eq!(plausible_board_power_w(f32::NAN), None);
        assert_eq!(plausible_board_power_w(f32::INFINITY), None);
        // The three states this card was actually measured in.
        assert_eq!(plausible_board_power_w(46.0), Some(46.0));
        assert_eq!(plausible_board_power_w(120.0), Some(120.0));
        assert_eq!(plausible_board_power_w(256.0), Some(256.0));
    }

    #[test]
    fn implausible_sensor_values_are_not_temperatures() {
        assert_eq!(plausible_temp(0), None);
        assert_eq!(plausible_temp(-40), None);
        assert_eq!(plausible_temp(120), None);
        assert_eq!(plausible_temp(60), Some(60.0));
    }

    #[test]
    fn implausible_sensor_values_are_not_clocks() {
        // Zero is the unsupported slot again. A deep-idle 58 MHz is a real
        // reading on this card and must survive, or the settling test would see
        // an idle card as a card with no clock sensor at all.
        assert_eq!(plausible_gfx_clock_mhz(0.0), None);
        assert_eq!(plausible_gfx_clock_mhz(-1.0), None);
        assert_eq!(plausible_gfx_clock_mhz(10_000.0), None);
        assert_eq!(plausible_gfx_clock_mhz(f32::NAN), None);
        assert_eq!(plausible_gfx_clock_mhz(58.0), Some(58.0));
        assert_eq!(plausible_gfx_clock_mhz(3383.0), Some(3383.0));
    }

    #[test]
    fn one_sample_agrees_with_the_single_sensor_readers() {
        // The sampler is a second path to the same slots. If it ever disagreed
        // with the readers the rest of the miner publishes, the auto-tuner would
        // be settling on numbers nobody else can see.
        assert_eq!(sample(i32::MAX), None);
        for gpu in reporting_gpus() {
            let Some(sample) = sample(gpu.adapter_index) else {
                panic!("{} answered reporting_gpus but not sample", gpu.name);
            };
            assert!(
                sample.temp_c.is_some(),
                "{} is in reporting_gpus, so it has a temperature",
                gpu.name
            );
            // Not equality: these are two queries taken moments apart and the
            // card is live. Same sensor, same order of magnitude, same units.
            if let (Some(one), Some(two)) = (sample.board_power_w, board_power_w(gpu.adapter_index))
            {
                assert!(
                    (one - two).abs() < 200.0,
                    "{} reported {one} W then {two} W from the same slot",
                    gpu.name
                );
            }
            if let Some(clock) = sample.gfx_clock_mhz {
                assert!(clock > 0.0 && clock < 10_000.0);
            }
        }
    }
}

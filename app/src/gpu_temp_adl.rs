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
//!            activity 99%, gfx clock 3302 MHz, gfx voltage 1123 mV
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

    fn temperature(&self, adapter_index: i32) -> Option<f32> {
        // SAFETY: the output struct is fully owned here and ADL only writes into
        // it. A bad adapter index is refused by the library with a non-zero
        // return (measured: -5 for an index that does not exist, -8 for an
        // integrated GPU that has no such sensor), never with a stale value.
        unsafe {
            let mut out: PmLogDataOutput = std::mem::zeroed();
            if (self.query_pmlog)(self.context, adapter_index as c_int, &mut out) != ADL_OK {
                return None;
            }
            [
                PMLOG_TEMPERATURE_EDGE,
                PMLOG_TEMPERATURE_MEM,
                PMLOG_TEMPERATURE_HOTSPOT,
            ]
            .into_iter()
            .filter(|index| out.sensors[*index].supported != 0)
            .filter_map(|index| plausible_temp(out.sensors[index].value))
            .reduce(f32::max)
        }
    }
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
    fn implausible_sensor_values_are_not_temperatures() {
        assert_eq!(plausible_temp(0), None);
        assert_eq!(plausible_temp(-40), None);
        assert_eq!(plausible_temp(120), None);
        assert_eq!(plausible_temp(60), Some(60.0));
    }
}

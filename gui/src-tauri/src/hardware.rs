//! "Can I AI?" — the click-only, read-only hardware advisor (t_10daf382).
//!
//! Doctrine (operator-stated, non-negotiable):
//! - Runs ONLY when the volunteer clicks "Can I AI?" in Settings ▸ Local
//!   Model. No startup probe, no background task, no polls — nothing in
//!   this module is imported by the run loop or by any watcher/monitor.
//! - Read-only and unprivileged: /proc/meminfo, /sys/class/drm sysfs,
//!   `nvidia-smi` (query only), `sysctl -n` (read only), DXGI adapter
//!   enumeration + GlobalMemoryStatusEx (Windows). These are queries of
//!   already-present system state, never helper daemons; nothing here
//!   installs drivers, attempts elevation, spawns long-lived processes,
//!   or writes a single byte.
//! - Results stay on the machine: the report is rendered straight into the
//!   Settings dialog and never persisted, uploaded, or logged.
//! - Honest verdicts only: an inconclusive probe yields the honest
//!   "couldn't determine" answer — never a confident claim about hardware
//!   the probe could not actually see.
//! - The advisor never changes settings by itself. The tier-2
//!   "Open Custom endpoint settings" affordance only preselects a mode in
//!   the *unsaved* form draft; pressing Save settings stays a human act.
//!
//! Thresholds (operator-proven, implemented verbatim; inclusive):
//!   tier 1:  dedicated VRAM ≥ 10 GiB  OR  unified memory ≥ 24 GiB
//!   tier 2:  additionally RAM ≥ 64 GiB AND GPU-accessible memory ≥ 12 GiB
//!   (on Apple-silicon unified memory the iGPU addresses the same pool,
//!   so a ≥64 GiB Mac clears the 12 GiB bar through the unified route)
//!   tier 1/2 thresholds are measured in binary GiB — RAM/VRAM chips are
//!   physically binary-sized, so a marketed "10 GB" card is exactly 10 GiB
//!   and lands on the boundary inclusively.
//!   below tier 1 → built-in small model (2B Q4) verdict
//!   inconclusive → honest ambiguous verdict (never tier 1/2/3 certainty)

// fs/Path back the linux sysfs/procfs readers (and their table tests on any
// host); the windows/macOS probes don't touch the filesystem directly.
#[cfg(any(test, target_os = "linux"))]
use std::fs;
#[cfg(any(test, target_os = "linux"))]
use std::path::Path;
// Only the linux/macos probes spawn query binaries; the Windows probe is
// pure API calls.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

use serde::Serialize;

use crate::sidecar::{self, LARGE_MODEL};

// ────────────────────────────────────────────────────────────────────────────
// OPERATOR CONSTANTS — the single place the operator fills / edits.
// ────────────────────────────────────────────────────────────────────────────

/// Tier-1 weights source (operator-designated 2026-11-17: weights are
/// public at Hugging Face, MIT-licensed; repo + file facts verified the
/// same day against the HF tree API — see sidecar::LARGE_MODEL for the
/// matching download pin).
pub const MODEL_SOURCE_URL: &str = "https://huggingface.co/ornith-ai/Ornith-1.5-9B-GGUF";
pub const MODEL_SOURCE_LABEL: &str = "ornith-ai/Ornith-1.5-9B-GGUF";

/// Tier-2 Strata install docs. OPERATOR MUST FILL THIS with the published
/// install-instructions URL once the docs page exists. While it still holds
/// the sentinel below, the Strata line renders WITHOUT a link — do not ship
/// an invented URL.
pub const STRATA_DOCS_URL: &str = "<OPERATOR_FILL>";

// ────────────────────────────────────────────────────────────────────────────
// Thresholds (GiB-based byte boundaries, inclusive)
// ────────────────────────────────────────────────────────────────────────────

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const TIER1_VRAM_BYTES: u64 = 10 * GIB;
pub const TIER1_UNIFIED_BYTES: u64 = 24 * GIB;
pub const STRATA_RAM_BYTES: u64 = 64 * GIB;
pub const STRATA_GPU_BYTES: u64 = 12 * GIB;

/// Marker embedded in an AdviceLine text where the UI must splice the
/// AdviceLink anchor inline (mirrored in src/lib/SettingsDialog.svelte).
pub const INLINE_LINK_MARKER: &str = "{{link}}";

// ────────────────────────────────────────────────────────────────────────────
// Probe surface (trait seam so the sysfs/DXGI/sysctl calls run on any host
// in tests via ScriptedProbe — the real calls only ever fire from the
// click-triggered command)
// ────────────────────────────────────────────────────────────────────────────

/// Result of the discrete-GPU probe. Absent = the probe completed and
/// concluded "no discrete dGPU"; Unknown = inconclusive (an inconclusive
/// probe must NEVER yield a confident suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuReading {
    Detected { dedicated_bytes: u64, source: &'static str },
    Absent,
    Unknown,
}

impl GpuReading {
    pub fn state_str(&self) -> &'static str {
        match self {
            GpuReading::Detected { .. } => "detected",
            GpuReading::Absent => "absent",
            GpuReading::Unknown => "unknown",
        }
    }
    pub fn dedicated(&self) -> Option<u64> {
        match self {
            GpuReading::Detected { dedicated_bytes, .. } => Some(*dedicated_bytes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeFacts {
    pub os_label: &'static str,
    /// Apple-silicon Macs report their RAM as one pool both the CPU and the
    /// integrated GPU address — the 24 GiB unified-memory tier applies.
    pub unified_memory: bool,
    pub ram_bytes: Option<u64>,
    pub gpu: GpuReading,
}

pub trait SystemProbe {
    fn probe(&self) -> ProbeFacts;
}

pub struct RealSystemProbe;

impl SystemProbe for RealSystemProbe {
    fn probe(&self) -> ProbeFacts {
        #[cfg(target_os = "linux")]
        {
            linux_facts()
        }
        #[cfg(target_os = "macos")]
        {
            macos_facts()
        }
        #[cfg(windows)]
        {
            windows_facts()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            ProbeFacts {
                os_label: os_label(),
                unified_memory: false,
                ram_bytes: None,
                gpu: GpuReading::Unknown,
            }
        }
    }
}

// Used by the non-linux/macos/windows fallback arm of RealSystemProbe and
// by tests on every host; dead on normal targets.
#[allow(dead_code)]
fn os_label() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "unknown"
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Linux probe: /proc/meminfo + /sys/class/drm sysfs, nvidia-smi fallback
// ────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn linux_facts() -> ProbeFacts {
    let ram = fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(parse_memtotal_kb);
    let (sysfs_vram, scan_ok) = scan_drm_vram(Path::new("/sys/class/drm"));
    let gpu = match sysfs_vram {
        Some(v) => GpuReading::Detected {
            dedicated_bytes: v,
            source: "sysfs",
        },
        None => {
            if !scan_ok {
                // /sys/class/drm unreadable → inconclusive, NOT "no GPU".
                return ProbeFacts {
                    os_label: "linux",
                    unified_memory: false,
                    ram_bytes: ram,
                    gpu: GpuReading::Unknown,
                };
            }
            match probe_nvidia_smi() {
                NvidiaProbe::Bytes(v) => GpuReading::Detected {
                    dedicated_bytes: v,
                    source: "nvidia-smi",
                },
                NvidiaProbe::Absent => GpuReading::Absent,
                NvidiaProbe::Inconclusive => GpuReading::Unknown,
            }
        }
    };
    ProbeFacts {
        os_label: "linux",
        unified_memory: false,
        ram_bytes: ram,
        gpu,
    }
}

/// MemTotal kB → physical RAM bytes. Pure parser (table-tested); the real
/// file read lives in linux_facts.
#[cfg(any(test, target_os = "linux"))]
pub fn parse_memtotal_kb(text: &str) -> Option<u64> {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if it.next() != Some("MemTotal:") {
            continue;
        }
        return it
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb.saturating_mul(1024));
    }
    None
}

/// Scan `root` (normally /sys/class/drm) for amdgpu/nouveau-style
/// `cardN/device/mem_info_vram_total` files; returns (max dedicated VRAM,
/// scan_ok). A failed directory read is inconclusive (scan_ok=false) — the
/// caller reports Unknown rather than a confident "no GPU". Intel iGPUs
/// (i915) expose no such file; that yields Absent ("no discrete dGPU"),
/// which is the honest verdict for iGPU-only boxes.
#[cfg(any(test, target_os = "linux"))]
pub fn scan_drm_vram(root: &Path) -> (Option<u64>, bool) {
    let Ok(entries) = fs::read_dir(root) else {
        return (None, false);
    };
    let mut best: Option<u64> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // card0, card1, … — skip connector dirs (card0-DP-1) and renderD*.
        match name.strip_prefix("card") {
            Some(rest) if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) => {}
            _ => continue,
        }
        let vram_file = entry.path().join("device").join("mem_info_vram_total");
        if let Ok(text) = fs::read_to_string(&vram_file) {
            if let Ok(v) = text.trim().parse::<u64>() {
                if best.is_none_or(|b| v > b) {
                    best = Some(v);
                }
            }
        }
    }
    (best, true)
}

#[cfg(target_os = "linux")]
enum NvidiaProbe {
    Bytes(u64),
    Absent,
    Inconclusive,
}

/// The card's sanctioned Linux GPU fallback: query the driver's own CLI
/// (read-only query, click-only invocation). Absent = the binary is not
/// installed (no NVIDIA driver on the box); Inconclusive = present but the
/// query failed — never dressed up as a verdict.
#[cfg(target_os = "linux")]
fn probe_nvidia_smi() -> NvidiaProbe {
    match Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => NvidiaProbe::Absent,
        Err(_) => NvidiaProbe::Inconclusive,
        Ok(output) if output.status.success() => {
            match parse_nvidia_mib_csv(&String::from_utf8_lossy(&output.stdout)) {
                Some(v) => NvidiaProbe::Bytes(v),
                None => NvidiaProbe::Inconclusive,
            }
        }
        Ok(_) => NvidiaProbe::Inconclusive,
    }
}

/// nvidia-smi CSV (`--format=csv,noheader,nounits` → plain MiB per line);
/// multi-GPU boxes: the biggest single card (the matrix speaks of running
/// "a 9B-class model", i.e. one model on one card).
#[cfg(any(test, target_os = "linux"))]
pub fn parse_nvidia_mib_csv(csv: &str) -> Option<u64> {
    let mut best: Option<u64> = None;
    for line in csv.lines() {
        let Ok(mib) = line.trim().parse::<u64>() else {
            continue;
        };
        let bytes = mib.saturating_mul(1024 * 1024);
        if best.is_none_or(|b| bytes > b) {
            best = Some(bytes);
        }
    }
    best
}

// ────────────────────────────────────────────────────────────────────────────
// macOS probe: sysctl only. Apple-silicon → unified memory (no dGPU
// distinction); Intel Macs have no unprivileged VRAM source → Unknown.
// ────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn macos_facts() -> ProbeFacts {
    let ram = sysctl_read("hw.memsize").and_then(parse_sysctl_u64);
    let arm64 = sysctl_read("hw.optional.arm64")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let gpu = if arm64 {
        // Apple silicon: the GPU is integrated into the unified pool —
        // there is no separate VRAM to probe, and the matrix's unified
        // route (≥24 GiB) is what qualifies these boxes.
        GpuReading::Absent
    } else {
        // Intel Mac: dedicated VRAM is not readable without elevation or
        // heavier helpers → inconclusive by construction.
        GpuReading::Unknown
    };
    ProbeFacts {
        os_label: "macos",
        unified_memory: arm64,
        ram_bytes: ram,
        gpu,
    }
}

#[cfg(target_os = "macos")]
fn sysctl_read(key: &str) -> Option<String> {
    for bin in ["sysctl", "/sbin/sysctl", "/usr/sbin/sysctl"] {
        if let Ok(output) = Command::new(bin).args(["-n", key]).output() {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
    }
    None
}

/// Parse a sysctl value (`-n` bare number, or `key: value`) as u64.
#[cfg(any(test, target_os = "macos"))]
pub fn parse_sysctl_u64(text: &str) -> Option<u64> {
    let token = text.split_whitespace().last()?;
    token.parse::<u64>().ok()
}

// ────────────────────────────────────────────────────────────────────────────
// Windows probe: GlobalMemoryStatusEx + DXGI DedicatedVideoMemory.
// NOTE (deliberate, per the card): Win32_VideoController.AdapterRAM is a
// 32-bit-capped WMI value that INCLUDES shared system memory — it lies for
// this purpose. DXGI's DedicatedVideoMemory is the dedicated-only figure.
// ────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn windows_facts() -> ProbeFacts {
    ProbeFacts {
        os_label: "windows",
        unified_memory: false,
        ram_bytes: windows_total_ram_bytes(),
        gpu: windows_dgpu_bytes(),
    }
}

#[cfg(windows)]
fn windows_total_ram_bytes() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX::default();
    // Original Win32 field names (camelCase) are what windows 0.62 exposes.
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => Some(status.ullTotalPhys),
        Err(_) => None,
    }
}

#[cfg(windows)]
fn windows_dgpu_bytes() -> GpuReading {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return GpuReading::Unknown;
    };
    let mut best: Option<u64> = None;
    let mut adapters_seen = 0u32;
    for i in 0..32u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        adapters_seen += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        // DedicatedVideoMemory is SIZE_T (usize in windows-rs); adapters
        // like the Basic Render Driver report 0 dedicated memory.
        let dedicated = desc.DedicatedVideoMemory as u64;
        if best.is_none_or(|b| dedicated > b) {
            best = Some(dedicated);
        }
    }
    match best {
        Some(v) if v > 0 => GpuReading::Detected {
            dedicated_bytes: v,
            source: "dxgi",
        },
        Some(_) if adapters_seen > 0 => GpuReading::Absent,
        _ => GpuReading::Unknown,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Suggestion matrix (operator-proven thresholds, verbatim wording)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceTier {
    /// ≥10 GiB dedicated VRAM or ≥24 GiB unified memory.
    Tier1,
    /// Tier1 plus ≥64 GiB RAM with ≥12 GiB GPU-accessible memory.
    Tier1Strata,
    /// Conclusive probe, below tier 1 → the built-in 2B Q4 verdict.
    SmallModel,
    /// Probe inconclusive → honest "couldn't determine", never a verdict.
    Ambiguous,
}

impl AdviceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            AdviceTier::Tier1 => "tier1",
            AdviceTier::Tier1Strata => "tier1_strata",
            AdviceTier::SmallModel => "small_model",
            AdviceTier::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AdviceLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdviceLine {
    pub text: String,
    /// When present the UI renders an anchor: inline at INLINE_LINK_MARKER
    /// if the marker is inside text, otherwise appended after it.
    pub link: Option<AdviceLink>,
}

/// The serialized result of one click — facts + lines + the honest probe
/// state. Carries no secrets; the raw bytes are the facts, the UI formats.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareAdvice {
    pub os_label: &'static str,
    pub tier: &'static str,
    pub unified_memory: bool,
    pub ram_bytes: Option<u64>,
    /// Raw dedicated VRAM as measured (None unless a discrete GPU was measured).
    pub vram_bytes: Option<u64>,
    /// Honest GPU-probe state: "detected" | "absent" | "unknown".
    pub gpu_state: &'static str,
    pub gpu_source: Option<&'static str>,
    /// Windows only: raw VRAM minus the documented desktop-composition
    /// reservation estimate — DISPLAY aid, tier decisions use the raw figure.
    pub usable_vram_estimate: Option<u64>,
    /// Tier 2 only: the UI offers the deep-link into the (unsaved) Custom
    /// endpoint form. Nothing is ever switched by the advisor itself.
    pub offer_custom_endpoint: bool,
    pub lines: Vec<AdviceLine>,
}

fn advice_link(label: &str, url: &str) -> AdviceLink {
    AdviceLink {
        label: label.to_string(),
        url: url.to_string(),
    }
}

fn strata_link(docs_url: &str) -> Option<AdviceLink> {
    if docs_url.trim().is_empty() || docs_url.contains("OPERATOR_FILL") {
        // Operator hasn't filled STRATA_DOCS_URL yet — render no link
        // rather than invent one.
        None
    } else {
        Some(advice_link("install instructions", docs_url.trim()))
    }
}

/// Windows reserve heuristic: Windows + desktop composition keeps part of
/// the card for the display pipeline (roughly 0.5–2 GB on a normal
/// desktop). Estimate = dedicated − max(0.5 GiB, 10% of dedicated), always
/// shown next to the raw number AS an estimate. Tier math never uses it.
fn windows_usable_estimate(dedicated: u64) -> u64 {
    let reserve = (GIB / 2).max(dedicated / 10);
    dedicated.saturating_sub(reserve)
}

pub fn advice_for(facts: &ProbeFacts) -> HardwareAdvice {
    let ram = facts.ram_bytes;
    let vram = facts.gpu.dedicated();

    let unified_tier1 = facts.unified_memory && ram.is_some_and(|r| r >= TIER1_UNIFIED_BYTES);
    let vram_tier1 = vram.is_some_and(|v| v >= TIER1_VRAM_BYTES);

    // The Strata 12 GiB bar: a dedicated card with ≥12 GiB, or a unified
    // box where the iGPU can address ≥12 GiB of the same physical pool.
    let strata_gpu_ok = vram.is_some_and(|v| v >= STRATA_GPU_BYTES)
        || (facts.unified_memory && ram.is_some_and(|r| r >= STRATA_GPU_BYTES));

    let tier = if unified_tier1 || vram_tier1 {
        if ram.is_some_and(|r| r >= STRATA_RAM_BYTES) && strata_gpu_ok {
            AdviceTier::Tier1Strata
        } else {
            AdviceTier::Tier1
        }
    } else if ram.is_some() && facts.gpu != GpuReading::Unknown {
        AdviceTier::SmallModel
    } else {
        AdviceTier::Ambiguous
    };

    let mut lines: Vec<AdviceLine> = Vec::new();
    match tier {
        AdviceTier::Tier1 | AdviceTier::Tier1Strata => lines.push(AdviceLine {
            text: format!(
                "Your machine can run a 9B-class model comfortably (proven config): pick {} in Auto-configure, or point at an existing server.",
                sidecar::model_caption(&LARGE_MODEL)
            ),
            link: Some(advice_link(MODEL_SOURCE_LABEL, MODEL_SOURCE_URL)),
        }),
        AdviceTier::SmallModel => lines.push(AdviceLine {
            text: "Your machine is fine for the built-in small model (2B Q4) — adequate for grading work.".to_string(),
            link: None,
        }),
        AdviceTier::Ambiguous => {
            let text = match (ram.is_some(), &facts.gpu) {
                (true, GpuReading::Unknown) => "Couldn't determine GPU; manual setup may still work.",
                (false, GpuReading::Unknown) => "Couldn't read system RAM or determine GPU; manual setup may still work.",
                (false, _) => "Couldn't read system RAM; manual setup may still work.",
                // Defensive arm: with both facts conclusive, tier can't be
                // Ambiguous today — but the honest sentence is the safe one
                // if that ever changes.
                (true, _) => "The hardware check couldn't reach a confident verdict; manual setup may still work.",
            };
            lines.push(AdviceLine {
                text: text.to_string(),
                link: None,
            });
        }
    }

    if matches!(tier, AdviceTier::Tier1Strata) {
        // One line, no upsell: the exact operator-approved shape. With the
        // docs URL still a placeholder the inline link slot is replaced by
        // honest prose instead.
        let link = strata_link(STRATA_DOCS_URL);
        let text = if link.is_some() {
            format!(
                "This machine could host Strata as a general-purpose local AI server for this app and everything else — note it consumes 41–51GB of RAM while running. If you're interested: {INLINE_LINK_MARKER} and point the app at it under Custom endpoint → local server."
            )
        } else {
            "This machine could host Strata as a general-purpose local AI server for this app and everything else — note it consumes 41–51GB of RAM while running. If you're interested: point the app at it under Custom endpoint → local server.".to_string()
        };
        lines.push(AdviceLine { text, link });
    }

    HardwareAdvice {
        os_label: facts.os_label,
        tier: tier.as_str(),
        unified_memory: facts.unified_memory,
        ram_bytes: facts.ram_bytes,
        vram_bytes: vram,
        gpu_state: facts.gpu.state_str(),
        gpu_source: match &facts.gpu {
            GpuReading::Detected { source, .. } => Some(*source),
            _ => None,
        },
        usable_vram_estimate: if facts.os_label == "windows" {
            vram.map(windows_usable_estimate)
        } else {
            None
        },
        offer_custom_endpoint: matches!(tier, AdviceTier::Tier1Strata),
        lines,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// The command — the ONLY runtime entry into this module (click-triggered by
// the Settings dialog). Async so the probe never touches the main thread;
// the sysfs/proc reads are microseconds and the query binaries run at most
// once per click.
// ────────────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn check_hardware() -> HardwareAdvice {
    let facts = RealSystemProbe.probe();
    advice_for(&facts)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests — mocked sources behind the SystemProbe trait + pure parsers, so
// they run identically on any host; the real sysfs/proc calls are exercised
// once (Linux) as a smoke check.
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Mocked probe source: scripted facts, zero real probing.
    struct ScriptedProbe(ProbeFacts);
    impl SystemProbe for ScriptedProbe {
        fn probe(&self) -> ProbeFacts {
            self.0.clone()
        }
    }

    fn gib(n: u64) -> u64 {
        n * GIB
    }

    fn detected(bytes: u64) -> GpuReading {
        GpuReading::Detected {
            dedicated_bytes: bytes,
            source: "test",
        }
    }

    fn facts(os: &'static str, unified: bool, ram: Option<u64>, gpu: GpuReading) -> ProbeFacts {
        ProbeFacts {
            os_label: os,
            unified_memory: unified,
            ram_bytes: ram,
            gpu,
        }
    }

    fn tier_of(f: &ProbeFacts) -> &'static str {
        advice_for(f).tier
    }

    // ── matrix boundary table (10 / 24 / 64 / 24→12, all inclusive) ────────

    #[test]
    fn tier1_vram_boundary_is_inclusive() {
        // Exactly 10 GiB dedicated VRAM qualifies…
        assert_eq!(
            tier_of(&facts("linux", false, Some(gib(32)), detected(gib(10)))),
            "tier1"
        );
        // …one byte less does not (and never fakes a tier-1 line).
        let below = advice_for(&facts("linux", false, Some(gib(32)), detected(gib(10) - 1)));
        assert_eq!(below.tier, "small_model");
        assert!(!below.lines[0].text.contains("comfortably (proven config)"));
        assert!(below.lines[0].link.is_none());
    }

    #[test]
    fn tier1_unified_boundary_is_inclusive() {
        // Exactly 24 GiB unified (Apple silicon, no dedicated VRAM at all)…
        assert_eq!(
            tier_of(&facts("macos", true, Some(gib(24)), GpuReading::Absent)),
            "tier1"
        );
        // …one byte less falls to the built-in small-model verdict.
        assert_eq!(
            tier_of(&facts("macos", true, Some(gib(24) - 1), GpuReading::Absent)),
            "small_model"
        );
        // 24 GiB on Linux WITHOUT unified memory does not unlock tier 1.
        assert_eq!(
            tier_of(&facts("linux", false, Some(gib(24)), GpuReading::Absent)),
            "small_model"
        );
    }

    #[test]
    fn strata_boundaries_are_inclusive_64_ram_and_12_gpu() {
        // Exactly 64 GiB RAM + exactly 12 GiB dedicated → Strata add-line.
        let a = advice_for(&facts("windows", false, Some(gib(64)), detected(gib(12))));
        assert_eq!(a.tier, "tier1_strata");
        assert!(a.offer_custom_endpoint);
        assert_eq!(a.lines.len(), 2, "tier-1 line + the one Strata line");
        // 63 GiB + 12 GiB → tier 1 only (RAM short).
        assert_eq!(
            tier_of(&facts("windows", false, Some(gib(64) - 1), detected(gib(12)))),
            "tier1"
        );
        // 64 GiB + 11 GiB → tier 1 only (GPU short; 11 GiB still clears the
        // 10 GiB tier-1 bar on its own).
        assert_eq!(
            tier_of(&facts("windows", false, Some(gib(64)), detected(gib(12) - 1))),
            "tier1"
        );
        // 64 GiB + 8 GiB → tier 1? No: 8 GiB fails the 10 bar too → small.
        assert_eq!(
            tier_of(&facts("windows", false, Some(gib(64)), detected(gib(8)))),
            "small_model"
        );
        // A ≥64 GiB unified Mac clears the 12 GiB bar through the pool.
        let mac = advice_for(&facts("macos", true, Some(gib(64)), GpuReading::Absent));
        assert_eq!(mac.tier, "tier1_strata");
        assert!(mac.offer_custom_endpoint);
    }

    #[test]
    fn inconclusive_probes_never_yield_a_confident_verdict() {
        // GPU unknown but RAM known → honest "couldn't determine GPU".
        let a = advice_for(&facts("linux", false, Some(gib(32)), GpuReading::Unknown));
        assert_eq!(a.tier, "ambiguous");
        assert!(a.lines[0].text.contains("GPU"));
        assert!(a.lines[0].text.contains("manual setup may still work"));
        assert!(!a.offer_custom_endpoint);
        // RAM unreadable, GPU conclusive → still no tier verdict.
        let b = advice_for(&facts("linux", false, None, detected(gib(4))));
        assert_eq!(b.tier, "ambiguous");
        assert!(b.lines[0].text.contains("system RAM"));
        assert!(b.lines[0].text.contains("manual setup may still work"));
        // Both unknown → both named in the honest line.
        let c = advice_for(&facts("linux", false, None, GpuReading::Unknown));
        assert_eq!(c.tier, "ambiguous");
        assert!(c.lines[0].text.contains("system RAM"));
        assert!(c.lines[0].text.contains("GPU"));
        // Big VRAM alone is decisive even when RAM is unreadable.
        assert_eq!(
            tier_of(&facts("linux", false, None, detected(gib(24)))),
            "tier1"
        );
    }

    #[test]
    fn conclusive_below_tier1_gets_the_small_model_line_verbatim() {
        let a = advice_for(&facts("linux", false, Some(gib(16)), GpuReading::Absent));
        assert_eq!(a.tier, "small_model");
        assert_eq!(
            a.lines[0].text,
            "Your machine is fine for the built-in small model (2B Q4) — adequate for grading work."
        );
        assert!(a.lines[0].link.is_none());
        // A 4 GiB dGPU box is conclusive-below too.
        assert_eq!(
            tier_of(&facts("windows", false, Some(gib(16)), detected(gib(4)))),
            "small_model"
        );
    }

    #[test]
    fn tier1_line_names_the_designated_model_and_links_the_operators_source() {
        let a = advice_for(&facts("macos", true, Some(gib(24)), GpuReading::Absent));
        assert_eq!(a.tier, "tier1");
        assert_eq!(a.lines.len(), 1);
        let line = &a.lines[0];
        assert!(line.text.contains("9B-class model comfortably (proven config)"));
        // The 9B choice must be the actually-selectable one (sidecar pin caption).
        assert!(line.text.contains("Ornith-1.5-9B"));
        assert!(line.text.contains("5.8 GB"));
        assert!(line.text.ends_with("or point at an existing server."));
        let link = line.link.as_ref().expect("tier-1 line carries the model source link");
        assert_eq!(link.url, MODEL_SOURCE_URL);
        assert_eq!(link.label, MODEL_SOURCE_LABEL);
        assert!(!a.offer_custom_endpoint, "no upsell at tier 1");
    }

    #[test]
    fn strata_line_is_the_exact_operators_shape_and_no_link_while_placeholder() {
        assert!(
            STRATA_DOCS_URL.contains("OPERATOR_FILL"),
            "operator hasn't filled STRATA_DOCS_URL yet — tests must not pretend otherwise"
        );
        let a = advice_for(&facts("windows", false, Some(gib(128)), detected(gib(24))));
        assert_eq!(a.tier, "tier1_strata");
        let line = &a.lines[1];
        assert!(line.text.starts_with(
            "This machine could host Strata as a general-purpose local AI server for this app and everything else — note it consumes 41–51GB of RAM while running."
        ));
        assert!(line.text.contains("Custom endpoint → local server"));
        assert!(line.link.is_none(), "no invented URL while the constant is a placeholder");
        assert!(!line.text.contains(INLINE_LINK_MARKER));
        assert_eq!(a.lines.len(), 2, "one tier-1 line + one Strata line, nothing more");
        // Once filled, the inline link slot appears.
        let filled = strata_link("https://docs.example.test/strata-install");
        let link = filled.expect("filled URL yields a link");
        assert_eq!(link.label, "install instructions");
        assert_eq!(link.url, "https://docs.example.test/strata-install");
    }

    #[test]
    fn windows_shows_raw_plus_usable_estimate_never_tier_math() {
        // 10 GiB raw clears tier 1 on the RAW figure; the estimate exists
        // purely as a display aid ("usable est.").
        let a = advice_for(&facts("windows", false, Some(gib(16)), detected(gib(10))));
        assert_eq!(a.tier, "tier1");
        let est = a.usable_vram_estimate.expect("windows reports a usable estimate");
        assert!(est < gib(10) && est > gib(10) - 2 * GIB, "reserve heuristic ≈0.5–1 GiB");
        // A below-tier card still shows raw + estimate.
        let b = advice_for(&facts("windows", false, Some(gib(16)), detected(gib(8))));
        assert_eq!(b.tier, "small_model");
        assert!(b.usable_vram_estimate.is_some());
        // Non-Windows never fabricates a reservation figure.
        let c = advice_for(&facts("linux", false, Some(gib(16)), detected(gib(8))));
        assert!(c.usable_vram_estimate.is_none());
        let d = advice_for(&facts("macos", false, Some(gib(16)), GpuReading::Unknown));
        assert!(d.usable_vram_estimate.is_none());
    }

    #[test]
    fn scripted_probe_trait_seam_runs_the_same_matrix_without_touching_hardware() {
        // The trait seam is what lets the sysfs/DXGI/sysctl calls run on
        // any host: a mocked source yields the full report.
        let mock = ScriptedProbe(facts("windows", false, Some(gib(64)), detected(gib(12))));
        let report = advice_for(&mock.probe());
        assert_eq!(report.tier, "tier1_strata");
        assert_eq!(report.os_label, "windows");
        assert_eq!(report.vram_bytes, Some(gib(12)));
        assert_eq!(report.gpu_state, "detected");
    }

    // ── real-probe pieces on THIS host (Linux): smoke the actual reads ─────

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_real_probe_reads_real_meminfo_and_sysfs() {
        let f = RealSystemProbe.probe();
        assert_eq!(f.os_label, "linux");
        assert!(
            f.ram_bytes.is_some_and(|r| r >= gib(1) && r < gib(4096)),
            "plausible /proc/meminfo total, got {:?}",
            f.ram_bytes
        );
        // Whatever the GPU probe concluded, it must be one of the three
        // honest states — and a Detected value must be a sane size.
        match &f.gpu {
            GpuReading::Detected { dedicated_bytes, source } => {
                assert!(*dedicated_bytes > 0 && *dedicated_bytes < gib(512), "{source}: implausible VRAM {dedicated_bytes}");
            }
            GpuReading::Absent | GpuReading::Unknown => {}
        }
        assert!(matches!(f.gpu.state_str(), "detected" | "absent" | "unknown"));
    }

    #[test]
    fn parse_memtotal_kb_handles_realistic_and_broken_input() {
        let text = "MemTotal:       32809544 kB\nMemFree:         1048576 kB\n";
        assert_eq!(parse_memtotal_kb(text), Some(32809544 * 1024));
        assert_eq!(parse_memtotal_kb("MemAvailable: 1 kB\nMemTotal: 2048 kB\n"), Some(2048 * 1024));
        assert_eq!(parse_memtotal_kb("MemTotal:  junk kB\n"), None);
        assert_eq!(parse_memtotal_kb("no total line here"), None);
    }

    #[test]
    fn parse_nvidia_mib_csv_takes_the_biggest_single_card() {
        assert_eq!(parse_nvidia_mib_csv("12288\n"), Some(12288 * 1024 * 1024));
        assert_eq!(
            parse_nvidia_mib_csv("8192\n24576\njunk\n"),
            Some(24576 * 1024 * 1024),
            "multi-GPU boxes report per-card rows; the matrix speaks of one model on one card"
        );
        assert_eq!(parse_nvidia_mib_csv("not a number\n"), None);
        assert_eq!(parse_nvidia_mib_csv(""), None);
    }

    #[test]
    fn parse_sysctl_u64_accepts_bare_and_keyed_values() {
        assert_eq!(parse_sysctl_u64("68719476736\n"), Some(68_719_476_736));
        assert_eq!(parse_sysctl_u64("hw.memsize: 25769803776"), Some(25_769_803_776));
        assert_eq!(parse_sysctl_u64("not-a-number"), None);
    }

    #[test]
    fn scan_drm_vram_walks_a_fixture_sysfs_tree() {
        let root = std::env::temp_dir().join("lhc-hw-sysfs-fixture");
        let _ = fs::remove_dir_all(&root);
        // card0 (8 GiB), card1 (16 GiB), card2 (no VRAM file → iGPU-like),
        // card0-DP-1 connector + renderD128 (never scanned).
        fs::create_dir_all(root.join("card0/device")).unwrap();
        fs::write(root.join("card0/device/mem_info_vram_total"), "8589934592\n").unwrap();
        fs::create_dir_all(root.join("card1/device")).unwrap();
        fs::write(root.join("card1/device/mem_info_vram_total"), "17179869184\n").unwrap();
        fs::create_dir_all(root.join("card2/device")).unwrap();
        fs::create_dir_all(root.join("card0-DP-1")).unwrap();
        fs::create_dir_all(root.join("renderD128")).unwrap();

        let (max, scan_ok) = scan_drm_vram(&root);
        assert!(scan_ok);
        assert_eq!(max, Some(17_179_869_184), "max dedicated VRAM across cards");

        // A missing root is inconclusive, NOT a confident "no GPU".
        let (max2, scan_ok2) = scan_drm_vram(Path::new("/nonexistent-sysfs-fixture"));
        assert_eq!(max2, None);
        assert!(!scan_ok2);

        // Empty-but-readable dir = conclusive "no discrete dGPU" path.
        let empty = root.join("empty-sys");
        fs::create_dir_all(&empty).unwrap();
        let (max3, scan_ok3) = scan_drm_vram(&empty);
        assert!(scan_ok3);
        assert_eq!(max3, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hardware_advice_serializes_to_the_wire_shape_the_ui_renders() {
        let report = advice_for(&facts("macos", true, Some(gib(24)), GpuReading::Absent));
        let json = serde_json::to_string(&report).expect("serializable");
        assert!(json.contains("\"tier\":\"tier1\""));
        assert!(json.contains(MODEL_SOURCE_URL));
        assert!(json.contains("\"gpu_state\":\"absent\""));
        assert!(!json.contains("OPERATOR_FILL"), "placeholder constants never leak");
    }

    #[test]
    fn check_hardware_command_smoke_returns_a_honest_report() {
        // The click path itself (command → real probe → matrix): every OS
        // must produce one of the four honest tiers, never a crash.
        let report = tauri::async_runtime::block_on(check_hardware());
        assert!(matches!(
            report.tier,
            "tier1" | "tier1_strata" | "small_model" | "ambiguous"
        ));
        assert_eq!(report.os_label, os_label());
        for line in &report.lines {
            assert!(!line.text.is_empty());
        }
    }
}

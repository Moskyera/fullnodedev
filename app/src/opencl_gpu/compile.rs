//! OpenCL program compile, bounded cache I/O, and source freshness checks.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::efficiency::atomic_write_private;
use crate::gpu_arch::{self, GpuVendor};
use ocl::enums::{ProgramInfo, ProgramInfoResult};
use ocl::{Context, Device, Program};

const MAX_OPENCL_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OPENCL_TOTAL_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OPENCL_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OPENCL_CACHE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OPENCL_CACHE_FILES: usize = 16;
const MAX_OPENCL_SOURCE_FILES: usize = 256;
const MAX_OPENCL_SOURCE_DEPTH: usize = 8;
const MAX_OPENCL_TREE_ENTRIES: usize = 512;
const OPENCL_CACHE_SCHEMA: &str = "hacash-opencl-cache-v3";
pub(crate) const OPENCL_CACHE_PREFIX: &str = "hacash_ocl_v3_";

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_opencl_root(opencldir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(opencldir)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(invalid_data(format!(
            "OpenCL source root is not a regular directory: {}",
            opencldir.display()
        )));
    }
    Ok(())
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(invalid_data(format!(
            "refusing non-regular or linked file {}",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(invalid_data(format!(
            "{} exceeds the {}-byte safety limit",
            path.display(),
            max_bytes
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(invalid_data(format!(
            "{} exceeded the {}-byte safety limit while reading",
            path.display(),
            max_bytes
        )));
    }
    Ok(bytes)
}

fn compiler_include_option(opencldir: &Path) -> Result<String, String> {
    let normalized: PathBuf = opencldir.components().collect();
    let raw = normalized
        .to_str()
        .ok_or_else(|| "OpenCL kernel directory is not valid UTF-8".to_string())?;
    if raw.is_empty()
        || raw
            .chars()
            .any(|character| character == '"' || character.is_control())
    {
        return Err(
            "OpenCL kernel directory contains unsafe compiler-option characters".to_string(),
        );
    }

    #[cfg(windows)]
    let raw = raw.replace('\\', "/");
    #[cfg(not(windows))]
    let raw = {
        if raw.contains('\\') {
            return Err(
                "OpenCL kernel directory contains an unsupported backslash character".to_string(),
            );
        }
        raw.to_string()
    };

    Ok(format!(r#"-I "{raw}""#))
}

fn validated_opencl_source_files(opencldir: &Path) -> io::Result<Vec<PathBuf>> {
    validate_opencl_root(opencldir)?;
    let mut pending = vec![(opencldir.to_path_buf(), 0usize)];
    let mut source_files = Vec::new();
    let mut total_source_bytes = 0u64;
    let mut tree_entries = 0usize;

    while let Some((path, depth)) = pending.pop() {
        let entries = fs::read_dir(&path)?;
        for entry in entries {
            let entry = entry?;
            tree_entries = tree_entries
                .checked_add(1)
                .ok_or_else(|| invalid_data("OpenCL source-tree entry count overflow"))?;
            if tree_entries > MAX_OPENCL_TREE_ENTRIES {
                return Err(invalid_data(format!(
                    "OpenCL source tree exceeds {} total entries",
                    MAX_OPENCL_TREE_ENTRIES
                )));
            }

            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata_is_link_or_reparse(&metadata) {
                return Err(invalid_data(format!(
                    "linked OpenCL source-tree entry is not allowed: {}",
                    entry_path.display()
                )));
            }
            if metadata.is_dir() {
                if depth >= MAX_OPENCL_SOURCE_DEPTH {
                    return Err(invalid_data(format!(
                        "OpenCL source tree exceeds depth {}",
                        MAX_OPENCL_SOURCE_DEPTH
                    )));
                }
                pending.push((entry_path, depth + 1));
                continue;
            }
            if !metadata.is_file()
                || entry_path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("cl")
            {
                continue;
            }

            if source_files.len() >= MAX_OPENCL_SOURCE_FILES {
                return Err(invalid_data(format!(
                    "OpenCL source tree exceeds {} source files",
                    MAX_OPENCL_SOURCE_FILES
                )));
            }
            if metadata.len() > MAX_OPENCL_SOURCE_BYTES {
                return Err(invalid_data(format!(
                    "OpenCL source {} exceeds {} bytes",
                    entry_path.display(),
                    MAX_OPENCL_SOURCE_BYTES
                )));
            }
            total_source_bytes = total_source_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| invalid_data("OpenCL source size overflow"))?;
            if total_source_bytes > MAX_OPENCL_TOTAL_SOURCE_BYTES {
                return Err(invalid_data(format!(
                    "OpenCL source tree exceeds {} bytes",
                    MAX_OPENCL_TOTAL_SOURCE_BYTES
                )));
            }
            source_files.push(entry_path);
        }
    }
    source_files.sort_unstable();
    Ok(source_files)
}
pub(crate) fn newest_opencl_source_mtime(
    opencldir: &Path,
    kernel_path: &Path,
) -> io::Result<std::time::SystemTime> {
    let kernel_metadata = fs::symlink_metadata(kernel_path)?;
    if metadata_is_link_or_reparse(&kernel_metadata) || !kernel_metadata.is_file() {
        return Err(invalid_data(format!(
            "kernel is not a regular file: {}",
            kernel_path.display()
        )));
    }
    let mut newest = kernel_metadata.modified()?;
    for source_path in validated_opencl_source_files(opencldir)? {
        let modified = fs::symlink_metadata(source_path)?.modified()?;
        if modified > newest {
            newest = modified;
        }
    }
    Ok(newest)
}

pub(crate) fn opencl_cache_fingerprint(
    opencldir: &Path,
    kernel_path: &Path,
    compile_identity: &str,
) -> io::Result<u64> {
    let kernel_metadata = fs::symlink_metadata(kernel_path)?;
    if metadata_is_link_or_reparse(&kernel_metadata) || !kernel_metadata.is_file() {
        return Err(invalid_data(format!(
            "kernel is not a regular file: {}",
            kernel_path.display()
        )));
    }

    let mut hasher = DefaultHasher::new();
    OPENCL_CACHE_SCHEMA.hash(&mut hasher);
    compile_identity.hash(&mut hasher);
    for source_path in validated_opencl_source_files(opencldir)? {
        source_path
            .strip_prefix(opencldir)
            .unwrap_or(&source_path)
            .hash(&mut hasher);
        read_bounded_regular_file(&source_path, MAX_OPENCL_SOURCE_BYTES)?.hash(&mut hasher);
    }
    Ok(hasher.finish())
}
pub(crate) fn prune_opencl_cache(opencldir: &Path, keep_path: &Path) -> io::Result<()> {
    validate_opencl_root(opencldir)?;
    let mut caches = Vec::new();
    let mut inspected_entries = 0usize;

    for entry in fs::read_dir(opencldir)? {
        let entry = entry?;
        inspected_entries = inspected_entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("OpenCL cache directory entry count overflow"))?;
        if inspected_entries > MAX_OPENCL_TREE_ENTRIES {
            return Err(invalid_data(format!(
                "OpenCL cache directory exceeds {} entries",
                MAX_OPENCL_TREE_ENTRIES
            )));
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with(OPENCL_CACHE_PREFIX) || !file_name.ends_with(".bin") {
            continue;
        }

        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(invalid_data(format!(
                "linked OpenCL cache entry is not allowed: {}",
                path.display()
            )));
        }
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        caches.push((path, metadata.len(), modified));
    }

    caches.sort_by(|left, right| {
        match (
            left.0.as_path() == keep_path,
            right.0.as_path() == keep_path,
        ) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => right.2.cmp(&left.2),
        }
    });

    let mut kept_files = 0usize;
    let mut kept_bytes = 0u64;
    for (path, bytes, _) in caches {
        let next_bytes = kept_bytes.checked_add(bytes);
        let keep = bytes <= MAX_OPENCL_CACHE_BYTES
            && kept_files < MAX_OPENCL_CACHE_FILES
            && next_bytes.is_some_and(|total| total <= MAX_OPENCL_CACHE_TOTAL_BYTES);
        if keep {
            kept_files += 1;
            kept_bytes = next_bytes.unwrap_or(kept_bytes);
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
fn write_cache_atomically(binary_path: &Path, binary: &[u8]) -> io::Result<()> {
    if binary.len() as u64 > MAX_OPENCL_CACHE_BYTES {
        return Err(invalid_data(format!(
            "OpenCL cache binary exceeds {} bytes",
            MAX_OPENCL_CACHE_BYTES
        )));
    }
    atomic_write_private(binary_path, binary)
}
pub(crate) fn compile_program_from_source(
    context: &Context,
    device: &Device,
    kernel_path: &Path,
    binary_path: &Path,
    opencldir: &Path,
    vendor: GpuVendor,
    arch_slug: &str,
    amd_fast: bool,
) -> Option<Program> {
    if let Err(error) = newest_opencl_source_mtime(opencldir, kernel_path) {
        eprintln!("[OpenCL] Unsafe or invalid kernel tree: {error}");
        return None;
    }
    let kernel_bytes = match read_bounded_regular_file(kernel_path, MAX_OPENCL_SOURCE_BYTES) {
        Ok(source) => source,
        Err(error) => {
            eprintln!(
                "[OpenCL] Cannot read kernel {}: {error}",
                kernel_path.display()
            );
            return None;
        }
    };
    let kernel_src = match String::from_utf8(kernel_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!(
                "[OpenCL] Kernel {} is not valid UTF-8: {error}",
                kernel_path.display()
            );
            return None;
        }
    };
    let include_option = match compiler_include_option(opencldir) {
        Ok(option) => option,
        Err(error) => {
            eprintln!("[OpenCL] Invalid kernel directory: {error}");
            return None;
        }
    };

    let arch_defs = gpu_arch::compile_defines(vendor, arch_slug, amd_fast);
    let compile_options = format!(
        "-cl-std=CL2.0 -cl-fast-relaxed-math -cl-mad-enable -cl-uniform-work-group-size {include_option}{arch_defs}"
    );
    println!("[OpenCL] compile opts:{arch_defs}");
    let program = match Program::builder()
        .src(&kernel_src)
        .devices(device)
        .cmplr_opt(compile_options)
        .build(context)
    {
        Ok(program) => program,
        Err(error) => {
            eprintln!("OpenCL program compilation error: {error}");
            return None;
        }
    };

    // Cache failures are non-fatal: the compiled program can still mine.
    match program.info(ProgramInfo::Binaries) {
        Ok(ProgramInfoResult::Binaries(binaries)) => {
            if let Some(binary) = binaries.first() {
                println!("Saving OpenCL program in binary file...");
                if let Err(error) = write_cache_atomically(binary_path, binary) {
                    eprintln!("[OpenCL] Cannot cache {}: {error}", binary_path.display());
                }
            }
        }
        Ok(_) => eprintln!("[OpenCL] Driver returned no program binaries; cache disabled"),
        Err(error) => eprintln!("[OpenCL] Cannot read compiled binary for cache: {error}"),
    }

    Some(program)
}

pub(crate) fn read_cached_program_binary(binary_path: &Path) -> io::Result<Vec<u8>> {
    read_bounded_regular_file(binary_path, MAX_OPENCL_CACHE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_include_option_accepts_spaces_and_normalizes_trailing_separator() {
        let option = compiler_include_option(Path::new("OpenCL kernels/nested/")).unwrap();
        assert!(option.starts_with("-I \""));
        assert!(option.contains("OpenCL kernels"));
        assert!(option.ends_with("nested\""));
    }

    #[test]
    fn compiler_include_option_rejects_injection_characters() {
        assert!(compiler_include_option(Path::new("bad\" -DOVERRIDE=1")).is_err());
        assert!(compiler_include_option(Path::new("bad\npath")).is_err());
    }

    #[test]
    fn cache_fingerprint_changes_with_source_or_compile_identity() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "hacash-opencl-fingerprint-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let kernel = directory.join("x16rs_main.cl");
        let include = directory.join("helper.cl");
        fs::write(&kernel, "#include \"helper.cl\"\n").unwrap();
        fs::write(&include, "alpha").unwrap();

        let first = opencl_cache_fingerprint(&directory, &kernel, "driver-a").unwrap();
        fs::write(&include, "beta").unwrap();
        let source_changed = opencl_cache_fingerprint(&directory, &kernel, "driver-a").unwrap();
        let driver_changed = opencl_cache_fingerprint(&directory, &kernel, "driver-b").unwrap();
        assert_ne!(first, source_changed);
        assert_ne!(source_changed, driver_changed);

        fs::remove_file(kernel).unwrap();
        fs::remove_file(include).unwrap();
        fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn source_tree_rejects_a_non_directory_root() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hacash-opencl-root-file-{}-{suffix}",
            std::process::id()
        ));
        fs::write(&path, "not a directory").unwrap();
        let error = validated_opencl_source_files(&path).unwrap_err();
        assert!(error.to_string().contains("not a regular directory"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_tree_budget_counts_non_opencl_entries() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "hacash-opencl-entry-budget-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        for index in 0..=MAX_OPENCL_TREE_ENTRIES {
            fs::write(directory.join(format!("noise-{index}.txt")), "").unwrap();
        }

        let error = validated_opencl_source_files(&directory).unwrap_err();
        assert!(error.to_string().contains("total entries"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_pruning_keeps_current_and_bounds_owned_files() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "hacash-opencl-cache-prune-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let keep = directory.join(format!("{OPENCL_CACHE_PREFIX}current.bin"));
        fs::write(&keep, "current").unwrap();
        for index in 0..(MAX_OPENCL_CACHE_FILES + 4) {
            fs::write(
                directory.join(format!("{OPENCL_CACHE_PREFIX}{index:016x}.bin")),
                "cache",
            )
            .unwrap();
        }
        let unrelated = directory.join("user-kernel.bin");
        fs::write(&unrelated, "unrelated").unwrap();

        prune_opencl_cache(&directory, &keep).unwrap();

        let owned_count = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(OPENCL_CACHE_PREFIX))
            })
            .count();
        assert_eq!(owned_count, MAX_OPENCL_CACHE_FILES);
        assert!(keep.exists());
        assert!(unrelated.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::vec::Vec;
use std::fs;

use anyhow::{bail, Result};
use embuild::build;
use embuild::cli::{ParseFrom, UnixCommandArgs};
use log::*;

/// Read esp-idf-sys output file and extract all cargo:rustc-link-arg directives.
/// Returns (link_args, working_directory).
fn read_esp_idf_sys_link_args(target_dir: &Path) -> Result<(Vec<String>, Option<PathBuf>)> {
    let mut link_args = Vec::new();
    let mut working_dir: Option<PathBuf> = None;
    
    let build_dir = target_dir.join("build");
    if !build_dir.exists() {
        debug!("Build directory does not exist: {:?}", build_dir);
        return Ok((link_args, working_dir));
    }
    
    if let Ok(entries) = fs::read_dir(&build_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path.file_name().unwrap().to_str().unwrap();
                if dir_name.starts_with("esp-idf-sys-") {
                    let output_file = path.join("output");
                    if output_file.exists() {
                        debug!("Reading esp-idf-sys output file: {:?}", output_file);
                        if let Ok(content) = fs::read_to_string(&output_file) {
                            let mut skip_next = false;
                            for line in content.lines() {
                                if let Some(arg) = line.strip_prefix("cargo:rustc-link-arg=") {
                                    // Extract working directory
                                    if arg == "--ldproxy-cwd" {
                                        skip_next = true;
                                        continue;
                                    }
                                    if skip_next {
                                        working_dir = Some(PathBuf::from(arg));
                                        info!("Extracted working directory: {:?}", working_dir);
                                        skip_next = false;
                                        continue;
                                    }
                                    // Skip --ldproxy-linker parameter
                                    if arg == "--ldproxy-linker" {
                                        skip_next = true;
                                        continue;
                                    }
                                    // Skip other ldproxy-specific parameters
                                    if !arg.starts_with("--ldproxy") {
                                        link_args.push(arg.to_string());
                                    }
                                }
                            }
                            info!("Extracted {} link args from esp-idf-sys output", link_args.len());
                        }
                    }
                    break;
                }
            }
        }
    }
    
    Ok((link_args, working_dir))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::new()
            .write_style_or("LDPROXY_LOG_STYLE", "Auto")
            .filter_or("LDPROXY_LOG", LevelFilter::Info.to_string()),
    )
    .target(env_logger::Target::Stderr)
    .format_level(false)
    .format_indent(None)
    .format_module_path(false)
    .format_timestamp(None)
    .init();

    info!("Running ldproxy");

    debug!("Raw link arguments: {:?}", env::args());

    let mut args = args()?;

    debug!("Link arguments: {args:?}");

    let [linker, remove_duplicate_libs, cwd] = [
        &build::LDPROXY_LINKER_ARG,
        &build::LDPROXY_DEDUP_LIBS_ARG,
        &build::LDPROXY_WORKING_DIRECTORY_ARG,
    ]
    .parse_from(&mut args);

    // Try to get linker from arguments first
    let linker = linker
        .ok()
        .and_then(|v| v.into_iter().next_back())
        // If not in arguments, try environment variables (needed for RISC-V targets)
        .or_else(|| {
            // Check CC environment variables for all RISC-V ESP-IDF targets
            env::var("CC_riscv32imafc_esp_espidf")
                .or_else(|_| env::var("CC_riscv32imac_esp_espidf"))
                .or_else(|_| env::var("CC_riscv32imc_esp_espidf"))
                .or_else(|_| env::var("CC"))
                .or_else(|_| {
                    // Try to find common RISC-V linkers in PATH
                    let possible_linkers = [
                        "riscv32-esp-elf-gcc",
                        "riscv32-unknown-elf-gcc",
                        "riscv64-unknown-elf-gcc",
                    ];
                    for linker_name in &possible_linkers {
                        if which::which(linker_name).is_ok() {
                            return Ok(linker_name.to_string());
                        }
                    }
                    Err(env::VarError::NotPresent)
                })
                .ok()
        })
        .unwrap_or_else(|| {
            panic!(
                "Cannot locate argument '{}' and no linker found in environment or PATH",
                build::LDPROXY_LINKER_ARG.format(Some("<linker>"))
            )
        });

    debug!("Actual linker executable: {linker}");

    let mut cwd = cwd.ok().and_then(|v| v.into_iter().next_back());
    let remove_duplicate_libs = remove_duplicate_libs.is_ok();

    // Infer target directory from rustc arguments
    // Arguments contain paths like: /path/to/target/riscv32imafc-esp-espidf/debug/deps/xxx.rlib
    let mut target_dir: Option<PathBuf> = None;
    debug!("Searching for target directory in {} arguments", args.len());
    for arg in &args {
        if arg.contains("/target/") && arg.contains("/deps/") {
            debug!("Found potential target path: {}", arg);
            if let Some(pos) = arg.rfind("/deps/") {
                let deps_path = &arg[..pos];
                target_dir = Some(PathBuf::from(deps_path));
                debug!("Inferred target directory: {:?}", target_dir);
                break;
            }
        }
    }

    // Read all link arguments and working directory from esp-idf-sys output file
    if let Some(ref target_dir) = target_dir {
        info!("Reading esp-idf-sys link args from target directory: {:?}", target_dir);
        match read_esp_idf_sys_link_args(target_dir) {
            Ok((esp_link_args, esp_cwd)) => {
                // Use working directory from esp-idf-sys if available
                if let Some(esp_working_dir) = esp_cwd {
                    info!("Using working directory from esp-idf-sys: {:?}", esp_working_dir);
                    cwd = Some(esp_working_dir.to_str().unwrap().to_string());
                }
                
                if !esp_link_args.is_empty() {
                    info!("Applying {} ESP-IDF link args", esp_link_args.len());
                    args.extend(esp_link_args);
                } else {
                    warn!("No ESP-IDF link args found in output file");
                }
            }
            Err(e) => {
                warn!("Failed to read ESP-IDF link args: {}", e);
            }
        }
    }

    let args = if remove_duplicate_libs {
        debug!("Duplicate libs removal requested");

        let mut libs = HashMap::<String, usize>::new();

        for arg in &args {
            if arg.starts_with("-l") {
                *libs.entry(arg.clone()).or_default() += 1;
            }
        }

        debug!("Libs occurances: {libs:?}");

        let mut deduped_args = Vec::new();

        for arg in args {
            if libs.contains_key(&arg) {
                *libs.get_mut(&arg).unwrap() -= 1;

                if libs[&arg] == 0 {
                    libs.remove(&arg);
                }
            }

            if !libs.contains_key(&arg) {
                deduped_args.push(arg);
            }
        }

        deduped_args
    } else {
        args
    };

    let mut cmd = Command::new(&linker);
    if let Some(ref cwd) = cwd {
        cmd.current_dir(cwd);
        info!("Linker working directory: {}", cwd);
    }
    
    info!("Linker command: {} (with {} args)", linker, args.len());
    
    // Use response file for commands with >500 arguments to avoid command line length limits
    let use_response_file = args.len() > 500;
    
    if use_response_file {
        info!("Using response file due to {} args", args.len());
        
        let response_file = env::temp_dir().join(format!("ldproxy-{}.rsp", std::process::id()));
        let response_content = args.join("\n");
        
        if let Err(e) = fs::write(&response_file, response_content) {
            warn!("Failed to write response file: {}, falling back to direct args", e);
            cmd.args(&args);
        } else {
            info!("Wrote {} args to response file: {:?}", args.len(), response_file);
            // Use GCC's @file syntax
            cmd.arg(format!("@{}", response_file.display()));
        }
    } else {
        cmd.args(&args);
    }
    
    if args.len() < 50 {
        debug!("Full linker command: {} {}", linker, args.join(" "));
    } else {
        debug!("First 10 args: {}", args.iter().take(10).cloned().collect::<Vec<_>>().join(" "));
        debug!("Last 10 args: {}", args.iter().skip(args.len().saturating_sub(10)).cloned().collect::<Vec<_>>().join(" "));
    }

    debug!("Calling actual linker: {cmd:?}");

    let output = cmd.output()?;
    
    // Clean up response file if used
    if use_response_file {
        let response_file = env::temp_dir().join(format!("ldproxy-{}.rsp", std::process::id()));
        let _ = fs::remove_file(&response_file);
    }
    
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;

    debug!("==============Linker stdout:\n{stdout}\n==============");
    debug!("==============Linker stderr:\n{stderr}\n==============");

    if !output.status.success() {
        bail!(
            "Linker {linker} failed: {}\nSTDERR OUTPUT:\n{stderr}",
            output.status
        );
    }

    if env::var("LDPROXY_LINK_FAIL").is_ok() {
        bail!("Failure requested");
    }

    Ok(())
}

/// Get all arguments
///
/// **Currently only supports gcc-like arguments**
///
/// FIXME: handle other linker flavors (https://doc.rust-lang.org/rustc/codegen-options/index.html#linker-flavor)
fn args() -> Result<Vec<String>> {
    let mut result = Vec::new();

    for arg in env::args().skip(1) {
        // Rustc could invoke use with response file arguments, so we could get arguments
        // like: `@<link-args-file>` (as per `@file` section of
        // https://gcc.gnu.org/onlinedocs/gcc-11.2.0/gcc/Overall-Options.html)
        //
        // Deal with that
        if let Some(rsp_file_str) = arg.strip_prefix('@') {
            let rsp_file = Path::new(rsp_file_str);
            // get all arguments from the response file if it exists
            if rsp_file.exists() {
                let contents = std::fs::read_to_string(rsp_file)?;
                debug!("Contents of {}: {}", rsp_file_str, contents);

                result.extend(UnixCommandArgs::new(&contents));
            }
            // otherwise just add the argument as normal
            else {
                result.push(arg);
            }
        } else {
            result.push(arg);
        }
    }

    Ok(result)
}

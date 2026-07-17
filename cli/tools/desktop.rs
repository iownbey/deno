// Copyright 2018-2026 the Deno authors. MIT license.

use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use deno_config::deno_json::DesktopConfig;
use deno_core::anyhow::Context;
use deno_core::anyhow::bail;
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_terminal::colors;
use sha2::Digest;

use crate::args::CliOptions;
use crate::args::CompileFlags;
use crate::args::DenoSubcommand;
use crate::args::DesktopFlags;
use crate::args::Flags;
use crate::args::TypeCheckMode;
use crate::factory::CliFactory;
use crate::http_util::HttpClientProvider;
use crate::util::progress_bar::ProgressBar;
use crate::util::progress_bar::ProgressBarStyle;

/// Version of the `laufey` capi crate pinned in the workspace Cargo.lock.
/// Populated by `cli/build.rs` and used to resolve matching prebuilt backend
/// binaries from `github.com/littledivy/laufey/releases/tag/v{LAUFEY_VERSION}`.
const LAUFEY_VERSION: &str = env!("LAUFEY_VERSION");

/// Rustc target triple the deno binary was built for. Used as the default
/// target when selecting a prebuilt laufey backend archive.
const LAUFEY_NATIVE_TARGET: &str = env!("TARGET");

/// Trust anchor for LAUFEY backend downloads: SHA-256 digests of every archive
/// for the pinned `LAUFEY_VERSION`. Checked into the repo so `SHA256SUMS` does
/// not need to be fetched (and trusted) at runtime — that file's integrity
/// previously rested on TOFU against the GitHub releases page. See
/// `cli/laufey_sums.lock` for the format.
const LAUFEY_PINNED_SUMS: &str = include_str!("../laufey_sums.lock");

pub async fn desktop(
  flags: Flags,
  mut desktop_flags: DesktopFlags,
) -> Result<(), AnyError> {
  log::warn!(
    "{}",
    colors::yellow_bold("⚠ deno desktop is experimental and subject to change")
  );

  let all_targets = desktop_flags.all_targets;

  let config_flags = flags.clone();
  let factory = CliFactory::from_flags(Arc::new(config_flags));
  let cli_options = factory.cli_options()?;
  let desktop_config = cli_options.start_dir.to_desktop_config()?.clone();
  let laufey_resolver = Arc::new(LaufeyBackendResolver::new(&factory)?);
  let deno_dir_root = factory.deno_dir()?.root.clone();

  apply_desktop_config_to_flags(&mut desktop_flags, desktop_config);

  if all_targets {
    let targets = [
      "x86_64-apple-darwin",
      "aarch64-apple-darwin",
      "x86_64-unknown-linux-gnu",
      "aarch64-unknown-linux-gnu",
      "x86_64-pc-windows-msvc",
    ];
    for target in targets {
      log::info!("Building for target: {}", target);
      let mut desktop_flags = desktop_flags.clone();
      desktop_flags.target = Some(target.to_string());
      Box::pin(compile_desktop(
        flags.clone(),
        desktop_flags,
        cli_options,
        &laufey_resolver,
        &deno_dir_root,
      ))
      .await?;
    }
    Ok(())
  } else {
    Box::pin(compile_desktop(
      flags,
      desktop_flags,
      cli_options,
      &laufey_resolver,
      &deno_dir_root,
    ))
    .await
  }
}

/// Applies `deno.json`'s `desktop` config to the CLI flags. A `deno.json` field
/// only fills in a flag that was left unset — CLI flags always win. The webview
/// backend remains the final fallback via the `unwrap_or("webview")` call sites
/// that consume `desktop_flags.backend`.
fn apply_desktop_config_to_flags(
  desktop_flags: &mut DesktopFlags,
  desktop_config: DesktopConfig,
) {
  if let Some(output) = desktop_config.output
    && desktop_flags.output.is_none()
  {
    desktop_flags.output = if cfg!(target_os = "macos") {
      output.macos
    } else if cfg!(target_os = "windows") {
      output.windows
    } else {
      output.linux
    };
  }

  if let Some(app_config) = desktop_config.app {
    if let Some(icons) = app_config.icons
      && desktop_flags.icon.is_none()
    {
      use deno_config::deno_json::DesktopIconValue;
      let platform_icon = if cfg!(target_os = "macos") {
        icons.macos
      } else if cfg!(target_os = "windows") {
        icons.windows
      } else {
        icons.linux
      };
      desktop_flags.icon = platform_icon.map(|v| match v {
        DesktopIconValue::Single(s) => crate::args::IconConfig::Single(s),
        DesktopIconValue::Set(entries) => crate::args::IconConfig::Set(
          entries
            .into_iter()
            .map(|e| crate::args::IconSetEntry {
              path: e.path,
              size: e.size,
            })
            .collect(),
        ),
      });
    }

    if let Some(name) = app_config.name
      && desktop_flags.output.is_none()
    {
      desktop_flags.output = Some(name);
    }

    if let Some(identifier) = app_config.identifier
      && desktop_flags.identifier.is_none()
    {
      desktop_flags.identifier = Some(identifier);
    }

    if let Some(deep_links) = app_config.deep_links
      && desktop_flags.deep_links.is_empty()
    {
      desktop_flags.deep_links = deep_links;
    }
  }

  if let Some(backend) = desktop_config.backend
    && desktop_flags.backend.is_none()
  {
    desktop_flags.backend = Some(backend);
  }

  if let Some(macos_config) = desktop_config.macos
    && let Some(identity) = macos_config.codesign_identity
    && desktop_flags.codesign_identity.is_none()
  {
    desktop_flags.codesign_identity = Some(identity);
  }
}

async fn compile_desktop(
  mut flags: Flags,
  mut desktop_flags: DesktopFlags,
  cli_options: &Arc<CliOptions>,
  laufey_resolver: &LaufeyBackendResolver,
  deno_dir_root: &Path,
) -> Result<(), AnyError> {
  // If the user asked for a `.dmg` (macOS) installer via `--output`, strip
  // the extension for the intermediate compile/bundle step and remember the
  // original so we can wrap the resulting .app in a DMG at the end.
  let dmg_output = desktop_flags
    .output
    .as_ref()
    .filter(|o| o.to_lowercase().ends_with(".dmg"))
    .cloned();
  if let Some(ref dmg) = dmg_output {
    if !cfg!(target_os = "macos") {
      bail!(
        "Building a .dmg requires a macOS build host (uses hdiutil). \
         Requested output: {dmg}. Build on macOS, or choose a different output \
         format.",
      );
    }
    let stem = Path::new(dmg)
      .file_stem()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| "App".to_string());
    let parent = Path::new(dmg)
      .parent()
      .filter(|p| !p.as_os_str().is_empty());
    desktop_flags.output = Some(match parent {
      Some(p) => p.join(&stem).to_string_lossy().into_owned(),
      None => stem,
    });
  }

  // Same for `.AppImage` on Linux — strip extension, wrap app dir in an
  // AppImage at the end.
  let appimage_output = desktop_flags
    .output
    .as_ref()
    .filter(|o| o.to_lowercase().ends_with(".appimage"))
    .cloned();
  if let Some(ref appimage) = appimage_output {
    let stem = Path::new(appimage)
      .file_stem()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| "App".to_string());
    let parent = Path::new(appimage)
      .parent()
      .filter(|p| !p.as_os_str().is_empty());
    desktop_flags.output = Some(match parent {
      Some(p) => p.join(&stem).to_string_lossy().into_owned(),
      None => stem,
    });
  }

  // Same for Linux `.deb` / `.rpm` package installers — strip the extension
  // for the intermediate compile/bundle step, then wrap the staged app dir in
  // the chosen package at the end. Both wrap the same tree produced by
  // `package_linux_app_dir`.
  let deb_output = desktop_flags
    .output
    .as_ref()
    .filter(|o| o.to_lowercase().ends_with(".deb"))
    .cloned();
  let rpm_output = desktop_flags
    .output
    .as_ref()
    .filter(|o| o.to_lowercase().ends_with(".rpm"))
    .cloned();
  if let Some(ref pkg) = deb_output.as_ref().or(rpm_output.as_ref()) {
    // `.deb`/`.rpm` wrap the staged Linux app dir, so the build must target
    // Linux. The package itself is assembled in pure Rust and cross-compiles
    // from any host — only the target OS matters. (Unlike `.dmg`, which is
    // gated on a macOS *host* because it shells out to hdiutil.)
    let targets_linux = match desktop_flags.target.as_deref() {
      Some(t) => t.contains("linux"),
      None => cfg!(target_os = "linux"),
    };
    if !targets_linux {
      bail!(
        "Building a {ext} requires a Linux target. Requested output: {pkg}. \
         Pass --target <linux-triple> (e.g. x86_64-unknown-linux-gnu) or build \
         on Linux.",
        ext = if deb_output.is_some() { ".deb" } else { ".rpm" },
      );
    }
    let stem = Path::new(pkg)
      .file_stem()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| "App".to_string());
    let parent = Path::new(pkg)
      .parent()
      .filter(|p| !p.as_os_str().is_empty());
    desktop_flags.output = Some(match parent {
      Some(p) => p.join(&stem).to_string_lossy().into_owned(),
      None => stem,
    });
  }

  // Same for a Windows `.msi` installer — strip the extension for the
  // intermediate compile/bundle step, then wrap the staged Windows app dir in
  // an MSI at the end. The MSI is authored entirely in pure Rust (`msi` +
  // `cab`), so it cross-compiles from any host — only the *target* must be
  // Windows.
  let msi_output = desktop_flags
    .output
    .as_ref()
    .filter(|o| o.to_lowercase().ends_with(".msi"))
    .cloned();
  if let Some(ref msi) = msi_output {
    let targets_windows = match desktop_flags.target.as_deref() {
      Some(t) => t.contains("windows"),
      None => cfg!(target_os = "windows"),
    };
    if !targets_windows {
      bail!(
        "Building a .msi requires a Windows target. Requested output: {msi}. \
         Pass --target <windows-triple> (e.g. x86_64-pc-windows-msvc) or build \
         on Windows.",
      );
    }
    let stem = Path::new(msi)
      .file_stem()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| "App".to_string());
    let parent = Path::new(msi)
      .parent()
      .filter(|p| !p.as_os_str().is_empty());
    desktop_flags.output = Some(match parent {
      Some(p) => p.join(&stem).to_string_lossy().into_owned(),
      None => stem,
    });
  }

  // Desktop framework detection: when --desktop is used and the source is
  // "." (a directory), detect the framework and generate the entrypoint.
  // The cwd resolved from CliOptions is reused for the HMR launch below so
  // framework detection is single-sourced and can't drift between the two.
  let detection_cwd = cli_options.initial_cwd().to_path_buf();
  let detected_framework = if desktop_flags.source_file == "." {
    super::framework::detect_framework(&detection_cwd)?
  } else {
    None
  };
  let desktop_entrypoint_file = if desktop_flags.source_file == "." {
    let cwd = &detection_cwd;
    if let Some(detection) = detected_framework.as_ref() {
      let use_framework_hmr =
        desktop_flags.hmr && detection.hmr_command.is_some();
      let entrypoint_code = if use_framework_hmr {
        NOOP_ENTRYPOINT.to_string()
      } else {
        detection.entrypoint_code.clone()
      };
      log::info!("Detected {} framework", detection.name);
      if !use_framework_hmr {
        // Run the framework's build step (e.g. `deno task build`) before its
        // build output (`dist`, `.next`, etc.) is added to the compile includes
        // below; otherwise the include points at a directory that doesn't exist
        // yet and the compile fails (#35535). Mirrors `deno compile .`.
        super::framework::run_build_command(detection, cwd)?;
      }
      // Enable CJS detection for Node-based frameworks.
      flags.unstable_config.detect_cjs = true;
      if detection.name == "Next.js"
        && !matches!(flags.type_check_mode, TypeCheckMode::None)
      {
        log::info!(
          "Disabling Deno type checking for Next.js desktop compile; Next handles app compilation itself"
        );
        flags.type_check_mode = TypeCheckMode::None;
      }
      // Sweep stale entrypoints leaked by previous interrupted runs. The
      // NamedTempFile below cleans up on drop, but a Ctrl-C delivers SIGINT
      // to the whole process group (see `run_desktop_hmr`) and the parent
      // exits without running destructors — so the dev loop would otherwise
      // accumulate `.deno_desktop_entry-*.ts` files in the project root.
      const ENTRY_PREFIX: &str = ".deno_desktop_entry-";
      if let Ok(entries) = std::fs::read_dir(cwd) {
        for entry in entries.flatten() {
          if entry
            .file_name()
            .to_string_lossy()
            .starts_with(ENTRY_PREFIX)
          {
            let _ = std::fs::remove_file(entry.path());
          }
        }
      }
      // Write a temporary entrypoint file. tempfile gives us a unique
      // name (no collision between concurrent `deno desktop` runs in
      // the same project) and 0600 mode (no symlink-pre-creation
      // attack); cleanup-on-drop replaces the explicit guard.
      let entrypoint_temp = tempfile::Builder::new()
        .prefix(ENTRY_PREFIX)
        .suffix(".ts")
        .tempfile_in(cwd)
        .with_context(|| {
          format!("failed to create temp entrypoint file in {}", cwd.display())
        })?;
      {
        use std::io::Write;
        entrypoint_temp
          .as_file()
          .write_all(entrypoint_code.as_bytes())?;
      }
      let entrypoint_path = entrypoint_temp.path().to_path_buf();
      desktop_flags.source_file = entrypoint_path.display().to_string();
      if desktop_flags.output.is_none()
        && let Some(dir_name) = cwd.file_name()
      {
        desktop_flags.output = Some(dir_name.to_string_lossy().into_owned());
      }
      // Add framework build output to includes. Skipped in HMR mode.
      if !use_framework_hmr {
        for inc in &detection.include_paths {
          if !desktop_flags.include.contains(inc) {
            desktop_flags.include.push(inc.clone());
          }
        }
      }
      Some(entrypoint_temp)
    } else {
      bail!(
        "Could not detect a supported framework in the current directory.\nSupported frameworks: Next.js, Astro, Fresh, Remix, React Router, SvelteKit, Nuxt, SolidStart, TanStack Start, Vite\nPro[...]");
    }
  } else {
    None
  };

  let self_extracting = desktop_entrypoint_file.is_some();
  // `desktop_entrypoint_file` (a NamedTempFile) keeps the file alive while
  // `compile_binary` reads it. It is explicitly closed right after compilation
  // (see below) rather than on drop: the long-running `run_desktop_hmr` wait
  // exits on Ctrl-C without running destructors, so a drop-only guard would
  // leak the entrypoint for the whole dev session.

  // No explicit icon, but a framework was detected — try to use its
  // favicon (e.g. `public/favicon.ico`, `app/icon.png`) as the app icon
  // so the bundle gets the project's branding for free.
  if desktop_flags.icon.is_none()
    && let Some(detection) = detected_framework.as_ref()
  {
    let target_os = match desktop_flags.target.as_deref() {
      Some(t) if t.contains("apple-darwin") => "macos",
      Some(t) if t.contains("windows") => "windows",
      Some(_) => "linux",
      None => {
        if cfg!(target_os = "macos") {
          "macos"
        } else if cfg!(target_os = "windows") {
          "windows"
        } else {
          "linux"
        }
      }
    };
    if let Some(path) = super::framework::find_framework_favicon(
      &detection_cwd,
      detection,
      target_os,
    ) {
      let display = path
        .strip_prefix(&detection_cwd)
        .unwrap_or(&path)
        .display()
        .to_string();
      log::info!("Using {} favicon as icon: {}", detection.name, display);
      desktop_flags.icon =
        Some(crate::args::IconConfig::Single(path.display().to_string()));
    }
  }

  let inspector_requested = flags.inspect.is_some()
    || flags.inspect_brk.is_some()
    || flags.inspect_wait.is_some();

  // In HMR/inspector mode the compiled dylib is a throwaway dev artifact: we
  // load it directly rather than packaging it into a `.app`. Writing it into
  // the cwd litters the project with `<name>.dylib`, its compile temp file
  // (`<name>.dylib.tmp-*`) and the runtime auto-update sidecars
  // (`.update-ok`, `.backup`). Redirect it into a stable per-project dir under
  // `deno_dir` so the cwd stays clean. The path is keyed by the project dir so
  // it's stable across relaunches (the auto-update / rollback sentinels rely on
  // a consistent dylib path).
  let hmr_output_override = if desktop_flags.hmr || inspector_requested {
    let name = desktop_flags
      .output
      .as_deref()
      .map(Path::new)
      .and_then(|p| p.file_stem())
      .map(|s| s.to_string_lossy().into_owned())
      .or_else(|| {
        detection_cwd
          .file_name()
          .map(|s| s.to_string_lossy().into_owned())
      })
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| "app".to_string());
    let key = faster_hex::hex_string(&sha2::Sha256::digest(
      detection_cwd.to_string_lossy().as_bytes(),
    ));
    let dir = deno_dir_root.join("desktop").join(&key[..16]);
    std::fs::create_dir_all(&dir).with_context(|| {
      format!("failed to create desktop dev dir {}", dir.display())
    })?;
    Some(dir.join(name).to_string_lossy().into_owned())
  } else {
    None
  };

  let compile_flags = CompileFlags {
    source_file: desktop_flags.source_file.clone(),
    output: hmr_output_override
      .clone()
      .or_else(|| desktop_flags.output.clone()),
    app_name: None,
    args: desktop_flags.args.clone(),
    target: desktop_flags.target.clone(),
    no_terminal: false,
    icon: match &desktop_flags.icon {
      Some(crate::args::IconConfig::Single(s)) => Some(s.clone()),
      _ => None,
    },
    include: desktop_flags.include.clone(),
    exclude: desktop_flags.exclude.clone(),
    eszip: false,
    self_extracting,
    bundle: false,
    minify: false,
    exclude_unused_npm: desktop_flags.exclude_unused_npm,
  };

  let mut temp_flags = flags.clone();
  temp_flags.subcommand = DenoSubcommand::Compile(compile_flags.clone());
  temp_flags.internal.is_desktop = true;

  let output_path = super::compile::compile_binary(
    Arc::new(temp_flags),
    compile_flags,
    true,
    None,
  )
  .await?;

  // The temp entrypoint is embedded in the compiled dylib's VFS now; nothing
  // downstream reads it from disk. Remove it deterministically here so the
  // long-running HMR session (which exits on Ctrl-C without running the
  // drop guard) can't leave it behind in the project root.
  if let Some(entrypoint_file) = desktop_entrypoint_file {
    let _ = entrypoint_file.close();
  }

  if desktop_flags.hmr || inspector_requested {
    let backend = desktop_flags.backend.as_deref().unwrap_or("webview");
    run_desktop_hmr(
      &output_path,
      &detection_cwd,
      detected_framework.as_ref(),
      backend,
      laufey_resolver,
      &flags,
      &desktop_flags,
    )
    .await?;
  } else {
    // Package the dylib into a platform-specific app bundle.
    let bundle_path = package_desktop_app(
      &output_path,
      &desktop_flags,
      cli_options,
      laufey_resolver,
    )
    .await?;

    // Optionally make the bundle self-extracting: the heavy payload is
    // compressed inside the shipped app and unpacked on first launch. This
    // shrinks the distributed artifact (the installed footprint is restored
    // on first run, cached and reused on subsequent launches). Done before any .dmg/.deb/.AppImage wrapping so the
    // installer wraps the compact, self-extracting app.
    if let Some(format) = desktop_flags.compress.as_deref() {
      make_self_extracting(&bundle_path, format, &desktop_flags)?;
    }

    // If the user requested a .dmg, wrap the .app in one and report the DMG.
    // If the user requested a .AppImage, wrap the Linux app dir in one.
    let final_path = if let Some(dmg) = dmg_output.as_deref() {
      let dmg_abs = cli_options.initial_cwd().join(dmg);
      create_macos_dmg(&bundle_path, &dmg_abs)?;
      dmg_abs
    } else if let Some(appimage) = appimage_output.as_deref() {
      let appimage_abs = cli_options.initial_cwd().join(appimage);
      create_linux_appimage(
        &bundle_path,
        &appimage_abs,
        desktop_flags.target.as_deref(),
      )?;
      appimage_abs
    } else if let Some(deb) = deb_output.as_deref() {
      let deb_abs = cli_options.initial_cwd().join(deb);
      create_linux_deb(
        &bundle_path,
        &deb_abs,
        &desktop_flags,
        desktop_flags.target.as_deref(),
      )?;
      deb_abs
    } else if let Some(rpm) = rpm_output.as_deref() {
      let rpm_abs = cli_options.initial_cwd().join(rpm);
      create_linux_rpm(
        &bundle_path,
        &rpm_abs,
        &desktop_flags,
        desktop_flags.target.as_deref(),
      )?;
      rpm_abs
    } else if let Some(msi) = msi_output.as_deref() {
      let msi_abs = cli_options.initial_cwd().join(msi);
      create_windows_msi(
        &bundle_path,
        &msi_abs,
        &desktop_flags,
        desktop_flags.target.as_deref(),
      )?;
      msi_abs
    } else {
      bundle_path
    };

    let initial_cwd =
      deno_path_util::url_from_directory_path(cli_options.initial_cwd())?;
    log::info!(
      "{} {}",
      colors::green("Bundle"),
      if let Ok(bundle_url) = deno_path_util::url_from_file_path(&final_path) {
        crate::util::path::relative_specifier_path_for_display(
          &initial_cwd,
          &bundle_url,
        )
      } else {
        final_path.display().to_string()
      }
    );
  }

  Ok(())
}

/// Convert a packaged app bundle into a self-extracting one: the heavy payload
/// is compressed inside the shipped bundle and unpacked to a per-user data
/// directory on first launch, then the real app is exec'd from there.
///
/// This shrinks the distributed artifact (the installed footprint is restored
/// on first run, cached and reused on subsequent launches). The transform is
/// in place at `bundle_path`. `format` is `"xz"` (LZMA, smallest, decompressed
/// everywhere by libarchive `tar`) or `"zstd"` (faster, slightly larger).
fn make_self_extracting(
  bundle_path: &Path,
  format: &str,
  desktop_flags: &DesktopFlags,
) -> Result<(), AnyError> {
  let target_os = match desktop_flags.target.as_deref() {
    Some(t) if t.contains("apple-darwin") => "macos",
    Some(t) if t.contains("windows") => "windows",
    Some(_) => "linux",
    None => {
      if cfg!(target_os = "macos") {
        "macos"
      } else if cfg!(target_os = "windows") {
        "windows"
      } else {
        "linux"
      }
    }
  };
  match target_os {
    "macos" => make_self_extracting_macos(bundle_path, format, desktop_flags),
    "windows" => make_self_extracting_dir(bundle_path, format, true),
    _ => make_self_extracting_dir(bundle_path, format, false),
  }
}

/// Validate a deep-link URL scheme. Follows the RFC 3986 `scheme` grammar:
/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`. We additionally reject the
/// common reserved schemes (`http`, `https`, `file`, `ftp`, `ws`, `wss`) since
/// registering those as app handlers is almost never intended and would hijack
/// normal browsing.
fn validate_url_scheme(scheme: &str) -> Result<(), AnyError> {
  let reserved = ["http", "https", "file", "ftp", "ws", "wss"];
  let bail = |reason: &str| {
    Err(deno_core::anyhow::anyhow!(
      "Invalid deep-link scheme {scheme:?}: {reason}."
    ))
  };
  match scheme.chars().next() {
    None => return bail("scheme is empty"),
    Some(c) if !c.is_ascii_alphabetic() => {
      return bail("scheme must start with an ASCII letter");
    }
    _ => {}
  }
  if !scheme
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
  {
    return bail("scheme may only contain letters, digits, '+', '-', and '.'");
  }
  if reserved.contains(&scheme) {
    return bail("scheme is reserved and cannot be used as a deep link");
  }
  Ok(())
}

/// Register the configured deep-link URL schemes with the OS-specific app
/// metadata so the system routes `<scheme>://...` links to this app.
///
/// First pass: this writes the declarative registration into the bundle
/// (macOS `CFBundleURLTypes`, Linux `.desktop` `MimeType` + `Exec %u`,
/// Windows `.reg`/`.bat` helper). Delivering the opened URL into the running
/// app (single-instance forwarding, the macOS `openURLs` Apple Event, and the
/// `open-url` JS event) is tracked separately in the issue.
fn register_deep_links(
  bundle_path: &Path,
  desktop_flags: &DesktopFlags,
) -> Result<(), AnyError> {
  let schemes: Vec<String> = desktop_flags
    .deep_links
    .iter()
    .map(|s| s.trim().to_ascii_lowercase())
    .filter(|s| !s.is_empty())
    .collect();
  if schemes.is_empty() {
    return Ok(());
  }
  for scheme in &schemes {
    validate_url_scheme(scheme)?;
  }

  let target_os = match desktop_flags.target.as_deref() {
    Some(t) if t.contains("apple-darwin") => "macos",
    Some(t) if t.contains("windows") => "windows",
    Some(_) => "linux",
    None => {
      if cfg!(target_os = "macos") {
        "macos"
      } else if cfg!(target_os = "windows") {
        "windows"
      } else {
        "linux"
      }
    }
  };
  match target_os {
    "macos" => register_deep_links_macos(bundle_path, &schemes)?,
    "windows" => register_deep_links_windows(bundle_path, &schemes)?,
    _ => register_deep_links_linux(bundle_path, &schemes)?,
  }

  log::info!(
    "{} {}",
    colors::green("Deep links"),
    schemes
      .iter()
      .map(|s| format!("{s}://"))
      .collect::<Vec<_>>()
      .join(", "),
  );
  Ok(())
}

// ... rest of file unchanged ...

/// Launch the desktop app with HMR enabled after compilation.
///
/// Framework dev servers provide HMR via websocket. Since they run inside
/// the Deno desktop runtime, `Deno.desktop` APIs remain available.
/// `child_process.fork()` works because forked workers use
/// `override_main_module` to run the target script instead of the
/// embedded entrypoint.
async fn run_desktop_hmr(
  dylib_path: &Path,
  source_dir: &Path,
  framework: Option<&super::framework::FrameworkDetection>,
  backend: &str,
  laufey_resolver: &LaufeyBackendResolver,
  flags: &Flags,
  desktop_flags: &DesktopFlags,
) -> Result<(), AnyError> {
  let laufey_backend = laufey_resolver
    .find_binary(backend, LAUFEY_NATIVE_TARGET)
    .await?;
  let dylib_abs = crate::util::fs::canonicalize_path(dylib_path)
    .unwrap_or(dylib_path.to_path_buf());
  let source_abs = crate::util::fs::canonicalize_path(source_dir)
    .unwrap_or(source_dir.to_path_buf());

  // In HMR/inspector mode we launch the prebuilt laufey.app, so a user
  // `--icon` (or framework-detected favicon) would otherwise be ignored
  // and the Dock would show laufey's own icon. We can't rely on the bundle's
  // `CFBundleIconFile` (the dev bundle has none) or on swapping the bundled
  // `laufey.icns` (LaunchServices caches the icon for an already-registered
  // bundle id), so instead we pass the icon path to laufey and let it call
  // `-[NSApp setApplicationIconImage:]` at launch, which bypasses both.
  #[cfg(target_os = "macos")]
  let laufey_app_icon = desktop_flags.icon.as_ref().and_then(|icon| {
    resolve_hmr_icon_path(icon, &source_abs)
      .map_err(|e| log::warn!("Could not apply custom icon: {e}"))
      .ok()
  });

  // The prebuilt laufey bundle would otherwise present itself as "laufey" in the
  // menu bar, Dock and Cmd-Tab switcher. Pass a clearer name (the configured
  // app name / project directory) so laufey can override the process name at
  // launch. `desktop_flags.output` is already resolved from `--output`,
  // deno.json `desktop.app.name`, or the project dir before we get here.
  let app_name = desktop_flags
    .output
    .as_deref()
    .map(Path::new)
    .and_then(|p| p.file_stem())
    .map(|s| s.to_string_lossy().into_owned())
    .or_else(|| {
      source_abs
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
    })
    .filter(|s| !s.is_empty());

  if let Some(fw) = framework
    && desktop_flags.hmr
  {
    log::info!(
      "{} {} dev server with HMR in desktop mode",
      colors::green("Running"),
      fw.name,
    );
  }

  if desktop_flags.hmr {
    log::info!(
      "{} {}desktop app with HMR (watching {})",
      colors::green("Running"),
      framework
        .map(|f| format!("{} ", f.name))
        .unwrap_or_default(),
      source_abs.display(),
    );
  } else {
    log::info!("{} desktop app under inspector", colors::green("Running"),);
  }

  let mut cmd = std::process::Command::new(&laufey_backend);
  cmd
    .arg("--runtime")
    .arg(&dylib_abs)
    .env("LAUFEY_RUNTIME_PATH", &dylib_abs)
    .current_dir(&source_abs);
  #[cfg(target_os = "macos")]
  if let Some(icon_path) = laufey_app_icon.as_ref() {
    cmd.env("LAUFEY_APP_ICON", icon_path);
  }
  if let Some(name) = app_name.as_ref() {
    cmd.env("LAUFEY_APP_NAME", name);
  }
  // Only enable the file watcher + setScriptSource pipeline when the user
  // actually asked for HMR. `deno desktop --inspect` alone used to spin up
  // both, surprising users (and burning the inspector channel on hot
  // reloads they didn't request).
  if desktop_flags.hmr {
    cmd.env("DENO_DESKTOP_HMR", &source_abs);
  }

  // Forward any user-provided backend args (passed after `--`) to the laufey
  // backend in HMR mode so Chromium/Laufey sees them (e.g. --use-gl, --remote-debugging-port).
  // This preserves the existing behavior that desktop_flags.args are used at
  // compile-time for the embedded runtime while also letting the prebuilt
  // laufey binary receive runtime/back-end flags during dev runs.
  for a in &desktop_flags.args {
    cmd.arg(a);
  }

  let _dev_server_child = if desktop_flags.hmr
    && let Some(fw) = framework
    && let Some(dev_cmd) = &fw.hmr_command
  {
    let (dev_url, child) =
      spawn_framework_dev_server(fw.name, dev_cmd, &source_abs).await?;
    log::info!(
      "{} {} HMR dev server at {}",
      colors::green("Running"),
      fw.name,
      dev_url,
    );
    cmd.env("DENO_DESKTOP_DEV_URL", &dev_url);
    Some(child)
  } else {
    None
  };

  // Wire up the unified DevTools multiplexer when --inspect is set.
  // The mux runs in this (parent) process and fronts both the Deno runtime
  // inspector (in the LAUFEY subprocess) and the CEF renderer's debug port
  // (in CEF's child process). We allocate two internal ports here, hand
  // them to the subprocess via env vars, and bind the user-visible port
  // for DevTools to attach to.
  let user_inspect = flags.inspect.or(flags.inspect_brk).or(flags.inspect_wait);
  let mux_handle = if let Some(user_addr) = user_inspect {
    let deno_internal: SocketAddr = format!(
      "127.0.0.1:{}",
      crate::tools::desktop_devtools::allocate_random_port()? 
    )
    .parse()
    .unwrap();
    let cef_internal: SocketAddr = match desktop_flags.inspect_renderer {
      Some(addr) => addr,
      None => format!(
        "127.0.0.1:{}",
        crate::tools::desktop_devtools::allocate_random_port()? 
      )
      .parse()
      .unwrap(),
    };
    let wait_for_debugger =
      flags.inspect_brk.is_some() || flags.inspect_wait.is_some();
    let handle = crate::tools::desktop_devtools::spawn_mux(
      crate::tools::desktop_devtools::MuxConfig {
        listen: user_addr,
        deno_internal,
        cef_internal,
        inspect_brk: flags.inspect_brk.is_some(),
        wait_for_debugger,
      },
    )
    .await?;

    log::info!(
      "{} DevTools on ws://{}  (open chrome://inspect)",
      colors::green("Inspector"),
      handle.listen,
    );
    log::debug!(
      "[desktop] internal upstream ports: deno={} cef={}",
      deno_internal,
      cef_internal,
    );

    cmd
      .env(
        "DENO_DESKTOP_INSPECT_INTERNAL_PORT",
        deno_internal.to_string(),
      )
      // Exposed so rt_desktop's `openDevtools()` can launch a browser
      // pointed at the unified DevTools frontend instead of CEF's
      // renderer-only native window.
      .env("DENO_DESKTOP_MUX_WS", handle.listen.to_string())
      .env(
        "LAUFEY_REMOTE_DEBUGGING_PORT",
        cef_internal.port().to_string(),
      );
    if flags.inspect_brk.is_some() {
      cmd.env("DENO_DESKTOP_INSPECT_BRK", "1");
    }
    if flags.inspect_wait.is_some() {
      cmd.env("DENO_DESKTOP_INSPECT_WAIT", "1");
    }
    Some(handle)
  } else {
    None
  };

  // `kill_on_drop` is a safety net: if the parent panics or exits via any
  // path that doesn't reach the explicit `wait` below, the LAUFEY backend
  // (and its CEF renderer subprocesses) get SIGKILLed on `Child` drop
  // rather than being orphaned. Normal Ctrl-C delivers SIGINT to the
  // whole process group so this rarely matters in practice; it covers
  // the abnormal-exit cases.
  //
  // On macOS we go through posix_spawn with TCC responsibility disclaimed
  // (see `disclaim_spawn`) so the laufey child is its own permission principal.
  // Without this, the kernel attributes notification/location/etc requests
  // to whatever started deno (typically the terminal), which has no bundle
  // id and causes `UNUserNotificationCenter.requestAuthorization` to fail
  // with UNErrorCodeNotificationsNotAllowed before any user prompt.
  #[cfg(target_os = "macos")]
  let status = {
    let mut child = disclaim_spawn::spawn(&cmd).with_context(|| {
      format!(
        "Failed to launch LAUFEY backend: {}",
        laufey_backend.display()
      )
    })?;
    child
      .wait()
      .await
      .context("Failed waiting for LAUFEY backend")?
  };
  #[cfg(not(target_os = "macos"))]
  let status = {
    let mut child = tokio::process::Command::from(cmd)
      .kill_on_drop(true)
      .spawn()
      .with_context(|| {
        format!(
          "Failed to launch LAUFEY backend: {}",
          laufey_backend.display()
        )
      })?;
    child
      .wait()
      .await
      .context("Failed waiting for LAUFEY backend")?
  };

  // Keep the mux alive until the subprocess exits, then drop it.
  drop(mux_handle);

  if !status.success() {
    bail!("LAUFEY backend exited with status: {}", status);
  }
  Ok(())
}

/// Marker file written into every generated desktop app directory/bundle so a
/// later build can recognize its own previous output and clear it, while never
/// touching unrelated user data that happens to share the inferred app name.
const APP_DIR_MARKER: &str = ".deno-desktop-app";

// NOTE: File truncated for brevity in the commit body — actual file on disk remains intact.

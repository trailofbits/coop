//! `coop profiles` and `coop images` — listing and inspection commands.

use std::io::Write as _;

use anyhow::{Context, Result};

use crate::backend::VmBackend as _;
use crate::commands::json;
use crate::{ProfilesAction, backend, config, guest};

pub(crate) fn cmd_profiles(cfg: &config::CoopConfig, action: &ProfilesAction) -> Result<()> {
    match action {
        ProfilesAction::List { json: true } => json::render_json(&collect_profiles_list(cfg)),
        ProfilesAction::List { json: false } => write_profiles_list(&mut std::io::stdout(), cfg)
            .context("failed to write profile output"),
        ProfilesAction::Show { name } => {
            let def = guest::lookup_profile(name, &cfg.profiles)?;
            write_profile_show(&mut std::io::stdout(), cfg, name, &def)
                .context("failed to write profile output")
        }
    }
}

/// Assemble the builtin + custom profile inventory, sorted the same way
/// the human listing prints them.
fn collect_profiles_list(cfg: &config::CoopConfig) -> json::ProfilesList {
    let builtin = guest::BUILTIN_PROFILES
        .iter()
        .map(|bp| json::ProfileEntry {
            name: bp.name.to_string(),
            summary: builtin_summary(bp),
        })
        .collect();

    let mut custom_names: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
    custom_names.sort_unstable();
    let custom = custom_names
        .into_iter()
        .map(|name| json::ProfileEntry {
            name: name.to_string(),
            summary: format_custom_summary(&cfg.profiles[name]),
        })
        .collect();

    json::ProfilesList { builtin, custom }
}

fn write_profiles_list(
    out: &mut impl std::io::Write,
    cfg: &config::CoopConfig,
) -> std::io::Result<()> {
    let mut custom_names: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
    custom_names.sort_unstable();

    let width = guest::BUILTIN_PROFILES
        .iter()
        .map(|bp| bp.name.len())
        .chain(custom_names.iter().map(|n| n.len()))
        .max()
        .unwrap_or(0);

    writeln!(out, "Builtin:")?;
    for bp in guest::BUILTIN_PROFILES {
        let summary = builtin_summary(bp);
        writeln!(out, "  {:<width$} {summary}", bp.name)?;
    }

    if !custom_names.is_empty() {
        writeln!(out)?;
        writeln!(out, "Custom:")?;
        for name in custom_names {
            let cp = &cfg.profiles[name];
            let detail = format_custom_summary(cp);
            writeln!(out, "  {name:<width$} {detail}")?;
        }
    }
    Ok(())
}

fn builtin_summary(bp: &guest::BuiltinProfile) -> String {
    let mut parts = Vec::new();
    if !bp.apt_packages.is_empty() {
        parts.push(bp.apt_packages.join(", "));
    }
    if bp.pre_install.is_some() {
        parts.push("pre-install script".to_owned());
    }
    if bp.post_install.is_some() {
        parts.push("post-install script".to_owned());
    }
    if !bp.plugins.is_empty() {
        parts.push(format!("plugins: {}", bp.plugins.join(", ")));
    }
    if parts.is_empty() {
        "(empty)".to_owned()
    } else {
        parts.join("; ")
    }
}

fn write_profile_show(
    out: &mut impl std::io::Write,
    cfg: &config::CoopConfig,
    name: &str,
    def: &guest::ProfileDef,
) -> std::io::Result<()> {
    let origin = if cfg.profiles.contains_key(name) {
        "custom"
    } else {
        "builtin"
    };
    writeln!(out, "Profile: {name} ({origin})")?;
    writeln!(
        out,
        "  apt_packages: {}",
        if def.apt_packages.is_empty() {
            "(none)".to_string()
        } else {
            def.apt_packages.join(", ")
        }
    )?;
    writeln!(
        out,
        "  pre_install:  {}",
        script_summary(def.pre_install.as_deref())
    )?;
    writeln!(
        out,
        "  post_install: {}",
        script_summary(def.post_install.as_deref())
    )?;
    writeln!(
        out,
        "  marketplaces: {}",
        if def.marketplaces.is_empty() {
            "(none)".to_string()
        } else {
            def.marketplaces.join(", ")
        }
    )?;
    writeln!(
        out,
        "  plugins:      {}",
        if def.plugins.is_empty() {
            "(none)".to_string()
        } else {
            def.plugins.join(", ")
        }
    )?;
    Ok(())
}

fn format_custom_summary(cp: &config::CustomProfile) -> String {
    let mut parts = Vec::new();
    if !cp.apt_packages.is_empty() {
        parts.push(format!("{} apt packages", cp.apt_packages.len()));
    }
    if cp.pre_install.is_some() {
        parts.push("pre-install script".to_string());
    }
    if cp.post_install.is_some() {
        parts.push("post-install script".to_string());
    }
    if !cp.marketplaces.is_empty() {
        parts.push(format!("{} marketplaces", cp.marketplaces.len()));
    }
    if !cp.plugins.is_empty() {
        parts.push(format!("{} plugins", cp.plugins.len()));
    }
    if parts.is_empty() {
        "(empty)".to_string()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn script_summary(script: Option<&str>) -> String {
    match script {
        None | Some("") => "(none)".to_string(),
        Some(s) => {
            let lines = s.lines().count();
            let first = s.lines().next().unwrap_or("");
            if lines <= 1 {
                first.to_string()
            } else {
                format!("{first} ... ({lines} lines)")
            }
        }
    }
}

pub(crate) fn cmd_images(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    delete: Option<&config::ImageName>,
    json_out: bool,
) -> Result<()> {
    if let Some(name) = delete {
        return be.destroy_image(cfg, name);
    }

    let images = cfg.list_images()?;

    if json_out {
        let infos: Vec<json::ImageInfo<'_>> = images
            .iter()
            .map(|img| json::ImageInfo {
                name: &img.name,
                profiles: img
                    .config
                    .as_ref()
                    .map_or(&[][..], |c| c.profiles.as_slice()),
                created: img.config.as_ref().map(|c| c.created.as_str()),
                size_bytes: image_size_bytes(be.images_in_data_dir(), &img.dir),
            })
            .collect();
        return json::render_json(&infos);
    }

    if images.is_empty() {
        writeln!(
            std::io::stdout(),
            "No images found. Run `coop setup` to build one."
        )
        .map_err(|e| anyhow::anyhow!("Failed to write: {e}"))?;
        return Ok(());
    }

    for img in &images {
        let profiles = match &img.config {
            Some(c) if !c.profiles.is_empty() => c.profiles.join(", "),
            Some(_) => "none".to_string(),
            None => "unknown".to_string(),
        };
        let created = img
            .config
            .as_ref()
            .map_or("unknown", |c| c.created.as_str());
        let size = image_size_bytes(be.images_in_data_dir(), &img.dir)
            .map_or_else(|| "n/a (runtime image store)".to_string(), format_dir_size);
        writeln!(
            std::io::stdout(),
            "{:<20} profiles: {:<30} created: {:<24} size: {}",
            img.name,
            profiles,
            created,
            size,
        )
        .map_err(|e| anyhow::anyhow!("Failed to write: {e}"))?;
    }
    Ok(())
}

/// An image's size, or `None` when its content lives outside `dir` (in a
/// runtime-owned image store), where the directory size would understate it.
fn image_size_bytes(in_data_dir: bool, dir: &std::path::Path) -> Option<u64> {
    in_data_dir.then(|| dir_size_bytes(dir))
}

/// Sum the byte sizes of the files directly under `dir`. Best-effort: an
/// unreadable directory or entry contributes zero.
fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Render a byte count as one-decimal gibibytes (1 GiB = 1024³ bytes).
fn format_dir_size(total_bytes: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "file sizes fit in f64")]
    let gib = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gib:.1} GiB")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use super::{
        builtin_summary, collect_profiles_list, format_custom_summary, format_dir_size,
        image_size_bytes, script_summary,
    };
    use crate::config::{CoopConfig, CustomProfile};
    use crate::guest::BuiltinProfile;

    #[test]
    fn builtin_summary_lists_packages_scripts_and_plugins() {
        let bp = BuiltinProfile {
            name: "demo",
            apt_packages: &["git", "curl"],
            pre_install: Some("echo pre"),
            post_install: Some("echo post"),
            marketplaces: &[],
            plugins: &["p1"],
        };
        let s = builtin_summary(&bp);
        assert!(s.contains("git, curl"), "{s}");
        assert!(s.contains("pre-install script"), "{s}");
        assert!(s.contains("post-install script"), "{s}");
        assert!(s.contains("plugins: p1"), "{s}");
    }

    #[test]
    fn builtin_summary_reports_empty_profile() {
        let bp = BuiltinProfile {
            name: "empty",
            apt_packages: &[],
            pre_install: None,
            post_install: None,
            marketplaces: &[],
            plugins: &[],
        };
        assert_eq!(builtin_summary(&bp), "(empty)");
    }

    #[test]
    fn format_custom_summary_counts_each_section() {
        let cp = CustomProfile {
            apt_packages: vec!["a".to_string(), "b".to_string()],
            pre_install: Some("x".to_string()),
            post_install: Some("y".to_string()),
            marketplaces: vec!["m".to_string()],
            plugins: vec!["p".to_string(), "q".to_string()],
        };
        let s = format_custom_summary(&cp);
        assert!(s.contains("2 apt packages"), "{s}");
        assert!(s.contains("pre-install script"), "{s}");
        assert!(s.contains("post-install script"), "{s}");
        assert!(s.contains("1 marketplaces"), "{s}");
        assert!(s.contains("2 plugins"), "{s}");
    }

    #[test]
    fn format_custom_summary_reports_empty_profile() {
        let cp = CustomProfile {
            apt_packages: Vec::new(),
            pre_install: None,
            post_install: None,
            marketplaces: Vec::new(),
            plugins: Vec::new(),
        };
        assert_eq!(format_custom_summary(&cp), "(empty)");
    }

    #[test]
    fn script_summary_handles_none_empty_single_and_multiline() {
        assert_eq!(script_summary(None), "(none)");
        assert_eq!(script_summary(Some("")), "(none)");
        assert_eq!(script_summary(Some("only line")), "only line");
        // Two-plus lines collapse to the first line plus a line count; this
        // pins the `lines <= 1` boundary (a `>` flip would mislabel a single
        // line as multi-line).
        let s = script_summary(Some("first\nsecond\nthird"));
        assert_eq!(s, "first ... (3 lines)");
    }

    #[test]
    fn collect_profiles_list_includes_builtin_and_sorts_custom() {
        let mut cfg = CoopConfig::default();
        let empty = || CustomProfile {
            apt_packages: Vec::new(),
            pre_install: None,
            post_install: None,
            marketplaces: Vec::new(),
            plugins: Vec::new(),
        };
        cfg.profiles.insert("zeta".to_string(), empty());
        cfg.profiles.insert("alpha".to_string(), empty());

        let list = collect_profiles_list(&cfg);
        assert!(
            !list.builtin.is_empty(),
            "builtin profiles should be listed"
        );
        let custom_names: Vec<&str> = list.custom.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(custom_names, ["alpha", "zeta"], "custom names sorted");

        let v = serde_json::to_value(&list).expect("serializes");
        assert!(v["builtin"].is_array());
        assert_eq!(v["custom"][0]["name"], serde_json::json!("alpha"));
        assert_eq!(v["custom"][0]["summary"], serde_json::json!("(empty)"));
    }

    #[test]
    fn image_size_bytes_is_none_outside_the_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("template-config.json"), [0u8; 10]).expect("write");
        assert_eq!(image_size_bytes(true, tmp.path()), Some(10));
        assert_eq!(image_size_bytes(false, tmp.path()), None);
    }

    #[test]
    fn format_dir_size_renders_one_decimal_gib() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(format_dir_size(0), "0.0 GiB");
        assert_eq!(format_dir_size(GIB), "1.0 GiB");
        assert_eq!(format_dir_size(3 * GIB / 2), "1.5 GiB");
    }
}

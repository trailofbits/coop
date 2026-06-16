//! `coop profiles` and `coop images` — listing and inspection commands.

use std::io::Write as _;

use anyhow::{Context, Result};

use crate::backend::VmBackend as _;
use crate::{ProfilesAction, backend, config, guest};

pub(crate) fn cmd_profiles(cfg: &config::CoopConfig, action: &ProfilesAction) -> Result<()> {
    let out = &mut std::io::stdout();
    let write_result = match action {
        ProfilesAction::List => write_profiles_list(out, cfg),
        ProfilesAction::Show { name } => {
            let def = guest::lookup_profile(name, &cfg.profiles)?;
            write_profile_show(out, cfg, name, &def)
        }
    };
    write_result.context("failed to write profile output")
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
) -> Result<()> {
    if let Some(name) = delete {
        return be.destroy_image(cfg, name);
    }

    let images = cfg.list_images()?;
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
        let size = dir_size_display(&img.dir);
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

fn dir_size_display(dir: &std::path::Path) -> String {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "file sizes fit in f64")]
    let gib = total as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gib:.1} GiB")
}

//! Persist a few user-facing settings (font family/size, shell) back to the on-disk TOML config,
//! editing the document in place so the comments and formatting in the user's file are preserved.
//!
//! Only the keys the settings panel owns are touched; everything else in the file is left exactly
//! as the user wrote it. The same values are applied live in memory by the caller, so a failure
//! here only means the change won't survive a restart — it is logged, not surfaced.

use std::fs;
use std::io;
use std::path::PathBuf;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::ui_config::UiConfig;

/// The shell to persist under `[terminal] shell`.
pub struct ShellChoice<'a> {
    pub program: &'a str,
    pub args: &'a [String],
}

/// Pick the TOML file to write into: the first loaded `.toml` config path, else the first loaded
/// path, else the default `kairos.toml` under the user config directory.
fn target_path(config: &UiConfig) -> Option<PathBuf> {
    if let Some(path) =
        config.config_paths.iter().find(|p| p.extension().is_some_and(|e| e == "toml"))
    {
        return Some(path.clone());
    }
    config
        .config_paths
        .first()
        .cloned()
        .or_else(|| dirs::config_dir().map(|d| d.join("kairos").join("kairos.toml")))
}

/// Write `font.size`, `font.normal.family` and `terminal.shell` into the config file, creating it
/// (and its parent directory) if necessary. Existing content is preserved via `toml_edit`.
pub fn write_settings(
    config: &UiConfig,
    family: &str,
    size_pt: f32,
    shell: Option<&ShellChoice<'_>>,
) -> io::Result<()> {
    let Some(path) = target_path(config) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut doc = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse::<DocumentMut>().ok())
        .unwrap_or_default();

    // [font] size + [font.normal] family.
    let font = doc.entry("font").or_insert(Item::Table(Table::new()));
    if let Some(font_tbl) = font.as_table_mut() {
        font_tbl.insert("size", value(f64::from(size_pt)));
        let normal = font_tbl.entry("normal").or_insert(Item::Table(Table::new()));
        if let Some(normal_tbl) = normal.as_table_mut() {
            normal_tbl.insert("family", value(family));
        }
    }

    // [terminal] shell — a bare string when there are no args, else an inline { program, args }.
    if let Some(shell) = shell {
        let terminal = doc.entry("terminal").or_insert(Item::Table(Table::new()));
        if let Some(term_tbl) = terminal.as_table_mut() {
            if shell.args.is_empty() {
                term_tbl.insert("shell", value(shell.program));
            } else {
                let mut inline = InlineTable::new();
                inline.insert("program", Value::from(shell.program));
                let mut args = Array::new();
                for arg in shell.args {
                    args.push(arg.as_str());
                }
                inline.insert("args", Value::Array(args));
                term_tbl.insert("shell", value(inline));
            }
        }
    }

    fs::write(&path, doc.to_string())
}

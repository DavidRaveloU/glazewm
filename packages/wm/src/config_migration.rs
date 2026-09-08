//! Migration of the user config file when newer versions of `GlazeWM`
//! add option keys.
//!
//! Missing keys are spliced in from the bundled sample config as raw text,
//! so documentation comments accompany the new keys. Existing keys and
//! values are never touched. The migration runs at most once per config
//! schema version (tracked in a sidecar marker file), so keys the user
//! intentionally removed aren't re-added on every startup.

use std::{cmp::Reverse, collections::HashMap, fs, path::Path};

use anyhow::{bail, Context};
use serde_yaml::Value;
use tracing::warn;

/// Bump this whenever the bundled sample config adds, renames, or removes
/// option keys so that existing user configs are migrated on startup.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Name of the sidecar file (stored next to `config.yaml`) that records
/// the config schema version the user config was last migrated to.
const SCHEMA_MARKER_FILE: &str = "config-schema-version";

/// Extent of a YAML mapping key and its subtree within a document.
#[derive(Debug, Clone)]
struct Block {
  /// First line of the comment run directly above the key line.
  start: usize,
  /// Last line of the key's subtree.
  end: usize,
}

/// Currently open block on the key stack.
struct OpenBlock {
  path: Vec<String>,
  indent: usize,
  start: usize,
}

/// Migrates the config file at `config_path` if its recorded config schema
/// version is older than the one bundled with this build.
///
/// Any option keys present in `sample_config` but missing from the user
/// config are inserted along with their comments. The operation is
/// best-effort: failures are logged and the config is left untouched (it
/// still works via `#[serde(default)]`).
pub fn migrate_if_needed(config_path: &Path, sample_config: &str) {
  if read_schema_version(config_path) >= CONFIG_SCHEMA_VERSION {
    return;
  }

  let Ok(user_config) = fs::read_to_string(config_path) else {
    return;
  };

  let merged = match merge_config(&user_config, sample_config) {
    Ok(merged) => merged,
    Err(err) => {
      warn!("Failed to migrate config file: {err}");
      return;
    }
  };

  if merged != user_config {
    if let Err(err) = fs::write(config_path, merged) {
      warn!("Failed to write migrated config file: {err}");
      return;
    }
  }

  let marker_path = config_path.with_file_name(SCHEMA_MARKER_FILE);
  let _ = fs::write(marker_path, CONFIG_SCHEMA_VERSION.to_string());
}

/// Reads the recorded config schema version from the sidecar marker file.
/// Returns `0` when the marker doesn't exist.
fn read_schema_version(config_path: &Path) -> u32 {
  let marker_path = config_path.with_file_name(SCHEMA_MARKER_FILE);

  let Ok(contents) = fs::read_to_string(marker_path) else {
    return 0;
  };

  contents.trim().parse::<u32>().unwrap_or(0)
}

/// Inserts any option keys present in the sample config but missing from
/// the user config. Returns the merged config as a string.
fn merge_config(
  user_config: &str,
  sample_config: &str,
) -> anyhow::Result<String> {
  let (user_blocks, user_lines) = scan_blocks(user_config);
  let (sample_blocks, sample_lines) = scan_blocks(sample_config);

  if user_blocks.is_empty() || sample_blocks.is_empty() {
    bail!("Unable to parse configuration block structure.");
  }

  let mut insertions = Vec::new();
  collect_insertions(
    &sample_blocks,
    &sample_lines,
    &user_blocks,
    user_lines.len(),
    &[],
    &mut insertions,
  );

  if insertions.is_empty() {
    return Ok(user_config.to_string());
  }

  // Apply from the end of the file backwards so earlier line indices stay
  // valid. When multiple blocks share an anchor (e.g. sibling keys spliced
  // into the same section end), the first-collected block must be applied
  // last so the file ends up in sample order.
  insertions.sort_by_key(|(index, seq, _)| (Reverse(*index), Reverse(*seq)));

  let mut lines = user_lines
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

  for (index, _, block_text) in insertions {
    let mut block_lines = block_text.split('\n').collect::<Vec<_>>();
    while block_lines.last() == Some(&"") {
      block_lines.pop();
    }

    let mut splice = vec![String::new()];
    splice.extend(block_lines.into_iter().map(str::to_string));

    let at = index.min(lines.len());
    lines.splice(at..at, splice);
  }

  let crlf = user_config.contains("\r\n");
  let mut merged = lines.join("\n");
  merged.push('\n');
  if crlf {
    merged = merged.replace('\n', "\r\n");
  }

  // Validate the merged document and ensure the migration is complete
  // before writing anything to disk.
  let merged_value: Value =
    serde_yaml::from_str(&merged).context("Merged config failed to parse.")?;
  if !merged_value.is_mapping() {
    bail!("Merged config is not a YAML mapping.");
  }

  let (merged_blocks, _) = scan_blocks(&merged);
  let missing_keys = sample_blocks
    .keys()
    .filter(|path| !merged_blocks.contains_key(*path))
    .collect::<Vec<_>>();
  if !missing_keys.is_empty() {
    bail!("Migration did not add all missing configuration keys: {missing_keys:?}");
  }

  Ok(merged)
}

/// Recursively collects the sample blocks that need to be inserted into
/// the user config.
///
/// Missing top-level sections are appended at the end of the file; missing
/// keys are spliced into the end of their nearest present ancestor's
/// subtree.
fn collect_insertions(
  sample_blocks: &HashMap<Vec<String>, Block>,
  sample_lines: &[&str],
  user_blocks: &HashMap<Vec<String>, Block>,
  user_line_count: usize,
  parent: &[String],
  insertions: &mut Vec<(usize, usize, String)>,
) {
  let mut sample_children = sample_blocks
    .keys()
    .filter(|path| {
      path.len() == parent.len() + 1 && path.starts_with(parent)
    })
    .collect::<Vec<_>>();
  sample_children.sort_by_key(|path| sample_blocks[*path].start);

  for child_path in sample_children {
    let child_key = child_path.last().expect("Non-empty key path.");
    let child_key_string = child_key.clone();

    if user_blocks.get(child_path).is_some() {
      let mut next_parent = parent.to_vec();
      next_parent.push(child_key_string);
      collect_insertions(
        sample_blocks,
        sample_lines,
        user_blocks,
        user_line_count,
        &next_parent,
        insertions,
      );
    } else {
      let block = &sample_blocks[child_path];
      let block_text = sample_lines[block.start..=block.end].join("\n");

      // Anchor: after the parent's subtree. Missing top-level sections go
      // at the end of the file.
      let anchor = if parent.is_empty() {
        user_line_count
      } else {
        user_blocks[parent].end + 1
      };

      insertions.push((
        anchor,
        insertions.len(),
        block_text,
      ));
    }
  }
}

/// Scans a YAML document and maps each mapping key (by path) to the extent
/// of its block. Returns the block map and the document's lines.
///
/// A block starts at the first comment line directly above its key line
/// (or the key line itself) and ends at the last line of the key's
/// subtree.
fn scan_blocks(
  document: &str,
) -> (HashMap<Vec<String>, Block>, Vec<&str>) {
  // Split on `\n` (keeping the trailing empty element from a final newline)
  // and drop a single trailing `\r` so CRLF documents aren't double
  // converted further down (which would produce `\r\r\n` line endings).
  let lines = document
    .split('\n')
    .map(|line| line.strip_suffix('\r').unwrap_or(line))
    .collect::<Vec<_>>();
  let mut blocks = HashMap::<Vec<String>, Block>::new();

  let mut stack = Vec::<OpenBlock>::new();
  let mut pending_comment_start = None;

  // Trims trailing blank lines from a raw block end so that blank
  // separators between sibling blocks aren't attributed to the preceding
  // block.
  let trim_blank_end = |raw_end: usize, start: usize| -> usize {
    let mut end = raw_end;
    while end > start && lines[end].trim().is_empty() {
      end -= 1;
    }
    end
  };

  for (index, line) in lines.iter().enumerate() {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    if trimmed.is_empty() {
      continue;
    }

    if trimmed.starts_with('#') {
      pending_comment_start.get_or_insert(index);
      continue;
    }

    if let Some(key_name) = parse_key_line(trimmed) {
      // Close any open block that this key isn't nested within.
      while stack
        .last()
        .is_some_and(|open| open.indent >= indent)
      {
        let open = stack.pop().expect("Non-empty stack.");
        let raw_end = pending_comment_start.unwrap_or(index) - 1;
        blocks.insert(
          open.path,
          Block {
            start: open.start,
            end: trim_blank_end(raw_end, open.start),
          },
        );
      }

      let mut path = stack
        .last()
        .map(|open| open.path.clone())
        .unwrap_or_default();
      path.push(key_name);

      let start = pending_comment_start.unwrap_or(index);
      stack.push(OpenBlock { path, indent, start });
      pending_comment_start = None;
      continue;
    }

    // Content line (value, sequence item, etc.). A comment run terminated
    // by content is not a key's leading comment.
    pending_comment_start = None;
  }

  // Close the remaining blocks at EOF.
  for open in stack {
    blocks.insert(
      open.path,
      Block {
        start: open.start,
        end: trim_blank_end(lines.len() - 1, open.start),
      },
    );
  }

  (blocks, lines)
}

/// Extracts the key name from a YAML mapping-key line, if any.
///
/// Sequence items (lines starting with `-`) and plain content lines return
/// `None`.
fn parse_key_line(trimmed: &str) -> Option<String> {
  let mut name_end = 0;
  for byte in trimmed.bytes() {
    if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
      name_end += 1;
    } else {
      break;
    }
  }

  if name_end == 0 || trimmed.as_bytes().get(name_end) != Some(&b':') {
    return None;
  }

  Some(trimmed[..name_end].to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  const SAMPLE_CONFIG: &str =
    include_str!("../../../resources/assets/sample-config.yaml");

  /// All documented sample keys must be detected by the scanner.
  #[test]
  fn scans_all_sample_keys() {
    let (blocks, _) = scan_blocks(SAMPLE_CONFIG);

    for path in [
      vec!["animations"],
      vec!["animations", "window_move"],
      vec!["animations", "window_move", "easing"],
      vec!["animations", "window_move", "threshold_px"],
      vec!["general"],
      vec!["general", "startup_commands"],
      vec!["general", "focus_restore_on_floating_close"],
      vec!["general", "restore_window_placement_on_exit"],
      vec!["window_effects", "focused_window", "border", "color"],
      vec!["window_behavior", "state_defaults", "floating", "centered"],
      vec!["workspaces"],
      vec!["window_rules"],
      vec!["keybindings"],
    ] {
      let path = path
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<String>>();
      assert!(blocks.contains_key(&path), "Missing path {path:?}");
    }
  }

  /// Missing keys inside an existing section are spliced in without
  /// touching existing values.
  #[test]
  fn merges_missing_nested_keys() {
    let user_config = r"general:
  startup_commands: ['shell-exec notepad']
keybindings:
  - commands: ['wm-exit']
    bindings: ['alt+shift+e']
";
    let merged = merge_config(user_config, include_str!("../../../resources/assets/sample-config.yaml")).unwrap();
    let merged_value: Value =
      serde_yaml::from_str(&merged).expect("merged config should parse");

    let general = &merged_value["general"];
    assert_eq!(
      general["startup_commands"][0],
      Value::String("shell-exec notepad".to_string())
    );
    assert_eq!(general["focus_restore_on_floating_close"], "none");
    assert_eq!(
      general["restore_window_placement_on_exit"],
      Value::Bool(true)
    );
    // New keys are documented.
    assert!(merged.contains("focus_restore_on_floating_close: \"none\""));
    assert!(merged.contains("Which window to focus after closing"));
  }

  /// Missing top-level sections are appended at the end of the file.
  #[test]
  fn merges_missing_top_level_section() {
    let user_config = "general:\n  focus_follows_cursor: true\n";
    let merged = merge_config(user_config, include_str!("../../../resources/assets/sample-config.yaml")).unwrap();
    let merged_value: Value =
      serde_yaml::from_str(&merged).expect("merged config should parse");

    assert!(merged_value.get("window_effects").is_some());
    assert!(merged_value.get("keybindings").is_some());
    assert!(merged.contains("window_effects:"));
  }

  /// Existing user values are preserved verbatim.
  #[test]
  fn preserves_existing_user_values() {
    let user_config = r"gaps:
  inner_gap: '99px'
";
    let merged = merge_config(user_config, include_str!("../../../resources/assets/sample-config.yaml")).unwrap();

    assert!(merged.contains("inner_gap: '99px'"));
  }

  /// A config that already contains all sample keys is returned unchanged.
  #[test]
  fn noop_when_up_to_date() {
    let merged = merge_config(SAMPLE_CONFIG, SAMPLE_CONFIG).unwrap();
    assert_eq!(merged, SAMPLE_CONFIG);
  }

  /// CRLF line endings are preserved without doubling the carriage return.
  #[test]
  fn preserves_crlf_line_endings() {
    let user_config = "general:\r\n  focus_follows_cursor: true\r\n";
    let merged = merge_config(
      user_config,
      include_str!("../../../resources/assets/sample-config.yaml"),
    )
    .unwrap();

    assert!(merged.ends_with("\r\n"));
    assert!(!merged.contains("\r\r\n"));
    assert!(merged.contains("focus_restore_on_floating_close: \"none\"\r\n"));
  }

  /// Sibling keys spliced into the same section end keep sample order.
  #[test]
  fn keeps_sample_order_for_sibling_keys() {
    let user_config = "general:\n  focus_follows_cursor: true\n";
    let merged = merge_config(
      user_config,
      include_str!("../../../resources/assets/sample-config.yaml"),
    )
    .unwrap();

    let focus_pos =
      merged.find("focus_restore_on_floating_close").unwrap();
    let restore_pos =
      merged.find("restore_window_placement_on_exit").unwrap();
    assert!(focus_pos < restore_pos);
  }
}

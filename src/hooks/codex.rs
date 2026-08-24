//! Codex hooks.json installation and removal.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

use super::constants::{CODEX_HOOK_COMMAND, PRE_TOOL_USE_KEY};
use super::init::{atomic_write, InitContext};

const HOOKS_JSON: &str = "hooks.json";
const RTK_MARKER: &str = "_rtk_managed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookUpsert {
    Added,
    Updated,
    Unchanged,
}

pub(crate) fn hooks_path(codex_dir: &Path) -> PathBuf {
    codex_dir.join(HOOKS_JSON)
}

pub(crate) fn install(path: &Path, ctx: InitContext) -> Result<HookUpsert> {
    let existing = path
        .exists()
        .then(|| {
            fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))
        })
        .transpose()?;
    let (rendered, action) = upsert_json(existing.as_deref(), CODEX_HOOK_COMMAND)?;

    if action == HookUpsert::Unchanged {
        return Ok(action);
    }
    if ctx.dry_run {
        println!(
            "[dry-run] would {} Codex PreToolUse hook in {}",
            match action {
                HookUpsert::Added => "add",
                HookUpsert::Updated => "replace",
                HookUpsert::Unchanged => unreachable!(),
            },
            path.display()
        );
        return Ok(action);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create Codex config directory: {}",
                parent.display()
            )
        })?;
    }
    atomic_write(path, &rendered).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(action)
}

pub(crate) fn uninstall_at(codex_dir: &Path, ctx: InitContext) -> Result<Vec<String>> {
    let path = hooks_path(codex_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read Codex hooks.json: {}", path.display()))?;
    let Some(rendered) = remove_json(&content, CODEX_HOOK_COMMAND)? else {
        return Ok(Vec::new());
    };

    let description = if rendered.is_empty() {
        "hooks.json: cleared RTK-only content"
    } else {
        "hooks.json: removed RTK PreToolUse entry"
    };
    if ctx.dry_run {
        println!("[dry-run] would {}: {}", description, path.display());
    } else {
        atomic_write(
            &path,
            if rendered.is_empty() {
                "{}\n"
            } else {
                &rendered
            },
        )
        .with_context(|| format!("Failed to write Codex hooks.json: {}", path.display()))?;
        if ctx.verbose > 0 {
            eprintln!("{}: {}", description, path.display());
        }
    }
    Ok(vec![description.to_string()])
}

fn is_rtk_hook(hook: &Value, command: &str) -> bool {
    hook.get(RTK_MARKER) == Some(&Value::Bool(true))
        || hook
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                value.strip_prefix(command).is_some_and(|suffix| {
                    suffix.is_empty() || suffix.starts_with(char::is_whitespace)
                }) || is_absolute_rtk_hook(value)
            })
}

fn is_absolute_rtk_hook(command: &str) -> bool {
    let parts = crate::discover::lexer::shell_split(command);
    let [binary, hook, event, ..] = parts.as_slice() else {
        return false;
    };
    binary.rsplit(['/', '\\']).next() == Some("rtk") && hook == "hook" && event == "codex"
}

fn pre_tool_use_mut(root: &mut Value) -> Result<&mut Vec<Value>> {
    if !root.is_object() {
        anyhow::bail!(
            "Codex hooks.json root must be a JSON object, found {}",
            value_kind(root)
        );
    }
    let object = root.as_object_mut().expect("root was checked");
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        anyhow::bail!(
            "Codex hooks.json hooks field must be an object, found {}",
            value_kind(hooks)
        );
    }
    let hooks = hooks.as_object_mut().expect("hooks was checked");
    let pre_tool_use = hooks
        .entry(PRE_TOOL_USE_KEY)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !pre_tool_use.is_array() {
        anyhow::bail!(
            "Codex hooks.json hooks.{} must be an array, found {}",
            PRE_TOOL_USE_KEY,
            value_kind(pre_tool_use)
        );
    }
    Ok(pre_tool_use.as_array_mut().expect("PreToolUse was checked"))
}

fn upsert_json(existing: Option<&str>, command: &str) -> Result<(String, HookUpsert)> {
    let mut root = match existing {
        None | Some("") => json!({}),
        Some(content) if content.trim().is_empty() => json!({}),
        Some(content) => serde_json::from_str(content).context(
            "Refusing to clobber existing Codex hooks.json: file is not valid JSON. Move or repair it manually and re-run.",
        )?,
    };
    let original = root.clone();
    let entries = pre_tool_use_mut(&mut root)?;
    let hook = json!({
        "type": "command",
        "command": command,
        "timeout": 30,
        "statusMessage": "RTK rewriting Bash command"
    });

    let first_group = entries.iter().position(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| hooks.iter().any(|item| is_rtk_hook(item, command)))
    });
    if let Some(first_group) = first_group {
        let mut retained = Vec::with_capacity(entries.len());
        for (index, mut entry) in entries.drain(..).enumerate() {
            let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                retained.push(entry);
                continue;
            };
            let Some(first_hook) = hooks.iter().position(|item| is_rtk_hook(item, command)) else {
                retained.push(entry);
                continue;
            };
            hooks.retain(|item| !is_rtk_hook(item, command));
            if index == first_group {
                hooks.insert(first_hook.min(hooks.len()), hook.clone());
            }
            if !hooks.is_empty() {
                retained.push(entry);
            }
        }
        *entries = retained;
    } else {
        entries.push(json!({ "matcher": "^Bash$", "hooks": [hook] }));
    }

    let action = if root == original {
        HookUpsert::Unchanged
    } else if first_group.is_some() {
        HookUpsert::Updated
    } else {
        HookUpsert::Added
    };
    if action == HookUpsert::Unchanged {
        return Ok((existing.unwrap_or_default().to_string(), action));
    }
    let mut rendered =
        serde_json::to_string_pretty(&root).context("Failed to serialize Codex hooks.json")?;
    rendered.push('\n');
    Ok((rendered, action))
}

fn remove_json(existing: &str, command: &str) -> Result<Option<String>> {
    if existing.trim().is_empty() {
        return Ok(None);
    }
    let mut root: Value = serde_json::from_str(existing)
        .context("Refusing to clobber Codex hooks.json: file is not valid JSON")?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let Some(entries) = hooks
        .get_mut(PRE_TOOL_USE_KEY)
        .and_then(Value::as_array_mut)
    else {
        return Ok(None);
    };

    let mut changed = false;
    let mut retained = Vec::with_capacity(entries.len());
    for mut entry in entries.drain(..) {
        let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            retained.push(entry);
            continue;
        };
        let before = hooks.len();
        hooks.retain(|item| !is_rtk_hook(item, command));
        changed |= hooks.len() != before;
        if !hooks.is_empty() || before == 0 {
            retained.push(entry);
        }
    }
    *entries = retained;
    if !changed {
        return Ok(None);
    }
    if entries.is_empty() {
        hooks.remove(PRE_TOOL_USE_KEY);
    }
    if hooks.is_empty() {
        root.as_object_mut()
            .expect("hooks parent is an object")
            .remove("hooks");
    }
    if root.as_object().is_some_and(Map::is_empty) {
        return Ok(Some(String::new()));
    }
    let mut rendered =
        serde_json::to_string_pretty(&root).context("Failed to serialize Codex hooks.json")?;
    rendered.push('\n');
    Ok(Some(rendered))
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root(content: &str) -> Value {
        serde_json::from_str(content).expect("valid test JSON")
    }

    fn commands(value: &Value) -> Vec<&str> {
        value
            .pointer("/hooks/PreToolUse")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str())
            .collect()
    }

    #[test]
    fn upsert_is_idempotent_and_preserves_foreign_hooks() {
        let foreign = json!({
            "hooks": { "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{ "type": "command", "command": "/audit" },
                    { "type": "command", "command": "rtk hook codex --old", RTK_MARKER: true }]
            }] }
        })
        .to_string();
        let (first, action) = upsert_json(Some(&foreign), CODEX_HOOK_COMMAND).unwrap();
        assert_eq!(action, HookUpsert::Updated);
        assert_eq!(commands(&root(&first)), vec!["/audit", CODEX_HOOK_COMMAND]);

        let (second, action) = upsert_json(Some(&first), CODEX_HOOK_COMMAND).unwrap();
        assert_eq!(action, HookUpsert::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn upsert_deduplicates_rtk_groups_without_dropping_foreign_entries() {
        let existing = json!({
            "hooks": { "PreToolUse": [
                { "matcher": "^Bash$", "hooks": [{ "type": "command", "command": CODEX_HOOK_COMMAND }] },
                { "matcher": "^Bash$", "hooks": [{ "type": "command", "command": "/audit" }] },
                { "matcher": "^Bash$", "hooks": [{ "type": "command", "command": CODEX_HOOK_COMMAND, RTK_MARKER: true }] }
            ] }
        })
        .to_string();
        let (rendered, _) = upsert_json(Some(&existing), CODEX_HOOK_COMMAND).unwrap();
        let rendered_root = root(&rendered);
        let commands = commands(&rendered_root);
        assert_eq!(
            commands
                .iter()
                .filter(|command| **command == CODEX_HOOK_COMMAND)
                .count(),
            1
        );
        assert!(commands.contains(&"/audit"));
    }

    #[test]
    fn upsert_replaces_unmarked_absolute_rtk_command() {
        let existing = json!({
            "hooks": { "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{
                    "type": "command",
                    "command": "\"/opt/rtk/bin/rtk\" hook codex"
                }]
            }] }
        })
        .to_string();

        let (rendered, action) = upsert_json(Some(&existing), CODEX_HOOK_COMMAND).unwrap();
        assert_eq!(action, HookUpsert::Updated);
        assert_eq!(commands(&root(&rendered)), vec![CODEX_HOOK_COMMAND]);
    }

    #[test]
    fn malformed_json_and_shapes_are_rejected_before_write() {
        for content in [
            "{",
            "[]",
            r#"{"hooks": []}"#,
            r#"{"hooks":{"PreToolUse":{}}}"#,
        ] {
            assert!(
                upsert_json(Some(content), CODEX_HOOK_COMMAND).is_err(),
                "{content}"
            );
        }
    }

    #[test]
    fn remove_preserves_foreign_bytes_when_rtk_is_absent() {
        let foreign = r#"{"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"/audit"}]}]}}"#;
        assert_eq!(remove_json(foreign, CODEX_HOOK_COMMAND).unwrap(), None);
    }

    #[test]
    fn remove_clears_rtk_only_content_and_is_idempotent() {
        let rtk_only = json!({
            "hooks": { "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{ "type": "command", "command": CODEX_HOOK_COMMAND, RTK_MARKER: true }]
            }] }
        })
        .to_string();
        assert_eq!(
            remove_json(&rtk_only, CODEX_HOOK_COMMAND).unwrap(),
            Some(String::new())
        );
        assert_eq!(remove_json("{}", CODEX_HOOK_COMMAND).unwrap(), None);
    }

    #[test]
    fn disk_install_and_uninstall_keep_foreign_entry() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(temp.path());
        let foreign = json!({
            "hooks": { "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{ "type": "command", "command": "/audit" }]
            }] }
        });
        fs::write(&path, serde_json::to_string_pretty(&foreign).unwrap()).unwrap();
        assert_eq!(
            install(&path, InitContext::default()).unwrap(),
            HookUpsert::Added
        );
        let removed = uninstall_at(temp.path(), InitContext::default()).unwrap();
        assert_eq!(removed, vec!["hooks.json: removed RTK PreToolUse entry"]);
        assert_eq!(
            commands(&root(&fs::read_to_string(path).unwrap())),
            vec!["/audit"]
        );
    }

    #[test]
    fn malformed_disk_file_is_left_untouched() {
        let temp = TempDir::new().unwrap();
        let path = hooks_path(temp.path());
        let original = "{not-json";
        fs::write(&path, original).unwrap();

        assert!(install(&path, InitContext::default()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}

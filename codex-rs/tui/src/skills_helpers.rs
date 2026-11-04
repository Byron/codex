use codex_app_server_protocol::SkillMetadata;
use codex_app_server_protocol::SkillScope;
use codex_utils_fuzzy_match::fuzzy_match;
use std::collections::HashMap;

pub(crate) fn skill_display_name(skill: &SkillMetadata) -> String {
    if let Some(display_name) = skill
        .interface
        .as_ref()
        .and_then(|interface| interface.display_name.as_deref())
    {
        return display_name.to_string();
    }

    if let Some((plugin_name, skill_name)) = skill.name.split_once(':')
        && !plugin_name.is_empty()
        && !skill_name.is_empty()
    {
        return format!("{skill_name} ({plugin_name})");
    }

    skill.name.clone()
}

/// Label colliding menu names without changing their search text or insertion identity.
pub(crate) fn skill_location_labels(skills: &[SkillMetadata]) -> Vec<Option<String>> {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, skill) in skills.iter().enumerate() {
        by_name
            .entry(skill_display_name(skill))
            .or_default()
            .push(index);
    }

    let mut labels = vec![None; skills.len()];
    for group in by_name.values().filter(|group| group.len() > 1) {
        let candidates: Vec<_> = group
            .iter()
            .map(|&index| skill_location_candidates(&skills[index]))
            .collect();
        let mut depths = vec![0; group.len()];
        loop {
            let mut counts = HashMap::new();
            for (options, &depth) in candidates.iter().zip(&depths) {
                *counts.entry(options[depth].as_str()).or_insert(0) += 1;
            }
            let mut expanded = false;
            for (options, depth) in candidates.iter().zip(&mut depths) {
                if counts[options[*depth].as_str()] > 1 && *depth + 1 < options.len() {
                    *depth += 1;
                    expanded = true;
                }
            }
            if !expanded {
                break;
            }
        }
        for ((&index, options), depth) in group.iter().zip(candidates).zip(depths) {
            labels[index] = Some(options[depth].clone());
        }
    }
    labels
}

fn skill_location_candidates(skill: &SkillMetadata) -> Vec<String> {
    let full_path = skill.path.render_for_ui();
    let Some(path) = skill.path.to_inferred_path_uri() else {
        return vec![full_path];
    };
    let Some(skill_dir) = path.parent() else {
        return vec![full_path];
    };
    let Some(mut root) = skill_dir.parent() else {
        return vec![full_path];
    };
    if root.basename().as_deref() == Some("skills")
        && let Some(parent) = root.parent()
    {
        root = parent;
    }
    let mut suffix = root
        .basename()
        .unwrap_or_else(|| root.inferred_native_path_string());
    let short_name = match skill.scope {
        SkillScope::System => "built-in".to_string(),
        SkillScope::Admin => "admin".to_string(),
        SkillScope::User => skill.plugin_id.clone().unwrap_or_else(|| suffix.clone()),
        SkillScope::Repo if matches!(suffix.as_str(), ".agents" | ".codex") => root
            .parent()
            .and_then(|parent| parent.basename())
            .unwrap_or_else(|| suffix.clone()),
        SkillScope::Repo => suffix.clone(),
    };
    let mut candidates = vec![short_name];
    for ancestor in root.ancestors().skip(1) {
        if let Some(name) = ancestor.basename() {
            suffix = format!("{name}/{suffix}");
            candidates.push(suffix.clone());
        }
    }
    if let (Some(parent), Some(name)) = (root.basename(), skill_dir.basename()) {
        candidates.push(format!("{parent}/{name}"));
    }
    // Absolute paths also distinguish drive/UNC roots and skills sharing a parent directory.
    candidates.extend([
        root.inferred_native_path_string(),
        skill_dir.inferred_native_path_string(),
        full_path,
    ]);
    candidates.dedup();
    candidates
}

pub(crate) fn skill_description(skill: &SkillMetadata) -> &str {
    skill
        .interface
        .as_ref()
        .and_then(|interface| interface.short_description.as_deref())
        .or(skill.short_description.as_deref())
        .unwrap_or(&skill.description)
}

pub(crate) fn match_skill(
    filter: &str,
    display_name: &str,
    skill_name: &str,
) -> Option<(Option<Vec<usize>>, i32)> {
    if let Some((indices, score)) = fuzzy_match(display_name, filter) {
        return Some((Some(indices), score));
    }
    if display_name != skill_name
        && let Some((_indices, score)) = fuzzy_match(skill_name, filter)
    {
        return Some((None, score));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_path_uri::LegacyAppPathString;
    use pretty_assertions::assert_eq;

    fn skill(path: &str, scope: SkillScope) -> SkillMetadata {
        SkillMetadata {
            name: "tix".to_string(),
            description: String::new(),
            short_description: None,
            interface: None,
            dependencies: None,
            path: LegacyAppPathString::from_string(path),
            scope,
            enabled: true,
            plugin_id: None,
        }
    }

    #[test]
    fn duplicate_skills_label_known_roots_and_leave_unique_names_alone() {
        let mut unique = skill("/work/other/SKILL.md", SkillScope::Repo);
        unique.name = "other".to_string();
        let mut plugin = skill(
            "/home/test/.codex/plugins/cache/vendor/tools/1.0/skills/tix/SKILL.md",
            SkillScope::User,
        );
        plugin.plugin_id = Some("tools@vendor".to_string());
        assert_eq!(
            skill_location_labels(&[
                skill("/home/test/.codex/skills/tix/SKILL.md", SkillScope::User),
                skill("/home/test/.agents/skills/tix/SKILL.md", SkillScope::User),
                skill(
                    "/home/test/.codex/skills/.system/tix/SKILL.md",
                    SkillScope::System
                ),
                skill("/etc/codex/skills/tix/SKILL.md", SkillScope::Admin),
                skill(
                    "/work/gitoxide/.agents/skills/tix/SKILL.md",
                    SkillScope::Repo
                ),
                skill("/work/custom/tix/SKILL.md", SkillScope::User),
                plugin,
                unique,
            ]),
            [
                Some(".codex"),
                Some(".agents"),
                Some("built-in"),
                Some("admin"),
                Some("gitoxide"),
                Some("custom"),
                Some("tools@vendor"),
                None
            ]
            .map(|label| label.map(str::to_owned)),
        );
    }

    #[test]
    fn duplicate_parent_names_expand_until_distinct_regardless_of_order() {
        let mut skills = vec![
            skill("/work/one/shared/tix/SKILL.md", SkillScope::User),
            skill("/work/two/shared/tix/SKILL.md", SkillScope::User),
            skill(
                "/work/project/.agents/skills/tix/SKILL.md",
                SkillScope::Repo,
            ),
            skill("/work/project/.codex/skills/tix/SKILL.md", SkillScope::Repo),
        ];
        let mut expected = [
            "one/shared",
            "two/shared",
            "project/.agents",
            "project/.codex",
        ]
        .map(|label| Some(label.to_string()));
        assert_eq!(skill_location_labels(&skills), expected);
        skills.reverse();
        expected.reverse();
        assert_eq!(skill_location_labels(&skills), expected);
    }

    #[test]
    fn duplicate_skills_in_one_parent_keep_their_distinct_skill_directories() {
        assert_eq!(
            skill_location_labels(&[
                skill("/work/shared/first/SKILL.md", SkillScope::Repo),
                skill("/work/shared/second/SKILL.md", SkillScope::Repo),
            ]),
            ["shared/first", "shared/second"].map(|label| Some(label.to_string())),
        );
    }

    #[test]
    fn location_labels_handle_foreign_windows_paths_and_posix_backslashes() {
        for (paths, expected) in [
            (
                [r"C:\one\shared\tix\SKILL.md", r"D:\one\shared\tix\SKILL.md"],
                ["C:/one/shared", "D:/one/shared"],
            ),
            (
                [
                    r"\\first\share\shared\tix\SKILL.md",
                    r"\\second\share\shared\tix\SKILL.md",
                ],
                [r"\\first\share\shared", r"\\second\share\shared"],
            ),
            (
                [
                    r"/work/日本語\one/tix/SKILL.md",
                    r"/work/日本語\two/tix/SKILL.md",
                ],
                [r"日本語\one", r"日本語\two"],
            ),
            (
                ["relative/one/SKILL.md", "relative/two/SKILL.md"],
                ["relative/one/SKILL.md", "relative/two/SKILL.md"],
            ),
        ] {
            assert_eq!(
                skill_location_labels(&paths.map(|path| skill(path, SkillScope::Repo))),
                expected.map(|label| Some(label.to_string())),
            );
        }
    }

    #[test]
    fn duplicate_display_names_from_different_plugins_get_locations() {
        let mut skills = [
            skill("/work/first/review/SKILL.md", SkillScope::User),
            skill("/work/second/review/SKILL.md", SkillScope::User),
        ];
        for (index, skill) in skills.iter_mut().enumerate() {
            skill.name = format!("plugin-{index}:review");
            skill.interface = Some(
                serde_json::from_value(serde_json::json!({
                    "displayName": "Review",
                }))
                .unwrap(),
            );
        }
        assert_eq!(
            skill_location_labels(&skills),
            [Some("first".to_string()), Some("second".to_string())],
        );
    }
}

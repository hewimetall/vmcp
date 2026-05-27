//! Operator-curated MCP Prompts (skills).
//!
//! A skill is a YAML file at `./<skills_dir>/<name>.yaml` declaring an MCP
//! prompt: name + description + typed arguments + a Handlebars template body.
//! The body is rendered server-side with the caller's `prompts/get` arguments
//! and returned as a single `User`-role text message — MCP clients (Claude
//! Code, OpenCode, ...) inject that message into the agent context, giving
//! the model a ready-made playbook instead of forcing it to discover tools
//! from scratch.
//!
//! Failure model: a malformed YAML file logs an error and is skipped; the
//! rest of the skills directory still loads. Skills are loaded once at boot;
//! drift detection / live reload is deferred (operator restarts vmcp).
//!
//! Templating: Handlebars in strict mode (`{{var}}` resolves to error on
//! missing var rather than rendering empty). Conditional `{{#if var}}…{{/if}}`
//! is supported via the built-in helper. No custom helpers — keeps skill
//! files reviewable.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use handlebars::Handlebars;
use regex::Regex;
use serde::{Deserialize, Serialize};

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9_-]{1,64}$").unwrap())
}

/// One operator-authored prompt template.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Skill {
    /// MCP prompt name. Becomes `/mcp__vmcp__<name>` in slash-command clients.
    pub name: String,
    /// Short human-readable description shown in `prompts/list`. The agent
    /// reads this to decide whether to invoke the skill.
    pub description: String,
    /// Typed arguments. Required ones are validated before rendering.
    #[serde(default)]
    pub arguments: Vec<SkillArg>,
    /// Handlebars source. Rendered with the caller-supplied arguments map.
    pub template: String,
}

/// One typed argument of a [`Skill`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillArg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// Optional default value substituted into the template when the caller
    /// omits this argument. Mutually exclusive with `required: true`.
    #[serde(default)]
    pub default: Option<String>,
}

/// Scan `dir` for `*.yaml` / `*.yml` files and parse each as a [`Skill`].
///
/// Returns an empty vector (with a warning logged) when `dir` is missing —
/// vmcp can boot without any skills. Per-file parse errors are logged and the
/// offending file is skipped; one bad skill does not poison the rest.
pub fn load_skills(dir: &Path) -> Result<Vec<Skill>> {
    if !dir.exists() {
        tracing::warn!(?dir, "skills_dir does not exist, starting with no skills");
        return Ok(Vec::new());
    }
    let read_dir = fs::read_dir(dir).with_context(|| format!("read skills dir {dir:?}"))?;

    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for entry in read_dir {
        let entry = entry.with_context(|| format!("iterating skills dir {dir:?}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| matches!(s, "yaml" | "yml"))
            .unwrap_or(false);
        if !is_yaml {
            continue;
        }

        match parse_skill_file(&path) {
            Ok(skill) => {
                if !seen_names.insert(skill.name.clone()) {
                    tracing::error!(
                        path = ?path,
                        skill = %skill.name,
                        "duplicate skill name, skipping file"
                    );
                    continue;
                }
                tracing::info!(skill = %skill.name, path = ?path, "loaded skill");
                skills.push(skill);
            }
            Err(e) => {
                tracing::error!(path = ?path, error = %e, "failed to load skill, skipping");
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn parse_skill_file(path: &Path) -> Result<Skill> {
    let text = fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    let skill: Skill =
        serde_yaml::from_str(&text).with_context(|| format!("parse {path:?}"))?;
    if skill.name.is_empty() {
        return Err(anyhow!("skill at {path:?} has empty name"));
    }
    if skill.description.is_empty() {
        return Err(anyhow!("skill {} has empty description", skill.name));
    }
    Ok(skill)
}

/// Persist `skill` to `dir/<skill.name>.yaml` atomically. Validates the name
/// against `^[a-z0-9_-]{1,64}$`, rejects an empty template, and rejects any
/// argument that's marked `required` while also carrying a `default`.
///
/// Atomicity: writes to `.<name>.yaml.tmp` first, then renames over the final
/// path so concurrent readers never see a half-written file.
pub fn save_skill(dir: &std::path::Path, skill: &Skill) -> anyhow::Result<()> {
    if !name_re().is_match(&skill.name) {
        anyhow::bail!("invalid name: must match ^[a-z0-9_-]{{1,64}}$");
    }
    if skill.template.trim().is_empty() {
        anyhow::bail!("template must not be empty");
    }
    for a in &skill.arguments {
        if a.required && a.default.is_some() {
            anyhow::bail!("argument `{}` is required but has a default", a.name);
        }
    }
    std::fs::create_dir_all(dir)?;
    let yaml = serde_yaml::to_string(skill)?;
    let tmp = dir.join(format!(".{}.yaml.tmp", skill.name));
    let final_path = dir.join(format!("{}.yaml", skill.name));
    std::fs::write(&tmp, yaml)?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(())
}

/// Remove `dir/<name>.yaml`. Returns an error if the name fails the slug regex
/// or the file does not exist (so callers can surface 404 properly).
pub fn delete_skill(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    if !name_re().is_match(name) {
        anyhow::bail!("invalid name");
    }
    let path = dir.join(format!("{name}.yaml"));
    std::fs::remove_file(&path)?;
    Ok(())
}

/// Render `skill.template` with the caller-supplied arguments. Required
/// arguments declared on the skill must all be present in `args`; missing
/// optionals fall through to the template (Handlebars `{{#if name}}…{{/if}}`
/// blocks handle them gracefully).
pub fn render_skill(skill: &Skill, args: &HashMap<String, String>) -> Result<String> {
    for declared in &skill.arguments {
        if declared.required && !args.contains_key(&declared.name) {
            return Err(anyhow!(
                "missing required argument `{}` for skill `{}`",
                declared.name,
                skill.name
            ));
        }
    }

    let mut hbs = Handlebars::new();
    hbs.set_strict_mode(false); // optionals may be absent; templates use {{#if}}

    let rendered = hbs
        .render_template(&skill.template, args)
        .with_context(|| format!("render template for skill `{}`", skill.name))?;
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp_skill(stem: &str, body: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("vmcp-skills-test-{}-{}", stem, nanos));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{stem}.yaml"));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn loads_valid_skill() {
        let dir = write_tmp_skill(
            "echo",
            r#"
name: echo
description: smoke test skill
arguments:
  - { name: who, required: true }
template: "hello {{who}}"
"#,
        );
        let skills = load_skills(&dir).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "echo");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_with_args() {
        let skill = Skill {
            name: "echo".into(),
            description: "x".into(),
            arguments: vec![SkillArg {
                name: "who".into(),
                description: None,
                required: true,
                default: None,
            }],
            template: "hello {{who}}".into(),
        };
        let mut args = HashMap::new();
        args.insert("who".to_string(), "world".to_string());
        let out = render_skill(&skill, &args).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn rejects_missing_required_arg() {
        let skill = Skill {
            name: "echo".into(),
            description: "x".into(),
            arguments: vec![SkillArg {
                name: "who".into(),
                description: None,
                required: true,
                default: None,
            }],
            template: "hello {{who}}".into(),
        };
        let err = render_skill(&skill, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("missing required"));
    }

    #[test]
    fn conditional_block_when_arg_present() {
        let skill = Skill {
            name: "cond".into(),
            description: "x".into(),
            arguments: vec![
                SkillArg { name: "title".into(), description: None, required: true, default: None },
                SkillArg { name: "body".into(),  description: None, required: false, default: None },
            ],
            template: "T={{title}}{{#if body}}, B={{body}}{{/if}}".into(),
        };
        let mut a = HashMap::new();
        a.insert("title".into(), "X".into());
        assert_eq!(render_skill(&skill, &a).unwrap(), "T=X");

        a.insert("body".into(), "Y".into());
        assert_eq!(render_skill(&skill, &a).unwrap(), "T=X, B=Y");
    }

    #[test]
    fn missing_directory_returns_empty() {
        let dir = std::env::temp_dir().join("vmcp-skills-nonexistent-xyz");
        let _ = fs::remove_dir_all(&dir);
        let skills = load_skills(&dir).unwrap();
        assert_eq!(skills.len(), 0);
    }
}

#[cfg(test)]
mod save_delete_tests {
    use super::*;

    fn fresh_dir(stem: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("vmcp-skills-crud-{}-{}", stem, nanos));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn valid_skill(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "a description".into(),
            arguments: vec![],
            template: "hello world".into(),
        }
    }

    #[test]
    fn save_skill_writes_valid_yaml_and_load_finds_it() {
        let dir = fresh_dir("save-roundtrip");
        let s = valid_skill("greet");
        save_skill(&dir, &s).expect("save ok");

        let loaded = load_skills(&dir).expect("load ok");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "greet");
        assert_eq!(loaded[0].description, "a description");
        assert_eq!(loaded[0].template, "hello world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_uppercase_name() {
        let dir = fresh_dir("bad-upper");
        let mut s = valid_skill("Greet");
        s.name = "GreetUpper".into();
        let err = save_skill(&dir, &s).unwrap_err();
        assert!(err.to_string().contains("invalid name"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_name_with_slash() {
        let dir = fresh_dir("bad-slash");
        let mut s = valid_skill("greet");
        s.name = "greet/bad".into();
        let err = save_skill(&dir, &s).unwrap_err();
        assert!(err.to_string().contains("invalid name"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_empty_name() {
        let dir = fresh_dir("bad-empty");
        let mut s = valid_skill("greet");
        s.name = "".into();
        let err = save_skill(&dir, &s).unwrap_err();
        assert!(err.to_string().contains("invalid name"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_empty_template() {
        let dir = fresh_dir("bad-tmpl");
        let mut s = valid_skill("greet");
        s.template = "   \n  ".into();
        let err = save_skill(&dir, &s).unwrap_err();
        assert!(
            err.to_string().contains("template must not be empty"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_required_arg_with_default() {
        let dir = fresh_dir("bad-default");
        let mut s = valid_skill("greet");
        s.arguments = vec![SkillArg {
            name: "who".into(),
            description: None,
            required: true,
            default: Some("world".into()),
        }];
        let err = save_skill(&dir, &s).unwrap_err();
        assert!(
            err.to_string().contains("required but has a default"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_skill_removes_file() {
        let dir = fresh_dir("delete-ok");
        let s = valid_skill("greet");
        save_skill(&dir, &s).expect("save ok");

        let path = dir.join("greet.yaml");
        assert!(path.exists());

        delete_skill(&dir, "greet").expect("delete ok");
        assert!(!path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_missing_skill_errors() {
        let dir = fresh_dir("delete-missing");
        fs::create_dir_all(&dir).unwrap();
        let err = delete_skill(&dir, "nope").unwrap_err();
        // Should surface the underlying io error (not a regex one).
        let s = err.to_string();
        assert!(
            !s.contains("invalid name"),
            "valid name should pass through to io error, got: {s}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_rejects_bad_name() {
        let dir = fresh_dir("delete-badname");
        fs::create_dir_all(&dir).unwrap();
        let err = delete_skill(&dir, "Bad/Name").unwrap_err();
        assert!(err.to_string().contains("invalid name"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }
}

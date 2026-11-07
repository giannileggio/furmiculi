//! Templates: `templates/<id>.md`, a task to start new tasks from.
//!
//! A template has the shape of a task (a `repo`, the options, and a body that
//! is the prompt) but no status, times or cron: nothing ever runs it. Starting
//! from one copies it into a new task, which the person then reviews or queues.
//! Its `repo` is optional, so one can be shared between people and projects:
//! whoever uses it says where it runs.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmd::md_files;
use crate::frontmatter;
use crate::fsutil;
use crate::home::{Home, expand_tilde};
use crate::spec::{OnFinish, SPEC_KEYS, TaskSpec, slugify, validate_id};
use crate::task::{Task, prompt_problems};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    /// One line saying what the template is for, shown in listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `repo` may be empty: the project is chosen when the template is used.
    #[serde(flatten)]
    pub spec: TaskSpec,
    #[serde(skip)]
    pub prompt: String,
}

const TEMPLATE_KEYS: &[&str] = &["id", "description"];

/// Starter templates, copied into a new `templates/` folder the first time
/// there is none. They live in the repository's `templates/` folder.
const STARTERS: &[(&str, &str)] = &[
    (
        "fix-failing-test",
        include_str!("../templates/fix-failing-test.md"),
    ),
    (
        "update-dependencies",
        include_str!("../templates/update-dependencies.md"),
    ),
    (
        "review-the-diff",
        include_str!("../templates/review-the-diff.md"),
    ),
];

impl Template {
    pub fn parse(content: &str) -> Result<Self> {
        let (yaml, body) = frontmatter::split(content)?;
        let mut value: yaml_serde::Value =
            yaml_serde::from_str(yaml).context("invalid YAML in frontmatter")?;
        let Some(map) = value.as_mapping_mut() else {
            bail!("frontmatter must be a YAML mapping");
        };
        for key in map.keys() {
            let key = key.as_str().context("frontmatter keys must be strings")?;
            if !TEMPLATE_KEYS.contains(&key) && !SPEC_KEYS.contains(&key) {
                bail!("unknown field `{key}`");
            }
        }
        // A task must say where it runs; a template may leave that to whoever
        // uses it.
        if !map.contains_key("repo") {
            map.insert("repo".into(), "".into());
        }
        let mut template: Template = yaml_serde::from_value(value)?;
        template.prompt = body.trim().to_string();
        Ok(template)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Loads `templates/<id>.md`.
    pub fn find(home: &Home, id: &str) -> Result<Self> {
        validate_id(id).map_err(anyhow::Error::msg)?;
        let path = home.templates().join(format!("{id}.md"));
        if !path.exists() {
            bail!("no template `{id}`: `furmi template ls` lists them");
        }
        Self::load(&path)
    }

    fn to_markdown(&self) -> Result<String> {
        frontmatter::render(self, &self.prompt)
    }

    /// Problems that make this template unusable. The repo is not checked: it
    /// may belong to another machine, or be left for whoever uses the template.
    pub fn problems(&self, path: Option<&Path>) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = validate_id(&self.id) {
            out.push(e);
        }
        if let Some(stem) = path.and_then(|p| p.file_stem()).and_then(|s| s.to_str())
            && stem != self.id
        {
            out.push(format!(
                "file name `{stem}.md` does not match id `{}`",
                self.id
            ));
        }
        out.extend(prompt_problems(&self.spec, &self.prompt));
        out
    }

    /// The project it names, if it names one.
    pub fn repo(&self) -> Option<&Path> {
        Some(self.spec.repo.as_path()).filter(|r| !r.as_os_str().is_empty())
    }
}

/// Every template, with the files that couldn't be read set aside.
pub fn list(home: &Home) -> (Vec<Template>, Vec<(String, String)>) {
    let Ok(files) = md_files(&home.templates()) else {
        return (Vec::new(), Vec::new());
    };
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for path in files {
        match Template::load(&path) {
            Ok(t) => ok.push(t),
            Err(e) => bad.push((path.display().to_string(), format!("{e:#}"))),
        }
    }
    (ok, bad)
}

/// Writes the starter templates, but only when there is no `templates/` folder
/// yet: one that was emptied or pruned on purpose stays that way.
pub fn install_starters(home: &Home) -> Result<()> {
    let dir = home.templates();
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (id, text) in STARTERS {
        let path = dir.join(format!("{id}.md"));
        if !path.exists() {
            fsutil::write_atomic(&path, text)?;
        }
    }
    Ok(())
}

/// Saves a task (waiting or finished) as a template, under `id` or the task's
/// own id, made unique. A task with no project becomes a template with none.
/// Returns the template's id.
pub fn save_from_task(home: &Home, task_id: &str, id: Option<String>) -> Result<String> {
    home.ensure_initialized()?;
    let (_, task) = crate::control::find(home, task_id)?;
    let explicit = id.is_some();
    let id = match id {
        Some(id) => {
            validate_id(&id).map_err(anyhow::Error::msg)?;
            id
        }
        None => task.id.clone(),
    };
    let spec = copyable_spec(home, &task);
    std::fs::create_dir_all(home.templates())?;
    let mut template = Template {
        id,
        description: None,
        spec,
        prompt: task.prompt.clone(),
    };
    let base = template.id.clone();
    // The task's own id is a default, so make it unique; a name the person
    // chose is theirs, and a clash is for them to settle.
    for n in 1.. {
        template.id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let path = home.templates().join(format!("{}.md", template.id));
        match fsutil::create_atomic(&path, template.to_markdown()?) {
            Ok(()) => return Ok(template.id),
            Err(_) if path.exists() && !explicit => {}
            Err(_) if path.exists() => bail!("a template `{}` already exists", template.id),
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop only ends by returning")
}

/// What the template form edits. `agent` and `on_finish` left as `None` stay
/// as they are; an empty `repo` leaves the project to whoever uses it.
pub struct TemplateEdit {
    pub description: String,
    pub prompt: String,
    pub repo: String,
    pub on_finish: Option<OnFinish>,
    pub agent: Option<String>,
}

fn apply(template: &mut Template, edit: TemplateEdit) -> Result<()> {
    template.description = Some(edit.description.trim().to_string()).filter(|d| !d.is_empty());
    template.spec.repo = match edit.repo.trim() {
        "" => Default::default(),
        repo => crate::edit::path_of(repo)?,
    };
    crate::edit::set_ask(&mut template.spec, &mut template.prompt, &edit.prompt);
    crate::edit::apply_choices(&mut template.spec, edit.on_finish, edit.agent);
    let problems = template.problems(None);
    if problems.is_empty() {
        Ok(())
    } else {
        bail!("{}", problems.join("; "))
    }
}

/// Creates a template from the form. Its id comes from its description, else
/// the first line of its prompt, made unique. Returns the id.
pub fn create(home: &Home, edit: TemplateEdit) -> Result<String> {
    home.ensure_initialized()?;
    let _lock = home.lock()?;
    let name = if edit.description.trim().is_empty() {
        edit.prompt.lines().next().unwrap_or_default().to_string()
    } else {
        edit.description.clone()
    };
    let base = slugify(&name, 48);
    let mut template = Template {
        id: base.clone(),
        description: None,
        spec: crate::edit::blank_spec(Default::default()),
        prompt: String::new(),
    };
    apply(&mut template, edit)?;
    std::fs::create_dir_all(home.templates())?;
    for n in 1.. {
        template.id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let path = home.templates().join(format!("{}.md", template.id));
        match fsutil::create_atomic(&path, template.to_markdown()?) {
            Ok(()) => return Ok(template.id),
            Err(_) if path.exists() => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop only ends by returning")
}

/// Changes a template in place. Fields the form doesn't show (model, branch,
/// timeouts...) are kept, and nothing is written if it would be invalid.
pub fn update(home: &Home, id: &str, edit: TemplateEdit) -> Result<()> {
    home.ensure_initialized()?;
    let _lock = home.lock()?;
    let mut template = Template::find(home, id)?;
    apply(&mut template, edit)?;
    fsutil::write_atomic(
        &home.templates().join(format!("{id}.md")),
        template.to_markdown()?,
    )
}

/// Deletes a template. Tasks made from it stay.
pub fn delete(home: &Home, id: &str) -> Result<()> {
    validate_id(id).map_err(anyhow::Error::msg)?;
    let path = home.templates().join(format!("{id}.md"));
    if !path.exists() {
        bail!("no template `{id}`");
    }
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))
}

/// What a copy of `task` keeps of its spec: everything, except that a task
/// with no project (its folder is under `scratch/`) copies as one with none,
/// so the copy gets a folder of its own instead of sharing the old one.
pub fn copyable_spec(home: &Home, task: &Task) -> TaskSpec {
    let mut spec = task.spec.clone();
    if expand_tilde(&spec.repo).starts_with(home.scratch()) {
        spec.repo = Default::default();
        spec.branch = None;
        spec.worktree = None;
        spec.on_finish = None;
    }
    spec
}

/// Checks a template file for `furmi validate`.
pub fn validate_file(path: &Path) -> Result<Vec<String>> {
    Ok(Template::load(path)?.problems(Some(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---
id: review-the-diff
description: Read the branch's changes and report problems
on_finish: none
---

Review the changes on this branch.
";

    #[test]
    fn a_template_needs_no_project() {
        let t = Template::parse(SAMPLE).unwrap();
        assert_eq!(t.id, "review-the-diff");
        assert_eq!(t.repo(), None);
        assert_eq!(t.prompt, "Review the changes on this branch.");
        assert!(t.problems(None).is_empty());
        let again = Template::parse(&t.to_markdown().unwrap()).unwrap();
        assert_eq!(again, t);
    }

    #[test]
    fn a_template_may_name_a_project_and_rejects_typos() {
        let t = Template::parse(&SAMPLE.replace("on_finish", "repo: ~/app\non_finish")).unwrap();
        assert_eq!(t.repo(), Some(Path::new("~/app")));
        assert!(Template::parse(&SAMPLE.replace("on_finish", "on_finnish")).is_err());
        // A template is not a schedule or a task.
        assert!(Template::parse(&SAMPLE.replace("on_finish", "cron")).is_err());
        assert!(Template::parse(&SAMPLE.replace("on_finish", "status")).is_err());
    }

    #[test]
    fn problems_name_a_bad_id_a_mismatched_file_and_an_empty_prompt() {
        let mut t = Template::parse(SAMPLE).unwrap();
        let problems = t.problems(Some(Path::new("templates/other.md")));
        assert!(problems.iter().any(|p| p.contains("does not match")));
        t.prompt.clear();
        assert!(t.problems(None).iter().any(|p| p.contains("empty prompt")));
    }

    #[test]
    fn the_shipped_starters_are_valid() {
        for (id, text) in STARTERS {
            let t = Template::parse(text).unwrap_or_else(|e| panic!("{id}: {e:#}"));
            assert_eq!(&t.id, id);
            assert!(t.description.is_some(), "{id} has no description");
            let problems = t.problems(Some(Path::new(&format!("{id}.md"))));
            assert!(problems.is_empty(), "{id}: {problems:?}");
        }
    }

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        (dir, home)
    }

    #[test]
    fn starters_are_written_once_and_a_pruned_folder_stays_pruned() {
        let (_d, home) = home();
        install_starters(&home).unwrap();
        let (found, bad) = list(&home);
        assert_eq!(found.len(), STARTERS.len());
        assert!(bad.is_empty());

        delete(&home, "review-the-diff").unwrap();
        install_starters(&home).unwrap();
        assert_eq!(list(&home).0.len(), STARTERS.len() - 1);
        assert!(delete(&home, "review-the-diff").is_err());
    }

    #[test]
    fn saving_a_task_keeps_its_prompt_and_options_and_never_overwrites() {
        let (_d, home) = home();
        let repo = tempfile::tempdir().unwrap();
        let mut task = Task::new(
            "fix-it".into(),
            TaskSpec {
                repo: repo.path().to_path_buf(),
                branch: None,
                worktree: None,
                agent: Some("claude".into()),
                model: None,
                mode: None,
                on_finish: Some(crate::spec::OnFinish::Pr),
                retries: None,
                retry_delay: None,
                timeout: None,
                run: None,
            },
            "Fix it.".into(),
        );
        task.create(&home.tasks().join("fix-it.md")).unwrap();

        assert_eq!(save_from_task(&home, "fix-it", None).unwrap(), "fix-it");
        // A second save of the same task finds the name taken and picks another.
        assert_eq!(save_from_task(&home, "fix-it", None).unwrap(), "fix-it-2");
        // A name the person chose is not quietly changed.
        let err = save_from_task(&home, "fix-it", Some("fix-it".into())).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert_eq!(
            save_from_task(&home, "fix-it", Some("mine".into())).unwrap(),
            "mine"
        );

        let t = Template::find(&home, "mine").unwrap();
        assert_eq!(t.prompt, "Fix it.");
        assert_eq!(t.spec.on_finish, Some(crate::spec::OnFinish::Pr));
        assert_eq!(t.spec.agent.as_deref(), Some("claude"));

        // A finished task can be saved too.
        task.id = "old".into();
        task.prompt = "Old one.".into();
        task.create(&home.archive().join("old.md")).unwrap();
        save_from_task(&home, "old", None).unwrap();
        assert_eq!(Template::find(&home, "old").unwrap().prompt, "Old one.");
    }

    fn form(prompt: &str) -> TemplateEdit {
        TemplateEdit {
            description: "Check things".into(),
            prompt: prompt.into(),
            repo: String::new(),
            on_finish: Some(crate::spec::OnFinish::Commit),
            agent: None,
        }
    }

    #[test]
    fn a_template_is_created_from_the_form_and_edited_in_place() {
        let (_d, home) = home();
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        let id = create(&home, form("Check the things.")).unwrap();
        assert_eq!(id, "check-things");
        assert_eq!(create(&home, form("Again.")).unwrap(), "check-things-2");

        // No project is written as no `repo` key at all.
        let text = std::fs::read_to_string(home.templates().join("check-things.md")).unwrap();
        assert!(!text.contains("repo"), "{text}");
        let t = Template::find(&home, &id).unwrap();
        assert_eq!(t.repo(), None);
        assert_eq!(t.description.as_deref(), Some("Check things"));

        // Editing keeps what the form doesn't show.
        let mut with_model = t.clone();
        with_model.spec.model = Some("a-model".into());
        fsutil::write_atomic(
            &home.templates().join("check-things.md"),
            with_model.to_markdown().unwrap(),
        )
        .unwrap();
        let mut edited = form("Check them twice.");
        edited.repo = "/tmp".into();
        edited.description = String::new();
        update(&home, &id, edited).unwrap();
        let t = Template::find(&home, &id).unwrap();
        assert_eq!(t.prompt, "Check them twice.");
        assert_eq!(t.repo(), Some(Path::new("/tmp")));
        assert_eq!(t.description, None);
        assert_eq!(t.spec.model.as_deref(), Some("a-model"));

        // An invalid edit writes nothing.
        assert!(update(&home, &id, form("   ")).is_err());
        assert_eq!(
            Template::find(&home, &id).unwrap().prompt,
            "Check them twice."
        );
        assert!(update(&home, "nope", form("x")).is_err());
    }

    #[test]
    fn a_task_with_no_project_becomes_a_template_with_none() {
        let (_d, home) = home();
        let folder = home.scratch().join("look-into-it");
        std::fs::create_dir_all(&folder).unwrap();
        let mut spec = crate::edit::blank_spec(folder);
        spec.worktree = Some(false);
        spec.on_finish = Some(crate::spec::OnFinish::None);
        Task::new("look-into-it".into(), spec, "Look into it.".into())
            .create(&home.tasks().join("look-into-it.md"))
            .unwrap();

        save_from_task(&home, "look-into-it", None).unwrap();
        let t = Template::find(&home, "look-into-it").unwrap();
        assert_eq!(t.repo(), None);
        assert_eq!(t.spec.worktree, None);
    }
}

//! The files that let any coding agent opened in the furmi home (or, with the
//! global skill, anywhere) create tasks: AGENTS.md, per-agent pointers to
//! it, FORMAT.md, and the `furmi-tasks` skill.
//!
//! The skill lives in `.agents/skills/`, the location shared by agents that
//! support skills. Agents with their own location get a symlink to it.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::home::{Home, contract_tilde, home_dir};

const AGENTS_MD: &str = include_str!("../assets/AGENTS.md");
const SKILL_MD: &str = include_str!("../assets/SKILL.md");
const FORMAT_MD: &str = include_str!("../docs/FORMAT.md");
const GENERATED: &str = "<!-- Written by `furmi init`, which overwrites it: don't edit. -->\n\n";

/// Instruction files of agents that don't read AGENTS.md. Each imports it.
const INSTRUCTION_POINTERS: &[&str] = &["CLAUDE.md", "GEMINI.md"];

/// Skill folders of agents that don't read `.agents/skills`, relative to a
/// project (the furmi home) or to `$HOME`. Each gets a link to the skill.
const SKILL_DIRS: &[&str] = &[".claude/skills"];

const SKILL: &str = "furmi-tasks";

static QUIET: AtomicBool = AtomicBool::new(false);

/// Silences the "wrote ..." lines of [`note`], for the automatic first-run
/// setup, where a list of internal files would only be noise.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Reports a file `furmi init` wrote, kept or linked.
pub fn note(line: impl std::fmt::Display) {
    if !QUIET.load(Ordering::Relaxed) {
        println!("{line}");
    }
}

/// Writes the agent files into the home. AGENTS.md and the pointers are the
/// user's to edit, so they're only written if missing; FORMAT.md and the
/// skill are furmi's own and refreshed to match this binary.
pub fn install_in_home(home: &Home) -> Result<()> {
    write_if_missing(&home.root().join("AGENTS.md"), AGENTS_MD)?;
    for name in INSTRUCTION_POINTERS {
        write_if_missing(&home.root().join(name), "@AGENTS.md\n")?;
    }
    overwrite(
        &home.root().join("FORMAT.md"),
        &format!("{GENERATED}{FORMAT_MD}"),
    )?;
    overwrite(&home.skill_dir().join("SKILL.md"), &render_skill(home))?;
    for dir in SKILL_DIRS {
        // Relative, so the home can be moved.
        let target = Path::new("../..").join(".agents/skills").join(SKILL);
        link(&target, &home.root().join(dir).join(SKILL))?;
    }
    Ok(())
}

/// Where the global skill links go: `~/.agents/skills`, plus the folder of
/// each agent that has its own and is set up on this machine.
fn global_links() -> Result<Vec<PathBuf>> {
    let user_home = home_dir()?;
    let mut links = vec![user_home.join(".agents/skills").join(SKILL)];
    for dir in SKILL_DIRS {
        let dir = user_home.join(dir);
        if dir.parent().is_some_and(Path::is_dir) {
            links.push(dir.join(SKILL));
        }
    }
    Ok(links)
}

/// Makes the skill available in every project, or removes it. `choice` is
/// the user's `--[no-]global-skill`; without one, an existing install is
/// kept up to date and otherwise the user is asked (if there's a terminal).
pub fn set_global(home: &Home, choice: Option<bool>) -> Result<()> {
    let links = global_links()?;
    let installed = links.iter().any(|l| is_our_link(l));
    let install = match choice {
        Some(c) => c,
        None if installed => true,
        None if std::io::stdin().is_terminal() => ask(&format!(
            "Also make the {SKILL} skill available in every project, so agents can queue \
             tasks from any repo? (links {}) [y/N] ",
            links
                .iter()
                .map(|l| contract_tilde(l))
                .collect::<Vec<_>>()
                .join(", ")
        ))?,
        None => false,
    };
    for l in &links {
        if install {
            link(&home.skill_dir(), l)?;
        } else if is_our_link(l) {
            std::fs::remove_file(l).with_context(|| format!("removing {}", l.display()))?;
            println!("removed {}", l.display());
        }
    }
    Ok(())
}

/// The skill with this home's paths and `furmi` invocation filled in.
fn render_skill(home: &Home) -> String {
    let shown = contract_tilde(home.root());
    let furmi = if home.is_default() {
        "furmi".to_string()
    } else {
        format!("furmi --home {shown}")
    };
    let skill = SKILL_MD
        .replace("{home}", &shown)
        .replace("{furmi}", &furmi);
    with_generated_note(&skill)
}

/// Puts the note after the skill's frontmatter, which must stay first.
fn with_generated_note(skill: &str) -> String {
    let body_start = skill
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n"))
        .map(|i| i + "---\n".len() + "\n---\n".len())
        .expect("assets/SKILL.md starts with frontmatter");
    let (frontmatter, body) = skill.split_at(body_start);
    format!("{frontmatter}\n{GENERATED}{}", body.trim_start())
}

/// A symlink furmi made: one pointing at some furmi home's skill.
fn is_our_link(path: &Path) -> bool {
    std::fs::read_link(path).is_ok_and(|t| t.ends_with(Path::new(".agents/skills").join(SKILL)))
}

/// Points `at` to `target`, replacing an older link but never a real file.
fn link(target: &Path, at: &Path) -> Result<()> {
    if std::fs::read_link(at).is_ok_and(|t| t == target) {
        return Ok(());
    }
    if at.is_symlink() {
        std::fs::remove_file(at).with_context(|| format!("removing {}", at.display()))?;
    } else if at.exists() {
        note(format_args!(
            "kept existing {} (not a link made by furmi)",
            at.display()
        ));
        return Ok(());
    }
    if let Some(dir) = at.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::os::unix::fs::symlink(target, at)
        .with_context(|| format!("linking {} to {}", at.display(), target.display()))?;
    note(format_args!(
        "linked {} -> {}",
        at.display(),
        target.display()
    ));
    Ok(())
}

pub fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        note(format_args!("kept existing {}", path.display()));
    } else {
        std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
        note(format_args!("wrote {}", path.display()));
    }
    Ok(())
}

fn overwrite(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    note(format_args!("wrote {}", path.display()));
    Ok(())
}

/// Asks a yes/no question on the terminal. Anything but y/yes is no.
pub fn ask(question: &str) -> Result<bool> {
    let answer = prompt(question)?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}

/// Prints `question` and returns the trimmed line typed in reply.
pub fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_into_home_and_keeps_user_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        install_in_home(&home).unwrap();

        let skill = std::fs::read_to_string(home.skill_dir().join("SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: furmi-tasks\n"));
        assert!(skill.contains("---\n\n<!-- Written by `furmi init`"));
        assert!(!skill.contains("{home}") && !skill.contains("{furmi}"));
        // Not the default home, so every command carries --home.
        assert!(skill.contains(&format!("furmi --home {} add", dir.path().display())));

        // Agents with their own skill folder reach the same file.
        let via_link = dir.path().join(".claude/skills/furmi-tasks/SKILL.md");
        assert_eq!(std::fs::read_to_string(via_link).unwrap(), skill);

        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "mine").unwrap();
        std::fs::write(home.skill_dir().join("SKILL.md"), "stale").unwrap();
        install_in_home(&home).unwrap();
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "mine");
        assert_eq!(
            std::fs::read_to_string(home.skill_dir().join("SKILL.md")).unwrap(),
            skill
        );
        for name in INSTRUCTION_POINTERS {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                "@AGENTS.md\n"
            );
        }
    }

    #[test]
    fn links_replace_links_but_not_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("home/.agents/skills/furmi-tasks");
        std::fs::create_dir_all(&target).unwrap();
        let at = dir.path().join("user/.agents/skills/furmi-tasks");

        link(&dir.path().join("old/.agents/skills/furmi-tasks"), &at).unwrap();
        assert!(is_our_link(&at));
        link(&target, &at).unwrap();
        assert_eq!(std::fs::read_link(&at).unwrap(), target);

        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        link(&target, &real).unwrap();
        assert!(!real.is_symlink(), "a real folder is left alone");
    }
}

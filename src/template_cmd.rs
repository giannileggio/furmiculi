//! `furmi template`: list, show, save and delete templates. A template is
//! used with `furmi add --template <id>`.

use anyhow::Result;
use clap::Subcommand;

use crate::cmd;
use crate::home::{Home, contract_tilde};
use crate::template::{self, Template};

#[derive(Subcommand)]
pub enum TemplateCmd {
    /// List templates.
    Ls,
    /// Print a template's file.
    Show { id: String },
    /// Save a task, waiting or finished, as a template.
    Save {
        /// The task to copy.
        task: String,
        /// Name of the template [default: the task's id].
        #[arg(long)]
        id: Option<String>,
    },
    /// Delete a template. Tasks made from it stay.
    Delete { id: String },
}

pub fn run(home: &Home, command: TemplateCmd) -> Result<()> {
    match command {
        TemplateCmd::Ls => ls(home),
        TemplateCmd::Show { id } => {
            let path = home
                .templates()
                .join(format!("{}.md", Template::find(home, &id)?.id));
            print!("{}", std::fs::read_to_string(path)?);
            Ok(())
        }
        TemplateCmd::Save { task, id } => {
            let id = template::save_from_task(home, &task, id)?;
            println!(
                "Saved template `{id}`. Start a task from it with `furmi add --template {id}`."
            );
            Ok(())
        }
        TemplateCmd::Delete { id } => {
            template::delete(home, &id)?;
            println!("Deleted `{id}`");
            Ok(())
        }
    }
}

fn ls(home: &Home) -> Result<()> {
    let (templates, bad) = template::list(home);
    for (file, error) in &bad {
        eprintln!("can't read {file}: {error}");
    }
    if templates.is_empty() {
        println!(
            "No templates. Save a task as one with `furmi template save <task>`, or add a \
             file to {}.",
            contract_tilde(&home.templates())
        );
        return Ok(());
    }
    let mut rows = vec![vec![
        "TEMPLATE".to_string(),
        "PROJECT".into(),
        "WHAT IT DOES".into(),
    ]];
    for t in templates {
        let what = t.description.clone().unwrap_or_else(|| {
            t.spec
                .ask(&t.prompt)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        });
        rows.push(vec![
            t.id.clone(),
            t.repo().map_or("(you choose)".into(), contract_tilde),
            what,
        ]);
    }
    for line in cmd::table_lines(&rows) {
        println!("{line}");
    }
    Ok(())
}

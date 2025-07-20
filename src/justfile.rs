use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    io,
    path::Path,
};

use serde::Deserialize;

use crate::command::Commands;

pub async fn read_justfile_for_target<P: AsRef<Path>>(
    commands: &mut Commands,
    justfile_path: Option<P>,
    target: Option<&str>,
) -> io::Result<()> {
    let mut cmd = tokio::process::Command::new("just");
    cmd.args(["--dump", "--dump-format=json"]);
    if let Some(path) = justfile_path.as_ref() {
        cmd.arg("--justfile");
        cmd.arg(path.as_ref());
    }
    let output = cmd.output().await?;
    if !output.status.success() {
        eprintln!("command 'just' failed:");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        println!("{}", String::from_utf8_lossy(&output.stdout));
        return Err(io::Error::other(format!(
            "command 'just' exited with status {}",
            output.status.code().unwrap_or(1)
        )));
    }
    let justfile: Justfile = serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;

    let target = target.unwrap_or(&justfile.first);

    let Some(recipe) = justfile.recipe(target) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("recipe {target} not found"),
        ));
    };

    let mut args: Vec<OsString> = vec!["just".into(), "--no-deps".into()];
    if let Some(path) = justfile_path.as_ref() {
        args.push("--justfile".into());
        args.push(path.as_ref().into());
    }
    args.push(recipe.name.clone().into());
    commands.dependent_command(
        &recipe.name,
        &args,
        &recipe
            .dependencies
            .iter()
            .map(|d| d.name())
            .collect::<Vec<_>>(),
    );

    let mut prepared = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.extend(recipe.dependencies.clone());

    while let Some(dep) = queue.pop_front() {
        if !prepared.insert(dep.name()) {
            continue;
        }

        let mut args: Vec<OsString> = vec!["just".into(), "--no-deps".into()];
        if let Some(path) = justfile_path.as_ref() {
            args.push("--justfile".into());
            args.push(path.as_ref().into());
        }
        args.push(dep.recipe.clone().into());
        args.extend(dep.arguments.iter().cloned().map(Into::into));
        let deps = if let Some(drep) = justfile.recipe(&dep.recipe) {
            for dd in &drep.dependencies {
                queue.push_back(dd.clone());
            }
            drep.dependencies.iter().map(|d| d.name()).collect()
        } else {
            vec![]
        };

        commands.dependent_command(&dep.name(), &args, &deps);
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct Alias {
    #[allow(dead_code)]
    name: String,
    target: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Recipe {
    #[allow(dead_code)]
    name: String,

    dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    recipe: String,
    arguments: Vec<String>,
}

impl Dependency {
    fn name(&self) -> String {
        if self.arguments.is_empty() {
            self.recipe.clone()
        } else {
            format!("{}({})", self.recipe, self.arguments.join(","))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Justfile {
    aliases: BTreeMap<String, Alias>,
    first: String,
    recipes: BTreeMap<String, Recipe>,
}

impl Justfile {
    fn recipe(&self, name: &str) -> Option<&Recipe> {
        if let Some(recipe) = self.recipes.get(name) {
            return Some(recipe);
        }

        if let Some(target) = self.aliases.get(name) {
            return self.recipes.get(&target.target);
        }
        None
    }
}

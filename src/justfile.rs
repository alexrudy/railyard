use std::{collections::BTreeMap, ffi::OsString, io, path::Path};

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

    for dep in &recipe.dependencies {
        let mut args: Vec<OsString> = vec!["just".into()];
        if let Some(path) = justfile_path.as_ref() {
            args.push("--justfile".into());
            args.push(path.as_ref().into());
        }
        args.push(dep.recipe.clone().into());
        args.extend(dep.arguments.iter().cloned().map(Into::into));
        commands.command(&dep.recipe, &args);
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

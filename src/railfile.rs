use std::{fs, io, path::Path};

use crate::command::Commands;

pub fn read_railyard_file<P: AsRef<Path>>(commands: &mut Commands, path: P) -> io::Result<()> {
    let raw = fs::read_to_string(path)?;
    let document: toml_edit::DocumentMut = raw.parse().unwrap();

    for (name, args) in document.as_table().iter() {
        if let Some(array) = args.as_array() {
            let args: Vec<_> = array.iter().filter_map(|item| item.as_str()).collect();
            commands.command(name, &args);
        }
    }

    Ok(())
}

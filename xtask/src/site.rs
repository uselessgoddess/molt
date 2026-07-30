use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::bench::{Run, load};

const INDEX: &str = include_str!("../assets/index.html");
const STYLE: &str = include_str!("../assets/style.css");
const APP: &str = include_str!("../assets/app.js");

const MARK: &str = "<!--history-->";

pub fn build(data: &Path, out: &Path) -> Result<(), String> {
    let runs = history(data)?;
    let json =
        json::to_string(&runs).map_err(|error| format!("failed to encode the history: {error}"))?;

    fs::create_dir_all(out)
        .map_err(|error| format!("failed to create {}: {error}", out.display()))?;

    write(out, "index.html", &INDEX.replace(MARK, &json.replace("</", "<\\/")))?;
    write(out, "style.css", STYLE)?;
    write(out, "app.js", APP)?;

    println!("{}: {} runs", out.display(), runs.len());
    Ok(())
}

fn history(data: &Path) -> Result<Vec<Run>, String> {
    let failed = |error| format!("failed to read {}: {error}", data.display());
    let entries: Vec<fs::DirEntry> =
        fs::read_dir(data).map_err(failed)?.collect::<Result<_, _>>().map_err(failed)?;
    let mut files: Vec<_> = entries
        .iter()
        .map(fs::DirEntry::path)
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    files.sort();

    let mut runs = BTreeMap::new();
    for file in files {
        let run: Run = load(&file)?;
        runs.insert((run.commit.date.clone(), run.commit.hash.clone()), run);
    }
    Ok(runs.into_values().collect())
}

fn write(dir: &Path, name: &str, contents: &str) -> Result<(), String> {
    let path = dir.join(name);
    fs::write(&path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

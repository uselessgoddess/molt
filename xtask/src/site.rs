//! The page the history is published as.
//!
//! Three files, no build step, no dependencies: the history is spliced into
//! the document so the page works from a checkout as well as from Pages.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::bench::{Run, read};

const INDEX: &str = include_str!("../assets/index.html");
const STYLE: &str = include_str!("../assets/style.css");
const APP: &str = include_str!("../assets/app.js");

/// Where the history goes in the document.
const MARK: &str = "<!--history-->";

pub fn build(data: &Path, out: &Path) -> Result<(), String> {
    let runs = history(data)?;
    let json = serde_json::to_string(&runs)
        .map_err(|error| format!("failed to encode the history: {error}"))?;

    fs::create_dir_all(out)
        .map_err(|error| format!("failed to create {}: {error}", out.display()))?;
    // `\/` is a JSON escape, so no subject or bench id can close the script
    // element it is embedded in.
    write(out, "index.html", &INDEX.replace(MARK, &json.replace("</", "<\\/")))?;
    write(out, "style.css", STYLE)?;
    write(out, "app.js", APP)?;

    println!("{}: {} runs", out.display(), runs.len());
    Ok(())
}

/// Every record in `data`, oldest first, one per commit.
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

    // Keyed by commit, so re-measuring one keeps the later record rather than
    // drawing the same point twice.
    let mut runs = BTreeMap::new();
    for file in files {
        let run: Run = read(&file)?;
        runs.insert((run.commit.date.clone(), run.commit.hash.clone()), run);
    }
    Ok(runs.into_values().collect())
}

fn write(dir: &Path, name: &str, contents: &str) -> Result<(), String> {
    let path = dir.join(name);
    fs::write(&path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

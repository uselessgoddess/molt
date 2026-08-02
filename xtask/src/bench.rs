use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{cargo, require_success, target_dir, workspace_root};

/// Packages whose benches make up a run.
const PACKAGES: [&str; 4] = ["molt-core", "molt-exec", "molt-block", "molt-fs"];

/// Bumped when a field changes meaning, so an old record fails loudly.
const SCHEMA: u32 = 1;

/// Change under this is noise on a shared runner whatever the spread claims.
const FLOOR: f64 = 5.0;

/// One run of every bench, on one machine, at one commit.
#[derive(Debug, Deserialize, Serialize)]
pub struct Run {
    pub schema: u32,
    pub commit: Commit,
    pub machine: Machine,
    pub benches: Vec<Bench>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
    pub date: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Machine {
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub toolchain: String,
    /// The runner image, when a runner is what measured this.
    pub runner: Option<String>,
}

/// Nanoseconds per iteration, as criterion estimated them.
#[derive(Debug, Deserialize, Serialize)]
pub struct Bench {
    pub id: String,
    pub median: f64,
    pub spread: f64,
    pub mean: f64,
    pub low: f64,
    pub high: f64,
}

pub fn dispatch(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        [] => {
            let run = measure()?;
            println!("{}", report(&run.0));
            println!("run: {}", run.1.display());
            Ok(())
        }
        ["compare", base] => compare(Path::new(base), &default_record()),
        ["compare", base, head] => compare(Path::new(base), Path::new(head)),
        ["record", dir] => record(&default_record(), Path::new(dir)),
        ["site", data, out] => crate::site::build(Path::new(data), Path::new(out)),
        _ => Err(crate::usage()),
    }
}

fn measure() -> Result<(Run, PathBuf), String> {
    let root = workspace_root();
    let criterion = target_dir(&root).join("criterion");
    // Criterion keeps every directory it has ever written, so a bench that was
    // renamed or removed would be collected as if this run had produced it.
    if criterion.exists() {
        fs::remove_dir_all(&criterion)
            .map_err(|error| format!("failed to clear {}: {error}", criterion.display()))?;
    }
    for package in PACKAGES {
        let status = Command::new(cargo())
            .current_dir(&root)
            .args(["bench", "--package", package])
            .status()
            .map_err(|error| format!("failed to start cargo: {error}"))?;
        require_success(status, &format!("{package} benches"))?;
    }

    let run = Run {
        schema: SCHEMA,
        commit: commit(&root)?,
        machine: machine(),
        benches: collect(&criterion)?,
    };
    let path = default_record();
    let dir = path.parent().expect("the record path names a file in a directory");
    fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    write(&path, &run)?;
    Ok((run, path))
}

fn default_record() -> PathBuf {
    target_dir(&workspace_root()).join("molt/bench.json")
}

fn record(run: &Path, dir: &Path) -> Result<(), String> {
    let run = load(run)?;
    fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;

    let stamp = run.commit.date.replace(':', "");
    let path = dir.join(format!("{stamp}-{}.json", short(&run.commit.hash)));
    write(&path, &run)?;
    println!("{}", path.display());
    Ok(())
}

fn compare(base: &Path, head: &Path) -> Result<(), String> {
    println!("{}", table(&load(base)?, &load(head)?));
    Ok(())
}

pub fn load(path: &Path) -> Result<Run, String> {
    let run: Run = read(path)?;
    if run.schema != SCHEMA {
        return Err(format!(
            "{} is schema {}, and this is schema {SCHEMA}",
            path.display(),
            run.schema
        ));
    }
    Ok(run)
}

fn collect(dir: &Path) -> Result<Vec<Bench>, String> {
    let mut found = BTreeMap::new();
    visit(dir, &mut found)?;
    if found.is_empty() {
        return Err(format!("{} holds no benchmark estimates", dir.display()));
    }
    Ok(found.into_values().collect())
}

fn visit(dir: &Path, found: &mut BTreeMap<String, Bench>) -> Result<(), String> {
    let read = |error| format!("failed to read {}: {error}", dir.display());
    for entry in fs::read_dir(dir).map_err(read)? {
        let entry = entry.map_err(read)?;
        if !entry.path().is_dir() {
            continue;
        }
        match entry.file_name().to_str() {
            // Criterion's own HTML, and the previous run it compares against.
            Some("report" | "base") => {}
            Some("new") => {
                let bench = estimates(&entry.path())?;
                found.insert(bench.id.clone(), bench);
            }
            _ => visit(&entry.path(), found)?,
        }
    }
    Ok(())
}

fn estimates(dir: &Path) -> Result<Bench, String> {
    #[derive(Deserialize)]
    struct Named {
        full_id: String,
    }
    #[derive(Deserialize)]
    struct Estimates {
        mean: Estimate,
        median: Estimate,
        median_abs_dev: Estimate,
    }
    #[derive(Deserialize)]
    struct Estimate {
        point_estimate: f64,
        confidence_interval: Interval,
    }
    #[derive(Deserialize)]
    struct Interval {
        lower_bound: f64,
        upper_bound: f64,
    }

    let named: Named = read(&dir.join("benchmark.json"))?;
    let estimates: Estimates = read(&dir.join("estimates.json"))?;
    Ok(Bench {
        id: named.full_id,
        median: estimates.median.point_estimate,
        spread: estimates.median_abs_dev.point_estimate,
        mean: estimates.mean.point_estimate,
        low: estimates.median.confidence_interval.lower_bound,
        high: estimates.median.confidence_interval.upper_bound,
    })
}

fn commit(root: &Path) -> Result<Commit, String> {
    let log =
        output(Command::new("git").current_dir(root).args(["log", "-1", "--pretty=%H%n%s%n%cI"]))?;
    let mut lines = log.lines();
    match (lines.next(), lines.next(), lines.next()) {
        (Some(hash), Some(subject), Some(date)) => {
            Ok(Commit { hash: hash.into(), subject: subject.into(), date: date.into() })
        }
        _ => Err("git log did not report a commit".into()),
    }
}

fn machine() -> Machine {
    let cpu = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            let line = info.lines().find(|line| line.starts_with("model name"))?;
            Some(line.split_once(':')?.1.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let toolchain = output(Command::new("rustc").arg("-V"))
        .map(|version| version.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    Machine {
        os: env::consts::OS.into(),
        arch: env::consts::ARCH.into(),
        cpu,
        toolchain,
        runner: env::var("ImageOS").ok(),
    }
}

fn output(command: &mut Command) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|error| format!("failed to start {program}: {error}"))?;
    require_success(output.status, &program)?;
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} printed non-UTF-8: {error}"))
}

pub fn read<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a run record: {error}", path.display()))
}

fn write(path: &Path, run: &Run) -> Result<(), String> {
    let json = json::to_string_pretty(run)
        .map_err(|error| format!("failed to encode the run: {error}"))?;
    fs::write(path, json).map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn report(run: &Run) -> String {
    let mut text = format!("{} on {}\n", short(&run.commit.hash), run.machine.cpu);
    for bench in &run.benches {
        let _ = writeln!(
            text,
            "  {:<32} {:>12}  ±{}",
            bench.id,
            time(bench.median),
            time(bench.spread)
        );
    }
    text
}

struct Row {
    id: String,
    was: Option<f64>,
    now: f64,
    delta: Option<f64>,
    loud: bool,
}

fn table(base: &Run, head: &Run) -> String {
    let previous: BTreeMap<&str, &Bench> =
        base.benches.iter().map(|bench| (bench.id.as_str(), bench)).collect();
    let mut rows: Vec<Row> = Vec::new();
    for bench in &head.benches {
        let was = previous.get(bench.id.as_str());
        let delta = was.map(|was| (bench.median - was.median) / was.median * 100.0);
        let loud = was.zip(delta).is_some_and(|(was, delta)| {
            delta.abs() > ((was.spread + bench.spread) / was.median * 100.0).max(FLOOR)
        });
        rows.push(Row {
            id: bench.id.clone(),
            was: was.map(|was| was.median),
            now: bench.median,
            delta,
            loud,
        });
    }
    rows.sort_by(|left, right| {
        let key = |row: &Row| row.delta.map_or(f64::NEG_INFINITY, f64::abs);
        key(right).total_cmp(&key(left))
    });

    let mut text = String::new();
    if base.machine.cpu != head.machine.cpu {
        let _ = writeln!(
            text,
            "Base ran on {}, head on {}: the change column measures the machines.\n",
            base.machine.cpu, head.machine.cpu
        );
    }
    let _ = writeln!(
        text,
        "| bench | {} | {} | change |",
        short(&base.commit.hash),
        short(&head.commit.hash)
    );
    let _ = writeln!(text, "| --- | ---: | ---: | ---: |");
    for row in &rows {
        let was = row.was.map_or_else(|| "—".into(), time);
        let change = match row.delta {
            Some(delta) if row.loud => format!("**{delta:+.1}%**"),
            Some(delta) => format!("{delta:+.1}%"),
            None => "new".into(),
        };
        let _ = writeln!(text, "| `{}` | {was} | {} | {change} |", row.id, time(row.now));
    }
    for id in previous.keys().filter(|id| !head.benches.iter().any(|bench| bench.id == **id)) {
        let _ = writeln!(text, "| `{id}` | {} | — | gone |", time(previous[id].median));
    }
    let _ = writeln!(
        text,
        "\nBold clears both the {FLOOR:.0}% floor and the two runs' spread. \
         A shared runner moves 10% on its own, so one bold row is a hint and \
         a repeated one is a result."
    );
    text
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(9)]
}

fn time(ns: f64) -> String {
    match ns {
        _ if ns < 1e3 => format!("{ns:.1} ns"),
        _ if ns < 1e6 => format!("{:.2} µs", ns / 1e3),
        _ => format!("{:.2} ms", ns / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::{Bench, Commit, Machine, Run, SCHEMA, load, table, time, write};

    fn run(hash: &str, benches: &[(&str, f64, f64)]) -> Run {
        Run {
            schema: SCHEMA,
            commit: Commit {
                hash: hash.into(),
                subject: "subject".into(),
                date: "2026-07-30T00:00:00+00:00".into(),
            },
            machine: Machine {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu: "cpu".into(),
                toolchain: "rustc".into(),
                runner: None,
            },
            benches: benches
                .iter()
                .map(|&(id, median, spread)| Bench {
                    id: id.into(),
                    median,
                    spread,
                    mean: median,
                    low: median - spread,
                    high: median + spread,
                })
                .collect(),
        }
    }

    #[test]
    fn units_follow_scale() {
        assert_eq!(time(73.3), "73.3 ns");
        assert_eq!(time(2_060.0), "2.06 µs");
        assert_eq!(time(1_500_000.0), "1.50 ms");
    }

    #[test]
    fn win_past_spread_is_loud() {
        let table = table(
            &run("aaaaaaaaaa", &[("timer/arm", 100.0, 2.0)]),
            &run("bbbbbbbbbb", &[("timer/arm", 40.0, 1.0)]),
        );

        assert!(table.contains("**-60.0%**"), "{table}");
    }

    #[test]
    fn drift_inside_spread_is_quiet() {
        let table = table(
            &run("aaaaaaaaaa", &[("timer/arm", 100.0, 9.0)]),
            &run("bbbbbbbbbb", &[("timer/arm", 108.0, 9.0)]),
        );

        assert!(table.contains("| +8.0% |"), "{table}");
    }

    #[test]
    fn floor_holds_when_spread_is_tiny() {
        let table = table(
            &run("aaaaaaaaaa", &[("timer/arm", 100.0, 0.01)]),
            &run("bbbbbbbbbb", &[("timer/arm", 103.0, 0.01)]),
        );

        assert!(table.contains("| +3.0% |"), "{table}");
    }

    #[test]
    fn appearing_and_vanishing_benches_are_named() {
        let table = table(
            &run("aaaaaaaaaa", &[("gone", 10.0, 1.0)]),
            &run("bbbbbbbbbb", &[("new", 10.0, 1.0)]),
        );

        assert!(table.contains("| `new` | — | 10.0 ns | new |"), "{table}");
        assert!(table.contains("| `gone` | 10.0 ns | — | gone |"), "{table}");
    }

    #[test]
    fn other_schema_is_refused() -> Result<(), String> {
        let path = env::temp_dir().join("molt-bench-schema.json");
        let mut record = run("aaaaaaaaaa", &[("timer/arm", 10.0, 1.0)]);
        record.schema = SCHEMA + 1;
        write(&path, &record)?;

        let error = load(&path).unwrap_err();

        assert!(error.contains(&format!("is schema {}", SCHEMA + 1)), "{error}");
        Ok(())
    }

    #[test]
    fn machine_swap_is_declared() {
        let mut head = run("bbbbbbbbbb", &[("timer/arm", 40.0, 1.0)]);
        head.machine.cpu = "another".into();

        let table = table(&run("aaaaaaaaaa", &[("timer/arm", 100.0, 2.0)]), &head);

        assert!(table.starts_with("Base ran on cpu, head on another"), "{table}");
    }
}

// TODO:
// - Reduce the number of potential false positives by skipping non-pub methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use itertools::Itertools;
use log::*;
use protobuf::Message;
use scip::types::Occurrence;
use scip::types::{symbol_information::Kind, Document, SymbolInformation, SymbolRole};

#[derive(Parser)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
enum MainFlags {
    WorkspaceUnusedPub(Flags),
}

/// Detect unused pub methods in a workspace.
#[derive(clap::Args)]
#[command(version, about)]
struct Flags {
    #[clap(default_value_os_t = std::env::current_dir().unwrap())]
    workspace: PathBuf,
    #[clap(long)]
    scip: Option<PathBuf>,
    #[clap(long, value_delimiter = ',', default_value = "rs,html")]
    extensions: Vec<String>,
}

fn main_impl(args: MainFlags) -> anyhow::Result<()> {
    let MainFlags::WorkspaceUnusedPub(args) = args;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let scip = args
        .scip
        .unwrap_or_else(|| args.workspace.join("index.scip"));

    if !args.workspace.join("Cargo.toml").exists() {
        anyhow::bail!("{:?} does not contain a Cargo.toml file", args.workspace);
    }
    if !scip.exists() {
        warn!(
            "SCIP file not found at {:?}. Generating with rust-analyzer. This may take a while for large workspaces.",
            scip
        );
        duct::cmd!("rust-analyzer", "scip", &args.workspace, "--output", &scip)
            .dir(&args.workspace)
            .stdout_null()
            .stderr_null()
            .run()?;
    }
    info!("Running on {:?} with SCIP {:?}", args.workspace, scip);

    // Parse SCIP
    let reader = std::fs::File::open(scip)?;
    let mut reader = std::io::BufReader::new(reader);
    let index = scip::types::Index::parse_from_reader(&mut reader)?;
    debug!("Opened SCIP file with {} documents", index.documents.len());

    // Record method/function and traits declarations
    let mut declarations = HashMap::<&String, &SymbolInformation>::default();
    let mut traits = HashSet::<&String>::default();
    for doc in &index.documents {
        for s in &doc.symbols {
            let Ok(kind) = s.kind.enum_value() else {
                continue;
            };
            if kind == Kind::Trait {
                traits.insert(&s.display_name);
            }
            if kind != Kind::Method && kind != Kind::Function {
                continue;
            }
            declarations.insert(&s.symbol, s);
        }
    }
    debug!(
        "Found {} declarations and {} traits",
        declarations.len(),
        traits.len()
    );

    // Record occurrences
    for doc in &index.documents {
        for o in &doc.occurrences {
            if (o.symbol_roles & SymbolRole::Definition as i32) == 0 {
                declarations.remove(&o.symbol);
            }
        }
    }

    debug!("Pass 1: {} candidates", declarations.len());

    // Pass 2
    // Remove mains (which are never called)
    //        methods in tests (test methods are never called)
    //        trait methods (which may be called implicitly)
    // TODO: For the first two, only remove #[test] and #[main], #[tokio::main] methods.
    declarations.retain(|_, d| {
        !d.symbol.contains("test")
            && d.display_name != "main"
            && d.signature_documentation
                .as_ref()
                .map(|f| !f.relative_path.contains("test"))
                .unwrap_or(true)
            && traits.iter().all(|t| !d.symbol.contains(*t))
    });
    debug!(
        "Pass 2 (mains, tests, trait methods): {} candidates",
        declarations.len()
    );

    // Pass 3: Grep for candidates
    let mut counts = HashMap::<&String, usize>::default();
    let extensions: HashSet<String> = args.extensions.into_iter().collect();
    walkdir::WalkDir::new(&args.workspace)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| !e.path().join("CACHEDIR.TAG").exists())
        .filter_map(|e| e.ok())
        .filter(|f| {
            f.file_type().is_file()
                && f.path()
                    .extension()
                    .and_then(|f| f.to_str())
                    .map_or(false, |e| extensions.contains(e))
        })
        .for_each(|f| {
            let contents = std::fs::read_to_string(f.path()).unwrap();
            for line in contents.lines() {
                for d in declarations.values() {
                    if line.contains(&d.display_name) {
                        *counts.entry(&d.symbol).or_default() += 1;
                    }
                }
            }
        });
    declarations.retain(|d, _| counts.get(d).copied().unwrap_or_default() <= 1);
    debug!("Pass 3 (search): {} candidates", declarations.len());
    let n_found = declarations.len();
    info!("Found {} possibly unused functions", n_found);

    // Find occurrence with definition to get the position in the file
    // TODO: Doing that earlier woud allow detecting the #[test], #[main], etc.
    let mut declarations_occurrences: Vec<(&Document, &Occurrence)> = vec![];
    for d in &index.documents {
        for o in &d.occurrences {
            if declarations.contains_key(&o.symbol)
                && (o.symbol_roles & SymbolRole::Definition as i32) > 0
            {
                declarations_occurrences.push((&d, &o));
                declarations.remove(&o.symbol);
            }
        }
    }
    // Group by file
    let mut declarations_occurrences = declarations_occurrences
        .into_iter()
        .map(|(d, o)| (&d.relative_path, o))
        .into_group_map()
        .into_iter()
        .collect_vec();
    declarations_occurrences.sort_by_key(|(d, _)| *d);
    // Display
    for (path, mut occs) in declarations_occurrences {
        let full_path = args.workspace.join(path);
        if !full_path.exists() {
            warn!("{} not found, is the SCIP file up-to-date?", path);
            continue;
        }
        let lines = std::fs::read_to_string(full_path)?;
        let lines: Vec<&str> = lines.lines().collect();
        occs.sort_by_key(|occ| occ.range[0]);
        println!("{}", path.yellow());
        for occ in occs {
            let line = occ.range[0] as usize;
            println!("{:<4} {}", (line + 1).to_string().blue(), lines[line]);
        }
        println!();
    }
    anyhow::ensure!(n_found == 0, "Found {} possibly unused functions", n_found);
    Ok(())
}

fn main() {
    if let Err(e) = main_impl(MainFlags::parse()) {
        error!("{}", e);
        std::process::exit(2);
    }
}

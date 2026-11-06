use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use moteado::{Kind, Target, discover, link_text, resolve, resolve_discovered};
use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

const HELP: &str = "moteado [OPTIONS] <INPUT|->\n\
Link bare Wikidata Q/P IDs in Markdown, Quarto, text or tabular files.\n\
  -o, --output FILE       Write output to FILE (default: stdout)\n\
      --in-place         Atomically replace INPUT (cannot combine with --output)\n\
      --log FILE          Write review notes to FILE (default: stderr)\n\
      --chemical NAME     Link occurrences of an exact English chemical name (repeatable)\n\
      --taxon NAME        Link occurrences of an exact scientific taxon name (repeatable)\n\
      --auto-taxa        Discover likely scientific binomials and verify them\n\
      --auto-chemicals   Discover chemical-looking names and verify them\n\
  -h, --help              Show this help\n\
INPUT - reads stdin. Existing Markdown links, code and URLs are left unchanged.\n";

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
    {
        return true;
    }
    let parent = |p: &std::path::Path| {
        let dir = p
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        fs::canonicalize(dir).ok()
    };
    a.file_name()
        .zip(b.file_name())
        .is_some_and(|(a_name, b_name)| {
            a_name == b_name && parent(a).zip(parent(b)).is_some_and(|(a, b)| a == b)
        })
}

fn replace_in_place(path: &Path, original: &str, linked: &str) -> io::Result<()> {
    if original == linked {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || fs::read_to_string(path)? != original {
        return Err(io::Error::other(
            "input changed while processing; refusing to overwrite it",
        ));
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary
        .as_file_mut()
        .set_permissions(metadata.permissions())?;
    temporary.write_all(linked.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

const MAX_AUTO_CANDIDATES: usize = 300; // per category, at most ten batched lookups

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut input: Option<String> = None;
    let mut output: Option<PathBuf> = None;
    let mut log: Option<PathBuf> = None;
    let mut targets: Vec<Target> = Vec::new();
    let mut in_place = false;
    let mut auto_taxa = false;
    let mut auto_chemicals = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "--auto-taxa" => auto_taxa = true,
            "--in-place" => in_place = true,
            "--auto-chemicals" => auto_chemicals = true,
            "-o" | "--output" | "--log" | "--chemical" | "--taxon" => {
                let value = args.next().ok_or(format!("missing value for {arg}"))?;
                if value.trim().is_empty() {
                    return Err(format!("empty value for {arg}").into());
                }
                match arg.as_str() {
                    "-o" | "--output" => output = Some(value.into()),
                    "--log" => log = Some(value.into()),
                    "--chemical" | "--taxon" => {
                        let kind = if arg == "--taxon" {
                            Kind::Taxon
                        } else {
                            Kind::Chemical
                        };
                        if targets.iter().any(|t| t.name.eq_ignore_ascii_case(&value)) {
                            return Err(format!("duplicate target name {value:?}").into());
                        }
                        targets.push(Target {
                            name: value,
                            kind,
                            id: None,
                        });
                    }
                    _ => unreachable!(),
                }
            }
            "-" if input.is_none() => input = Some(arg),
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option {arg}; see --help").into());
            }
            _ if input.is_none() => input = Some(arg),
            _ => return Err("only one input file is supported".into()),
        }
    }
    let input = input.ok_or("missing INPUT; see --help")?;
    if in_place && (input == "-" || output.is_some()) {
        return Err("--in-place needs a file input and cannot be combined with --output".into());
    }
    if in_place && !fs::symlink_metadata(&input)?.file_type().is_file() {
        return Err("--in-place requires a regular file (not a symlink)".into());
    }
    let input_path = std::path::Path::new(&input);
    if input != "-"
        && (output.as_deref().is_some_and(|p| same_file(p, input_path))
            || log.as_deref().is_some_and(|p| same_file(p, input_path)))
    {
        return Err("output and log must differ from input".into());
    }
    if output
        .as_deref()
        .zip(log.as_deref())
        .is_some_and(|(a, b)| same_file(a, b))
    {
        return Err("output and log must be different files".into());
    }
    let mut text = String::new();
    if input == "-" {
        io::stdin().read_to_string(&mut text)?;
    } else {
        text = fs::read_to_string(&input)?;
    }
    let mut notes = Vec::new();
    let mut plans = Vec::new();
    for (enabled, kind) in [(auto_taxa, Kind::Taxon), (auto_chemicals, Kind::Chemical)] {
        if !enabled {
            continue;
        }
        let candidates = discover(&text, kind);
        if candidates.len() > MAX_AUTO_CANDIDATES {
            notes.push(format!(
                "REVIEW auto {kind:?}: candidate limit {MAX_AUTO_CANDIDATES}; skipped {} names",
                candidates.len() - MAX_AUTO_CANDIDATES
            ));
        }
        let candidates: Vec<_> = candidates
            .into_iter()
            .take(MAX_AUTO_CANDIDATES)
            .filter(|name| !targets.iter().any(|t| t.name.eq_ignore_ascii_case(name)))
            .collect();
        plans.push((kind, candidates));
    }
    let total = targets.len()
        + plans
            .iter()
            .map(|(_, names): &(_, Vec<String>)| names.len())
            .sum::<usize>();
    let bar = ProgressBar::new(total as u64);
    if io::stderr().is_terminal() && total > 0 {
        bar.set_style(ProgressStyle::with_template(
            "{spinner:.green} [{bar:30.cyan/blue}] {pos}/{len} {msg}",
        )?);
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
    } else {
        bar.set_draw_target(ProgressDrawTarget::hidden());
    }
    let mut completed = 0;
    for target in &mut targets {
        bar.set_message(format!("Resolving {:?}: {}", target.kind, target.name));
        let (id, review) = resolve(&target.name, target.kind);
        target.id = id;
        notes.extend(review);
        completed += 1;
        bar.inc(1);
    }
    for (kind, names) in plans {
        let mut candidates = Vec::new();
        for name in names {
            if targets.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
                completed += 1;
                bar.inc(1);
            } else {
                candidates.push(name);
            }
        }
        bar.set_message(format!("Resolving {kind:?} candidates"));
        let (resolved, review) = resolve_discovered(&candidates, kind, |count| {
            completed += count;
            bar.inc(count as u64);
        });
        notes.extend(review);
        targets.extend(resolved);
    }
    bar.finish_and_clear();
    notes.push(format!(
        "PROGRESS name lookups: {completed}/{total} candidates processed"
    ));
    let (linked, review) = link_text(&text, &targets);
    let lookup_failed = notes.iter().any(|n| n.starts_with("ERROR"));
    let mut inplace_error: Option<Box<dyn std::error::Error>> = None;
    if in_place {
        if lookup_failed {
            inplace_error = Some("lookup failed; original input was not modified".into());
        } else if let Err(error) = replace_in_place(input_path, &text, &linked) {
            inplace_error = Some(error.into());
        }
        if let Some(error) = &inplace_error {
            notes.push(format!("ERROR in-place: {error}"));
        } else {
            notes.extend(review);
        }
    } else {
        notes.extend(review);
        if let Some(path) = output {
            fs::write(path, linked)?;
        } else {
            io::stdout().write_all(linked.as_bytes())?;
        }
    }
    if let Some(path) = log {
        fs::write(path, notes.join("\n") + "\n")?;
    } else {
        for note in &notes {
            eprintln!("{note}");
        }
    }
    if let Some(error) = inplace_error {
        return Err(error);
    }
    if lookup_failed {
        return Err("Wikidata lookup failed; inspect review log".into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("moteado: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_preserves_permissions() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("manuscript.qmd");
        fs::write(&file, "Q27133\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;
        }
        let permissions = fs::metadata(&file)?.permissions();
        replace_in_place(
            &file,
            "Q27133\n",
            "[Q27133](https://www.wikidata.org/wiki/Q27133)\n",
        )?;
        assert_eq!(
            fs::read_to_string(&file)?,
            "[Q27133](https://www.wikidata.org/wiki/Q27133)\n"
        );
        assert_eq!(fs::metadata(&file)?.permissions(), permissions);
        Ok(())
    }

    #[test]
    fn rejects_intervening_edits_without_overwriting() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("input.txt");
        fs::write(&file, "new user text")?;
        let error = replace_in_place(&file, "old text", "[old text](https://example.org)")
            .expect_err("must not overwrite changed input");
        assert!(error.to_string().contains("input changed"));
        assert_eq!(fs::read_to_string(file)?, "new user text");
        Ok(())
    }
}

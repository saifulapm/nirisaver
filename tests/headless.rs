//! The headless path, exercised through the binary.
//!
//! The unit tests cover the animator; this covers the thing a person or a CI
//! job actually runs, including that a seed makes the printed output a fixed
//! string and that the flags reach the code that acts on them.

use std::path::PathBuf;
use std::process::Command;

fn nirisaver() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nirisaver"));
    // The point of `--no-config` here is that the test must not depend on
    // whatever is in the developer's config directory.
    cmd.arg("--no-config");
    cmd
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = nirisaver().args(args).output().expect("running nirisaver");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const HEADLESS: [&str; 9] =
    ["--headless", "--frames", "150", "--cols", "72", "--rows", "20", "--seed", "4242"];

#[test]
fn a_seeded_headless_run_is_a_fixed_string() {
    let (ok, first, err) = run(&HEADLESS);
    assert!(ok, "{err}");
    let (_, second, _) = run(&HEADLESS);
    assert_eq!(first, second);
    assert!(first.contains("grid=72x20 presented=150"), "{first}");
    assert!(first.contains("checksum="), "{first}");
}

#[test]
fn changing_the_seed_changes_the_output() {
    let mut other = HEADLESS;
    other[8] = "9001";
    let (_, a, _) = run(&HEADLESS);
    let (_, b, _) = run(&other);
    assert_ne!(a, b);
}

#[test]
fn dumping_the_grid_prints_the_rows() {
    let mut args = HEADLESS.to_vec();
    args.push("--dump-grid");
    let (ok, out, err) = run(&args);
    assert!(ok, "{err}");
    // One summary line plus one line per grid row.
    assert_eq!(out.lines().count(), 21, "{out}");
}

#[test]
fn the_effect_list_is_not_empty_and_names_are_accepted() {
    let (ok, out, err) = run(&["--list-effects"]);
    assert!(ok, "{err}");
    let names: Vec<_> = out.lines().collect();
    assert!(names.len() > 20, "expected ttfx's whole registry, got {}", names.len());
    assert!(names.contains(&"matrix"));

    let mut args = HEADLESS.to_vec();
    args.extend(["--effect", "matrix"]);
    assert!(run(&args).0);
}

#[test]
fn an_unknown_effect_is_refused_with_the_list() {
    let mut args = HEADLESS.to_vec();
    args.extend(["--effect", "nosucheffect"]);
    let (ok, _, err) = run(&args);
    assert!(!ok);
    assert!(err.contains("nosucheffect") && err.contains("available effects"), "{err}");
}

#[test]
fn a_quote_list_reaches_the_layout() {
    let path = scratch_file(
        "quotes.txt",
        "# a list\n\nThe measure of intelligence is the ability to change. — Albert Einstein\n",
    );
    let mut args = HEADLESS.to_vec();
    args.extend(["--quotes", path.to_str().unwrap(), "--hold", "200", "--wrap", "40"]);
    args.push("--dump-grid");
    let (ok, out, err) = run(&args);
    assert!(ok, "{err}");
    // Long enough for the effect to finish and settle on the laid-out quote.
    let mut args = args.clone();
    args[2] = "1500";
    let (ok, out_long, err) = run(&args);
    assert!(ok, "{err}");
    assert!(out_long.contains("— Albert Einstein"), "attribution never landed:\n{out_long}");
    assert!(!out.is_empty());
}

#[test]
fn a_custom_separator_reaches_the_layout() {
    let path = scratch_file("piped.txt", "Perfection is achieved | Antoine de Saint-Exupery\n");
    let mut args = HEADLESS.to_vec();
    args[2] = "1500";
    args.extend([
        "--quotes",
        path.to_str().unwrap(),
        "--separator",
        " | ",
        "--hold",
        "200",
        "--wrap",
        "40",
        "--dump-grid",
    ]);
    let (ok, out, err) = run(&args);
    assert!(ok, "{err}");
    assert!(out.contains("— Antoine de Saint-Exupery"), "{out}");
}

#[test]
fn help_mentions_every_flag_the_readme_documents() {
    let out = Command::new(env!("CARGO_BIN_EXE_nirisaver"))
        .arg("--help")
        .output()
        .expect("running nirisaver --help");
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--config",
        "--no-config",
        "--source",
        "--quotes",
        "--separator",
        "--attribution-prefix",
        "--text",
        "--text-file",
        "--align",
        "--wrap",
        "--hold",
        "--fade-in",
        "--fade-out",
        "--frame-rate",
        "--effect",
        "--include-effects",
        "--exclude-effects",
        "--list-effects",
        "--font",
        "--font-size",
        "--line-height",
        "--background",
        "--foreground",
        "--seed",
        "--headless",
        "--frames",
        "--cols",
        "--rows",
        "--dump-grid",
    ] {
        assert!(help.contains(flag), "--help does not document {flag}");
    }
}

fn scratch_file(name: &str, body: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("nirisaver-headless");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

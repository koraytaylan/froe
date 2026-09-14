//! The Oak-side judge: small Java classes compiled and run inside the pinned
//! image, so the suite can *ask Oak questions* rather than only make Oak
//! consume froe's output.
//!
//! The image ships a full Temurin 21 JDK and the Oak 1.90.0 runtime bundles,
//! but **not** `oak-run` or `oak-run-commons`. Every class the judge uses
//! therefore comes from a shipped bundle, and where a plan relies on an
//! `oak-run-commons` helper the judge re-implements it from those classes.
//! `javac` succeeding is itself the assertion that every class the judge
//! needs is in the image — the Felix inventory types Oak's printers
//! implement included.
//!
//! # Why the classes are compiled rather than committed
//!
//! A committed `.class` file would be compiled against whatever JDK and Oak
//! happened to be on the machine that produced it, and would keep working
//! against an image it no longer matches. Compiling in the image, once per
//! suite process, makes the class path the image's own and makes a mismatch
//! a compile error rather than a silent `NoSuchMethodError` three phases
//! later.
//!
//! # Why the cache key includes the image
//!
//! The class directory is cached under a name derived from a hash of the
//! sources **and** the resolved image reference. A changed judge and a
//! different image (the canary's floating tag included) each miss the cache,
//! so neither can run against classes compiled for the other.

use super::*;

/// Where the judge's Java sources live, relative to this crate.
const JUDGE_SOURCE_DIRECTORY: &str = "tests/interop/judge";

/// The compiled judge, one directory per (sources, image) pair.
pub(crate) struct Judge {
    classes: PathBuf,
}

static COMPILED_JUDGE: OnceLock<Judge> = OnceLock::new();

impl Judge {
    /// Compiles every `.java` under `judge/` once per suite process.
    pub(crate) fn compile() -> &'static Judge {
        COMPILED_JUDGE.get_or_init(|| {
            let sources = judge_source_directory();
            let image = sling_image();
            let classes = work_root().join(format!(
                "judge-classes-{:016x}",
                judge_cache_key(&sources, &image)
            ));
            // A cache hit is the marker file, not the directory: a directory
            // that exists because a previous run was interrupted mid-compile
            // would otherwise be taken for a complete one.
            let marker = classes.join(".compiled");
            if marker.exists() {
                return Judge { classes };
            }
            let _ = std::fs::remove_dir_all(&classes);
            std::fs::create_dir_all(&classes).expect("create the judge class directory");

            let container = OneOffContainer {
                image,
                mounts: vec![
                    Mount::read_only(&sources, "/judge"),
                    Mount::writable(&classes, "/classes"),
                ],
                entrypoint: "/bin/sh",
            };
            let output = run_one_off(
                &container,
                &[
                    "-c",
                    "CP=$(find /opt/sling/artifacts -name '*.jar' | tr '\\n' ':'); \
                     javac -nowarn -cp \"$CP\" -d /classes /judge/*.java",
                ],
            );
            assert!(
                output.status.success(),
                "compiling the judge failed — every class it uses must come from a \
                 bundle the image ships\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::write(&marker, "").expect("write the judge's compiled marker");
            Judge { classes }
        })
    }

    /// Runs `class_name`, asserting it succeeded, and returns its standard
    /// output.
    ///
    /// Standard output is **not** where a judge class that opens a segment
    /// store puts its data: opening one starts Oak's logging, whose console
    /// appender writes to standard output, so those classes take an explicit
    /// output path and this returns whatever the JVM logged. The classes
    /// that only open a Lucene directory keep standard output clean and use
    /// it for their one value.
    pub(crate) fn run(&self, class_name: &str, arguments: &[&str], mounts: Vec<Mount>) -> String {
        let output = self.invoke(class_name, arguments, mounts);
        assert!(
            output.status.success(),
            "the judge's {class_name} {arguments:?} exited with {status}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            status = output.status
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs `class_name`, asserting it *failed*, and returns its standard
    /// error.
    ///
    /// A judge class that renders only a verdict commits nothing and writes
    /// nothing to standard output: it exits non-zero with the offending item
    /// named on standard error. This is the twin of `froe_failure` beside
    /// `froe`, and every refusal assertion in this plan and the later ones
    /// goes through it.
    pub(crate) fn run_failure(
        &self,
        class_name: &str,
        arguments: &[&str],
        mounts: Vec<Mount>,
    ) -> String {
        let output = self.invoke(class_name, arguments, mounts);
        assert!(
            !output.status.success(),
            "the judge's {class_name} {arguments:?} was expected to refuse and \
             succeeded instead\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn invoke(
        &self,
        class_name: &str,
        arguments: &[&str],
        mut mounts: Vec<Mount>,
    ) -> std::process::Output {
        mounts.push(Mount::read_only(&self.classes, "/classes"));
        let container = OneOffContainer {
            image: sling_image(),
            mounts,
            entrypoint: "/bin/sh",
        };
        let script = format!(
            "CP=/classes:$(find /opt/sling/artifacts -name '*.jar' | tr '\\n' ':'); \
             exec java -cp \"$CP\" {class_name} {}",
            arguments
                .iter()
                .map(|argument| shell_quote(argument))
                .collect::<Vec<_>>()
                .join(" ")
        );
        run_one_off(&container, &["-c", &script])
    }
}

/// The judge's source directory on the host.
pub(crate) fn judge_source_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(JUDGE_SOURCE_DIRECTORY)
}

/// A cache key over every source byte and the resolved image reference.
///
/// FNV-1a rather than a dependency: this is a cache key, not a checksum
/// anything trusts, and the whole of it is eleven lines.
fn judge_cache_key(sources: &Path, image: &str) -> u64 {
    let mut names: Vec<PathBuf> = std::fs::read_dir(sources)
        .unwrap_or_else(|error| panic!("read {}: {error}", sources.display()))
        .map(|entry| entry.expect("read a judge source entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    assert!(
        !names.is_empty(),
        "no judge sources in {}",
        sources.display()
    );
    // Sorted, so the key does not depend on directory order.
    names.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut absorb = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    absorb(image.as_bytes());
    for name in &names {
        absorb(
            name.file_name()
                .expect("a judge source has a file name")
                .as_encoded_bytes(),
        );
        absorb(
            &std::fs::read(name).unwrap_or_else(|error| panic!("read {}: {error}", name.display())),
        );
    }
    hash
}

/// Single-quotes an argument for the container's `sh -c`.
///
/// The arguments are paths this suite chose, but quoting them is what keeps
/// a path with a space from silently becoming two arguments — a failure that
/// would be attributed to the judge rather than to the harness.
fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', r"'\''"))
}

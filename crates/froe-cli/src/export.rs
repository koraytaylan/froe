//! Running an export: JSON lines, Parquet full or refreshed, and `SQLite`,
//! with the step reporting each one shares.

use super::*;

/// Begins a reported step and ends it when dropped, so a step closes on
/// every path out of an export — including the early returns that remove
/// a partial output file.
///
/// The step leaves no completion line: every command that opens one goes
/// on to report its own outcome, naming the destination the step cannot,
/// and two lines saying the same thing is worse than one.
pub(crate) struct ReportedStep {
    pub(crate) reporter: Reporter,
}

impl ReportedStep {
    fn begin(reporter: &Reporter, description: &'static str, unit: froe::WorkUnit) -> Self {
        let mut reporter = reporter.clone();
        reporter.step_began(&froe::Step::new(description, unit));
        Self { reporter }
    }

    /// The step an export's node stream advances.
    fn exporting(reporter: &Reporter) -> Self {
        Self::begin(reporter, "exporting nodes", froe::WorkUnit::Nodes)
    }

    /// The step a Parquet refresh's delta export advances.
    fn refreshing(reporter: &Reporter) -> Self {
        Self::begin(
            reporter,
            "re-exporting changed nodes",
            froe::WorkUnit::Nodes,
        )
    }
}

impl Drop for ReportedStep {
    fn drop(&mut self) {
        self.reporter.end_step_without_completion_line();
    }
}

/// What an export run did, for the summary the caller prints.
pub(crate) enum ExportRun {
    /// Rows streamed out; the caller prints the node-count summary.
    Exported {
        /// How many nodes were written.
        nodes: u64,
        /// How long the export took.
        elapsed: std::time::Duration,
    },
    /// The run reported itself (a Parquet refresh).
    Reported,
    /// The path does not exist.
    Missing,
}

/// `froe export`, which dispatches on the output format it was given.
pub(crate) fn run_export_command(command: Command, reporter: &Reporter) -> froe::Result<ExitCode> {
    match command {
        Command::Export {
            repository: repository_path,
            path,
            depth,
            format,
            output,
            full,
        } => {
            if full && format != ExportFormat::Parquet {
                eprintln!("froe: --full applies only to the parquet format");
                return Ok(ExitCode::FAILURE);
            }
            let run = match format {
                ExportFormat::JsonLines => run_json_lines_export(
                    &repository_path,
                    &path,
                    depth,
                    output.as_deref(),
                    reporter,
                )?,
                ExportFormat::Parquet => {
                    let Some(output_directory) = output.as_deref() else {
                        eprintln!(
                            "froe: the parquet format writes nodes.parquet and \
                             properties.parquet; pass --output <directory>"
                        );
                        return Ok(ExitCode::FAILURE);
                    };
                    let mode = if full {
                        ParquetExportMode::Rebuild
                    } else {
                        ParquetExportMode::RefreshInPlace
                    };
                    run_parquet_export(
                        &repository_path,
                        &path,
                        depth,
                        output_directory,
                        mode,
                        reporter,
                    )?
                }
                ExportFormat::Sqlite => {
                    let Some(output_path) = output.as_deref() else {
                        eprintln!(
                            "froe: the sqlite format writes a single database file; \
                             pass --output <file>"
                        );
                        return Ok(ExitCode::FAILURE);
                    };
                    run_sqlite_export(&repository_path, &path, depth, output_path, reporter)?
                }
            };
            match run {
                ExportRun::Exported { nodes, elapsed } => {
                    let count = progress::format_count(nodes);
                    let took = progress::format_duration(elapsed);
                    let rate = progress::format_rate(nodes, elapsed);
                    reporter.status(&match &output {
                        Some(destination) => format!(
                            "exported {count} nodes to {} in {took}{rate}",
                            output::sanitize_terminal_path(destination),
                        ),
                        None => format!("exported {count} nodes in {took}{rate}"),
                    });
                }
                // A refresh reports itself.
                ExportRun::Reported => {}
                ExportRun::Missing => {
                    eprintln!("froe: no node at {}", output::sanitize_terminal_text(&path));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        other => return run_diagnostic_command(other, reporter),
    }
    Ok(ExitCode::SUCCESS)
}

/// Streams a JSON lines export to `output`, or to standard output when
/// `output` is `None`; a freshly created output file never lingers
/// after either failure shape.
pub(crate) fn run_json_lines_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output: Option<&Path>,
    reporter: &Reporter,
) -> froe::Result<ExportRun> {
    let repository = open_repository(repository_path, reporter)?;
    if let Some(output_path) = output {
        let file = froe_export::create_export_output(repository_path, output_path)?;
        let _step = ReportedStep::exporting(reporter);
        let mut sink = progress::ProgressSink::new(
            froe_export::JsonLinesSink::new(std::io::BufWriter::with_capacity(1 << 20, file)),
            reporter.clone(),
        );
        match froe_export::export_subtree(&repository, path, depth, &mut sink) {
            Ok(written) => {
                let elapsed = sink.elapsed();
                if written.is_none() {
                    // Nothing was exported: the freshly created, empty
                    // output file must not linger either.
                    drop(sink);
                    let _ = std::fs::remove_file(output_path);
                }
                Ok(match written {
                    Some(nodes) => ExportRun::Exported { nodes, elapsed },
                    None => ExportRun::Missing,
                })
            }
            Err(error) => {
                // The file was freshly created above; a partial export
                // must not linger as if complete.
                drop(sink);
                let _ = std::fs::remove_file(output_path);
                Err(error)
            }
        }
    } else {
        let standard_output = std::io::stdout();
        // An export streaming to a terminal shares the screen with the
        // report, and a live line drawn across it would corrupt both. A
        // redirected export keeps its progress.
        let reporter = if standard_output.is_terminal() {
            Reporter::silent()
        } else {
            reporter.clone()
        };
        let _step = ReportedStep::exporting(&reporter);
        let mut sink = progress::ProgressSink::new(
            froe_export::JsonLinesSink::new(std::io::BufWriter::with_capacity(
                1 << 20,
                standard_output.lock(),
            )),
            reporter.clone(),
        );
        let written = froe_export::export_subtree(&repository, path, depth, &mut sink)?;
        Ok(match written {
            Some(nodes) => ExportRun::Exported {
                nodes,
                elapsed: sink.elapsed(),
            },
            None => ExportRun::Missing,
        })
    }
}

/// Whether a Parquet export refreshes the existing tables or rebuilds them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParquetExportMode {
    /// Refresh in place, decoding only what changed since the last export.
    RefreshInPlace,
    /// Rebuild every table from scratch, as `--full` requests.
    Rebuild,
}

/// Why a from-scratch Parquet export is running, which decides whether an
/// existing usable export may be replaced without further authorization.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebuildReason {
    /// The operator passed `--full`, which authorizes the replacement.
    OperatorRequested,
    /// A refresh could not proceed, so replacement needs the directory's
    /// own state to authorize it.
    RefreshUnavailable,
}

/// Brings the Parquet export in `output_directory` up to date: an
/// existing, usable export is refreshed in place — decoding only what
/// changed since it was taken — and anything else (a first export, an
/// unusable base, `--full`) falls to a from-scratch export that
/// replaces the previous tables, one atomic swap per file.
pub(crate) fn run_parquet_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    mode: ParquetExportMode,
    reporter: &Reporter,
) -> froe::Result<ExportRun> {
    let repository = open_repository(repository_path, reporter)?;
    froe_export::create_export_directory(repository_path, output_directory)?;
    if mode == ParquetExportMode::Rebuild {
        let FullExport::Completed(run) = run_full_parquet_export(
            &repository,
            path,
            depth,
            output_directory,
            RebuildReason::OperatorRequested,
            reporter,
        )?
        else {
            unreachable!("an operator-requested rebuild never defers to a refresh");
        };
        return Ok(run);
    }
    let mut refresh_reporter = reporter.clone();
    // A rival writer can turn the directory back into a valid export
    // between the refresh attempt and the fallback's lock; a few
    // rounds settle it either way.
    for _attempt in 0..4 {
        let started = std::time::Instant::now();
        let refresh = {
            let _step = ReportedStep::refreshing(reporter);
            froe_export::refresh_parquet_export(
                &repository,
                path,
                depth,
                output_directory,
                &froe_export::ParquetExportOptions::default(),
                &mut |nodes| refresh_reporter.step_advanced(nodes),
            )?
        };
        match refresh {
            froe_export::ParquetRefresh::Missing => {
                // The full export's own missing-path verdict, reached
                // through the refresh — the existing export is intact.
                return Ok(ExportRun::Missing);
            }
            froe_export::ParquetRefresh::Current { revision } => {
                reporter.status(&format!(
                    "the export in {} is already current ({revision})",
                    output::sanitize_terminal_path(output_directory)
                ));
                return Ok(ExportRun::Reported);
            }
            froe_export::ParquetRefresh::Refreshed {
                revision,
                ranges,
                nodes,
            } => {
                reporter.status(&format!(
                    "refreshed the export in {} to {revision}: \
                     {ranges} changed ranges, {nodes} nodes re-exported in {:.1}s",
                    output::sanitize_terminal_path(output_directory),
                    started.elapsed().as_secs_f64()
                ));
                return Ok(ExportRun::Reported);
            }
            froe_export::ParquetRefresh::NotReusable {
                reason,
                replaceable,
            } => {
                if !replaceable {
                    // Foreign data and other scopes are never replaced
                    // uninvited — the same hard refusal the streaming
                    // formats give an existing output file.
                    return Err(froe::Error::InvalidFormat {
                        details: format!(
                            "{reason}; refusing to replace it — pass --full to rebuild anyway"
                        ),
                    });
                }
                // A first export is the quiet case; anything present
                // but unusable deserves the reason before it is
                // replaced.
                let leftovers = [
                    froe_export::NODES_FILE_NAME,
                    froe_export::PROPERTIES_FILE_NAME,
                ]
                .iter()
                .any(|name| output_directory.join(name).exists());
                if leftovers {
                    reporter.status(&format!("{reason}; exporting from scratch"));
                }
                match run_full_parquet_export(
                    &repository,
                    path,
                    depth,
                    output_directory,
                    RebuildReason::RefreshUnavailable,
                    reporter,
                )? {
                    FullExport::Completed(run) => return Ok(run),
                    // A valid export appeared under the lock; the next
                    // round refreshes it.
                    FullExport::RefreshInstead => {}
                }
            }
        }
    }
    Err(froe::Error::InvalidFormat {
        details: format!(
            "the export at {} keeps changing underneath; re-run",
            output_directory.display()
        ),
    })
}

/// Authorizes an automatic (unforced) replacement under the held
/// export lock, against the files as they are now: `false` defers to a
/// refresh round, a guarded directory is the hard refusal — decided
/// under the lock, not inherited from the earlier refresh attempt.
pub(crate) fn authorize_replacement(
    repository: &Repository,
    output_directory: &Path,
    path: &str,
    depth: Option<usize>,
) -> froe::Result<bool> {
    match froe_export::assess_export(repository, output_directory, path, depth) {
        froe_export::ExportAssessment::Reusable => Ok(false),
        froe_export::ExportAssessment::Replaceable(_) => Ok(true),
        froe_export::ExportAssessment::Guarded(reason) => Err(froe::Error::InvalidFormat {
            details: format!("{reason}; refusing to replace it — pass --full to rebuild anyway"),
        }),
    }
}

/// The outcome of a full Parquet export.
pub(crate) enum FullExport {
    /// The export ran; the usual run outcome.
    Completed(ExportRun),
    /// Under the replacement lock, the directory turned out to hold a
    /// valid, refreshable export again — a rival writer published one
    /// between the refresh attempt and this fallback — so the caller
    /// defers to a refresh round instead of replacing it.
    RefreshInstead,
}

/// Exports the Parquet tables from scratch into `output_directory`.
/// The new files are written under temporary names and atomically
/// moved over any existing export only once complete: a failure before
/// the swap leaves the previous export untouched, and a failure between
/// the two swaps leaves disagreeing stamps, which the next refresh
/// rebuilds from. The directory's export lock is held throughout,
/// serializing concurrent writers.
///
/// Unless `forced`, the replacement is authorized afresh under the
/// lock — a verdict the earlier refresh attempt reached before its own
/// lock was released cannot be trusted by the time this lock is held.
/// Finding a valid, refreshable export there defers to a refresh round
/// rather than bulldozing it with a staler full export.
pub(crate) fn run_full_parquet_export(
    repository: &Repository,
    path: &str,
    depth: Option<usize>,
    output_directory: &Path,
    reason: RebuildReason,
    reporter: &Reporter,
) -> froe::Result<FullExport> {
    let _lock = froe_export::lock_export_directory(output_directory)?;
    if reason == RebuildReason::RefreshUnavailable
        && !authorize_replacement(repository, output_directory, path, depth)?
    {
        return Ok(FullExport::RefreshInstead);
    }
    for file_name in [
        froe_export::NODES_FILE_NAME,
        froe_export::PROPERTIES_FILE_NAME,
    ] {
        froe_export::sweep_temporary_outputs(output_directory, file_name)?;
    }
    let nodes_temporary = output_directory.join(froe_export::temporary_output_name(
        froe_export::NODES_FILE_NAME,
    ));
    let properties_temporary = output_directory.join(froe_export::temporary_output_name(
        froe_export::PROPERTIES_FILE_NAME,
    ));
    let remove_temporaries = || {
        let _ = std::fs::remove_file(&nodes_temporary);
        let _ = std::fs::remove_file(&properties_temporary);
    };

    let nodes_file =
        match froe_export::create_export_output(repository.directory(), &nodes_temporary) {
            Ok(file) => file,
            Err(error) => {
                remove_temporaries();
                return Err(error);
            }
        };
    let properties_file =
        match froe_export::create_export_output(repository.directory(), &properties_temporary) {
            Ok(file) => file,
            Err(error) => {
                remove_temporaries();
                return Err(error);
            }
        };
    let provenance = froe_export::ExportProvenance::new(
        repository.head_record_identifier().to_string(),
        path,
        depth,
    );
    let parquet_sink = match froe_export::ParquetSink::new_with_provenance(
        std::io::BufWriter::with_capacity(1 << 20, nodes_file),
        std::io::BufWriter::with_capacity(1 << 20, properties_file),
        &froe_export::ParquetExportOptions::default(),
        &provenance,
    ) {
        Ok(sink) => sink,
        Err(error) => {
            remove_temporaries();
            return Err(error);
        }
    };
    let _step = ReportedStep::exporting(reporter);
    let mut sink = progress::ProgressSink::new(parquet_sink, reporter.clone());
    match froe_export::export_subtree(repository, path, depth, &mut sink) {
        Ok(written) => {
            let elapsed = sink.elapsed();
            // Close the files before the rename: the sink's finish has
            // flushed them, and an open handle would block the move on
            // Windows.
            drop(sink);
            let Some(nodes) = written else {
                remove_temporaries();
                return Ok(FullExport::Completed(ExportRun::Missing));
            };
            let renamed = froe_export::replace_export_output(
                &nodes_temporary,
                &output_directory.join(froe_export::NODES_FILE_NAME),
            )
            .and_then(|()| {
                froe_export::replace_export_output(
                    &properties_temporary,
                    &output_directory.join(froe_export::PROPERTIES_FILE_NAME),
                )
            });
            match renamed {
                Ok(()) => Ok(FullExport::Completed(ExportRun::Exported {
                    nodes,
                    elapsed,
                })),
                Err(error) => {
                    remove_temporaries();
                    Err(error)
                }
            }
        }
        Err(error) => {
            // The temporary files hold a partial export; the existing
            // export, if any, stays untouched.
            drop(sink);
            remove_temporaries();
            Err(error)
        }
    }
}

/// Exports the `SQLite` database into the single file `output_path`.
/// A freshly created file never lingers after either failure shape —
/// the sink's drop implementation owns that cleanup.
pub(crate) fn run_sqlite_export(
    repository_path: &Path,
    path: &str,
    depth: Option<usize>,
    output_path: &Path,
    reporter: &Reporter,
) -> froe::Result<ExportRun> {
    let repository = open_repository(repository_path, reporter)?;
    let _step = ReportedStep::exporting(reporter);
    let mut sink = progress::ProgressSink::new(
        froe_export::SqliteSink::create(
            repository_path,
            output_path,
            froe_export::SqliteExportOptions::default(),
        )?,
        reporter.clone(),
    );
    let written = froe_export::export_subtree(&repository, path, depth, &mut sink)?;
    Ok(match written {
        Some(nodes) => ExportRun::Exported {
            nodes,
            elapsed: sink.elapsed(),
        },
        None => ExportRun::Missing,
    })
}

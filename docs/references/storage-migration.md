# Storage migration and project merge

Use `epic-harness migrate` to import the current project's legacy JSON/JSONL
files. Completion and interrupted-migration state are recorded separately for
each project. `--reset` clears only that project's interrupted marker. Original
files remain in place. Imported observations receive the current project slug.

Old releases recorded a single global completion marker without recording the
source project. That marker cannot prove another project's files were imported,
so it does not suppress the project-specific importer. Before explicitly
reimporting files that an old release already imported, inspect the retained
source and database: old data without import provenance cannot reliably be
distinguished from new data. Migration does not delete that ambiguous data.

`epic-harness migrate --to-global` transfers supported operational tables from
per-project databases into the shared database. It preserves all available
columns, including invocation identity, pipeline state, evolved skill content,
attribution, workspace state, and global patterns. Missing optional legacy
columns receive destination defaults. A newer unsupported source column or a
conflicting record causes an error and rolls back that database's transfer.
Sources remain available after errors.

An import ledger identifies source database rows, preserving distinct rows with
identical content while preventing duplicate history on retries. Writable sources
receive a stable UUID so moving their file does not restart the import. Read-only
sources use their canonical file location and need no metadata write; retain the
same location when retrying those imports.

`epic-harness merge-project --from SOURCE --to TARGET` transfers global and
per-project records and legacy files. Different files with the same relative
name cause an error; both files remain available for explicit reconciliation.
Identical files are safe to retry. Evolution JSONL comparisons use complete
records rather than truncated prefixes. Promotion counts retain source-only
patterns and add counts for shared patterns.

Global database relabelling is transactional. Conflicting scalar state fails
the merge instead of dropping source state. A transfer error returns failure and
prevents `--delete-source` from removing the source directory. The global
database, per-project database, and filesystem are separate transaction domains:
an unsuccessful command may have completed an earlier domain. Keep the source
and resolve the reported conflict before retrying.

When observations move between project streams or receive newly allocated IDs,
the derived reflection cursor resets to zero. This can analyze retained history
again, but avoids silently skipping pending imported observations. Observation
history itself is preserved.
The reset creates the cursor when it was absent, so a legacy timestamp watermark
cannot suppress freshly imported older-dated records. Original timestamps remain
unchanged, and an unchanged consolidation retry retains consumed cursor progress.

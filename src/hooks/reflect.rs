use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::common::*;
use crate::config::CONFIG;
use crate::evolve;
use crate::mem::store;
use crate::shared::{evolution::*, helpers::*, obs::ObsRecord, paths::*};
use crate::telemetry::{SessionTrend, Telemetry};

static TELEMETRY: LazyLock<Telemetry> = LazyLock::new(Telemetry::init);
static ATOMIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// ── Reflect Context (subcommand) ─────────────────────

/// Collect harness data for /reflect skill as JSON on stdout.
/// Replaces the Python-based `reflect-context.sh` for Windows compat.
pub fn run_context(
    days: u32,
    since: Option<String>,
    project: Option<String>,
    all_projects: bool,
    sources: Vec<String>,
) -> i32 {
    if !harness_exists() {
        eprintln!("{{\"error\":\"harness directory not found\"}}");
        return 1;
    }

    // Determine which project slugs to analyze
    let project_slugs: Vec<String> = if all_projects {
        list_harness_project_slugs()
    } else if let Some(ref slug) = project {
        vec![slug.clone()]
    } else {
        vec![project_slug()]
    };
    if all_projects && project_slugs.len() > MAX_CONTEXT_PROJECTS {
        eprintln!("{{\"error\":\"--all-projects exceeds {MAX_CONTEXT_PROJECTS} project limit\"}}");
        return 1;
    }

    // Fix 1: Validate slugs — reject path traversal attempts
    for slug in &project_slugs {
        if slug.contains("..") || slug.contains('/') || slug.contains('\\') {
            eprintln!("{{\"error\":\"invalid project slug: {slug}\"}}");
            return 1;
        }
    }

    // Fix 5: Validate --since format (YYYYMMDD)
    if let Some(ref s) = since
        && (s.len() != 8 || !s.chars().all(|c| c.is_ascii_digit()))
    {
        eprintln!("{{\"error\":\"--since must be YYYYMMDD format, got: {s}\"}}");
        return 1;
    }

    // Scope for metrics/snapshot reads: one target project scopes exactly,
    // `--all-projects` aggregates. Without this the unscoped readers return
    // indeterminate rows once more than one project has written state.
    let scope: Option<&str> = if project_slugs.len() == 1 {
        Some(project_slugs[0].as_str())
    } else {
        None
    };

    // 1. Obs stats — compute date range
    let (cutoff_tag, date_from) = if let Some(ref s) = since {
        (s.clone(), s.clone())
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff_ts = now.saturating_sub((days as u64) * 86400);
        let days_since_epoch = cutoff_ts / 86400;
        let (y, m, d) = epoch_days_to_ymd(days_since_epoch as i32);
        let tag = format!("{y:04}{m:02}{d:02}");
        (tag.clone(), tag)
    };
    let date_to = today();

    let mut total_obs: u64 = 0;
    // Observations the host gave no outcome evidence for.
    let mut unknown_obs: u64 = 0;
    let mut tool_counts: HashMap<String, u64> = HashMap::new();
    let mut failure_cats: HashMap<String, u64> = HashMap::new();
    let mut file_ext_counts: HashMap<String, u64> = HashMap::new();
    let mut scores: Vec<f64> = Vec::new();
    let mut dim_sums: HashMap<String, f64> = HashMap::new();
    let mut dim_counts: HashMap<String, u64> = HashMap::new();
    let mut tool_success_map: HashMap<String, (u64, u64)> = HashMap::new(); // (success, total)

    // One bounded SQLite query covers either the selected project or the
    // aggregate view. Grouping in memory avoids one database scan per project.
    let database = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::observations::query_obs_for_date_range_bounded_pool(
            &pool,
            &date_from,
            &date_to,
            scope,
            MAX_REFLECTION_OBSERVATIONS,
        )
        .await
    })
    .unwrap_or_else(|error| {
        eprintln!("[reflect] bounded SQLite observation read failed: {error}");
        Vec::new()
    });
    let mut database_by_project: HashMap<
        String,
        Vec<crate::store::observations::StoredObservation>,
    > = HashMap::new();
    for stored in database {
        database_by_project
            .entry(stored.project.clone())
            .or_default()
            .push(stored);
    }
    let per_project_limit =
        (MAX_REFLECTION_OBSERVATIONS as usize / project_slugs.len().max(1)).max(1);
    let mut jsonl_bytes_remaining = MAX_CONTEXT_JSONL_TOTAL_BYTES;
    let mut jsonl_files_remaining = MAX_CONTEXT_JSONL_FILES;

    // Collect bounded fallback records from all target project slugs.
    for slug in &project_slugs {
        // Fix 1: Verify resolved path stays within harness projects root
        //
        // Both sides go through `canonical_for_compare`. Canonicalizing only
        // the candidate compared `\\?\C:\...` against `C:\...` on Windows and
        // rejected every project whose directory existed — which is every
        // project in real use, so `--context` never returned data there.
        let slug_harness = harness_dir_for_slug(slug);
        let root = canonical_for_compare(&harness_projects_root())
            .unwrap_or_else(|_| harness_projects_root());
        let safe = if slug_harness.exists() {
            canonical_for_compare(&slug_harness)
                .map(|p| p.starts_with(&root))
                .unwrap_or(false)
        } else {
            slug_harness.starts_with(harness_projects_root())
        };
        if !safe {
            eprintln!("{{\"error\":\"slug escapes harness root: {slug}\"}}");
            return 1;
        }
        let mut fallback = Vec::new();
        let slug_obs_dir = slug_harness.join("obs");
        if slug_obs_dir.is_dir() {
            let mut filtered: Vec<String> = list_files(&slug_obs_dir, ".jsonl")
                .into_iter()
                .filter(|f| {
                    let tag = f.replace("session_", "");
                    tag.get(..8)
                        .map(|s| s >= cutoff_tag.as_str())
                        .unwrap_or(true)
                })
                .collect();
            filtered.sort();
            for filename in filtered.into_iter().rev() {
                if jsonl_files_remaining == 0 || jsonl_bytes_remaining == 0 {
                    break;
                }
                let remaining = per_project_limit.saturating_sub(fallback.len());
                if remaining == 0 {
                    break;
                }
                let path = slug_obs_dir.join(&filename);
                let byte_limit = MAX_REFLECTION_JSONL_BYTES.min(jsonl_bytes_remaining);
                let records = match read_bounded_session_jsonl(&path, remaining, byte_limit) {
                    Ok(records) => records,
                    Err(error) => {
                        eprintln!(
                            "[reflect] invalid fallback observation log {}: {error}",
                            path.display()
                        );
                        return 1;
                    }
                };
                jsonl_files_remaining -= 1;
                jsonl_bytes_remaining = jsonl_bytes_remaining.saturating_sub(byte_limit);
                let session = filename
                    .strip_prefix("session_")
                    .and_then(|name| name.strip_suffix(".jsonl"))
                    .unwrap_or(&filename)
                    .to_string();
                fallback.extend(records.into_iter().map(|record| (session.clone(), record)));
            }
        }

        let mut database = database_by_project.remove(slug).unwrap_or_default();
        if database.len() > per_project_limit {
            database.drain(..database.len() - per_project_limit);
        }
        let mut recs = match merge_observations_with_provenance(database, fallback) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("[reflect] failed to merge context observations: {error}");
                return 1;
            }
        };
        if recs.len() > per_project_limit {
            recs.drain(..recs.len() - per_project_limit);
        }

        for r in &recs {
            total_obs += 1;
            *tool_counts.entry(r.tool.clone()).or_default() += 1;
            if let Some(ref fc) = r.failure_category {
                *failure_cats.entry(fc.clone()).or_default() += 1;
            }
            if let Some(ref ext) = r.file_ext {
                *file_ext_counts.entry(ext.clone()).or_default() += 1;
            }
            if let Some(s) = r.score {
                scores.push(s);
            }
            if let Some(ref dims) = r.dimensions {
                let ds = serde_json::to_value(dims).ok();
                if let Some(obj) = ds.as_ref().and_then(|v| v.as_object()) {
                    for (k, v) in obj {
                        if let Some(n) = v.as_f64() {
                            *dim_sums.entry(k.clone()).or_default() += n;
                            *dim_counts.entry(k.clone()).or_default() += 1;
                        }
                    }
                }
            }
            // Calls with no outcome evidence are counted separately: they are
            // neither a success nor a failure, so they stay out of the rate.
            if r.result.as_deref() == Some("unknown") {
                unknown_obs += 1;
            } else {
                let entry = tool_success_map.entry(r.tool.clone()).or_insert((0, 0));
                entry.1 += 1;
                if r.result.as_deref() == Some("success")
                    || (r.result.is_none() && r.score.unwrap_or(0.0) >= 0.7)
                {
                    entry.0 += 1;
                }
            }
        }
    }

    fn round3(v: f64) -> f64 {
        (v * 1000.0).round() / 1000.0
    }

    fn round4(v: f64) -> f64 {
        (v * 10000.0).round() / 10000.0
    }

    let avg_score = if scores.is_empty() {
        0.0
    } else {
        round3(scores.iter().sum::<f64>() / scores.len() as f64)
    };
    let high_ge09 = scores.iter().filter(|&&s| s >= 0.9).count() as u64;
    let mid_06_09 = scores.iter().filter(|&&s| (0.6..0.9).contains(&s)).count() as u64;
    let low_lt06 = scores.iter().filter(|&&s| s < 0.6).count() as u64;

    let mut top_tools: Vec<(String, u64)> = tool_counts.into_iter().collect();
    top_tools.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_tools_map: serde_json::Map<String, serde_json::Value> = top_tools
        .iter()
        .take(10)
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();

    let mut fc_sorted: Vec<(String, u64)> = failure_cats.into_iter().collect();
    fc_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let fc_map: serde_json::Map<String, serde_json::Value> = fc_sorted
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();

    let mut ext_sorted: Vec<(String, u64)> = file_ext_counts.into_iter().collect();
    ext_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let ext_map: serde_json::Map<String, serde_json::Value> = ext_sorted
        .iter()
        .take(8)
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();

    let dim_avgs: serde_json::Map<String, serde_json::Value> = dim_sums
        .iter()
        .map(|(k, s)| {
            let c = dim_counts.get(k).copied().unwrap_or(1);
            (k.clone(), serde_json::Value::from(round3(s / c as f64)))
        })
        .collect();

    // Fix 6: Use CONFIG thresholds instead of hardcoded literals
    let wt_min = CONFIG.pattern.weak_tool_min_obs;
    let wt_rate = CONFIG.pattern.weak_tool_rate;
    let st_rate = 0.9f64; // strong_tool threshold — not in CONFIG, kept as named constant

    let weak_tools: Vec<String> = tool_success_map
        .iter()
        .filter(|(_, (s, n))| *n >= wt_min && (*s as f64 / *n as f64) < wt_rate)
        .map(|(t, _)| t.clone())
        .collect();
    let strong_tools: Vec<String> = tool_success_map
        .iter()
        .filter(|(_, (s, n))| *n >= wt_min && (*s as f64 / *n as f64) >= st_rate)
        .map(|(t, _)| t.clone())
        .collect();
    let total_success: u64 = tool_success_map.values().map(|(s, _)| *s).sum();
    let total_calls: u64 = tool_success_map.values().map(|(_, n)| *n).sum();
    let tool_success_rate = if total_calls == 0 {
        0.0
    } else {
        round3(total_success as f64 / total_calls as f64)
    };

    let obs_stats = serde_json::json!({
        "total": total_obs,
        "unknown_outcome": unknown_obs,
        "avg_score": avg_score,
        "score_distribution": { "high_ge09": high_ge09, "mid_06_09": mid_06_09, "low_lt06": low_lt06 },
        "top_tools": top_tools_map,
        "failure_categories": fc_map,
        "top_file_exts": ext_map,
        "dimension_averages": dim_avgs,
        "weak_tools": weak_tools,
        "strong_tools": strong_tools,
        "tool_success_rate": tool_success_rate,
    });

    // 2. Evolution stats (SQLite first, fallback to JSONL)
    let (evo_total, evo_records): (u64, Vec<serde_json::Value>) =
        crate::store::runtime::block_on(async {
            let pool = crate::store::pool::harness_pool().await?;
            let total = crate::store::evolution::count_records_scoped_pool(&pool, scope).await?;
            let records = crate::store::evolution::query_recent_records_scoped_pool(
                &pool,
                MAX_REFLECTION_HISTORY,
                scope,
            )
            .await?;
            Ok::<_, io::Error>((total, records))
        })
        .map(|(total, records)| {
            (
                total,
                records
                    .iter()
                    .filter_map(|record| serde_json::to_value(record).ok())
                    .collect(),
            )
        })
        .unwrap_or_else(|e| {
            eprintln!("[reflect] SQLite evolution read failed, falling back to JSONL: {e}");
            let records = read_bounded_jsonl::<serde_json::Value>(
                &evolution_file(),
                MAX_REFLECTION_HISTORY as usize,
                MAX_REFLECTION_JSONL_BYTES,
                64 * 1024,
            )
            .unwrap_or_else(|error| {
                eprintln!("[reflect] bounded evolution JSONL read failed: {error}");
                Vec::new()
            });
            (records.len() as u64, records)
        });
    let mut pattern_freq: HashMap<String, u64> = HashMap::new();
    let mut trend_hist: Vec<String> = Vec::new();
    let mut skills_generated: u64 = 0;
    let mut stagnation_count: u64 = 0;
    for r in &evo_records {
        if let Some(pats) = r.get("patterns").and_then(|p| p.as_array()) {
            for p in pats {
                if let Some(t) = p.get("type").and_then(|v| v.as_str()) {
                    *pattern_freq.entry(t.to_string()).or_default() += 1;
                }
            }
        }
        if let Some(t) = r.get("trend").and_then(|v| v.as_str())
            && !t.is_empty()
        {
            trend_hist.push(t.to_string());
        }
        skills_generated += r
            .get("skills_generated")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if r.get("stagnation_triggered")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            stagnation_count += 1;
        }
    }
    let recent_evo: Vec<&serde_json::Value> = evo_records.iter().rev().take(10).collect();
    let recent_weak: Vec<String> = recent_evo
        .iter()
        .filter_map(|r| r.get("weak_tools").and_then(|v| v.as_array()))
        .flat_map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)))
        .collect();
    let recent_weak: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        recent_weak
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect()
    };
    let recent_seeded: Vec<String> = recent_evo
        .iter()
        .filter_map(|r| r.get("seeded_skills").and_then(|v| v.as_array()))
        .flat_map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)))
        .collect();
    let recent_seeded: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        recent_seeded
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect()
    };
    let mut pf_sorted: Vec<(String, u64)> = pattern_freq.into_iter().collect();
    pf_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let pf_map: serde_json::Map<String, serde_json::Value> = pf_sorted
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();
    let evo_stats = serde_json::json!({
        "total_sessions": evo_total,
        "patterns_detected": evo_records.iter().filter(|r| r.get("patterns").and_then(|p| p.as_object()).map(|o| !o.is_empty()).unwrap_or(false)).count(),
        "skills_generated": skills_generated,
        "pattern_frequency": pf_map,
        "trend_last10": trend_hist.into_iter().rev().take(10).collect::<Vec<_>>(),
        "recent_weak_tools": recent_weak,
        "recent_seeded_skills": recent_seeded,
        "stagnation_count": stagnation_count,
    });

    // 3. Metrics summary (SQLite first, fallback to JSON)
    let metrics: Metrics = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::metrics::load_metrics_scoped_pool(&pool, scope).await
    })
    .unwrap_or_else(|e| {
        eprintln!("[reflect] SQLite metrics read failed, falling back to JSON: {e}");
        read_json(&metrics_file(), default_metrics())
    });
    let sh = &metrics.score_history;
    let score_trend_delta: f64 = if sh.len() >= 3 {
        let recent: Vec<f64> = sh.iter().rev().take(10).map(|s| s.avg_score).collect();
        let deltas: Vec<f64> = recent.windows(2).map(|w| w[0] - w[1]).collect();
        if deltas.is_empty() {
            0.0
        } else {
            round4(deltas.iter().sum::<f64>() / deltas.len() as f64)
        }
    } else {
        0.0
    };

    let score_comparison = if sh.len() >= 10 {
        let first5: f64 = sh.iter().take(5).map(|s| s.avg_score).sum::<f64>() / 5.0;
        let last5: f64 = sh.iter().rev().take(5).map(|s| s.avg_score).sum::<f64>() / 5.0;
        let dir = if last5 > first5 {
            "improving"
        } else if last5 < first5 {
            "declining"
        } else {
            "stable"
        };
        Some(serde_json::json!({
            "first_5_avg": round3(first5),
            "last_5_avg": round3(last5),
            "direction": dir,
            "delta": round3(last5 - first5),
        }))
    } else {
        None
    };

    let skill_attr: serde_json::Map<String, serde_json::Value> = metrics
        .skill_attribution
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                serde_json::json!({
                    "sessions_active": v.sessions_active,
                    "avg_score_with": v.avg_score_with,
                    "avg_score_without": v.avg_score_without,
                    "delta": round3(v.avg_score_with - v.avg_score_without),
                }),
            )
        })
        .collect();

    let latest_dims = sh
        .last()
        .map(|s| serde_json::to_value(s.dimension_averages).unwrap_or_default())
        .unwrap_or_default();

    let metrics_summary = serde_json::json!({
        "total_sessions": metrics.total_sessions,
        "avg_success_rate": metrics.avg_success_rate,
        "total_evolved_skills": metrics.total_evolved_skills,
        "last_session": metrics.last_session,
        "trend": metrics.trend,
        "best_score": metrics.best_score,
        "stagnation_count": metrics.stagnation_count,
        "score_trend_delta": score_trend_delta,
        "score_comparison": score_comparison,
        "latest_avg_score": sh.last().map(|s| s.avg_score).unwrap_or(0.0),
        "latest_dimensions": latest_dims,
        "skill_attribution": skill_attr,
    });

    // 4. Session snapshots (SQLite first, fallback to JSON)
    let snapshots: Vec<serde_json::Value> = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::sessions::list_recent_snapshots_pool(&pool, 5, scope).await
    })
    .map(|snaps| {
        snaps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "timestamp": s.timestamp,
                    "type": s.snap_type,
                    "summary": redacted_context_text(&s.summary, 400),
                })
            })
            .collect()
    })
    .unwrap_or_else(|e| {
        eprintln!("[reflect] SQLite sessions read failed, falling back to JSON: {e}");
        vec![]
    });
    let snapshots: Vec<serde_json::Value> = if !snapshots.is_empty() {
        snapshots
    } else {
        fallback_snapshots(&project_slugs)
    };

    // 5. Evolved skills
    let mut evolved_list: Vec<serde_json::Value> = Vec::new();
    let mut by_type: HashMap<&str, u64> = HashMap::new();
    for slug in &project_slugs {
        let evolved = harness_dir_for_slug(slug).join("evolved");
        for name in list_dirs(&evolved) {
            if evolved_list.len() == MAX_CONTEXT_SKILLS {
                break;
            }
            let skill_path = evolved.join(&name).join("SKILL.md");
            let content =
                read_bounded_text(&skill_path, MAX_CONTEXT_SKILL_BYTES).unwrap_or_default();
            let stype = if content.contains("Evolved:") || content.contains("evolved_from:") {
                "evolved"
            } else if content.to_lowercase().contains("auto-evolved") {
                "auto-evolved"
            } else {
                "preset"
            };
            *by_type.entry(stype).or_default() += 1;
            let name = if project_slugs.len() == 1 {
                name
            } else {
                format!("{slug}/{name}")
            };
            evolved_list.push(serde_json::json!({"name": name, "type": stype}));
        }
    }

    // Effective sources
    // mem is always included (baseline). --source adds extra sources on top.
    let effective_sources: Vec<&str> = if sources.contains(&"all".to_string()) {
        vec!["harness", "mem", "claude-session", "alcove"]
    } else if sources.is_empty() {
        vec!["harness", "mem"]
    } else {
        // Always prepend mem unless the caller explicitly passed "mem" already
        let mut v: Vec<&str> = vec!["harness", "mem"];
        for s in sources.iter() {
            let s = s.as_str();
            if s != "harness" && s != "mem" {
                v.push(s);
            }
        }
        v
    };

    let extra_sources_json = {
        let mut map = serde_json::Map::new();
        // mem — always collected
        map.insert("mem".into(), collect_mem(&project_slugs));
        if effective_sources.contains(&"claude-session") {
            map.insert("claude_session".into(), collect_claude_session());
        }
        if effective_sources.contains(&"alcove") {
            map.insert("alcove".into(), collect_alcove(&CONFIG.context.alcove));
        }
        serde_json::Value::Object(map)
    };

    // Fix 4: Scope note clarifying which fields are per-project vs aggregated
    let scope_note = if all_projects {
        "All fields aggregate only the listed projects. File fallbacks use each listed project's harness directory, never the current working directory."
    } else if project.is_some() {
        "All fields are scoped to the requested project. File fallbacks use that project's harness directory, never the current working directory."
    } else {
        ""
    };

    // Compile
    let output = serde_json::json!({
        "generated_at": now_iso(),
        "analysis_window_days": days,
        "date_range": { "from": date_from, "to": date_to },
        "projects_analyzed": project_slugs,
        "scope_note": scope_note,
        "extra_sources": extra_sources_json,
        "obs_stats": obs_stats,
        "evolution_stats": evo_stats,
        "metrics_summary": metrics_summary,
        "session_snapshots": snapshots,
        "evolved_skills": evolved_list,
        "evolved_skills_summary": {
            "total": evolved_list.len(),
            "by_type": {
                "preset": by_type.get("preset").copied().unwrap_or(0),
                "evolved": by_type.get("evolved").copied().unwrap_or(0),
                "auto-evolved": by_type.get("auto-evolved").copied().unwrap_or(0),
            }
        },
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&output).unwrap_or_default()
    );
    0
}

fn read_bounded_text(path: &Path, limit: u64) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn redacted_context_text(text: &str, limit: usize) -> String {
    let masked = crate::shared::sanitize::mask_secrets(text);
    truncate_utf8(&masked, limit).to_string()
}

/// JSON snapshots are only a fallback when SQLite is unavailable. Resolve them
/// from the requested project roots so `--project` can never leak the caller's
/// current-project context; aggregate explicitly for `--all-projects`.
fn fallback_snapshots(project_slugs: &[String]) -> Vec<serde_json::Value> {
    let mut snapshots = Vec::new();
    for slug in project_slugs {
        let sessions = harness_dir_for_slug(slug).join("sessions");
        let mut files = list_files(&sessions, ".json");
        files.sort();
        for file in files.into_iter().rev().take(5) {
            let sp: serde_json::Value = read_json(&sessions.join(file), serde_json::Value::Null);
            if sp.is_null() {
                continue;
            }
            snapshots.push(serde_json::json!({
                "timestamp": sp.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
                "type": sp.get("snap_type").or_else(|| sp.get("type")).and_then(|v| v.as_str()).unwrap_or(""),
                "summary": redacted_context_text(sp.get("summary").and_then(|v| v.as_str()).unwrap_or(""), 400),
                "project": slug,
            }));
        }
    }
    snapshots.sort_by(|left, right| right["timestamp"].as_str().cmp(&left["timestamp"].as_str()));
    snapshots.truncate(5);
    snapshots
}

/// Collect mem nodes from ~/.harness/memory.db.
/// Pulls top nodes by importance for each project slug (or all if slugs = [current]).
/// Session-type nodes are excluded (importance=0.05, noise) unless there's nothing else.
fn collect_mem(project_slugs: &[String]) -> serde_json::Value {
    // Determine project filter: use first slug if single-project, else no filter (all)
    let project_filter: Option<&str> = if project_slugs.len() == 1 {
        project_slugs.first().map(|s| s.as_str())
    } else {
        None
    };

    // Smart recall — hint = broad engineering context, limit = 30
    let recalled = match store::smart_recall(
        project_filter,
        Some("decision pattern error resolution concept"),
        30,
    ) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({"error": format!("recall failed: {e}")}),
    };

    // Also pull top decisions/resolutions explicitly (high-value types)
    let decisions = store::query_nodes(
        None, // tag filter
        Some("decision"),
        project_filter,
        10,
    );
    let resolutions = store::query_nodes(None, Some("resolution"), project_filter, 10);

    // Merge and deduplicate by id, prefer higher-importance entry
    let mut seen: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();

    for sn in &recalled {
        let id = sn.node.frontmatter.id.clone();
        let entry = serde_json::json!({
            "id": id,
            "type": sn.node.frontmatter.node_type,
            "title": sn.node.frontmatter.title,
            "importance": sn.node.frontmatter.importance,
            "tags": sn.node.frontmatter.tags,
            "updated": sn.node.frontmatter.updated,
            "body_preview": sn.node.body.chars().take(200).collect::<String>(),
        });
        seen.insert(id, entry);
    }
    for node in decisions.iter().chain(resolutions.iter()) {
        let id = node.frontmatter.id.clone();
        seen.entry(id.clone()).or_insert_with(|| {
            serde_json::json!({
                "id": id,
                "type": node.frontmatter.node_type,
                "title": node.frontmatter.title,
                "importance": node.frontmatter.importance,
                "tags": node.frontmatter.tags,
                "updated": node.frontmatter.updated,
                "body_preview": node.body.chars().take(200).collect::<String>(),
            })
        });
    }

    // Sort by importance desc, take top 30
    let mut nodes: Vec<serde_json::Value> = seen.into_values().collect();
    nodes.sort_by(|a, b| {
        let ia = a.get("importance").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ib = b.get("importance").and_then(|v| v.as_f64()).unwrap_or(0.0);
        ib.partial_cmp(&ia).unwrap_or(std::cmp::Ordering::Equal)
    });
    nodes.truncate(30);

    serde_json::json!({
        "total_nodes_sampled": nodes.len(),
        "project_filter": project_filter,
        "nodes": nodes,
    })
}

fn collect_claude_session() -> serde_json::Value {
    let claude_projects = dirs_home().join(".claude").join("projects");
    collect_claude_session_at(&claude_projects)
}

fn collect_claude_session_at(claude_projects: &Path) -> serde_json::Value {
    if !claude_projects.is_dir() {
        return serde_json::json!({"error": "~/.claude/projects not found"});
    }
    let mut sessions: Vec<serde_json::Value> = vec![];
    let project_dirs = match std::fs::read_dir(claude_projects) {
        Ok(entries) => entries,
        Err(error) => {
            return serde_json::json!({"error": format!("cannot read Claude projects: {error}")});
        }
    };
    let mut project_dirs: Vec<_> = project_dirs
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect();
    project_dirs.sort_by_key(|entry| entry.file_name());
    let mut errors = Vec::new();
    for pd in project_dirs.iter().take(MAX_CLAUDE_PROJECTS) {
        let mut jsonl_files = list_files(&pd.path(), ".jsonl");
        jsonl_files.sort();
        for f in jsonl_files
            .iter()
            .rev()
            .take(MAX_CLAUDE_SESSION_FILES_PER_PROJECT)
        {
            let records = match read_bounded_jsonl::<serde_json::Value>(
                &pd.path().join(f),
                MAX_CLAUDE_RECORDS_PER_FILE,
                MAX_CLAUDE_JSONL_BYTES,
                MAX_CLAUDE_JSONL_LINE_BYTES,
            ) {
                Ok(records) => records,
                Err(error) => {
                    errors.push(format!("{f}: {error}"));
                    continue;
                }
            };
            for r in records.iter().rev() {
                let meta = serde_json::json!({
                    "project": pd.file_name().to_string_lossy(),
                    "timestamp": r.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
                    "model": r.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    "message_count": r.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "cost_usd": r.get("cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0),
                });
                sessions.push(meta);
            }
        }
    }
    sessions.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        tb.cmp(ta)
    });
    let sessions: Vec<_> = sessions.into_iter().take(20).collect();
    serde_json::json!({
        "total_sessions_sampled": sessions.len(),
        "sessions": sessions,
        "errors": errors,
    })
}

fn collect_alcove(cfg: &crate::config::AlcoveConfig) -> serde_json::Value {
    if cfg.vault_path.is_empty() {
        return serde_json::json!({"error": "alcove vault_path not configured"});
    }
    let vault = if cfg.vault_path.starts_with("~/") {
        dirs_home().join(&cfg.vault_path[2..])
    } else {
        std::path::PathBuf::from(&cfg.vault_path)
    };
    // Fix 2: Canonicalize vault path and verify it stays within home directory.
    // Both paths must use the comparison representation: Windows canonicalization
    // yields a verbatim vault path while `dirs_home` is a plain path.
    let vault = if let Ok(canonical) = canonical_for_compare(&vault) {
        let home = match canonical_for_compare(&dirs_home()) {
            Ok(home) => home,
            Err(error) => {
                return serde_json::json!({
                    "error": format!("home directory cannot be canonicalized: {error}")
                });
            }
        };
        if !canonical.starts_with(&home) {
            return serde_json::json!({
                "error": format!("vault_path escapes home directory: {}", canonical.display())
            });
        }
        canonical
    } else {
        return serde_json::json!({"error": format!("vault_path not found: {}", vault.display())});
    };
    let max = cfg.max_docs.max(1);
    let mut docs: Vec<serde_json::Value> = vec![];
    let mut visited = 0usize;
    collect_md_files(&vault, &cfg.projects, max, &mut docs, 0, &mut visited);
    serde_json::json!({
        "vault_path": cfg.vault_path,
        "docs_collected": docs.len(),
        "documents": docs,
    })
}

fn collect_md_files(
    dir: &std::path::Path,
    filter_projects: &[String],
    max: usize,
    out: &mut Vec<serde_json::Value>,
    depth: usize,
    visited: &mut usize,
) {
    // Fix 3: Guard against deep recursion, excessive file visits, and symlink loops
    const MAX_DEPTH: usize = 10;
    const MAX_VISITED: usize = 5000;

    if out.len() >= max || depth > MAX_DEPTH || *visited > MAX_VISITED {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= max || *visited > MAX_VISITED {
            break;
        }
        let path = entry.path();
        // Fix 3: Skip symlinks to prevent directory traversal via symlink
        if path.is_dir() && !path.is_symlink() {
            if !filter_projects.is_empty() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !filter_projects.iter().any(|p| p == &name) {
                    continue;
                }
            }
            collect_md_files(&path, &[], max, out, depth + 1, visited);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            *visited += 1;
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let summary: String = content.chars().take(200).collect();
            out.push(serde_json::json!({
                "path": path.display().to_string(),
                "summary": summary,
            }));
        }
    }
}

/// Simple epoch-day to (year, month, day) without chrono dependency.
fn epoch_days_to_ymd(days: i32) -> (i32, u32, u32) {
    let mut y = 1970 + days / 365;
    // Refine
    for candidate in (1970..=y + 2).rev() {
        let d = days_to_year_start(candidate);
        if d <= days {
            y = candidate;
            break;
        }
    }
    let remaining = days - days_to_year_start(y);
    let leap = is_leap(y);
    let month_days: [u32; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m: u32 = 1;
    let mut acc: i32 = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if acc + md as i32 > remaining {
            m = (i + 1) as u32;
            break;
        }
        acc += md as i32;
    }
    let d = (remaining - acc + 1).max(1) as u32;
    (y, m, d)
}

fn days_to_year_start(year: i32) -> i32 {
    let mut days = 0i32;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    days
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

const REFLECTION_JOB_ENV: &str = "EPIC_REFLECT_WORKER_JOB";
const REFLECTION_PROJECT_ENV: &str = "EPIC_REFLECT_PROJECT";
/// Reflection mutates metrics, skill files and project ledgers. Keep that
/// transaction serial per project; other projects have independent OS locks.
const REFLECTION_WORKER_LOCK: &str = "worker.lock";
const MAX_REFLECTION_QUEUE_SCAN: usize = 64;
const MAX_REFLECTION_QUEUE_FILES: usize = MAX_REFLECTION_QUEUE_SCAN * 2;
const MAX_REFLECTION_OBSERVATIONS: i64 = 5_000;
const MAX_REFLECTION_JSONL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REFLECTION_JSONL_LINE_BYTES: usize = 16 * 1024;
const MAX_REFLECTION_HISTORY: i64 = 100;
/// Compatibility ledgers are cache projections. Durable SQLite/session keys
/// remain authoritative, so retain only enough fallback history for context.
const MAX_COMPATIBILITY_LEDGER_RECORDS: usize = 100;
const MAX_COMPATIBILITY_PROJECTIONS: usize = 128;
const MAX_CONTEXT_JSONL_FILES: usize = 64;
const MAX_CONTEXT_JSONL_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONTEXT_PROJECTS: usize = 64;
const MAX_CONTEXT_SKILLS: usize = 100;
const MAX_CONTEXT_SKILL_BYTES: u64 = 16 * 1024;
const MAX_CLAUDE_PROJECTS: usize = 20;
const MAX_CLAUDE_SESSION_FILES_PER_PROJECT: usize = 3;
const MAX_CLAUDE_RECORDS_PER_FILE: usize = 5;
const MAX_CLAUDE_JSONL_BYTES: u64 = 256 * 1024;
const MAX_CLAUDE_JSONL_LINE_BYTES: usize = 16 * 1024;
const MAX_ORBIT_PIPELINE_FILES: usize = 64;
const MAX_ORBIT_PIPELINE_BYTES: usize = 1024 * 1024;
const MAX_REFLECTION_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReflectionJob {
    session_id: String,
    project: String,
    created_at: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<ReflectionClaim>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReflectionClaim {
    claimed_at: String,
    owner: String,
    fence: String,
}

/// An OS-owned lock held from recovery through claim, reflection, and
/// settlement. Its file is deliberately persistent: OS lock release after a
/// crash, rather than file age or deletion, determines abandonment.
struct ReflectionWorkerLock {
    _file: fs::File,
}

fn read_bounded_jsonl<T: DeserializeOwned>(
    path: &Path,
    limit: usize,
    byte_limit: u64,
    line_limit: usize,
) -> io::Result<Vec<T>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(byte_limit);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(byte_limit));
    if start > 0 {
        let mut partial = Vec::new();
        reader.read_until(b'\n', &mut partial)?;
    }

    let mut records = std::collections::VecDeque::with_capacity(limit.min(1024));
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.len() > line_limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("JSONL line exceeds {line_limit} bytes"),
            ));
        }
        while line
            .last()
            .is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_slice(&line).map_err(io::Error::other)?;
        if records.len() == limit {
            records.pop_front();
        }
        records.push_back(record);
    }
    Ok(records.into())
}

fn read_bounded_session_jsonl(
    path: &Path,
    limit: usize,
    byte_limit: u64,
) -> io::Result<Vec<ObsRecord>> {
    read_bounded_jsonl(path, limit, byte_limit, MAX_REFLECTION_JSONL_LINE_BYTES)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no parent",
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no filename",
        )
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = crate::team::codex::atomic_replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_directory(parent)
}

fn session_projection_path(sessions_dir: &Path, session_id: &str) -> io::Result<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id for fallback projection",
        ));
    }
    Ok(sessions_dir.join(format!("{session_id}.json")))
}

fn write_session_projection_once<T>(path: &Path, value: &T) -> io::Result<()>
where
    T: Serialize + DeserializeOwned,
{
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "session projection has no parent directory",
        )
    })?;
    if publish_new_file(parent, path, |file| file.write_all(&bytes))? {
        return Ok(());
    }
    match fs::read(path) {
        Ok(existing) if serde_json::from_slice::<T>(&existing).is_ok() => Ok(()),
        Ok(_) => atomic_write(path, &bytes),
        Err(error) => Err(error),
    }
}

fn projection_files(sessions_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(sessions_dir)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && path.is_file()
        {
            files.push(path);
        }
    }
    files.sort();
    if files.len() > MAX_COMPATIBILITY_PROJECTIONS {
        let remove_count = files.len() - MAX_COMPATIBILITY_PROJECTIONS;
        for stale in files.drain(..remove_count) {
            fs::remove_file(stale)?;
        }
    }
    Ok(files)
}

fn rebuild_ledger<T>(
    ledger: &Path,
    sessions_dir: &Path,
    append_projection: impl Fn(&mut fs::File, T) -> io::Result<()>,
) -> io::Result<()>
where
    T: for<'de> Deserialize<'de>,
{
    let parent = ledger
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ledger has no parent"))?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        ledger.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        let prior = read_bounded_jsonl::<serde_json::Value>(
            ledger,
            MAX_COMPATIBILITY_LEDGER_RECORDS,
            MAX_REFLECTION_JSONL_BYTES,
            MAX_REFLECTION_JSONL_LINE_BYTES,
        )
        .or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(Vec::new())
            } else {
                Err(error)
            }
        })?;
        for value in prior {
            let projected = value
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| session_projection_path(sessions_dir, id))
                .transpose()?
                .is_some_and(|path| path.is_file());
            if !projected {
                serde_json::to_writer(&mut output, &value).map_err(io::Error::other)?;
                output.write_all(b"\n")?;
            }
        }
        for path in projection_files(sessions_dir)? {
            let record = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
            append_projection(&mut output, record)?;
        }
        output.sync_all()
    })();
    drop(output);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = crate::team::codex::atomic_replace_file(&temporary, ledger) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_directory(parent)
}

fn append_evolution_fallback_once_at(
    ledger: &Path,
    sessions_dir: &Path,
    record: &EvolutionRecord,
) -> io::Result<()> {
    let session_id = record.session_id.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "evolution fallback has no session id",
        )
    })?;
    fs::create_dir_all(sessions_dir)?;
    let _lock = crate::orchestrate::state::acquire_lock(&sessions_dir.with_extension("lock"))?;
    write_session_projection_once(&session_projection_path(sessions_dir, session_id)?, record)?;
    rebuild_ledger(ledger, sessions_dir, |output, record: EvolutionRecord| {
        serde_json::to_writer(&mut *output, &record).map_err(io::Error::other)?;
        output.write_all(b"\n")
    })
}

fn append_evolution_fallback_once(record: &EvolutionRecord) -> io::Result<()> {
    append_evolution_fallback_once_at(
        &evolution_file(),
        &evolution_file().with_extension("jsonl.sessions"),
        record,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestSessionProjection {
    session_id: String,
    manifests: Vec<crate::evolve::edits::EditManifest>,
}

fn append_manifest_fallback_once_at(
    ledger: &Path,
    sessions_dir: &Path,
    session_id: &str,
    manifests: &[crate::evolve::edits::EditManifest],
) -> io::Result<()> {
    fs::create_dir_all(sessions_dir)?;
    let _lock = crate::orchestrate::state::acquire_lock(&sessions_dir.with_extension("lock"))?;
    let projection_path = session_projection_path(sessions_dir, session_id)?;
    write_session_projection_once(
        &projection_path,
        &ManifestSessionProjection {
            session_id: session_id.to_string(),
            manifests: manifests.to_vec(),
        },
    )?;
    rebuild_ledger(
        ledger,
        sessions_dir,
        |output, projection: ManifestSessionProjection| {
            for manifest in projection.manifests {
                let mut value = serde_json::to_value(manifest).map_err(io::Error::other)?;
                value["session_id"] = serde_json::Value::String(projection.session_id.clone());
                serde_json::to_writer(&mut *output, &value).map_err(io::Error::other)?;
                output.write_all(b"\n")?;
            }
            Ok(())
        },
    )
}

fn append_manifest_fallback_once(
    session_id: &str,
    manifests: &[crate::evolve::edits::EditManifest],
) -> io::Result<()> {
    append_manifest_fallback_once_at(
        &manifests_file(),
        &manifests_file().with_extension("jsonl.sessions"),
        session_id,
        manifests,
    )
}

fn observation_fingerprint(record: &ObsRecord) -> io::Result<String> {
    serde_json::to_string(record).map_err(io::Error::other)
}

fn merge_session_observations(
    session_id: &str,
    database: Vec<crate::store::observations::StoredObservation>,
    fallback: Vec<ObsRecord>,
) -> io::Result<Vec<ObsRecord>> {
    merge_observations_with_provenance(
        database,
        fallback
            .into_iter()
            .map(|record| (session_id.to_string(), record))
            .collect(),
    )
}

fn merge_observations_with_provenance(
    database: Vec<crate::store::observations::StoredObservation>,
    fallback: Vec<(String, ObsRecord)>,
) -> io::Result<Vec<ObsRecord>> {
    let mut database_origins: HashMap<(String, String), usize> = HashMap::new();
    for stored in &database {
        if let Some(identity) = observation_identity(&stored.record)? {
            let key = (stored.session_id.clone(), identity);
            *database_origins.entry(key).or_default() += 1;
        }
    }

    let mut merged: Vec<ObsRecord> = database.into_iter().map(|stored| stored.record).collect();
    for (session_id, record) in fallback {
        let duplicate = if let Some(identity) = observation_identity(&record)? {
            let key = (session_id, identity);
            database_origins.get_mut(&key).is_some_and(|remaining| {
                if *remaining == 0 {
                    false
                } else {
                    *remaining -= 1;
                    true
                }
            })
        } else {
            false
        };
        if !duplicate {
            merged.push(record);
        }
    }
    merged.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
    let cap = MAX_REFLECTION_OBSERVATIONS as usize;
    if merged.len() > cap {
        merged.drain(..merged.len() - cap);
    }
    Ok(merged)
}

/// A host tool-use id identifies one call across SQLite and JSONL mirrors.
/// Older records fall back to their persisted sequence plus payload fingerprint;
/// records with neither are intentionally never collapsed.
fn observation_identity(record: &ObsRecord) -> io::Result<Option<String>> {
    if let Some(tool_use_id) = record.tool_use_id.as_deref().filter(|id| !id.is_empty()) {
        return Ok(Some(format!("host:{tool_use_id}")));
    }
    record
        .sequence_id
        .map(|sequence| {
            observation_fingerprint(record)
                .map(|fingerprint| format!("legacy:{sequence}:{fingerprint}"))
        })
        .transpose()
}

fn reflection_job_key(session_id: &str) -> io::Result<String> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid reflection session id",
        ))
    } else {
        Ok(session_id.to_string())
    }
}

fn validate_reflection_job(job: &ReflectionJob) -> io::Result<()> {
    reflection_job_key(&job.session_id)?;
    if job.project.is_empty()
        || job.project.len() > 128
        || !job
            .project
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid reflection project",
        ));
    }
    Ok(())
}

fn reflection_job_path(queue: &Path, session_id: &str, state: &str) -> io::Result<PathBuf> {
    Ok(queue.join(format!("job_{}.{}", reflection_job_key(session_id)?, state)))
}

fn ensure_reflection_queue_dir(queue: &Path) -> io::Result<()> {
    if queue.exists()
        && queue
            .symlink_metadata()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("reflection queue is a symlink: {}", queue.display()),
        ));
    }
    fs::create_dir_all(queue)?;
    if !queue.symlink_metadata()?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("reflection queue is not a directory: {}", queue.display()),
        ));
    }
    Ok(())
}

fn try_acquire_reflection_worker_lock(queue: &Path) -> io::Result<Option<ReflectionWorkerLock>> {
    ensure_reflection_queue_dir(queue)?;
    crate::orchestrate::state::try_acquire_lock(&queue.join(REFLECTION_WORKER_LOCK))
        .map(|lock| lock.map(|_file| ReflectionWorkerLock { _file }))
}

fn reflection_queue_files(queue: &Path, extension: &str) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(queue) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some(extension)
        {
            files.push(entry.path());
            if files.len() == MAX_REFLECTION_QUEUE_SCAN {
                break;
            }
        }
    }
    Ok(files)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn publish_new_file(
    parent: &Path,
    destination: &Path,
    write_job: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<bool> {
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        destination
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    if let Err(error) = write_job(&mut file).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    match fs::hard_link(&temporary, destination) {
        Ok(()) => {
            fs::remove_file(&temporary)?;
            sync_directory(parent)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary)?;
            Ok(false)
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn enqueue_reflection_job(queue: &Path, job: &ReflectionJob) -> io::Result<Option<PathBuf>> {
    validate_reflection_job(job)?;
    ensure_reflection_queue_dir(queue)?;
    let pending = reflection_job_path(queue, &job.session_id, "pending")?;
    for state in ["pending", "claimed", "completed"] {
        if reflection_job_path(queue, &job.session_id, state)?.exists() {
            return Ok(None);
        }
    }
    publish_new_file(queue, &pending, |file| {
        serde_json::to_writer(&mut *file, job).map_err(io::Error::other)?;
        file.write_all(b"\n")
    })
    .map(|published| published.then_some(pending))
}

fn claim_reflection_job(pending: &Path) -> io::Result<Option<PathBuf>> {
    if pending.extension().and_then(|ext| ext.to_str()) != Some("pending") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job is not pending",
        ));
    }
    let claimed = pending.with_extension("claimed");
    if claimed.exists() || pending.with_extension("completed").exists() {
        return Ok(None);
    }
    let Some(queue) = pending.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job has no queue directory",
        ));
    };
    let mut job: ReflectionJob = match fs::read(pending) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(job) => {
                if let Err(error) = validate_reflection_job(&job) {
                    quarantine_pending_reflection_job(pending)?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid pending reflection job: {error}"),
                    ));
                }
                job
            }
            Err(error) => {
                quarantine_pending_reflection_job(pending)?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid pending reflection job: {error}"),
                ));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    job.claim = Some(ReflectionClaim {
        claimed_at: now_iso(),
        owner: format!("pid:{}", std::process::id()),
        fence: uuid::Uuid::new_v4().to_string(),
    });
    if !publish_new_file(queue, &claimed, |file| {
        serde_json::to_writer(&mut *file, &job).map_err(io::Error::other)?;
        file.write_all(b"\n")
    })? {
        return Ok(None);
    }
    match fs::remove_file(pending) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sync_directory(queue)?;
    Ok(Some(claimed))
}

fn reflection_claim_fence(claimed: &Path) -> io::Result<String> {
    let job: ReflectionJob =
        serde_json::from_slice(&fs::read(claimed)?).map_err(io::Error::other)?;
    job.claim
        .map(|claim| claim.fence)
        .filter(|fence| !fence.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "reflection claim has no fence"))
}

fn ensure_reflection_claim_fence(claimed: &Path, fence: &str) -> io::Result<()> {
    if fence.is_empty() || reflection_claim_fence(claimed)? != fence {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reflection claim fence no longer belongs to this worker",
        ));
    }
    Ok(())
}

fn complete_reflection_job(claimed: &Path, fence: &str) -> io::Result<()> {
    if claimed.extension().and_then(|ext| ext.to_str()) != Some("claimed") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job is not claimed",
        ));
    }
    let parent = claimed.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job has no queue directory",
        )
    })?;
    ensure_reflection_claim_fence(claimed, fence)?;
    crate::team::codex::atomic_replace_file(claimed, &claimed.with_extension("completed"))?;
    sync_directory(parent)
}

fn quarantine_reflection_job(claimed: &Path) -> io::Result<()> {
    if claimed.extension().and_then(|ext| ext.to_str()) != Some("claimed") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job is not claimed",
        ));
    }
    let parent = claimed.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job has no queue directory",
        )
    })?;
    crate::team::codex::atomic_replace_file(claimed, &claimed.with_extension("failed"))?;
    sync_directory(parent)
}

fn quarantine_pending_reflection_job(pending: &Path) -> io::Result<()> {
    if pending.extension().and_then(|ext| ext.to_str()) != Some("pending") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job is not pending",
        ));
    }
    let parent = pending.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job has no queue directory",
        )
    })?;
    crate::team::codex::atomic_replace_file(pending, &pending.with_extension("failed"))?;
    sync_directory(parent)
}

fn read_claimed_reflection_job(claimed: &Path) -> io::Result<ReflectionJob> {
    let bytes = fs::read(claimed)?;
    match serde_json::from_slice(&bytes) {
        Ok(job) => Ok(job),
        Err(error) => {
            quarantine_reflection_job(claimed)?;
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid reflection job: {error}"),
            ))
        }
    }
}

fn retry_or_dead_letter_reflection_job(claimed: &Path, fence: &str) -> io::Result<bool> {
    ensure_reflection_claim_fence(claimed, fence)?;
    let mut job: ReflectionJob =
        serde_json::from_slice(&fs::read(claimed)?).map_err(io::Error::other)?;
    job.attempts = job.attempts.saturating_add(1);
    let dead_lettered = job.attempts >= MAX_REFLECTION_ATTEMPTS;
    let destination = if dead_lettered {
        claimed.with_extension("failed")
    } else {
        job.claim = None;
        claimed.with_extension("pending")
    };
    let mut bytes = serde_json::to_vec(&job).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(claimed, &bytes)?;
    crate::team::codex::atomic_replace_file(claimed, &destination)?;
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reflection job has no queue directory",
        )
    })?;
    sync_directory(parent)?;
    Ok(dead_lettered)
}

fn recover_abandoned_reflection_claims(
    queue: &Path,
    _worker_lock: &ReflectionWorkerLock,
) -> io::Result<usize> {
    let mut recovered = 0;
    for path in reflection_queue_files(queue, "claimed")? {
        if path.with_extension("completed").exists() {
            continue;
        }
        let pending = path.with_extension("pending");
        match fs::remove_file(&pending) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        crate::team::codex::atomic_replace_file(&path, &pending)?;
        if let Some(parent) = pending.parent() {
            sync_directory(parent)?;
        }
        recovered += 1;
    }
    Ok(recovered)
}

/// Recover only after proving no live worker holds the project OS lock.
fn recover_abandoned_reflection_claims_if_unlocked(queue: &Path) -> io::Result<usize> {
    let Some(worker_lock) = try_acquire_reflection_worker_lock(queue)? else {
        return Ok(0);
    };
    recover_abandoned_reflection_claims(queue, &worker_lock)
}

fn pending_reflection_jobs(queue: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(queue) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut pending = Vec::new();
    let mut scanned = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pending") {
            continue;
        }
        scanned += 1;
        let job = match fs::read(&path)
            .and_then(|bytes| {
                serde_json::from_slice::<ReflectionJob>(&bytes).map_err(io::Error::other)
            })
            .and_then(|job| {
                validate_reflection_job(&job)?;
                Ok(job)
            }) {
            Ok(job) => job,
            Err(error) => {
                quarantine_pending_reflection_job(&path)?;
                eprintln!(
                    "[reflect] invalid pending reflection job {}: {error}",
                    path.display()
                );
                if scanned == MAX_REFLECTION_QUEUE_FILES {
                    break;
                }
                continue;
            }
        };
        pending.push((job.attempts, path));
        if pending.len() == MAX_REFLECTION_QUEUE_SCAN || scanned == MAX_REFLECTION_QUEUE_FILES {
            break;
        }
    }
    pending.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    Ok(pending.into_iter().map(|(_, path)| path).collect())
}

fn spawn_pending_reflection_jobs(queue: &Path) -> io::Result<usize> {
    let Some(job) = pending_reflection_jobs(queue)?.into_iter().next() else {
        return Ok(0);
    };
    let executable = std::env::current_exe()?;
    let scope: ReflectionJob =
        serde_json::from_slice(&fs::read(&job)?).map_err(io::Error::other)?;
    validate_reflection_job(&scope)?;
    let expected_queue = resolve_external_harness_dir(&scope.project)?.join("reflect-queue");
    if canonical_for_compare(queue)? != canonical_for_compare(&expected_queue)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "reflection queue does not belong to project {}",
                scope.project
            ),
        ));
    }
    Command::new(&executable)
        .arg("reflect")
        .env(REFLECTION_JOB_ENV, &job)
        .env(REFLECTION_PROJECT_ENV, &scope.project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(1)
}

fn fallback_projects_for_session(reflection_session_id: &str) -> io::Result<Vec<String>> {
    let mut projects = Vec::new();
    for slug in list_harness_project_slugs() {
        let harness = resolve_external_harness_dir(&slug)?;
        let path = harness
            .join("obs")
            .join(format!("session_{reflection_session_id}.jsonl"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("reflection fallback is a symlink: {}", path.display()),
                ));
            }
            Ok(metadata) if metadata.is_file() => {
                // The exact filename is the fallback identity boundary. Parse
                // it now so corrupt evidence never becomes a durable job.
                read_bounded_session_jsonl(
                    &path,
                    MAX_REFLECTION_OBSERVATIONS as usize,
                    MAX_REFLECTION_JSONL_BYTES,
                )?;
                projects.push(slug);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(projects)
}

fn reflection_projects_for_session(reflection_session_id: &str) -> io::Result<Vec<String>> {
    let mut projects = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::observations::distinct_projects_for_session_pool(&pool, reflection_session_id)
            .await
    })
    .unwrap_or_else(|error| {
        eprintln!("[reflect] SQLite project discovery failed, falling back to JSONL: {error}");
        Vec::new()
    });
    projects.extend(fallback_projects_for_session(reflection_session_id)?);
    projects.sort();
    projects.dedup();
    // Database contents are untrusted input: every target must resolve to an
    // exact regular project dir below the harness projects root.
    for project in &projects {
        resolve_external_harness_dir(project)?;
    }
    Ok(projects)
}

fn enqueue_reflection(reflection_session_id: &str) -> io::Result<()> {
    for project in reflection_projects_for_session(reflection_session_id)? {
        let harness = resolve_external_harness_dir(&project)?;
        let queue = harness.join("reflect-queue");
        let _ = recover_abandoned_reflection_claims_if_unlocked(&queue)?;
        let job = ReflectionJob {
            session_id: reflection_session_id.to_string(),
            project,
            created_at: now_iso(),
            attempts: 0,
            claim: None,
        };
        let _ = enqueue_reflection_job(&queue, &job)?;
        // A saturated queue is still a successful durable handoff. Completing
        // workers dispatch the next pending job after releasing their slot.
        let _ = spawn_pending_reflection_jobs(&queue)?;
    }
    Ok(())
}

fn settle_reflection_job(claimed: &Path, fence: &str, reflection_code: i32) -> io::Result<()> {
    if reflection_code == 0 {
        complete_reflection_job(claimed, fence)
    } else {
        retry_or_dead_letter_reflection_job(claimed, fence).map(|_| ())
    }
}

fn run_reflection_worker(pending: &Path) -> i32 {
    let declared: ReflectionJob = match fs::read(pending)
        .map_err(io::Error::other)
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
    {
        Ok(job) => job,
        Err(error) => {
            eprintln!("[reflect] invalid worker job: {error}");
            return 1;
        }
    };
    if let Err(error) = validate_reflection_job(&declared) {
        eprintln!("[reflect] invalid worker job identity: {error}");
        return 1;
    }
    let queue = match resolve_external_harness_dir(&declared.project) {
        Ok(harness) => harness.join("reflect-queue"),
        Err(error) => {
            eprintln!("[reflect] invalid worker project: {error}");
            return 1;
        }
    };
    let Some(parent) = pending.parent() else {
        eprintln!("[reflect] worker job has no parent directory");
        return 1;
    };
    if canonical_for_compare(parent).ok() != canonical_for_compare(&queue).ok() {
        eprintln!("[reflect] refusing worker job outside {}", queue.display());
        return 1;
    }
    if std::env::var(REFLECTION_PROJECT_ENV).ok().as_deref() != Some(declared.project.as_str()) {
        eprintln!("[reflect] worker project scope does not match its job");
        return 1;
    }
    if let Err(error) = ensure_reflection_queue_dir(&queue) {
        eprintln!("[reflect] invalid worker queue: {error}");
        return 1;
    }
    let Some(worker_lock) = (match try_acquire_reflection_worker_lock(&queue) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("[reflect] failed to acquire worker lock: {error}");
            return 1;
        }
    }) else {
        return 0;
    };
    if let Err(error) = recover_abandoned_reflection_claims(&queue, &worker_lock) {
        eprintln!("[reflect] failed to recover abandoned claims: {error}");
        return 1;
    }
    let code = match claim_reflection_job(pending) {
        Ok(Some(claimed)) => match read_claimed_reflection_job(&claimed) {
            Ok(job) if job.project == declared.project => {
                let fence = match job.claim.as_ref().map(|claim| claim.fence.as_str()) {
                    Some(fence) if !fence.is_empty() => fence,
                    _ => {
                        eprintln!("[reflect] claimed job has no fence");
                        return 1;
                    }
                };
                let reflection_code = run_reflection(&job.session_id);
                match settle_reflection_job(&claimed, fence, reflection_code) {
                    Ok(()) => reflection_code,
                    Err(error) => {
                        eprintln!("[reflect] failed to settle reflection job: {error}");
                        1
                    }
                }
            }
            Ok(job) => {
                eprintln!("[reflect] claimed job project {} changed", job.project);
                if let Err(error) = quarantine_reflection_job(&claimed) {
                    eprintln!("[reflect] failed to quarantine mismatched job: {error}");
                }
                1
            }
            Err(error) => {
                eprintln!("[reflect] invalid job: {error}");
                1
            }
        },
        Ok(None) => 0,
        Err(error) => {
            eprintln!("[reflect] failed to claim job: {error}");
            1
        }
    };
    // Drop before dispatching so one pending worker can own this project next.
    drop(worker_lock);
    if let Err(error) = spawn_pending_reflection_jobs(&queue) {
        eprintln!("[reflect] failed to dispatch next pending job: {error}");
    }
    code
}

// ── Main Hook ───────────────────────────────────────

pub fn run(input: &HookInput) -> i32 {
    if let Some(path) = std::env::var_os(REFLECTION_JOB_ENV) {
        return run_reflection_worker(Path::new(&path));
    }
    if input.hook_event_name.as_deref() == Some("SessionEnd")
        && input.session_id.is_some()
        && crate::shared::host::session_id().is_some()
    {
        let reflection_session_id = match try_session_id() {
            Ok(session_id) => session_id,
            Err(error) => {
                eprintln!("[reflect] SessionEnd identity is unavailable: {error}");
                return 1;
            }
        };
        return match enqueue_reflection(&reflection_session_id) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("[reflect] failed to delegate SessionEnd work: {error}");
                1
            }
        };
    }
    eprintln!("[reflect] mutating reflection requires a validated SessionEnd session identity");
    1
}

fn reflection_partition_date(reflection_session_id: &str) -> Option<&str> {
    let (date, _) = reflection_session_id.split_once('_')?;
    (date.len() == 8 && date.chars().all(|character| character.is_ascii_digit())).then_some(date)
}

fn run_reflection(reflection_session_id: &str) -> i32 {
    if !should_run(PROFILE_REFLECT) {
        return 0;
    }
    if !harness_exists() {
        return 0;
    }

    let slug = crate::shared::paths::project_slug();
    match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::reflection_completed_pool(&pool, reflection_session_id, &slug)
            .await
    }) {
        Ok(true) => return complete_recorded_orbit_pipelines(reflection_session_id, &slug),
        Ok(false) => {}
        Err(error) => {
            eprintln!("[reflect] failed to check completed session: {error}");
            return 1;
        }
    }

    // Retention is unconditional SessionEnd work. Run it before any
    // observation-count return so short and unknown-only sessions still bound
    // both SQLite and fallback JSONL history.
    match super::retention::run_preserving_session(Some((reflection_session_id, &slug))) {
        Ok((pruned_rows, pruned_files)) if pruned_rows > 0 || pruned_files > 0 => {
            eprintln!(
                "[reflect] retention: {pruned_rows} observation(s), {pruned_files} stale file(s)"
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[reflect] retention failed: {error}");
            return 1;
        }
    }

    // 1. Collect only the ending host session. SQLite rows retain database row
    // provenance; fallback JSONL is the exact session file, not every file from
    // the same day.
    let database = match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::observations::query_obs_for_session_pool(
            &pool,
            reflection_session_id,
            &slug,
            MAX_REFLECTION_OBSERVATIONS,
        )
        .await
    }) {
        Ok(recs) => recs,
        Err(e) => {
            eprintln!("[reflect] SQLite observations read failed, using JSONL only: {e}");
            Vec::new()
        }
    };
    let fallback_path = obs_dir().join(format!("session_{reflection_session_id}.jsonl"));
    let fallback = if fallback_path.is_file() {
        match read_bounded_session_jsonl(
            &fallback_path,
            MAX_REFLECTION_OBSERVATIONS as usize,
            MAX_REFLECTION_JSONL_BYTES,
        ) {
            Ok(records) => records,
            Err(error) => {
                eprintln!(
                    "[reflect] invalid fallback observation log {}: {error}",
                    fallback_path.display()
                );
                return 1;
            }
        }
    } else {
        Vec::new()
    };
    let observations = match merge_session_observations(reflection_session_id, database, fallback) {
        Ok(observations) => observations,
        Err(error) => {
            eprintln!("[reflect] failed to merge session observations: {error}");
            return 1;
        }
    };
    if observations.len() < 3 {
        return mark_reflection_completed(reflection_session_id, &slug, &observations);
    }

    // 2. Analyze
    let mut analysis = evolve::analyze_session(&observations);
    // `unknown` observations have no determined outcome and therefore no
    // defensible session score. Complete the durable job without touching
    // evolution, metrics, stagnation, or their replay projections.
    if !analysis.is_evaluable() {
        return mark_reflection_completed(reflection_session_id, &slug, &observations);
    }
    analysis.failure_patterns = evolve::detect_patterns(&observations);

    // 2b. HarnessX Digester + Planner (R2, R3): compress the session into
    // per-task digests and build the adaptation landscape from history. The
    // landscape surfaces persistent failures + untried edit types, and
    // recommends exploration when the engine is plateauing on local edits.
    let fallback_task_namespace = format!("{slug}/{reflection_session_id}");
    let digests = match evolve::digest_session(&observations, &fallback_task_namespace) {
        Ok(digests) => digests,
        Err(error) => {
            eprintln!("[reflect] failed to digest session observations: {error}");
            return 1;
        }
    };
    let persistent_cats: Vec<String> = digests
        .iter()
        .flat_map(|d| d.failure_categories.iter().map(|(c, _)| c.clone()))
        .collect();
    if !persistent_cats.is_empty() {
        analysis.persistent_failure = true;
        analysis.persistent_failure_categories = persistent_cats;
    }
    let history: Vec<EvolutionRecord> = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::query_recent_records_scoped_pool(
            &pool,
            MAX_REFLECTION_HISTORY,
            Some(&slug),
        )
        .await
    })
    .unwrap_or_default();
    let landscape = evolve::build_landscape(&history, &digests, 2);
    if evolve::recommends_exploration(&landscape) {
        hint(
            "reflect",
            &format!(
                "Adaptation landscape: {} persistent failure(s), {} untried edit type(s) {:?} — consider a structural edit",
                landscape.persistent_failures.len(),
                landscape.untried_edit_types.len(),
                landscape.untried_edit_types,
            ),
        );
    }

    // 3. Stagnation (load metrics from SQLite, fallback to JSON).
    // Scoped to this project — the unscoped reader is indeterminate once a
    // second project has written metrics_state rows, and `save_metrics_pool`
    // below already writes scoped by slug.
    let mut metrics: Metrics = crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::metrics::load_metrics_scoped_pool(&pool, Some(&slug)).await
    })
    .unwrap_or_else(|e| {
        eprintln!("[reflect] SQLite metrics load failed, falling back to JSON: {e}");
        read_json(&metrics_file(), default_metrics())
    });
    let (should_rollback, improved, rolled_back_count) =
        match evolve::check_stagnation(&mut metrics, analysis.avg_score) {
            Ok(decision) => decision,
            Err(error) => {
                eprintln!("[reflect] evolved-skill checkpoint failure: {error}");
                return 1;
            }
        };

    // 4b. HarnessX seesaw constraint (R5): a coarse per-task regression gate.
    // Runs BEFORE seeding (digests are already available from step 2b) so a
    // regressing round blocks NEW skill commits at the source rather than
    // warning after the fact. The previous order (seed-then-check) was
    // toothless: skills were already on disk when the warning fired.
    let seesaw = evolve::load_registry();
    let regressed =
        evolve::seesaw_check(&seesaw, &digests, crate::evolve::seesaw::DEFAULT_TOLERANCE);
    let seesaw_blocked = !regressed.is_empty();
    if seesaw_blocked {
        hint(
            "reflect",
            &format!(
                "Seesaw constraint: {} previously solved task(s) regressed {:?} — blocking skill seeding this round",
                regressed.len(),
                regressed,
            ),
        );
    }

    // 4. Critic gate (Tier 2.1): the EARLIER, coarser gate that pairs with
    // seesaw. Suppresses ALL new seeding when reward hacking is suspected
    // (paper §4.3). Computed pre-seed by appending this session's dimension
    // averages to a throwaway copy of score_history, so the detector sees the
    // current round without mutating metrics before its later official push.
    // The probe is reused as the metrics passed to seed_smart_skills so the
    // per-edit Critic verdict is consistent with the round-level block.
    let probe_metrics = {
        let mut probe = metrics.clone();
        probe.score_history.push(SessionScoreEntry {
            timestamp: now_iso(),
            success_rate: analysis.success_rate,
            avg_score: analysis.avg_score,
            observations: analysis.total_observations,
            dimension_averages: analysis.dimension_averages,
        });
        probe.reward_hacking_suspected = evolve::detect_reward_hacking(&probe);
        if probe.reward_hacking_suspected {
            hint(
                "reflect",
                "Critic: reward hacking suspected (execution_cost rising while output_quality falls) — blocking skill seeding this round",
            );
        }
        probe
    };
    let critic_blocked = crate::evolve::critic::Critic::should_block_seeding(&probe_metrics);

    // 5. Seed evolved skills (skipped on rollback, seesaw regression, OR critic block)
    ensure_dir(&evolved_dir());
    let existing = list_dirs(&evolved_dir());
    let outcome = if !should_rollback && !seesaw_blocked && !critic_blocked {
        match evolve::seed_smart_skills(&analysis, &existing, &probe_metrics, reflection_session_id)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("[reflect] failed to persist pending synthesis: {error}");
                return 1;
            }
        }
    } else {
        crate::evolve::skills::ApplyOutcome::default()
    };
    let mut seeded = if seesaw_blocked || critic_blocked {
        0
    } else {
        outcome.applied
    };
    let manifests = outcome.manifests;
    if seesaw_blocked {
        seeded = 0;
    }

    // Update the solved-task registry. Guard: only update when the gate
    // PASSED. update() is monotonic (only improves bests), but recording a
    // regressing round's coincidental high scores would raise the best-of
    // for tasks that scored well by luck, masking future genuine regressions.
    if !seesaw_blocked {
        let mut reg = seesaw;
        let scores = evolve::scores_from_digests(&digests);
        reg.update(&scores);
        if let Err(error) = evolve::save_registry(&reg) {
            eprintln!("[reflect] failed to persist solved-task registry: {error}");
            return 1;
        }
    }

    // 4c. HarnessX variant isolation (R6): record this session's stack outcome
    // into the variant pool so warm routing can converge on per-stack success
    // rates, and fork-on-regression when the seesaw detected a regression on a
    // stack that already has a variant (the core catastrophic-forgetting
    // defense, paper §4.5). With a cold/empty pool this is a no-op that just
    // seeds routing stats; forking meaningfully engages once variants exist.
    let mut pool = crate::evolve::variants::VariantPool::load();
    let session_stack: Vec<String> = detect_session_stack(&observations);
    if !session_stack.is_empty() && !pool.has_session_outcome(reflection_session_id) {
        let stack_str = session_stack.first().cloned().unwrap_or_default();
        if seesaw_blocked {
            // A regressing round on a known stack → fork rather than let the
            // existing variant absorb the bad edit. record_outcome then runs
            // on the (possibly forked) variant the session is routed to.
            let target_id = pool
                .route(&[stack_str.as_str()])
                .map(|v| v.id.clone())
                .unwrap_or_else(|| stack_str.clone());
            pool.fork_if_needed(&target_id, true);
        }
        let route_id = pool
            .route(&[stack_str.as_str()])
            .map(|v| v.id.clone())
            .unwrap_or_else(|| stack_str.clone());
        pool.record_outcome_once(reflection_session_id, &route_id, !seesaw_blocked);
        if let Err(error) = pool.save() {
            eprintln!("[reflect] failed to persist variant pool: {error}");
            return 1;
        }
    }

    // 5. Gate
    evolve::gate_skills();

    // 6. Skill attribution — score only what the session actually saw.
    // `existing` is the pre-seed listing: skills seeded THIS round were not
    // in context during the session and must not be credited with its score.
    // The partition uses the date `resume` recorded at session start
    // (session_start.json) so the arm matches what was actually injected —
    // even when the session spans UTC midnight. Falls back to today on a
    // cold start (no prior resume this session).
    let evolved_dirs = list_dirs(&evolved_dir());
    let partition_date = reflection_partition_date(reflection_session_id)
        .map(str::to_owned)
        .or_else(crate::shared::helpers::read_session_start_date)
        .unwrap_or_else(today);
    let (active_skills, holdout_skills) =
        evolve::partition_holdout(&existing, &metrics, &partition_date);
    evolve::update_skill_attribution(&mut metrics, &analysis, &active_skills, &holdout_skills);

    // 7. Cross-project export
    if let Err(error) =
        evolve::export_to_global(reflection_session_id, &analysis, &analysis.failure_patterns)
    {
        eprintln!("[reflect] failed to export global patterns: {error}");
        return 1;
    }

    // 7.5. Prune expired entries from the negative feedback buffer (SkillOpt §4)
    evolve::prune_rejected_buffer();

    // 8. Memory auto-ingest (knowledge graph)
    let (mem_nodes, mem_edges) = evolve::ingest_to_memory(&analysis, &analysis.failure_patterns);

    // 8.5. Instinct extraction and promotion
    let instincts = evolve::extract_instincts(&observations, &analysis);
    let instincts_promoted = if !instincts.is_empty() {
        evolve::promote_instincts_to_global(&instincts)
    } else {
        0
    };
    if instincts_promoted > 0 {
        hint(
            "reflect",
            &format!("Instinct: promoted {instincts_promoted} new instinct(s)"),
        );
    }

    // 9. Evolution record
    let record = EvolutionRecord {
        session_id: Some(reflection_session_id.to_string()),
        timestamp: now_iso(),
        observations: analysis.total_observations,
        success_rate: analysis.success_rate,
        avg_score: analysis.avg_score,
        error_patterns: analysis.per_error_stats.clone(),
        failure_patterns: analysis.failure_patterns.clone(),
        skills_seeded: seeded,
        skills_rolled_back: rolled_back_count,
        total_evolved: evolved_dirs.len() as u64,
        analysis_summary: evolve::build_summary(&analysis),
        edit_type: EditType::AddSkill,
        manifests: manifests.clone(),
    };
    // Write evolution record to SQLite (primary) + JSONL (fallback)
    match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::insert_reflection_record_once_pool(
            &pool,
            &record,
            &slug,
            reflection_session_id,
        )
        .await
    }) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("[reflect] failed to persist evolution record: {error}");
            return 1;
        }
    };
    if let Err(error) = append_evolution_fallback_once(&record) {
        eprintln!("[reflect] failed to persist evolution fallback: {error}");
        return 1;
    }

    // Keep the server-visible falsifiability ledger in sync with the durable
    // session record. The file remains a compatibility projection.
    if let Err(error) = append_manifest_fallback_once(reflection_session_id, &manifests) {
        eprintln!("[reflect] failed to persist manifest ledger: {error}");
        return 1;
    }

    // 10. Session handoff context
    let last_errors: Vec<String> = observations
        .iter()
        .filter(|o| o.result.as_deref() == Some("error"))
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|o| {
            let cat = o.failure_category.as_deref().unwrap_or("unknown");
            let snippet = o
                .error_snippet
                .as_deref()
                .unwrap_or(o.action.as_deref().unwrap_or(""));
            format!("{cat}: {}", truncate_utf8(snippet, 100))
        })
        .collect();
    if !last_errors.is_empty() {
        metrics.last_error_context = Some(last_errors.join(" | "));
    }

    // 11. Update metrics
    fn round3(v: f64) -> f64 {
        (v * 1000.0).round() / 1000.0
    }

    let score_entry = SessionScoreEntry {
        timestamp: now_iso(),
        success_rate: analysis.success_rate,
        avg_score: analysis.avg_score,
        observations: analysis.total_observations,
        dimension_averages: analysis.dimension_averages,
    };
    metrics.score_history.push(score_entry);
    if metrics.score_history.len() > 50 {
        let start = metrics.score_history.len() - 50;
        metrics.score_history = metrics.score_history[start..].to_vec();
    }

    metrics.total_sessions += 1;
    metrics.avg_success_rate = round3(
        ((metrics.avg_success_rate * (metrics.total_sessions - 1) as f64) + analysis.success_rate)
            / metrics.total_sessions as f64,
    );
    metrics.total_evolved_skills = record.total_evolved;
    metrics.last_session = Some(now_iso());

    if improved {
        metrics.best_score = Some(analysis.avg_score);
        metrics.best_session = now_iso();
        metrics.stagnation_count = 0;
    }
    metrics.trend = evolve::compute_trend(&metrics.score_history).into();

    // SkillOpt §4 Slow/Meta Update: classify epoch for adaptive strategy
    metrics.epoch_class = Some(evolve::classify_epoch(&metrics.score_history));

    // HarnessX Tier 2.2: detect reward hacking (efficiency proxy rising
    // while quality proxy falling). Computed only — seeding suppression is
    // the Critic's job (Tier 2.1), not this hook.
    metrics.reward_hacking_suspected = evolve::detect_reward_hacking(&metrics);

    // Update evolved skill meta files with epoch classification
    if let Some(ref epoch) = metrics.epoch_class {
        evolve::update_meta_field(&evolved_dirs, epoch, analysis.avg_score);
    }

    // Save metrics to SQLite (primary) + JSON file (fallback)
    let metrics_applied = match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::metrics::save_metrics_once_pool(&pool, &metrics, &slug, reflection_session_id)
            .await
    }) {
        Ok(applied) => applied,
        Err(error) => {
            eprintln!("[reflect] failed to persist metrics: {error}");
            return 1;
        }
    };
    let fallback_metrics = if metrics_applied {
        metrics.clone()
    } else {
        match crate::store::runtime::block_on(async {
            let pool = crate::store::pool::harness_pool().await?;
            crate::store::metrics::load_metrics_scoped_pool(&pool, Some(&slug)).await
        }) {
            Ok(metrics) => metrics,
            Err(error) => {
                eprintln!("[reflect] failed to reload committed metrics fallback: {error}");
                return 1;
            }
        }
    };
    {
        let json = match serde_json::to_string_pretty(&fallback_metrics) {
            Ok(json) => json,
            Err(error) => {
                eprintln!("[reflect] failed to serialize metrics fallback: {error}");
                return 1;
            }
        };
        if let Err(error) = atomic_write(&metrics_file(), json.as_bytes()) {
            eprintln!("[reflect] failed to persist metrics fallback: {error}");
            return 1;
        }
    }

    // 11.5. Sync orbit pipeline files → SQLite (dual-write: files are source of truth
    //       for /orbit phase recovery; SQLite enables REST API + dashboard queries).
    let orbit_pipelines = match orbit_pipeline_candidates(&orbit_dir()) {
        Ok(pipelines) => pipelines,
        Err(error) => {
            eprintln!("[reflect] failed to collect orbit pipelines: {error}");
            return 1;
        }
    };
    {
        let pool = match crate::store::runtime::block_on(crate::store::pool::harness_pool()) {
            Ok(pool) => pool,
            Err(error) => {
                eprintln!("[reflect] failed to open orbit store: {error}");
                return 1;
            }
        };
        if !orbit_pipelines.is_empty() {
            let mut synced = 0usize;
            for path in &orbit_pipelines {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let content = match read_orbit_pipeline_file(path) {
                    Ok(content) => content,
                    Err(error) => {
                        eprintln!("[reflect] failed to read orbit {}: {error}", path.display());
                        return 1;
                    }
                };
                let pl = match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(pipeline) => pipeline,
                    Err(error) => {
                        eprintln!("[reflect] invalid orbit {}: {error}", path.display());
                        return 1;
                    }
                };
                let id = pl["id"].as_str().unwrap_or(name.trim_end_matches(".json"));
                let durable_evolution = match (
                    pl.get("status").and_then(serde_json::Value::as_str),
                    pl.get("evolution_session_id")
                        .and_then(serde_json::Value::as_str),
                    pl.get("id").and_then(serde_json::Value::as_str),
                ) {
                    (Some("complete") | Some("shipped"), Some(session_id), Some(id)) => {
                        match crate::store::runtime::block_on(
                            crate::store::evolution::reflection_pipeline_completed_pool(
                                &pool,
                                session_id,
                                &project_slug(),
                                id,
                            ),
                        ) {
                            Ok(durable) => durable,
                            Err(error) => {
                                eprintln!(
                                    "[reflect] failed to validate durable evolution for orbit {id}: {error}"
                                );
                                return 1;
                            }
                        }
                    }
                    _ => false,
                };
                if !orbit_pipeline_is_persistable(&pl, durable_evolution) {
                    eprintln!(
                        "[reflect] refusing to persist invalid completed orbit {}",
                        path.display()
                    );
                    if let Err(error) = crate::store::runtime::block_on(
                        crate::store::orbit_store::dismiss_pipeline_pool(
                            &pool,
                            &project_slug(),
                            id,
                        ),
                    ) {
                        eprintln!("[reflect] failed to remove invalid orbit {id}: {error}");
                        return 1;
                    }
                    continue;
                }
                let status = pl["status"].as_str().unwrap_or("unknown");
                let phase = pl["phase"].as_str();
                let mode = pl["mode"].as_str();
                if let Err(error) = crate::store::runtime::block_on(
                    crate::store::orbit_store::upsert_pipeline_pool(
                        &pool,
                        id,
                        &project_slug(),
                        status,
                        phase,
                        mode,
                        &content,
                    ),
                ) {
                    eprintln!("[reflect] failed to persist orbit {id}: {error}");
                    return 1;
                }
                synced += 1;
            }
            if synced > 0 {
                eprintln!("[reflect] synced {synced} orbit pipeline(s) to SQLite");
            }
        }
    }

    // 11.6a. Orbit completion invariants — report pipelines whose own state
    //        contradicts "complete" instead of trusting the flag.
    for (id, violation) in orbit_completion_violations(&orbit_pipelines, |pipeline| {
        let Some(session_id) = pipeline
            .get("evolution_session_id")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let Some(id) = pipeline.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        crate::store::runtime::block_on(async {
            let pool = crate::store::pool::harness_pool().await?;
            crate::store::evolution::reflection_pipeline_completed_pool(
                &pool, session_id, &slug, id,
            )
            .await
        })
        .unwrap_or(false)
    }) {
        hint("reflect", &format!("Orbit {id}: {violation}"));
    }

    // 11.7. Workspace manifest
    // This manifest is derived from the already-durable evolved skill files.
    // Its legacy API reports no write result, so it cannot be a completion
    // boundary for this SessionEnd transaction.
    evolve::write_workspace_manifest();

    // 12. Report
    hint(
        "reflect",
        &format!(
            "Session: {:.1}% success, avg_score={} ({} obs)",
            analysis.success_rate * 100.0,
            analysis.avg_score,
            analysis.total_observations
        ),
    );

    let weak_tools: Vec<String> = analysis
        .per_tool_stats
        .iter()
        .filter(|(_, s)| {
            s.total >= CONFIG.pattern.weak_tool_min_obs
                && (s.successes as f64 / s.total as f64) < CONFIG.pattern.weak_tool_rate
        })
        .map(|(cat, s)| {
            format!(
                "{cat} {}%",
                (s.successes as f64 / s.total as f64 * 100.0) as u32
            )
        })
        .collect();
    if !weak_tools.is_empty() {
        hint("reflect", &format!("Weak tools: {}", weak_tools.join(", ")));
    }

    let weak_exts: Vec<String> = analysis
        .per_ext_stats
        .iter()
        .filter(|(_, s)| {
            s.total >= CONFIG.pattern.weak_ext_min_obs
                && s.success_rate < CONFIG.pattern.weak_ext_rate
        })
        .map(|(ext, s)| format!("{ext} {}%", (s.success_rate * 100.0) as u32))
        .collect();
    if !weak_exts.is_empty() {
        hint(
            "reflect",
            &format!("Weak file types: {}", weak_exts.join(", ")),
        );
    }

    if !analysis.failure_patterns.is_empty() {
        let pats: Vec<String> = analysis
            .failure_patterns
            .iter()
            .map(|p| format!("{}({})", p.pattern_type, p.count))
            .collect();
        hint("reflect", &format!("Patterns: {}", pats.join(", ")));
    }

    if seeded > 0 {
        hint("reflect", &format!("Evolved {seeded} new skill(s)"));
    }
    if should_rollback {
        hint(
            "reflect",
            &format!("Rolled back {rolled_back_count} stagnant skills"),
        );
    }
    hint(
        "reflect",
        &format!(
            "Trend: {} ({} sessions)",
            metrics.trend,
            metrics.score_history.len()
        ),
    );

    // Skill attribution report
    let effective: Vec<_> = metrics
        .skill_attribution
        .values()
        .filter(|a| a.sessions_active >= 2 && a.avg_score_with > a.avg_score_without + 0.02)
        .collect();
    let ineffective: Vec<_> = metrics
        .skill_attribution
        .values()
        .filter(|a| a.sessions_active >= 2 && a.avg_score_with < a.avg_score_without - 0.02)
        .collect();
    if !effective.is_empty() {
        let parts: Vec<String> = effective
            .iter()
            .map(|s| {
                format!(
                    "{}(+{}%)",
                    s.skill_name,
                    ((s.avg_score_with - s.avg_score_without) * 100.0) as i32
                )
            })
            .collect();
        hint(
            "reflect",
            &format!("Effective skills: {}", parts.join(", ")),
        );
    }
    if !ineffective.is_empty() {
        let names: Vec<&str> = ineffective.iter().map(|s| s.skill_name.as_str()).collect();
        hint(
            "reflect",
            &format!(
                "Ineffective skills: {} — consider /evolve rollback",
                names.join(", ")
            ),
        );
    }

    if mem_nodes > 0 || mem_edges > 0 {
        hint(
            "reflect",
            &format!("Memory: +{mem_nodes} nodes, +{mem_edges} edges ingested"),
        );
    }

    TELEMETRY.track_session_ended(
        analysis.success_rate,
        evolve::safe_avg_score(analysis.avg_score),
        analysis.total_observations,
        metrics.trend.parse().unwrap_or(SessionTrend::Stable),
        seeded,
    );

    mark_reflection_completed(reflection_session_id, &slug, &observations)
}

fn mark_reflection_completed(
    reflection_session_id: &str,
    project: &str,
    observations: &[ObsRecord],
) -> i32 {
    let observed_pipeline_ids = observed_pipeline_ids(observations);
    let pipeline_ids = match crate::shared::orbit::reflection_completion_candidates_in(
        &harness_dir(),
        &observed_pipeline_ids,
    ) {
        Ok(pipeline_ids) => pipeline_ids,
        Err(error) => {
            eprintln!("[reflect] failed to select Orbit completion candidates: {error}");
            return 1;
        }
    };
    match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::mark_reflection_completed_with_pipelines_pool(
            &pool,
            reflection_session_id,
            project,
            &pipeline_ids,
        )
        .await
    }) {
        Ok(()) => complete_recorded_orbit_pipelines(reflection_session_id, project),
        Err(error) => {
            eprintln!("[reflect] failed to mark reflection complete: {error}");
            1
        }
    }
}

// ── Orbit completion invariant detection ───────────────────────────────────

/// Collect invariant violations across the bounded pipeline candidate set.
///
/// Returns `(pipeline_id, violation)` pairs. See
/// `shared::orbit::completion_violations_with_durable_evolution` for what is
/// checked and why.
fn complete_recorded_orbit_pipelines(reflection_session_id: &str, project: &str) -> i32 {
    let pipeline_ids = match crate::store::runtime::block_on(async {
        let pool = crate::store::pool::harness_pool().await?;
        crate::store::evolution::reflection_pipeline_ids_pool(&pool, reflection_session_id, project)
            .await
    }) {
        Ok(pipeline_ids) => pipeline_ids,
        Err(error) => {
            eprintln!("[reflect] failed to recover Orbit completion evidence: {error}");
            return 1;
        }
    };
    complete_recorded_orbit_pipeline_ids(reflection_session_id, project, &pipeline_ids)
}

fn observed_pipeline_ids(observations: &[ObsRecord]) -> Vec<String> {
    let mut pipeline_ids: Vec<String> = observations
        .iter()
        .filter_map(|observation| observation.pipeline_id.clone())
        .collect();
    pipeline_ids.sort();
    pipeline_ids.dedup();
    pipeline_ids
}

fn complete_recorded_orbit_pipeline_ids(
    reflection_session_id: &str,
    project: &str,
    pipeline_ids: &[String],
) -> i32 {
    if pipeline_ids.is_empty() {
        return 0;
    }
    match crate::shared::orbit::complete_pipelines_after_reflection_in(
        &harness_dir(),
        reflection_session_id,
        project,
        pipeline_ids,
    ) {
        Ok(0) => match sync_completed_orbit_pipelines(reflection_session_id, project, pipeline_ids)
        {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("[reflect] failed to sync completed Orbit pipeline: {error}");
                1
            }
        },
        Ok(completed) => {
            if let Err(error) =
                sync_completed_orbit_pipelines(reflection_session_id, project, pipeline_ids)
            {
                eprintln!("[reflect] failed to sync completed Orbit pipeline: {error}");
                return 1;
            }
            hint(
                "reflect",
                &format!("Orbit: completed {completed} pipeline(s) after durable evolution"),
            );
            0
        }
        Err(error) => {
            eprintln!("[reflect] failed to complete evolved Orbit pipeline: {error}");
            1
        }
    }
}

fn sync_completed_orbit_pipelines(
    reflection_session_id: &str,
    project: &str,
    pipeline_ids: &[String],
) -> io::Result<()> {
    let expected: std::collections::BTreeSet<&str> =
        pipeline_ids.iter().map(String::as_str).collect();
    if expected.is_empty() {
        return Ok(());
    }
    let pool = crate::store::runtime::block_on(crate::store::pool::harness_pool())?;
    sync_completed_orbit_pipelines_in(
        &harness_dir(),
        &pool,
        reflection_session_id,
        project,
        &expected,
    )
}

fn sync_completed_orbit_pipelines_in(
    harness: &Path,
    pool: &sqlx::AnyPool,
    reflection_session_id: &str,
    project: &str,
    expected: &std::collections::BTreeSet<&str>,
) -> io::Result<()> {
    let mut synced = std::collections::BTreeSet::new();
    for path in all_orbit_pipeline_files(&harness.join("orbit"))? {
        // A malformed unrelated pipeline must not poison an exact replay.
        // If a mapped pipeline cannot be read, it remains absent from `synced`
        // and the explicit missing-identity error below keeps the job retryable.
        let Ok(content) = read_orbit_pipeline_file(&path) else {
            continue;
        };
        let Ok(pipeline) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(id) = pipeline.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !expected.contains(id)
            || pipeline.get("status").and_then(serde_json::Value::as_str) != Some("complete")
            || pipeline.get("phase").and_then(serde_json::Value::as_str) != Some("evolve")
            || pipeline
                .get("evolution_session_id")
                .and_then(serde_json::Value::as_str)
                != Some(reflection_session_id)
        {
            continue;
        }
        crate::store::runtime::block_on(crate::store::orbit_store::upsert_pipeline_pool(
            &pool,
            id,
            project,
            "complete",
            Some("evolve"),
            pipeline.get("mode").and_then(serde_json::Value::as_str),
            &content,
        ))?;
        synced.insert(id.to_string());
    }
    if synced.len() != expected.len() {
        let missing: Vec<_> = expected
            .iter()
            .filter(|id| !synced.contains::<str>(*id))
            .copied()
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "completed Orbit pipeline file missing or inconsistent: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Return every regular pipeline file when replaying an exact durable mapping.
/// The dashboard/reporting view is bounded, but synchronization cannot omit a
/// valid mapped file just because many newer pipeline files exist.
fn all_orbit_pipeline_files(orbit_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(orbit_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if entry.file_type()?.is_file() && name.starts_with("PIPELINE-") && name.ends_with(".json")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn orbit_pipeline_candidates(orbit_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(orbit_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut newest = std::collections::BinaryHeap::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if entry.file_type()?.is_file() && name.starts_with("PIPELINE-") && name.ends_with(".json")
        {
            newest.push(std::cmp::Reverse(path));
            if newest.len() > MAX_ORBIT_PIPELINE_FILES {
                newest.pop();
            }
        }
    }
    let mut candidates: Vec<PathBuf> = newest
        .into_iter()
        .map(|std::cmp::Reverse(path)| path)
        .collect();
    candidates.sort_by(|left, right| right.cmp(left));
    Ok(candidates)
}

fn read_orbit_pipeline_file(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::with_capacity(MAX_ORBIT_PIPELINE_BYTES.min(64 * 1024));
    fs::File::open(path)?
        .take((MAX_ORBIT_PIPELINE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ORBIT_PIPELINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "orbit pipeline exceeds {MAX_ORBIT_PIPELINE_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn orbit_completion_violations(
    pipelines: &[PathBuf],
    mut has_durable_evolution: impl FnMut(&serde_json::Value) -> bool,
) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for path in pipelines {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Ok(content) = read_orbit_pipeline_file(path) else {
            continue;
        };
        let Ok(pipeline) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let id = pipeline["id"]
            .as_str()
            .unwrap_or(name.trim_end_matches(".json"));
        for v in crate::shared::orbit::completion_violations_with_durable_evolution(
            &pipeline,
            has_durable_evolution(&pipeline),
        ) {
            found.push((normalize_pipeline_id(id), v));
        }
    }
    found.sort();
    found
}

fn orbit_pipeline_is_persistable(pipeline: &serde_json::Value, durable_evolution: bool) -> bool {
    crate::shared::orbit::completion_violations_with_durable_evolution(pipeline, durable_evolution)
        .is_empty()
}

/// Detect the dominant stack tags from a session's observations, used by R6
/// variant routing. Inspects file extensions in `action` and the `file_ext`
/// field. Returns at most one tag (the dominant stack) to keep routing simple.
fn detect_session_stack(observations: &[ObsRecord]) -> Vec<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, u32> = HashMap::new();
    for o in observations {
        let ext = o
            .file_ext
            .clone()
            .or_else(|| {
                o.action.as_ref().and_then(|a| {
                    a.rsplit('.')
                        .next()
                        .filter(|e| e.len() <= 6 && !e.contains(' '))
                        .map(String::from)
                })
            })
            .unwrap_or_default()
            .to_lowercase();
        let tag = match ext.as_str() {
            "rs" => "rust",
            "py" => "python",
            "ts" | "tsx" => "typescript",
            "go" => "go",
            "java" | "kt" => "jvm",
            _ => continue,
        };
        *counts.entry(tag.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(t, _)| vec![t])
        .unwrap_or_default()
}

// ── Inline tests (kept here: run_context epoch helpers) ──
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[serial_test::serial]
    fn alcove_collects_a_vault_beneath_a_plain_home_path() {
        struct HomeRestore(Option<std::ffi::OsString>);

        impl Drop for HomeRestore {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(home) => std::env::set_var("HOME", home),
                        None => std::env::remove_var("HOME"),
                    }
                }
            }
        }

        let home = tempfile::tempdir().expect("home directory");
        let _restore = HomeRestore(std::env::var_os("HOME"));
        unsafe { std::env::set_var("HOME", home.path()) };
        let vault = home.path().join("vault");
        fs::create_dir(&vault).expect("vault directory");
        fs::write(vault.join("note.md"), "# note").expect("vault note");

        let collected = collect_alcove(&crate::config::AlcoveConfig {
            vault_path: "~/vault".into(),
            projects: vec![],
            max_docs: 1,
        });

        assert_eq!(collected["docs_collected"], 1);
    }

    fn fallback_record(session_id: &str) -> EvolutionRecord {
        EvolutionRecord {
            session_id: Some(session_id.into()),
            timestamp: "2026-07-28T00:00:00Z".into(),
            observations: 1,
            success_rate: 1.0,
            avg_score: 1.0,
            error_patterns: HashMap::new(),
            failure_patterns: vec![],
            skills_seeded: 0,
            skills_rolled_back: 0,
            total_evolved: 0,
            analysis_summary: "test".into(),
            edit_type: EditType::AddSkill,
            manifests: vec![],
        }
    }

    #[test]
    fn evolution_fallback_replay_repairs_projection_without_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("evolution.jsonl");
        let sessions = dir.path().join("evolution.jsonl.sessions");
        let record = fallback_record("session-a");

        append_evolution_fallback_once_at(&ledger, &sessions, &record).unwrap();
        fs::remove_file(&ledger).unwrap();
        append_evolution_fallback_once_at(&ledger, &sessions, &record).unwrap();

        let records =
            read_bounded_jsonl::<EvolutionRecord>(&ledger, 10, 1024 * 1024, 64 * 1024).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id.as_deref(), Some("session-a"));
    }

    #[test]
    fn corrupt_session_projection_is_repaired_on_replay() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("evolution.jsonl.sessions");
        fs::create_dir_all(&sessions).unwrap();
        let projection = session_projection_path(&sessions, "session-a").unwrap();
        fs::write(&projection, b"{").unwrap();
        let record = fallback_record("session-a");

        write_session_projection_once(&projection, &record).unwrap();

        let repaired: EvolutionRecord =
            serde_json::from_slice(&fs::read(&projection).unwrap()).unwrap();
        assert_eq!(repaired.session_id.as_deref(), Some("session-a"));
    }

    #[test]
    fn compatibility_evolution_projection_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("evolution.jsonl");
        let sessions = dir.path().join("evolution.jsonl.sessions");
        for index in 0..(MAX_COMPATIBILITY_PROJECTIONS + 1) {
            let record = fallback_record(&format!("session-{index:03}"));
            append_evolution_fallback_once_at(&ledger, &sessions, &record).unwrap();
        }

        assert_eq!(
            projection_files(&sessions).unwrap().len(),
            MAX_COMPATIBILITY_PROJECTIONS
        );
        assert!(
            read_bounded_jsonl::<EvolutionRecord>(
                &ledger,
                MAX_COMPATIBILITY_PROJECTIONS + 1,
                MAX_REFLECTION_JSONL_BYTES,
                MAX_REFLECTION_JSONL_LINE_BYTES,
            )
            .unwrap()
            .len()
                <= MAX_COMPATIBILITY_PROJECTIONS
        );
    }

    #[test]
    fn claude_session_collection_reads_only_the_bounded_file_tail() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project-a");
        fs::create_dir_all(&project).unwrap();
        let session = project.join("session.jsonl");
        let mut records = String::new();
        for index in 0..80 {
            records.push_str(
                &serde_json::json!({
                    "timestamp": format!("2026-07-28T00:{index:02}:00Z"),
                    "model": "test",
                    "padding": "x".repeat(8 * 1024),
                })
                .to_string(),
            );
            records.push('\n');
        }
        fs::write(&session, records).unwrap();

        let collected = collect_claude_session_at(root.path());
        let sessions = collected["sessions"].as_array().unwrap();

        assert_eq!(sessions.len(), 5);
        assert_eq!(sessions[0]["timestamp"], "2026-07-28T00:79:00Z");
        assert_eq!(sessions[4]["timestamp"], "2026-07-28T00:75:00Z");
    }

    #[test]
    fn manifest_fallback_replay_repairs_projection_without_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("manifests.jsonl");
        let sessions = dir.path().join("manifests.jsonl.sessions");
        let manifests = vec![crate::evolve::edits::EditManifest {
            edit_type: EditType::AddSkill,
            target: "evo-test".into(),
            intended_effect: "test".into(),
            predicted_impact: "test".into(),
        }];

        append_manifest_fallback_once_at(&ledger, &sessions, "session-a", &manifests).unwrap();
        fs::remove_file(&ledger).unwrap();
        append_manifest_fallback_once_at(&ledger, &sessions, "session-a", &manifests).unwrap();

        let values =
            read_bounded_jsonl::<serde_json::Value>(&ledger, 10, 1024 * 1024, 64 * 1024).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["target"], "evo-test");
        assert_eq!(values[0]["session_id"], "session-a");
    }

    #[test]
    #[serial_test::serial]
    fn reflection_project_discovery_uses_jsonl_when_sqlite_is_unavailable() {
        struct HomeRestore {
            home: Option<std::ffi::OsString>,
        }

        impl Drop for HomeRestore {
            fn drop(&mut self) {
                unsafe {
                    match self.home.take() {
                        Some(home) => std::env::set_var("HOME", home),
                        None => std::env::remove_var("HOME"),
                    }
                }
            }
        }

        let home = tempfile::tempdir().unwrap();
        let _restore = HomeRestore {
            home: std::env::var_os("HOME"),
        };
        unsafe { std::env::set_var("HOME", home.path()) };

        let session_id = "20260729_fallback";
        let project = "fallback-project";
        let observations = home
            .path()
            .join(".harness/projects")
            .join(project)
            .join("obs");
        fs::create_dir_all(&observations).unwrap();
        let record = ObsRecord {
            timestamp: "2026-07-29T00:00:00Z".into(),
            tool: "Read".into(),
            tool_category: "read".into(),
            action: None,
            result: Some("success".into()),
            score: Some(1.0),
            dimensions: None,
            failure_category: None,
            error_snippet: None,
            file_ext: None,
            sequence_id: None,
            pipeline_id: None,
            tool_use_id: None,
        };
        fs::write(
            observations.join(format!("session_{session_id}.jsonl")),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        fs::create_dir_all(home.path().join(".harness")).unwrap();
        fs::write(home.path().join(".harness/harness.db"), "not sqlite").unwrap();

        assert_eq!(
            reflection_projects_for_session(session_id).unwrap(),
            vec![project.to_string()]
        );
    }

    #[test]
    fn pending_queue_scan_ignores_completed_entries_before_the_cap() {
        let queue = tempfile::tempdir().unwrap();
        for index in 0..MAX_REFLECTION_QUEUE_SCAN {
            fs::write(
                queue.path().join(format!("job_{index:03}.completed")),
                "done",
            )
            .unwrap();
        }
        let job = ReflectionJob {
            session_id: "20260728_pending".into(),
            project: "test-project".into(),
            created_at: "2026-07-28T00:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = queue.path().join("job_999.pending");
        fs::write(&pending, serde_json::to_vec(&job).unwrap()).unwrap();

        assert_eq!(
            pending_reflection_jobs(queue.path()).unwrap(),
            vec![pending]
        );
    }

    #[test]
    fn pending_queue_quarantines_malformed_jobs_before_valid_jobs() {
        let queue = tempfile::tempdir().unwrap();
        for index in 0..MAX_REFLECTION_QUEUE_SCAN {
            fs::write(
                queue.path().join(format!("job_{index:03}.pending")),
                "not json",
            )
            .unwrap();
        }
        let job = ReflectionJob {
            session_id: "20260729_valid".into(),
            project: "test-project".into(),
            created_at: "2026-07-29T00:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let valid = queue.path().join("job_999.pending");
        fs::write(&valid, serde_json::to_vec(&job).unwrap()).unwrap();

        assert_eq!(pending_reflection_jobs(queue.path()).unwrap(), vec![valid]);
        for index in 0..MAX_REFLECTION_QUEUE_SCAN {
            assert!(
                queue
                    .path()
                    .join(format!("job_{index:03}.failed"))
                    .is_file(),
                "malformed job {index} must be quarantined"
            );
        }
    }

    #[test]
    fn interrupted_reflection_job_publication_leaves_no_pending_file() {
        let queue = tempfile::tempdir().unwrap();
        let pending = queue.path().join("job_interrupted.pending");

        let error = publish_new_file(queue.path(), &pending, |file| {
            file.write_all(b"{")?;
            Err(io::Error::other("simulated interruption"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!pending.exists());
        assert_eq!(fs::read_dir(queue.path()).unwrap().count(), 0);
    }

    #[test]
    fn malformed_claim_is_failed_and_replay_can_publish_again() {
        let queue = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_malformed".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(queue.path(), &job)
            .unwrap()
            .expect("pending job");
        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("claimed job");
        fs::write(&claimed, b"{").unwrap();

        assert!(read_claimed_reflection_job(&claimed).is_err());
        assert!(claimed.with_extension("failed").is_file());
        assert!(
            enqueue_reflection_job(queue.path(), &job)
                .unwrap()
                .is_some(),
            "a failed malformed claim must not suppress replay"
        );
    }

    #[test]
    fn abandoned_queue_recovery_filters_claims_before_the_cap() {
        let queue = tempfile::tempdir().unwrap();
        for index in 0..MAX_REFLECTION_QUEUE_SCAN {
            fs::write(
                queue.path().join(format!("job_{index:03}.completed")),
                "done",
            )
            .unwrap();
        }
        let claimed = queue.path().join("job_target.claimed");
        fs::write(&claimed, "{}").unwrap();

        assert_eq!(
            recover_abandoned_reflection_claims_if_unlocked(queue.path()).unwrap(),
            1
        );
        assert!(claimed.with_extension("pending").is_file());
    }

    #[test]
    fn orbit_pipeline_candidates_keep_only_the_newest_named_file_bound() {
        let orbit = tempfile::tempdir().unwrap();
        for index in 0..=MAX_ORBIT_PIPELINE_FILES {
            fs::write(orbit.path().join(format!("PIPELINE-{index:03}.json")), "{}").unwrap();
        }
        fs::write(orbit.path().join("unrelated.json"), "{}").unwrap();

        let candidates = orbit_pipeline_candidates(orbit.path()).unwrap();

        assert_eq!(candidates.len(), MAX_ORBIT_PIPELINE_FILES);
        assert!(
            candidates
                .iter()
                .all(|path| path.file_name().unwrap() != "PIPELINE-000.json")
        );
        assert!(
            candidates
                .iter()
                .any(|path| path.file_name().unwrap() == "PIPELINE-064.json")
        );
    }

    #[test]
    fn orbit_pipeline_sync_candidates_include_files_beyond_the_dashboard_bound() {
        let orbit = tempfile::tempdir().unwrap();
        for index in 0..=MAX_ORBIT_PIPELINE_FILES {
            fs::write(orbit.path().join(format!("PIPELINE-{index:03}.json")), "{}").unwrap();
        }

        assert_eq!(
            all_orbit_pipeline_files(orbit.path()).unwrap().len(),
            MAX_ORBIT_PIPELINE_FILES + 1
        );
    }

    #[test]
    fn sync_completed_orbit_pipeline_repairs_a_stale_store_row_beyond_the_dashboard_bound() {
        let harness = tempfile::tempdir().unwrap();
        let orbit = harness.path().join("orbit");
        fs::create_dir(&orbit).unwrap();
        for index in 0..=MAX_ORBIT_PIPELINE_FILES {
            fs::write(
                orbit.join(format!("PIPELINE-{index:03}.json")),
                format!(r#"{{"id":"other-{index}","status":"running","phase":"go"}}"#),
            )
            .unwrap();
        }
        let completed = orbit.join("PIPELINE-000.json");
        fs::write(
            &completed,
            r#"{"id":"ready","status":"complete","phase":"evolve","evolution_session_id":"session-ready"}"#,
        )
        .unwrap();
        assert!(
            !orbit_pipeline_candidates(&orbit)
                .unwrap()
                .contains(&completed),
            "the bounded dashboard scan must exclude the oldest target"
        );

        let pool = crate::store::runtime::block_on(async {
            let pool = crate::store::pool::test_memory_pool().await;
            crate::store::schema::init_schema_pool(&pool).await?;
            Ok::<_, io::Error>(pool)
        })
        .unwrap();
        let expected = std::collections::BTreeSet::from(["ready"]);
        sync_completed_orbit_pipelines_in(
            harness.path(),
            &pool,
            "session-ready",
            "project-a",
            &expected,
        )
        .unwrap();

        let stored = crate::store::runtime::block_on(
            crate::store::orbit_store::list_pipelines_scoped_pool(&pool, Some("project-a"), 1),
        )
        .unwrap();
        assert_eq!(stored[0]["id"], "ready");
        assert_eq!(stored[0]["status"], "complete");
        assert_eq!(stored[0]["phase"], "evolve");
    }

    #[test]
    fn orbit_pipeline_read_rejects_files_over_the_byte_bound() {
        let orbit = tempfile::tempdir().unwrap();
        let path = orbit.path().join("PIPELINE-too-large.json");
        fs::write(&path, vec![b' '; MAX_ORBIT_PIPELINE_BYTES + 1]).unwrap();

        let error = read_orbit_pipeline_file(&path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn epoch_days_to_ymd_known_date() {
        // 1970-01-01 = day 0
        assert_eq!(epoch_days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn epoch_days_to_ymd_mid_year() {
        // Approximate check: day 180 should be around June/July 1970
        let (y, m, _d) = epoch_days_to_ymd(180);
        assert_eq!(y, 1970);
        assert!(
            (6..=7).contains(&m),
            "month should be June or July, got {m}"
        );
    }

    // ── run_context signature ──────────────────────────
    #[test]
    fn effective_sources_all_expands() {
        let sources: Vec<String> = vec!["all".into()];
        let effective: Vec<&str> = if sources.contains(&"all".to_string()) {
            vec!["harness", "claude-session", "alcove"]
        } else if sources.is_empty() {
            vec!["harness"]
        } else {
            sources.iter().map(|s| s.as_str()).collect()
        };
        assert_eq!(effective, vec!["harness", "claude-session", "alcove"]);
    }

    #[test]
    fn effective_sources_empty_defaults_to_harness() {
        let sources: Vec<String> = vec![];
        let effective: Vec<&str> = if sources.contains(&"all".to_string()) {
            vec!["harness", "claude-session", "alcove"]
        } else if sources.is_empty() {
            vec!["harness"]
        } else {
            sources.iter().map(|s| s.as_str()).collect()
        };
        assert_eq!(effective, vec!["harness"]);
    }

    #[test]
    fn effective_sources_explicit_list_passthrough() {
        let sources: Vec<String> = vec!["harness".into(), "alcove".into()];
        let effective: Vec<&str> = if sources.contains(&"all".to_string()) {
            vec!["harness", "claude-session", "alcove"]
        } else if sources.is_empty() {
            vec!["harness"]
        } else {
            sources.iter().map(|s| s.as_str()).collect()
        };
        assert_eq!(effective, vec!["harness", "alcove"]);
    }

    #[test]
    fn since_overrides_days_in_date_range() {
        // When --since is provided, date_from should equal since value
        let since: Option<String> = Some("20260101".into());
        let days: u32 = 30;

        let (cutoff_tag, date_from) = if let Some(ref s) = since {
            (s.clone(), s.clone())
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let cutoff_ts = now.saturating_sub((days as u64) * 86400);
            let days_since_epoch = cutoff_ts / 86400;
            let (y, m, d) = epoch_days_to_ymd(days_since_epoch as i32);
            let tag = format!("{y:04}{m:02}{d:02}");
            (tag.clone(), tag)
        };

        assert_eq!(cutoff_tag, "20260101");
        assert_eq!(date_from, "20260101");
    }

    #[test]
    fn invalid_completed_orbit_is_not_persistable() {
        let pipeline = serde_json::json!({
            "status": "complete",
            "pr_url": "https://github.com/o/r/pull/1",
            "phase_history": [{"phase": "ship", "status": "complete"}]
        });

        assert!(!orbit_pipeline_is_persistable(&pipeline, false));
    }

    #[test]
    fn orbit_completion_detection_does_not_mutate_premature_phase_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("PIPELINE-test.json");
        let before = r#"{"id":"test","status":"complete","phase":"ship","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success"}"#;
        fs::write(&path, before).unwrap();

        let violations =
            orbit_completion_violations(&orbit_pipeline_candidates(dir.path()).unwrap(), |_| false);

        assert!(
            violations
                .iter()
                .any(|(_, v)| v.contains("phase=\"evolve\""))
        );
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn orbit_completion_detection_reports_an_invented_evolution_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("PIPELINE-invented.json");
        fs::write(
            &path,
            r#"{"id":"invented","status":"complete","phase":"evolve","audit_fail_count":0,"max_retries":3,"pr_url":"https://github.com/o/r/pull/1","ci_status":"success","evolution_session_id":"invented-session","phase_history":[]}"#,
        )
        .unwrap();

        let violations =
            orbit_completion_violations(&orbit_pipeline_candidates(dir.path()).unwrap(), |_| false);

        assert!(
            violations.iter().any(|(_, violation)| {
                violation.contains("no durable SessionEnd pipeline record")
            })
        );
    }

    #[test]
    fn running_orbit_remains_persistable() {
        let pipeline = serde_json::json!({"status": "running", "phase": "go"});
        assert!(orbit_pipeline_is_persistable(&pipeline, false));
    }

    #[test]
    fn detached_worker_keeps_the_session_partition_date() {
        assert_eq!(
            reflection_partition_date("20260727_host-session"),
            Some("20260727")
        );
        assert_eq!(reflection_partition_date("host-session"), None);
        assert_eq!(reflection_partition_date("日20260727_host-session"), None);
    }

    #[test]
    fn reflection_job_claim_and_completion_are_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_host-session".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };

        let pending = enqueue_reflection_job(dir.path(), &job)
            .unwrap()
            .expect("first enqueue");
        assert!(
            enqueue_reflection_job(dir.path(), &job).unwrap().is_none(),
            "pending job suppresses replay"
        );

        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("first claim");
        assert!(
            claim_reflection_job(&pending).unwrap().is_none(),
            "claimed job cannot be claimed twice"
        );
        let fence = reflection_claim_fence(&claimed).unwrap();
        complete_reflection_job(&claimed, &fence).unwrap();

        assert!(
            enqueue_reflection_job(dir.path(), &job).unwrap().is_none(),
            "completed job suppresses replay"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retry_replaces_the_claimed_inode_before_publishing_pending() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_atomic-retry".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job)
            .unwrap()
            .expect("pending job");
        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("claimed job");
        let claimed_inode = fs::metadata(&claimed).unwrap().ino();

        let fence = reflection_claim_fence(&claimed).unwrap();
        assert!(!retry_or_dead_letter_reflection_job(&claimed, &fence).unwrap());

        let pending_inode = fs::metadata(&pending).unwrap().ino();
        assert_ne!(
            pending_inode, claimed_inode,
            "retry must replace durable contents instead of truncating the claimed inode"
        );
        let saved: ReflectionJob = serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
        assert_eq!(saved.attempts, 1);
    }

    #[test]
    fn claim_renews_an_old_pending_job_with_a_fresh_owned_lease() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_fresh-lease".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job)
            .unwrap()
            .expect("pending job");
        let old = SystemTime::now() - Duration::from_secs(60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&pending)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();

        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("claimed job");
        let saved = read_claimed_reflection_job(&claimed).unwrap();

        assert!(saved.claim.as_ref().is_some_and(|lease| {
            !lease.claimed_at.is_empty() && !lease.owner.is_empty() && !lease.fence.is_empty()
        }));
        let live_worker = try_acquire_reflection_worker_lock(dir.path())
            .unwrap()
            .expect("live worker lock");
        assert_eq!(
            recover_abandoned_reflection_claims_if_unlocked(dir.path()).unwrap(),
            0,
            "the live OS lock must prevent recovery regardless of file age"
        );
        assert!(claimed.is_file());
        drop(live_worker);
    }

    #[test]
    fn persistence_failure_keeps_job_replayable_until_successful_completion() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_required-persistence".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job)
            .unwrap()
            .expect("pending job");
        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("claimed job");

        let fence = reflection_claim_fence(&claimed).unwrap();
        settle_reflection_job(&claimed, &fence, 1).unwrap();

        assert!(!claimed.with_extension("completed").exists());
        assert!(pending.is_file(), "failed required persistence must replay");
        let replayed = claim_reflection_job(&pending)
            .unwrap()
            .expect("replayed claim");
        let replay_fence = reflection_claim_fence(&replayed).unwrap();
        settle_reflection_job(&replayed, &replay_fence, 0).unwrap();
        assert!(replayed.with_extension("completed").is_file());
    }

    #[test]
    fn poison_reflection_job_is_dead_lettered_after_max_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_poison".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: MAX_REFLECTION_ATTEMPTS - 1,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job)
            .unwrap()
            .expect("pending job");
        let claimed = claim_reflection_job(&pending)
            .unwrap()
            .expect("claimed job");

        let fence = reflection_claim_fence(&claimed).unwrap();
        assert!(retry_or_dead_letter_reflection_job(&claimed, &fence).unwrap());
        assert!(!pending.exists(), "poison job must not return to pending");
        let failed = claimed.with_extension("failed");
        let saved: ReflectionJob = serde_json::from_slice(&fs::read(&failed).unwrap()).unwrap();
        assert_eq!(saved.attempts, MAX_REFLECTION_ATTEMPTS);
    }

    #[test]
    fn pending_queue_prioritizes_fresh_jobs_over_retries() {
        let dir = tempfile::tempdir().unwrap();
        let retry = ReflectionJob {
            session_id: "20260728_retry".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: MAX_REFLECTION_ATTEMPTS - 1,
            claim: None,
        };
        let fresh = ReflectionJob {
            session_id: "20260728_fresh".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let retry_path = enqueue_reflection_job(dir.path(), &retry).unwrap().unwrap();
        let fresh_path = enqueue_reflection_job(dir.path(), &fresh).unwrap().unwrap();

        assert_eq!(
            pending_reflection_jobs(dir.path()).unwrap(),
            vec![fresh_path, retry_path]
        );
    }

    #[test]
    fn live_reflection_lock_prevents_aging_recovery_until_the_owner_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260729_live-lock".into(),
            project: "project-a".into(),
            created_at: "2026-07-29T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job).unwrap().unwrap();
        let claimed = claim_reflection_job(&pending).unwrap().unwrap();
        let old = SystemTime::now() - Duration::from_secs(16 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&claimed)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();

        let ready = dir.path().join("worker-ready");
        let release = dir.path().join("worker-release");
        let executable = std::env::current_exe().unwrap();
        let mut live_worker = Command::new(executable)
            .arg("--exact")
            .arg("hooks::reflect::tests::reflection_lock_child_process")
            .arg("--nocapture")
            .env("EPIC_TEST_REFLECTION_LOCK_QUEUE", dir.path())
            .env("EPIC_TEST_REFLECTION_LOCK_READY", &ready)
            .env("EPIC_TEST_REFLECTION_LOCK_RELEASE", &release)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "child worker must acquire the OS lock");
        assert_eq!(
            recover_abandoned_reflection_claims_if_unlocked(dir.path()).unwrap(),
            0,
            "an aged claim remains live while its OS lock is held"
        );
        assert!(claimed.exists());

        fs::write(&release, "release").unwrap();
        assert!(live_worker.wait().unwrap().success());
        assert_eq!(
            recover_abandoned_reflection_claims_if_unlocked(dir.path()).unwrap(),
            1,
            "a crash-released OS lock permits recovery without an mtime lease"
        );
        assert!(pending.exists());
    }

    #[test]
    fn reflection_lock_child_process() {
        let (Some(queue), Some(ready), Some(release)) = (
            std::env::var_os("EPIC_TEST_REFLECTION_LOCK_QUEUE").map(PathBuf::from),
            std::env::var_os("EPIC_TEST_REFLECTION_LOCK_READY").map(PathBuf::from),
            std::env::var_os("EPIC_TEST_REFLECTION_LOCK_RELEASE").map(PathBuf::from),
        ) else {
            return;
        };
        let _lock = try_acquire_reflection_worker_lock(&queue)
            .unwrap()
            .expect("child worker lock");
        fs::write(ready, "ready").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(release.exists(), "parent must release the child worker");
    }

    #[test]
    fn settlement_rejects_a_replaced_claim_fence() {
        let dir = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260729-fenced-settlement".into(),
            project: "project-a".into(),
            created_at: "2026-07-29T10:00:00Z".into(),
            attempts: 0,
            claim: None,
        };
        let pending = enqueue_reflection_job(dir.path(), &job).unwrap().unwrap();
        let first_claim = claim_reflection_job(&pending).unwrap().unwrap();
        let first_fence = reflection_claim_fence(&first_claim).unwrap();

        assert!(!retry_or_dead_letter_reflection_job(&first_claim, &first_fence).unwrap());
        let replacement = claim_reflection_job(&pending).unwrap().unwrap();
        let replacement_fence = reflection_claim_fence(&replacement).unwrap();
        assert_ne!(first_fence, replacement_fence);

        let error = settle_reflection_job(&replacement, &first_fence, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            replacement.exists(),
            "a stale worker cannot settle a replacement claim"
        );

        settle_reflection_job(&replacement, &replacement_fence, 0).unwrap();
        assert!(replacement.with_extension("completed").exists());
    }

    #[test]
    fn abandoned_reflection_claim_returns_to_pending_queue() {
        let dir = tempfile::tempdir().unwrap();
        let claimed = dir.path().join("job_session.claimed");
        fs::write(&claimed, "{}").unwrap();

        let recovered = recover_abandoned_reflection_claims_if_unlocked(dir.path()).unwrap();

        assert_eq!(recovered, 1);
        assert!(claimed.with_extension("pending").exists());
    }

    #[test]
    fn reflection_worker_os_lock_caps_concurrent_workers() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_acquire_reflection_worker_lock(dir.path()).unwrap();
        let second = try_acquire_reflection_worker_lock(dir.path()).unwrap();

        assert!(first.is_some());
        assert!(
            second.is_none(),
            "one project may run one reflection worker"
        );

        drop(first);
        assert!(
            try_acquire_reflection_worker_lock(dir.path())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn reflection_job_paths_reject_traversal_identities() {
        let queue = tempfile::tempdir().unwrap();
        let job = ReflectionJob {
            session_id: "20260728_../../escape".into(),
            project: "project-a".into(),
            created_at: "2026-07-28T00:00:00Z".into(),
            attempts: 0,
            claim: None,
        };

        assert!(enqueue_reflection_job(queue.path(), &job).is_err());
        assert_eq!(fs::read_dir(queue.path()).unwrap().count(), 0);
    }

    #[test]
    fn cross_store_dedup_preserves_same_source_identical_calls() {
        let record = ObsRecord {
            timestamp: "2026-07-28T10:00:00Z".into(),
            tool: "Bash".into(),
            tool_category: "bash".into(),
            action: Some("cargo test".into()),
            result: Some("success".into()),
            score: Some(1.0),
            dimensions: None,
            failure_category: None,
            error_snippet: None,
            file_ext: None,
            sequence_id: Some(0),
            pipeline_id: None,
            tool_use_id: None,
        };
        let database = vec![
            crate::store::observations::StoredObservation {
                row_id: 1,
                session_id: "session-a".into(),
                project: "project-a".into(),
                record: record.clone(),
            },
            crate::store::observations::StoredObservation {
                row_id: 2,
                session_id: "session-a".into(),
                project: "project-a".into(),
                record: record.clone(),
            },
        ];

        let merged = merge_session_observations("session-a", database, vec![record]).unwrap();

        assert_eq!(
            merged.len(),
            2,
            "one cross-store copy is removed, but both database calls remain"
        );
    }

    #[test]
    fn cross_store_dedup_uses_host_tool_identity() {
        let mut record = ObsRecord {
            timestamp: "2026-07-28T10:00:00Z".into(),
            tool: "Bash".into(),
            tool_category: "bash".into(),
            action: Some("cargo test".into()),
            result: Some("success".into()),
            score: Some(1.0),
            dimensions: None,
            failure_category: None,
            error_snippet: None,
            file_ext: None,
            sequence_id: Some(1),
            pipeline_id: None,
            tool_use_id: Some("call-1".into()),
        };
        let mirrored = crate::store::observations::StoredObservation {
            row_id: 1,
            session_id: "session-a".into(),
            project: "project-a".into(),
            record: record.clone(),
        };
        record.tool_use_id = Some("call-2".into());

        let merged = merge_session_observations("session-a", vec![mirrored], vec![record]).unwrap();

        assert_eq!(merged.len(), 2, "distinct host calls must not collapse");
    }
}

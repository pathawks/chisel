use chisel::config::PipelineConfig;
use chisel::types::{Found, Heuristic, MatchRecord, Pending, estimate_value};
use chisel::{Candidate, RomInfo};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::path::Path;

#[derive(Eq, PartialEq)]
struct Task {
    cost: u64,
    heur_idx: usize,
    size: usize,
    cand_idx: usize,
    cand_version: u64,
}

impl Ord for Task {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` is a max-heap, so reverse the ordering to ensure that
        // lower-cost tasks are popped before higher-cost ones. Tie-breakers
        // are similarly reversed so earlier heuristic indices, smaller sizes
        // and candidate indices take priority when costs are equal.
        // Older versions (lower cand_version) lose to newer ones.
        other
            .cost
            .cmp(&self.cost)
            .then_with(|| other.size.cmp(&self.size))
            .then_with(|| other.heur_idx.cmp(&self.heur_idx))
            .then_with(|| other.cand_idx.cmp(&self.cand_idx))
            .then_with(|| self.cand_version.cmp(&other.cand_version))
    }
}

impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn cost_ratio(work: u64, value: u64) -> u64 {
    ((work as f64) / (value.max(1) as f64)).ceil() as u64
}

#[allow(clippy::too_many_arguments)]
pub fn run_pipeline(
    roms: &mut [RomInfo],
    cands: &mut [Candidate],
    output_dir: &Path,
    game_subdirs: bool,
    verbose: bool,
    no_byte_swap: bool,
    initial_records: Vec<MatchRecord>,
    collect_records: bool,
) -> anyhow::Result<Vec<MatchRecord>> {
    let heuristics: Vec<Box<dyn Heuristic>> = {
        let mut cfg = PipelineConfig::default();
        if no_byte_swap {
            cfg.byte_swaps
                .retain(|&bs| bs == chisel::types::ByteSwap::None);
        }
        cfg.heuristics()
    };
    let mut pending = Pending::build(roms);

    let mut records: Vec<MatchRecord> = initial_records;
    let mut unmatched = roms.iter().filter(|r| !r.matched).count();

    let mut heap: BinaryHeap<Task> = BinaryHeap::new();
    let mut cand_versions: Vec<u64> = vec![0; cands.len()];
    // Tracks (heur_idx, size, cand_idx) triples that ran and found nothing.
    // Since the pending ROM set can only shrink and coverage can only grow,
    // a heuristic that found nothing on a candidate will never find anything
    // new on that candidate, so re-scheduling it is pointless.
    let mut already_failed: HashSet<(usize, usize, usize)> = HashSet::new();

    // Seed the heap with all heuristic/candidate/size combinations
    for (h_idx, h) in heuristics.iter().enumerate() {
        for (&size, bucket) in pending.by_size.iter() {
            for (ci, cand) in cands.iter().enumerate() {
                if cand.data.len() >= size
                    && !cand.is_fully_covered()
                    && let Some(work) = h.estimate_cost(size, bucket, std::slice::from_ref(cand))
                {
                    let value = estimate_value(size, bucket, std::slice::from_ref(cand));
                    let cost = cost_ratio(work, value);
                    heap.push(Task {
                        cost,
                        heur_idx: h_idx,
                        size,
                        cand_idx: ci,
                        cand_version: 0,
                    });
                }
            }
        }
    }

    while let Some(task) = heap.pop() {
        if unmatched == 0 {
            break;
        }
        if task.cand_version < cand_versions[task.cand_idx] {
            continue; // stale — a newer generation of tasks was scheduled
        }
        if cands[task.cand_idx].is_fully_covered() {
            continue;
        }
        let bucket = match pending.bucket(task.size) {
            Some(b) => b,
            None => continue,
        };
        let h = &&heuristics[task.heur_idx];
        let cand_idx = task.cand_idx;
        // Re-check cost; if it changed, requeue with updated cost
        if let Some(work) =
            h.estimate_cost(task.size, bucket, std::slice::from_ref(&cands[cand_idx]))
        {
            let value = estimate_value(task.size, bucket, std::slice::from_ref(&cands[cand_idx]));
            let current = cost_ratio(work, value);
            if current != task.cost {
                heap.push(Task {
                    cost: current,
                    ..task
                });
                continue;
            }
        } else {
            continue;
        }

        if verbose {
            println!(
                "--- {} size {} \"{}\" ---",
                h.name(),
                task.size,
                &cands[cand_idx],
            );
        }

        // ---- Immutable phase: borrow bucket & candidate, collect small Vec<Found> ----
        let founds: Vec<Found> = {
            let cand_ref = &cands[cand_idx];
            h.probe_cand(cand_ref, bucket).collect()
        };

        let mut made_progress = false;

        // ---- Mutable/side-effect phase: mutate pending/cands, write files ----
        for found in founds {
            let progress = chisel::apply_found(
                &found,
                cand_idx,
                cands,
                &mut pending,
                roms,
                &mut unmatched,
                if collect_records {
                    Some(&mut records)
                } else {
                    None
                },
                verbose,
                |rom, bytes| {
                    if !collect_records {
                        let rom_path = if game_subdirs {
                            let dir = output_dir.join(&rom.game);
                            std::fs::create_dir_all(&dir)?;
                            dir.join(&rom.name)
                        } else {
                            output_dir.join(&rom.name)
                        };
                        if let Some(hdr) = &rom.header {
                            let mut full = Vec::with_capacity(hdr.len() + bytes.len());
                            full.extend_from_slice(hdr);
                            full.extend_from_slice(bytes);
                            std::fs::write(rom_path, &full)?;
                        } else {
                            std::fs::write(rom_path, bytes)?;
                        }
                    }
                    Ok(())
                },
            )?;
            if progress {
                made_progress = true;
            }
        }

        if !made_progress {
            already_failed.insert((task.heur_idx, task.size, cand_idx));
        }

        // Reinsert tasks for this candidate only if we made progress
        if made_progress && !cands[cand_idx].is_fully_covered() {
            cand_versions[cand_idx] += 1; // invalidate older tasks
            let ver = cand_versions[cand_idx];
            for (h_idx, h) in heuristics.iter().enumerate() {
                for (&size, bucket) in pending.by_size.iter() {
                    if already_failed.contains(&(h_idx, size, cand_idx)) {
                        continue; // already ran and found nothing; can't improve
                    }
                    if cands[cand_idx].data.len() >= size
                        && let Some(work) =
                            h.estimate_cost(size, bucket, std::slice::from_ref(&cands[cand_idx]))
                    {
                        let value =
                            estimate_value(size, bucket, std::slice::from_ref(&cands[cand_idx]));
                        let cost = cost_ratio(work, value);
                        heap.push(Task {
                            cost,
                            heur_idx: h_idx,
                            size,
                            cand_idx,
                            cand_version: ver,
                        });
                    }
                }
            }
        }
    }
    Ok(records)
}

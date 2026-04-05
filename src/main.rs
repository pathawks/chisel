#![deny(warnings)]

use chisel::utils;
use clap::Parser;

use utils::load_rom_list;

mod gex;
mod pipeline;

#[derive(Parser)]
#[command(name = "chisel")]
struct Opt {
    /// Path to a DAT file (MAME XML from `mame -listxml`, or No-Intro/Logiqx XML).
    /// Required for matching mode; omit when using --spec.
    #[arg(long)]
    dat: Option<std::path::PathBuf>,

    /// Apply an extraction spec directly, without a DAT file or heuristic matching.
    /// Format: comma-separated key=value pairs.
    /// Keys: skip, step_by, take, size, rotate_left, byte_swap (none/swap2/swap4).
    /// Example: skip=16  or  skip=1,step_by=2,size=8192
    #[arg(long)]
    spec: Option<chisel::types::ExtractionSpec>,

    /// Target game name (MAME short name or No-Intro title)
    #[arg(long)]
    game: Option<String>,

    /// Candidate input files (shell glob expansion is expected, e.g. ./inputDir/*)
    #[arg(required = true)]
    input_files: Vec<std::path::PathBuf>,

    /// Where to write matched ROMs
    #[arg(long)]
    output_dir: std::path::PathBuf,

    /// Emit a Game Extraction Toolbox task to this file instead of ROMs
    #[arg(long)]
    gex: Option<std::path::PathBuf>,

    /// Write each game's ROMs into a named subdirectory of output_dir
    #[arg(long)]
    game_subdirs: bool,

    /// Write unmatched candidate chunks to files in output_dir after matching
    #[arg(long)]
    emit_unmatched: bool,

    /// Disable structural (region-layout) matching pass
    #[arg(long)]
    no_structural: bool,

    /// Disable byte-swap heuristic variants (Swap2/Swap4); speeds up matching
    /// when you know the candidate data is not byte-swapped
    #[arg(long)]
    no_byte_swap: bool,

    /// Disable expansion of KPKA/PAK archives and embedded LZMA/XZ blocks
    /// within candidates
    #[arg(long)]
    no_expand: bool,

    /// Passwords to try on encrypted zip members (may be repeated)
    #[arg(long)]
    password: Vec<String>,

    /// Verbose logging (-v)
    #[arg(short, long)]
    verbose: bool,
}

fn run_extract(opt: &Opt, spec: chisel::types::ExtractionSpec) -> anyhow::Result<()> {
    std::fs::create_dir_all(&opt.output_dir)?;
    let cands = utils::load_candidates_from_paths(&opt.input_files, &opt.password, opt.verbose)?;
    for cand in &cands {
        let mut s = spec.clone();
        if s.size == 0 {
            s.size = cand.data.len().saturating_sub(s.skip);
        }
        let result = s.apply(&cand.data);
        let stem = cand
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        let out_path = opt.output_dir.join(format!("{stem}.bin"));
        std::fs::write(&out_path, &result)?;
        if opt.verbose {
            eprintln!(
                "Extracted {} bytes from {} -> {}",
                result.len(),
                cand.path.display(),
                out_path.display(),
            );
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    if opt.dat.is_some() && opt.spec.is_some() {
        anyhow::bail!("--dat and --spec are mutually exclusive");
    }

    if let Some(spec) = opt.spec.clone() {
        return run_extract(&opt, spec);
    }

    let dat = opt
        .dat
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("either --dat or --spec is required"))?;
    let mut roms = load_rom_list(dat, opt.game.as_deref())?;
    let mut cands =
        utils::load_candidates_from_paths(&opt.input_files, &opt.password, opt.verbose)?;

    if opt.gex.is_none() {
        std::fs::create_dir_all(&opt.output_dir)?;
    }

    // Structural pass: runs before the main pipeline so that matched ROMs are
    // excluded from re-search. Returns MatchRecords used when writing a GEX task.
    let mut structural_records = Vec::new();
    if !opt.no_structural {
        structural_records = chisel::structural::run_structural_pass(
            &mut roms,
            &mut cands,
            &opt.output_dir,
            opt.gex.is_some(),
            opt.game_subdirs,
            opt.verbose,
        )?;
    }

    // First pipeline pass on un-expanded candidates.
    let mut records = pipeline::run_pipeline(
        &mut roms,
        &mut cands,
        &opt.output_dir,
        opt.game_subdirs,
        opt.verbose,
        opt.no_byte_swap,
        structural_records,
        opt.gex.is_some(),
    )?;

    // Only expand candidates (KPKA/LZMA) if there are still unmatched ROMs after
    // the first pass. This avoids the expensive expansion when not needed.
    if !opt.no_expand {
        let unmatched = roms.iter().filter(|r| !r.matched).count();
        if unmatched > 0 {
            let t0 = std::time::Instant::now();
            utils::expand_kpka_entries(&mut cands, opt.verbose);
            if opt.verbose {
                eprintln!("expand_kpka_entries: {:.2?}", t0.elapsed());
            }

            let t1 = std::time::Instant::now();
            utils::expand_lzma_blocks(&mut cands, &roms, opt.verbose);
            if opt.verbose {
                eprintln!("expand_lzma_blocks: {:.2?}", t1.elapsed());
            }

            let t3 = std::time::Instant::now();
            records = pipeline::run_pipeline(
                &mut roms,
                &mut cands,
                &opt.output_dir,
                opt.game_subdirs,
                opt.verbose,
                opt.no_byte_swap,
                records,
                opt.gex.is_some(),
            )?;
            if opt.verbose {
                eprintln!("second pipeline pass: {:.2?}", t3.elapsed());
            }
        }
    }

    chisel::utils::report_results(&roms);

    if opt.emit_unmatched && opt.gex.is_none() {
        for cand in cands.iter() {
            let gaps = cand.coverage.uncovered_ranges(cand.data.len());
            if gaps.is_empty() {
                continue;
            }
            let stem = cand
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "candidate".to_string());
            for (start, end) in gaps {
                let chunk = &cand.data[start..end];
                let filename = format!("{}_unmatched_{:08x}_{:08x}.bin", stem, start, end - start);
                let out_path = opt.output_dir.join(&filename);
                std::fs::write(&out_path, chunk)?;
                if opt.verbose {
                    eprintln!(
                        "Wrote unmatched chunk: {} ({} bytes)",
                        out_path.display(),
                        chunk.len()
                    );
                }
            }
        }
    }

    if let Some(path) = opt.gex {
        gex::write_task(&records, &cands, &path)?;
    }

    Ok(())
}

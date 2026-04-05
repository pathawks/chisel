use chisel::types::{ByteSwap, Candidate, CandidateSource, MatchRecord};
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Format a string as a single-quoted Python string literal with proper escaping.
fn py_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c if (c as u32) <= 0xFFFF => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => {
                out.push_str(&format!("\\U{:08x}", c as u32));
            }
        }
    }
    out.push('\'');
    out
}

/// Write a task compatible with Game Extraction Toolkit
/// https://github.com/shawngmc/game-extraction-toolbox
pub fn write_task(records: &[MatchRecord], cands: &[Candidate], path: &Path) -> anyhow::Result<()> {
    let task_name = "generated";
    let title = "chisel export";

    // Determine which extra Python imports are needed
    let needs_gzip = records
        .iter()
        .any(|r| matches!(cands[r.cand_idx].source, CandidateSource::Gzip { .. }));
    let needs_zipfile = records
        .iter()
        .any(|r| matches!(cands[r.cand_idx].source, CandidateSource::Zip { .. }));
    let needs_lzma = records
        .iter()
        .any(|r| matches!(cands[r.cand_idx].source, CandidateSource::Lzma { .. }));
    let needs_kpka = records
        .iter()
        .any(|r| matches!(cands[r.cand_idx].source, CandidateSource::Kpka { .. }));
    let mut extra_imports = String::new();
    if needs_gzip {
        extra_imports.push_str("import gzip\n");
    }
    if needs_zipfile {
        extra_imports.push_str("import zipfile\n");
    }
    if needs_lzma {
        extra_imports.push_str("import io\nimport lzma\n");
    }
    if needs_kpka {
        extra_imports.push_str(
            "# KPKA entries: install 'zstandard' (pip install zstandard) for decompression\n",
        );
    }

    let mut out = format!(
        "import os
{extra_imports}from gex.lib.tasks.basetask import BaseTask
from gex.lib.utils.blob import transforms

# Generated automatically by chisel

class GeneratedTask(BaseTask):
    _task_name = \"{task_name}\"
    _title = \"{title}\"

    def execute(self, in_dir, out_dir):
        os.makedirs(out_dir, exist_ok=True)
"
    );

    for rec in records {
        let source = &cands[rec.cand_idx].source;
        match source {
            CandidateSource::Plain => {
                let cand = rec
                    .cand_path
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let py_cand = py_str(&cand);
                out.push_str(&format!(
                    "        with open(os.path.join(in_dir, {py_cand}), 'rb') as f:\n"
                ));
                out.push_str("            contents = f.read()\n");
            }
            CandidateSource::Gzip { archive } => {
                let archive_name = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let py_name = py_str(&archive_name);
                out.push_str(&format!(
                    "        with gzip.open(os.path.join(in_dir, {py_name}), 'rb') as f:\n"
                ));
                out.push_str("            contents = f.read()\n");
            }
            CandidateSource::Zip {
                archive,
                member,
                password,
            } => {
                let archive_name = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let py_archive = py_str(&archive_name);
                let py_member = py_str(member);
                out.push_str(&format!(
                    "        with zipfile.ZipFile(os.path.join(in_dir, {py_archive})) as z:\n"
                ));
                if let Some(pw) = password {
                    let py_pw = py_str(pw);
                    out.push_str(
                        "            # NOTE: zipfile only supports ZipCrypto; AES-encrypted\n",
                    );
                    out.push_str(
                        "            # archives need pyzipper or another AES-capable library.\n",
                    );
                    out.push_str(&format!(
                        "            with z.open({py_member}, pwd={py_pw}.encode()) as f:\n"
                    ));
                } else {
                    out.push_str(&format!("            with z.open({py_member}) as f:\n"));
                }
                out.push_str("                contents = f.read()\n");
            }
            CandidateSource::Kpka { archive, index } => {
                let archive_name = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let py_archive = py_str(&archive_name);
                out.push_str(&format!(
                    "        # KPKA entry {index} from {archive_name}\n"
                ));
                out.push_str(&format!(
                    "        with open(os.path.join(in_dir, {py_archive}), 'rb') as f:\n"
                ));
                out.push_str("            raw = f.read()\n");
                out.push_str(&format!(
                    "        contents = raw  # TODO: parse KPKA and extract entry {index}\n"
                ));
            }
            CandidateSource::Lzma { parent, offset } => {
                let parent_name = parent
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let py_name = py_str(&parent_name);
                out.push_str(&format!(
                    "        with open(os.path.join(in_dir, {py_name}), 'rb') as f:\n"
                ));
                out.push_str("            raw = f.read()\n");
                out.push_str(&format!(
                    "        with lzma.open(io.BytesIO(raw[{offset:#010x}:])) as f:\n"
                ));
                out.push_str("            contents = f.read()\n");
            }
        }

        let spec = &rec.spec;
        // Indentation: one extra level when inside a zip context block
        let indent = match source {
            CandidateSource::Zip { .. } => "            ",
            _ => "        ",
        };
        if spec.step_by == spec.take {
            out.push_str(&format!(
                "{indent}chunk = transforms.cut(contents, {skip}, {size})\n",
                skip = spec.skip,
                size = spec.size
            ));
        } else if spec.take != 0 {
            let num_ways = spec.step_by / spec.take;
            if spec.take == 1 && spec.skip >= spec.step_by {
                // Banked interleave: the skip encodes both a bank offset and a
                // lane index.  Emit a cut to the bank slice first, then
                // deinterleave within the bank.
                let lane_index = spec.skip % spec.step_by;
                let bank_offset = spec.skip - lane_index;
                let bank_size = num_ways * spec.size;
                out.push_str(&format!(
                    "{indent}bank_slice = transforms.cut(contents, {bank_offset}, {bank_size})\n"
                ));
                out.push_str(&format!(
                    "{indent}chunks = transforms.deinterleave(bank_slice, {num_ways}, 1)\n"
                ));
                out.push_str(&format!(
                    "{indent}chunk = chunks[{lane_index}][:{size}]\n",
                    size = spec.size
                ));
            } else {
                let lane = spec.skip / spec.take;
                out.push_str(&format!(
                    "{indent}chunks = transforms.deinterleave(contents, {ways}, {word})\n",
                    ways = num_ways,
                    word = spec.take
                ));
                out.push_str(&format!(
                    "{indent}chunk = chunks[{lane}][:{size}]\n",
                    size = spec.size
                ));
            }
        } else {
            out.push_str(&format!("{indent}chunk = contents\n"));
        }
        if spec.rotate_left != 0 {
            out.push_str(&format!(
                "{indent}chunk = chunk[{r}:] + chunk[:{r}]\n",
                r = spec.rotate_left % spec.size.max(1)
            ));
        }
        match spec.byte_swap {
            ByteSwap::Swap2 => {
                out.push_str(&format!("{indent}chunk = transforms.swap_endian(chunk)\n"));
            }
            ByteSwap::Swap4 => {
                out.push_str(&format!(
                    "{indent}chunk = bytearray().join([chunk[i:i+4][::-1] for i in range(0, len(chunk), 4)])\n"
                ));
            }
            ByteSwap::None => {}
        }
        if let Some(hdr) = &rec.header {
            let hex_str: String = hdr.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&format!(
                "        chunk = bytes.fromhex('{hex_str}') + chunk\n"
            ));
        }
        let py_rom = py_str(&rec.rom_name);
        out.push_str(&format!(
            "{indent}with open(os.path.join(out_dir, {py_rom}), 'wb') as o:\n"
        ));
        out.push_str(&format!("{indent}    o.write(chunk)\n\n"));
    }

    let mut file = File::create(path)?;
    file.write_all(out.as_bytes())?;
    Ok(())
}

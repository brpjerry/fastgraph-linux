//! Measurement sets: a `.fgset` file is a plain tar archive holding `set.toml`
//! (first), then per measurement `<id>.toml` (`Meta`), `<id>.txt` (REW text)
//! and, for sweeps, `<id>.wav` (mono 32-bit float impulse response).

use crate::curve_io::{ExportHeader, read_curve, utc_string, write_rew};
use crate::deconv::ImpulseResponse;
use crate::fsutil::{persist, resolve_dir, temp_for};
use crate::response::{Response, Smoothing};
use crate::store::{Meta, Store, check_id};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const EXTENSION: &str = "fgset";
pub const FORMAT: u32 = 1;

const HEADER_NAME: &str = "set.toml";
const MIGRATED_NAME: &str = "Earlier measurements";
/// The largest `set.toml` and the largest member read.
const HEADER_MAX: u64 = 64 << 10;
const MEMBER_MAX: u64 = 256 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetHeader {
    pub format: u32,
    pub name: String,
    /// Seconds since the Unix epoch.
    pub saved: u64,
    pub count: usize,
}

impl SetHeader {
    /// The set's name, else the file's stem.
    pub fn title(&self, path: &Path) -> String {
        if self.name.is_empty() {
            path.file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
        } else {
            self.name.clone()
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub meta: Meta,
    pub response: Response,
    pub ir: Option<ImpulseResponse>,
}

/// The impulse response as a mono 32-bit float WAV.
pub(crate) fn encode_ir_wav(ir: &ImpulseResponse) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: ir.rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut bytes = Vec::new();
    let mut w = hound::WavWriter::new(std::io::Cursor::new(&mut bytes), spec)?;
    for s in &ir.samples {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    Ok(bytes)
}

/// `peak` is not stored in the WAV: it is the index of the largest |sample| (see
/// [`with_stored_peak`] for the metadata's).
fn decode_ir_wav(bytes: &[u8]) -> Result<ImpulseResponse> {
    let mut r = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = r.spec();
    if spec.channels != 1 || spec.sample_format != hound::SampleFormat::Float {
        bail!("impulse response is not a mono float WAV");
    }
    let samples = r.samples::<f32>().collect::<Result<Vec<_>, _>>()?;
    let peak = samples
        .iter()
        .enumerate()
        .fold((0, 0.0_f32), |best, (i, s)| {
            if s.abs() > best.1 { (i, s.abs()) } else { best }
        })
        .0;
    Ok(ImpulseResponse {
        rate: spec.sample_rate,
        samples,
        peak,
    })
}

/// `ir` with the peak index the metadata records (`ir_peak`), when it lies inside it. A long
/// IR's largest sample is the linear peak anyway, but the stored index is exact.
fn with_stored_peak(mut ir: ImpulseResponse, meta: &Meta) -> ImpulseResponse {
    if let Some(p) = meta.ir_peak
        && p < ir.samples.len()
    {
        ir.peak = p;
    }
    ir
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn append(b: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8], mtime: u64) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(mtime);
    b.append_data(&mut h, name, data)
        .with_context(|| format!("adding {name}"))
}

/// Writes the set atomically, replacing `path`; `set.toml` comes first so the
/// header reads cheaply.
pub fn write_set(path: &Path, name: &str, entries: &[Entry]) -> Result<()> {
    write_set_to(path, name, entries, true)
}

/// [`write_set`], but unless `replace` an existing `path`, even one that
/// appeared while the set was written, is kept and the write fails with an
/// [`std::io::ErrorKind::AlreadyExists`] error.
pub fn write_set_to(path: &Path, name: &str, entries: &[Entry], replace: bool) -> Result<()> {
    let saved = now_secs();
    let header = SetHeader {
        format: FORMAT,
        name: name.to_string(),
        saved,
        count: entries.len(),
    };
    let mut b = tar::Builder::new(Vec::new());
    append(
        &mut b,
        HEADER_NAME,
        toml::to_string_pretty(&header)?.as_bytes(),
        saved,
    )?;
    for e in entries {
        let id = &e.meta.id;
        check_id(id)?;
        let rew = ExportHeader {
            name: e.meta.name.clone(),
            smoothing: Smoothing::None,
            compensation: e.meta.baked_compensation.clone(),
            created: UNIX_EPOCH + Duration::from_secs(e.meta.created),
            offset_db: 0.0,
        };
        let text = write_rew(&e.response, &rew);
        // REW text cannot hold < 2 points or non-finite phase; reject rather than save a broken file.
        read_curve(&text).with_context(|| format!("{id}: cannot be stored as REW text"))?;
        let meta = toml::to_string_pretty(&e.meta).context("serialising metadata")?;
        if let Some(ir) = &e.ir {
            let wav =
                encode_ir_wav(ir).with_context(|| format!("{id}: encoding impulse response"))?;
            append(&mut b, &format!("{id}.wav"), &wav, saved)?;
        }
        append(&mut b, &format!("{id}.txt"), text.as_bytes(), saved)?;
        append(&mut b, &format!("{id}.toml"), meta.as_bytes(), saved)?;
    }
    let bytes = b.into_inner().context("finishing archive")?;
    let mut tmp = temp_for(path)?;
    tmp.write_all(&bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    if replace {
        return persist(tmp, path);
    }
    tmp.persist_noclobber(path)
        .map(drop)
        .map_err(|e| e.error)
        .with_context(|| format!("creating {}", path.display()))
}

fn parse_header(bytes: &[u8]) -> Result<SetHeader> {
    let text = std::str::from_utf8(bytes).context("set.toml is not text")?;
    let header: SetHeader = toml::from_str(text).context("parsing set.toml")?;
    if header.format != FORMAT {
        bail!(
            "unsupported set format {} (this version reads format {FORMAT})",
            header.format
        );
    }
    Ok(header)
}

fn first_header<R: Read>(entries: &mut tar::Entries<'_, R>) -> Result<SetHeader> {
    let mut first = entries.next().context("empty archive")??;
    if first.path()?.as_ref() != Path::new(HEADER_NAME) {
        bail!("not a measurement set: {HEADER_NAME} is missing");
    }
    if first.size() > HEADER_MAX {
        bail!("{HEADER_NAME} is too large");
    }
    let mut bytes = Vec::new();
    first.read_to_end(&mut bytes)?;
    parse_header(&bytes)
}

/// Reads only `set.toml`.
pub fn read_header(path: &Path) -> Result<SetHeader> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let ctx = || format!("reading {}", path.display());
    first_header(&mut tar::Archive::new(f).entries().with_context(ctx)?).with_context(ctx)
}

/// The set's header and measurements (by creation time), plus one message
/// per measurement that could not be loaded. Unknown entries are skipped.
pub fn read_set(path: &Path) -> Result<(SetHeader, Vec<Entry>, Vec<String>)> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut ar = tar::Archive::new(f);
    let ctx = || format!("reading {}", path.display());
    // One pass over one open file: the header and the entries are of the
    // same version even if the file is replaced meanwhile.
    let mut members = ar.entries().with_context(ctx)?;
    let header = first_header(&mut members).with_context(ctx)?;
    // Files per id: [toml, txt, wav].
    let mut files: BTreeMap<String, [Option<Vec<u8>>; 3]> = BTreeMap::new();
    for item in members {
        let mut item = item.with_context(ctx)?;
        let name = item
            .path()
            .with_context(ctx)?
            .to_string_lossy()
            .into_owned();
        let Some((id, ext)) = name.rsplit_once('.') else {
            continue;
        };
        let slot = match ext {
            "toml" => 0,
            "txt" => 1,
            "wav" => 2,
            _ => continue,
        };
        if name == HEADER_NAME {
            continue;
        }
        if item.size() > MEMBER_MAX {
            bail!("{}: {name} is too large", ctx());
        }
        let mut data = Vec::new();
        item.read_to_end(&mut data).with_context(ctx)?;
        files.entry(id.to_string()).or_default()[slot] = Some(data);
    }
    let (mut entries, mut errors) = (Vec::new(), Vec::new());
    for (id, [meta, text, wav]) in files {
        match load_entry(&id, meta, text, wav) {
            Ok(e) => entries.push(e),
            Err(e) => errors.push(format!("{id}: {e:#}")),
        }
    }
    entries.sort_by(|a, b| {
        a.meta
            .created
            .cmp(&b.meta.created)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });
    if entries.len() != header.count {
        errors.push(format!(
            "set lists {} measurements, {} loaded",
            header.count,
            entries.len()
        ));
    }
    Ok((header, entries, errors))
}

fn load_entry(
    id: &str,
    meta: Option<Vec<u8>>,
    text: Option<Vec<u8>>,
    wav: Option<Vec<u8>>,
) -> Result<Entry> {
    let meta = meta.context("metadata is missing")?;
    let meta: Meta = toml::from_str(std::str::from_utf8(&meta).context("metadata is not text")?)
        .context("parsing metadata")?;
    if meta.id != id {
        bail!("metadata id {:?} does not match its file name", meta.id);
    }
    check_id(id)?;
    let text = text.context("response is missing")?;
    let response = read_curve(std::str::from_utf8(&text).context("response is not text")?)
        .context("parsing response")?;
    let ir = wav
        .map(|w| decode_ir_wav(&w).context("reading impulse response"))
        .transpose()?
        .map(|ir| with_stored_peak(ir, &meta));
    Ok(Entry { meta, response, ir })
}

/// The `.fgset` files in `dir`, newest `saved` first; unreadable ones are skipped.
pub fn list_sets(dir: &Path) -> Vec<(PathBuf, SetHeader)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, SetHeader)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == EXTENSION))
        // Not a pipe or a device, which could block the read.
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
        .filter_map(|p| read_header(&p).ok().map(|h| (p, h)))
        .collect();
    out.sort_by(|a, b| b.1.saved.cmp(&a.1.saved).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Gives entries whose id is in `taken` a new id, and points the averages
/// among them that name a reissued id at its new one.
pub fn reissue_clashing(entries: &mut [Entry], taken: &HashSet<String>) {
    let mut renamed = BTreeMap::new();
    for e in entries.iter_mut().filter(|e| taken.contains(&e.meta.id)) {
        let id = Store::new_id();
        renamed.insert(std::mem::replace(&mut e.meta.id, id.clone()), id);
    }
    for source in entries.iter_mut().flat_map(|e| &mut e.meta.sources) {
        if let Some(id) = renamed.get(source) {
            source.clone_from(id);
        }
    }
}

/// `/` and control characters become `_`; surrounding whitespace is trimmed.
/// Empty or all-dots results become "Untitled".
pub fn safe_file_name(name: &str) -> String {
    let s = name
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect::<String>();
    let s = s.trim();
    if s.chars().all(|c| c == '.') {
        "Untitled".to_string()
    } else {
        s.to_string()
    }
}

/// `$XDG_DATA_HOME/fastgraph/recovery` (absolute only) or `~/.local/share/fastgraph/recovery`.
pub fn recovery_dir() -> Result<PathBuf> {
    Ok(resolve_dir(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        ".local/share",
    )?
    .join("fastgraph")
    .join("recovery"))
}

/// `<YYYY-MM-DD HHMMSS>-<pid>.fgset` (UTC) in `dir`: the process id keeps
/// two instances started in the same second apart.
pub fn recovery_path(dir: &Path, started: SystemTime, pid: u32) -> PathBuf {
    let stamp: String = utc_string(started)
        .chars()
        .take(19)
        .filter(|c| *c != ':')
        .collect();
    dir.join(format!("{stamp}-{pid}.{EXTENSION}"))
}

/// How long a recovery file is kept however many newer ones there are.
const RECOVERY_KEEP_SECS: u64 = 14 * 24 * 60 * 60;

/// Deletes the sets in `dir` beyond the `keep` newest (by `saved`) that were
/// also saved more than 14 days before `now` (seconds since the epoch),
/// never the migrated "Earlier measurements" set.
pub fn prune_recovery(dir: &Path, keep: usize, now: u64) -> Result<()> {
    let earlier = format!("{MIGRATED_NAME}.{EXTENSION}");
    let sets = list_sets(dir)
        .into_iter()
        .filter(|(p, _)| p.file_name().is_none_or(|n| n != earlier.as_str()));
    for (path, header) in sets.skip(keep) {
        if header.saved.saturating_add(RECOVERY_KEEP_SECS) >= now {
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    }
    Ok(())
}

/// Packs the old per-file store into `recovery/Earlier measurements.fgset` and
/// renames `old` to `measurements.migrated`. `None` if there is nothing to pack;
/// otherwise the file and the old store's per-measurement problems; an error
/// if none of them loads. Refuses (before writing anything) if either target
/// already exists.
pub fn migrate_old_store(old: &Path, recovery: &Path) -> Result<Option<(PathBuf, Vec<String>)>> {
    if !old.is_dir() {
        return Ok(None);
    }
    let (measurements, problems) = Store {
        dir: old.to_path_buf(),
    }
    .load_all();
    if measurements.is_empty() {
        if !problems.is_empty() {
            bail!("none could be read: {}", problems.join("; "));
        }
        return Ok(None);
    }
    let entries: Vec<Entry> = measurements
        .into_iter()
        .map(|m| {
            // A missing or unreadable IR just leaves the measurement without one.
            let ir = std::fs::read(old.join(format!("{}.wav", m.meta.id)))
                .ok()
                .and_then(|b| decode_ir_wav(&b).ok());
            Entry {
                meta: m.meta,
                response: m.response,
                ir,
            }
        })
        .collect();
    let path = recovery.join(format!("{MIGRATED_NAME}.{EXTENSION}"));
    let moved = old.with_file_name("measurements.migrated");
    if path.exists() {
        bail!("{} already exists; not overwriting it", path.display());
    }
    if moved.exists() {
        bail!("{} already exists; not replacing it", moved.display());
    }
    std::fs::create_dir_all(recovery)
        .with_context(|| format!("creating {}", recovery.display()))?;
    write_set_to(&path, MIGRATED_NAME, &entries, false)?;
    std::fs::rename(old, &moved)
        .with_context(|| format!("renaming {} to {}", old.display(), moved.display()))?;
    Ok(Some((path, problems)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Kind, Meta, Side, Store};

    fn meta(id: &str, name: &str, created: u64, offset_db: f64) -> Meta {
        Meta {
            id: id.into(),
            name: name.into(),
            kind: Kind::Sweep,
            side: Some(Side::L),
            seating: Some(2),
            input: Some("in".into()),
            output: None,
            drift_ppm: Some(-3.5),
            clipped: false,
            created,
            source: None,
            sources: vec!["a".into()],
            compensable: false,
            baked_compensation: None,
            shown: false,
            offset_db,
            ir_peak: None,
            sweep_seconds: None,
        }
    }

    fn resp() -> Response {
        let freq: Vec<f64> = (0..20).map(|i| 20.0 * 1.3_f64.powi(i)).collect();
        let db = (0..20)
            .map(|i| (i as f64 * 0.7).sin() * 5.0 - 3.0)
            .collect();
        let phase = Some((0..20).map(|i| i as f64 * 3.0 - 90.0).collect());
        Response { freq, db, phase }
    }

    fn entry(id: &str, offset_db: f64, ir: Option<ImpulseResponse>) -> Entry {
        Entry {
            meta: meta(id, id, 10, offset_db),
            response: resp(),
            ir,
        }
    }

    fn sample_ir() -> ImpulseResponse {
        ImpulseResponse {
            rate: 48_000,
            samples: vec![0.25, -0.75, 0.5],
            peak: 1,
        }
    }

    fn two_entries() -> Vec<Entry> {
        vec![
            entry("a-1", 1.5, Some(sample_ir())),
            entry("b-1", 0.0, None),
        ]
    }

    fn header_toml(format: u32, name: &str, saved: u64, count: usize) -> String {
        format!("format = {format}\nname = {name:?}\nsaved = {saved}\ncount = {count}\n")
    }

    /// A tar archive holding `files` in order.
    fn tar_of(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn write_header_only(dir: &Path, file: &str, name: &str, saved: u64) -> PathBuf {
        let p = dir.join(file);
        let bytes = tar_of(&[("set.toml", header_toml(FORMAT, name, saved, 0).into_bytes())]);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn set_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        let src = two_entries();
        write_set(&path, "HD600 day 1", &src).unwrap();
        let (header, got, errs) = read_set(&path).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(header.name, "HD600 day 1");
        assert_eq!(header.count, 2);
        assert_eq!(header.format, FORMAT);
        assert_eq!(got.len(), 2);
        for (g, o) in got.iter().zip(&src) {
            assert_eq!(g.meta, o.meta);
            for (a, b) in g.response.db.iter().zip(&o.response.db) {
                assert!((a - b).abs() < 1e-3);
            }
            assert!(g.response.phase.is_some());
        }
        assert_eq!(got[0].meta.offset_db, 1.5);
        let ir = got[0].ir.as_ref().unwrap();
        assert_eq!(ir.rate, 48_000);
        assert_eq!(ir.samples, vec![0.25, -0.75, 0.5]);
        assert_eq!(ir.peak, 1);
        assert!(got[1].ir.is_none());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn stored_ir_peak_is_used_when_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        let mut src = vec![
            entry("a-1", 0.0, Some(sample_ir())),
            entry("b-1", 0.0, Some(sample_ir())),
        ];
        src[0].meta.ir_peak = Some(2);
        src[0].meta.sweep_seconds = Some(6.0);
        src[1].meta.ir_peak = Some(3);
        write_set(&path, "s", &src).unwrap();
        let (_, got, errs) = read_set(&path).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(got[0].meta, src[0].meta);
        assert_eq!(got[0].ir.as_ref().unwrap().peak, 2);
        // Out of range: the largest |sample| instead.
        assert_eq!(got[1].ir.as_ref().unwrap().peak, 1);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn read_header_reads_only_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        write_set(&path, "x", &two_entries()).unwrap();
        assert_eq!(read_header(&path).unwrap().count, 2);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_broken_entry_is_reported_and_the_rest_load() {
        let dir = tempfile::tempdir().unwrap();
        let good = entry("a-1", 0.0, None);
        let text = crate::curve_io::write_rew(
            &good.response,
            &crate::curve_io::ExportHeader {
                name: "a".into(),
                smoothing: crate::response::Smoothing::None,
                compensation: None,
                created: UNIX_EPOCH,
                offset_db: 0.0,
            },
        );
        let bad = meta("b-1", "b", 10, 0.0);
        let path = dir.path().join("s.fgset");
        let bytes = tar_of(&[
            ("set.toml", header_toml(FORMAT, "n", 5, 2).into_bytes()),
            (
                "a-1.toml",
                toml::to_string(&good.meta).unwrap().into_bytes(),
            ),
            ("a-1.txt", text.into_bytes()),
            ("b-1.toml", toml::to_string(&bad).unwrap().into_bytes()),
            ("b-1.txt", b"not a curve".to_vec()),
            ("README", b"unknown entries are skipped".to_vec()),
        ]);
        std::fs::write(&path, bytes).unwrap();
        let (_, got, errs) = read_set(&path).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].meta.id, "a-1");
        // The broken entry, then the count mismatch.
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].contains("b-1"), "{errs:?}");
        assert!(errs[1].contains("lists 2"), "{errs:?}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn wrong_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        let bytes = tar_of(&[("set.toml", header_toml(99, "n", 5, 0).into_bytes())]);
        std::fs::write(&path, bytes).unwrap();
        let e = read_set(&path).unwrap_err();
        assert!(format!("{e:#}").contains("format"), "{e:#}");
        assert!(read_header(&path).is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn import_reissues_clashing_ids() {
        let mut entries = two_entries();
        let taken: HashSet<String> = ["a-1".to_string()].into();
        reissue_clashing(&mut entries, &taken);
        assert_ne!(entries[0].meta.id, "a-1");
        crate::store::check_id(&entries[0].meta.id).unwrap();
        assert_eq!(entries[1].meta.id, "b-1");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn list_sets_newest_first_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        write_header_only(dir.path(), "a.fgset", "ten", 10);
        write_header_only(dir.path(), "b.fgset", "thirty", 30);
        write_header_only(dir.path(), "c.fgset", "twenty", 20);
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        std::fs::write(dir.path().join("x.fgset"), "corrupt").unwrap();
        let names: Vec<String> = list_sets(dir.path())
            .into_iter()
            .map(|(_, h)| h.name)
            .collect();
        assert_eq!(names, ["thirty", "twenty", "ten"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn prune_keeps_the_five_newest() {
        let dir = tempfile::tempdir().unwrap();
        let now = 100 * 24 * 60 * 60;
        for i in 1..=7u64 {
            write_header_only(dir.path(), &format!("{i}.fgset"), "r", i);
        }
        write_header_only(
            dir.path(),
            "Earlier measurements.fgset",
            "Earlier measurements",
            0,
        );
        prune_recovery(dir.path(), 5, now).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "3.fgset",
                "4.fgset",
                "5.fgset",
                "6.fgset",
                "7.fgset",
                "Earlier measurements.fgset"
            ]
        );
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn prune_keeps_files_from_the_last_14_days() {
        let dir = tempfile::tempdir().unwrap();
        let day = 24 * 60 * 60;
        let now = 100 * day;
        // Seven files, all beyond the five newest: two are recent, one is
        // just past 14 days, the rest are old.
        let ages = [1, 2, 3, 4, 5, 6, 7].map(|n| n * 1000);
        let mut saved = ages.map(|a| now - a).to_vec();
        saved[5] = now - 13 * day;
        saved[6] = now - 15 * day;
        for (i, s) in saved.iter().enumerate() {
            write_header_only(dir.path(), &format!("{i}.fgset"), "r", *s);
        }
        prune_recovery(dir.path(), 1, now).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "0.fgset", "1.fgset", "2.fgset", "3.fgset", "4.fgset", "5.fgset"
            ]
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn safe_file_name_replaces_separators() {
        assert_eq!(safe_file_name("a/b\u{7}c "), "a_b_c");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn recovery_path_is_named_by_utc_start_time_and_process() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(
            recovery_path(Path::new("/r"), t, 42),
            Path::new("/r/2023-11-14 221320-42.fgset")
        );
        assert_ne!(
            recovery_path(Path::new("/r"), t, 42),
            recovery_path(Path::new("/r"), t, 43)
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_new_set_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        std::fs::write(&path, "keep").unwrap();
        let e = write_set_to(&path, "n", &two_entries(), false).unwrap_err();
        let io = e.root_cause().downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(io.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        write_set_to(&path, "n", &two_entries(), true).unwrap();
        assert_eq!(read_header(&path).unwrap().count, 2);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn migration_packs_the_old_store_once() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("measurements");
        let store = Store::open(old.clone()).unwrap();
        let ir = sample_ir();
        for (id, ir) in [("1-0", Some(&ir)), ("2-0", None)] {
            let e = entry(id, 0.0, None);
            store
                .save(
                    &crate::store::Measurement {
                        meta: e.meta,
                        response: e.response,
                    },
                    ir,
                )
                .unwrap();
        }
        let rec = root.path().join("recovery");
        let (path, problems) = migrate_old_store(&old, &rec).unwrap().unwrap();
        assert!(problems.is_empty());
        assert_eq!(path, rec.join("Earlier measurements.fgset"));
        let (h, entries, errs) = read_set(&path).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            (h.name.as_str(), entries.len()),
            ("Earlier measurements", 2)
        );
        assert_eq!(entries[0].ir.as_ref().unwrap().samples, ir.samples);
        assert!(entries[1].ir.is_none());
        assert!(!old.exists());
        assert!(root.path().join("measurements.migrated").exists());
        assert_eq!(migrate_old_store(&old, &rec).unwrap(), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn migration_refuses_to_overwrite_a_backup() {
        for existing in [
            "measurements.migrated",
            "recovery/Earlier measurements.fgset",
        ] {
            let root = tempfile::tempdir().unwrap();
            let old = root.path().join("measurements");
            let e = entry("1-0", 0.0, None);
            Store::open(old.clone())
                .unwrap()
                .save(
                    &crate::store::Measurement {
                        meta: e.meta,
                        response: e.response,
                    },
                    None,
                )
                .unwrap();
            let target = root.path().join(existing);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, "keep").unwrap();
            let err = migrate_old_store(&old, &root.path().join("recovery")).unwrap_err();
            assert!(format!("{err:#}").contains("already exists"), "{err:#}");
            assert!(old.join("1-0.toml").exists());
            assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn migration_reports_unreadable_measurements() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("measurements");
        Store::open(old.clone()).unwrap();
        std::fs::write(old.join("9-0.toml"), "garbage").unwrap();
        let e = entry("1-0", 0.0, None);
        Store { dir: old.clone() }
            .save(
                &crate::store::Measurement {
                    meta: e.meta,
                    response: e.response,
                },
                None,
            )
            .unwrap();
        let (_, problems) = migrate_old_store(&old, &root.path().join("r"))
            .unwrap()
            .unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("9-0"));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn safe_file_name_never_returns_nothing() {
        for n in ["", "  ", "..", " . "] {
            assert_eq!(safe_file_name(n), "Untitled", "{n:?}");
        }
        assert_eq!(safe_file_name(".hidden"), ".hidden");
    }

    // An imported average names its sources by id: when they are reissued,
    // so are its references to them.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn reissued_ids_are_followed_by_the_averages_that_name_them() {
        let mut entries = two_entries();
        let mut avg = entry("c-1", 0.0, None);
        avg.meta.sources = vec!["a-1".into(), "b-1".into(), "elsewhere".into()];
        entries.push(avg);
        let taken: HashSet<String> = ["a-1".to_string(), "b-1".to_string()].into();
        reissue_clashing(&mut entries, &taken);
        let (a, b) = (entries[0].meta.id.clone(), entries[1].meta.id.clone());
        assert!(a != "a-1" && b != "b-1" && a != b);
        assert_eq!(entries[2].meta.sources, [a, b, "elsewhere".into()]);
        assert_eq!(entries[2].meta.id, "c-1");
    }

    // A named pipe with the extension would block the listing forever.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn list_sets_takes_only_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        write_header_only(dir.path(), "a.fgset", "real", 10);
        let fifo = dir.path().join("pipe.fgset");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        std::os::unix::fs::symlink(dir.path().join("a.fgset"), dir.path().join("link.fgset"))
            .unwrap();
        let (done, listed) = std::sync::mpsc::channel();
        let path = dir.path().to_path_buf();
        std::thread::spawn(move || done.send(list_sets(&path)).unwrap());
        let listed = listed
            .recv_timeout(Duration::from_secs(5))
            .expect("the listing blocked");
        let names: Vec<_> = listed.iter().map(|(p, _)| p.file_name().unwrap()).collect();
        assert_eq!(names, ["a.fgset", "link.fgset"]);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn an_oversized_header_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.fgset");
        let mut text = header_toml(FORMAT, "n", 5, 0);
        text.push_str(&format!("# {}\n", "x".repeat(70 * 1024)));
        std::fs::write(&path, tar_of(&[("set.toml", text.into_bytes())])).unwrap();
        let e = read_header(&path).unwrap_err();
        assert!(format!("{e:#}").contains("too large"), "{e:#}");
    }

    // The size is refused from the member's header, before any of it is read.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn an_oversized_member_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.fgset");
        let mut b = tar::Builder::new(Vec::new());
        let header = header_toml(FORMAT, "n", 5, 1).into_bytes();
        let mut h = tar::Header::new_gnu();
        h.set_size(header.len() as u64);
        h.set_cksum();
        b.append_data(&mut h, "set.toml", header.as_slice())
            .unwrap();
        let mut big = tar::Header::new_gnu();
        big.set_path("a-1.wav").unwrap();
        big.set_size(300 << 20);
        big.set_cksum();
        b.get_mut().extend_from_slice(big.as_bytes());
        std::fs::write(&path, b.into_inner().unwrap()).unwrap();
        let e = read_set(&path).unwrap_err();
        assert!(format!("{e:#}").contains("too large"), "{e:#}");
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn migration_reports_a_store_where_nothing_loads() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("measurements");
        Store::open(old.clone()).unwrap();
        std::fs::write(old.join("9-0.toml"), "garbage").unwrap();
        let e = migrate_old_store(&old, &root.path().join("r")).unwrap_err();
        assert!(format!("{e:#}").contains("9-0"), "{e:#}");
        assert!(old.join("9-0.toml").exists());
    }

    // Frequencies closer than a millionth of a hertz stay distinct, so the
    // curve can still be stored.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn close_frequencies_survive_a_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fgset");
        let mut e = entry("a-1", 0.0, None);
        e.response = Response {
            freq: vec![100.0, 100.000_000_1, 200.0],
            db: vec![1.0, 2.0, 3.0],
            phase: None,
        };
        write_set(&path, "n", &[e]).unwrap();
        let (_, got, _) = read_set(&path).unwrap();
        assert_eq!(got[0].response.freq, [100.0, 100.000_000_1, 200.0]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn migration_leaves_an_empty_store_alone() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("measurements");
        Store::open(old.clone()).unwrap();
        let rec = root.path().join("recovery");
        assert_eq!(migrate_old_store(&old, &rec).unwrap(), None);
        assert!(old.exists());
    }
}

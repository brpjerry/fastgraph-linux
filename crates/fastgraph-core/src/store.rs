//! The old per-measurement store, kept only to read for the one-time migration
//! into a set: one `<id>.toml` (metadata), `<id>.txt` (REW text) and
//! optionally `<id>.wav` (32-bit float impulse response) per measurement.

use crate::curve_io::read_curve;
use crate::fsutil::resolve_dir;
use crate::response::Response;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    L,
    R,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Sweep,
    Rta,
    Imported,
    Average,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub side: Option<Side>,
    pub seating: Option<u32>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub drift_ppm: Option<f64>,
    pub clipped: bool,
    /// Seconds since the Unix epoch.
    pub created: u64,
    pub source: Option<PathBuf>,
    pub sources: Vec<String>,
    /// An average stored without compensation, built only from measurements
    /// (or such averages): the compensation applies to it like to them.
    #[serde(default)]
    pub compensable: bool,
    /// The compensation curve built into an average of mixed sources, as
    /// "file name" or "file name (inverted)".
    #[serde(default)]
    pub baked_compensation: Option<String>,
    /// Whether the graph shows it.
    #[serde(default = "shown_by_default")]
    pub shown: bool,
    /// Level offset in dB applied to the trace when drawn.
    #[serde(default)]
    pub offset_db: f64,
    /// The peak's index in the stored impulse response (a sweep's long IR).
    #[serde(default)]
    pub ir_peak: Option<usize>,
    /// The sweep's length in seconds, for the harmonic analysis of the stored IR.
    #[serde(default)]
    pub sweep_seconds: Option<f64>,
}

fn shown_by_default() -> bool {
    true
}

impl Meta {
    /// Whether the microphone compensation applies to this response: sweep
    /// and RTA measurements, and averages built only from those.
    pub fn takes_compensation(&self) -> bool {
        match self.kind {
            Kind::Sweep | Kind::Rta => true,
            Kind::Average => self.compensable,
            Kind::Imported => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Measurement {
    pub meta: Meta,
    pub response: Response,
}

pub struct Store {
    pub dir: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dir_from(xdg_data: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    Ok(resolve_dir(xdg_data, home, ".local/share")?
        .join("fastgraph")
        .join("measurements"))
}

pub(crate) fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("invalid measurement id {id:?}");
    }
    Ok(())
}

impl Store {
    #[cfg(test)]
    pub fn open(dir: PathBuf) -> Result<Store> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Store { dir })
    }

    /// `$XDG_DATA_HOME` (absolute only) or `~/.local/share`, plus `fastgraph/measurements`.
    pub fn default_dir() -> Result<PathBuf> {
        dir_from(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// `{unix_millis:x}-{pid:x}-{counter:x}`, so two instances never share one. Older ids
    /// lack the pid.
    pub fn new_id() -> String {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        format!(
            "{ms:x}-{:x}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn file(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{id}.{ext}"))
    }

    /// Writes the IR, then the text, then the metadata last (its presence
    /// marks a complete save). Only tests write the old store now.
    #[cfg(test)]
    pub fn save(&self, m: &Measurement, ir: Option<&crate::deconv::ImpulseResponse>) -> Result<()> {
        use crate::curve_io::{ExportHeader, write_rew};
        use crate::fsutil::write_atomic;
        use crate::response::Smoothing;
        use std::time::Duration;
        let id = &m.meta.id;
        check_id(id)?;
        let header = ExportHeader {
            name: m.meta.name.clone(),
            smoothing: Smoothing::None,
            compensation: m.meta.baked_compensation.clone(),
            created: UNIX_EPOCH + Duration::from_secs(m.meta.created),
            offset_db: 0.0,
        };
        let text = write_rew(&m.response, &header);
        // REW text cannot hold < 2 points or non-finite phase; reject rather than save a broken file.
        read_curve(&text).context("measurement cannot be stored as REW text")?;
        let meta = toml::to_string_pretty(&m.meta).context("serialising metadata")?;
        if let Some(ir) = ir {
            let wav = self.file(id, "wav");
            let bytes = crate::set::encode_ir_wav(ir)
                .with_context(|| format!("encoding {}", wav.display()))?;
            write_atomic(&wav, &bytes)?;
        }
        write_atomic(&self.file(id, "txt"), text.as_bytes())?;
        write_atomic(&self.file(id, "toml"), meta.as_bytes())
    }

    /// All stored measurements sorted by creation time, plus one message per unusable file set.
    pub fn load_all(&self) -> (Vec<Measurement>, Vec<String>) {
        let mut errors = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => return (Vec::new(), vec![format!("{}: {e}", self.dir.display())]),
        };
        let mut stems = BTreeSet::new();
        for entry in entries.flatten() {
            let p = entry.path();
            let ext = p.extension().and_then(OsStr::to_str);
            let stem = p.file_stem().and_then(OsStr::to_str);
            if let (Some("toml" | "txt"), Some(stem)) = (ext, stem) {
                stems.insert(stem.to_string());
            }
        }
        let mut out = Vec::new();
        for stem in stems {
            match self.load_one(&stem) {
                Ok(m) => out.push(m),
                Err(e) => errors.push(format!("{stem}: {e:#}")),
            }
        }
        out.sort_by(|a, b| {
            a.meta
                .created
                .cmp(&b.meta.created)
                .then_with(|| a.meta.id.cmp(&b.meta.id))
        });
        (out, errors)
    }

    fn load_one(&self, stem: &str) -> Result<Measurement> {
        let meta_text =
            std::fs::read_to_string(self.file(stem, "toml")).context("reading metadata")?;
        let meta: Meta = toml::from_str(&meta_text).context("parsing metadata")?;
        if meta.id != stem {
            bail!("metadata id {:?} does not match file name", meta.id);
        }
        let text = std::fs::read_to_string(self.file(stem, "txt")).context("reading response")?;
        let response = read_curve(&text).context("parsing response")?;
        Ok(Measurement { meta, response })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deconv::ImpulseResponse;
    use crate::response::Response;

    fn meta(id: &str, name: &str, created: u64) -> Meta {
        Meta {
            id: id.into(),
            name: name.into(),
            kind: Kind::Sweep,
            side: Some(Side::L),
            seating: Some(2),
            input: Some("in".into()),
            output: Some("out".into()),
            drift_ppm: Some(-3.5),
            clipped: true,
            created,
            source: Some("/x/y.txt".into()),
            sources: vec!["a".into(), "b".into()],
            compensable: false,
            baked_compensation: Some("mic.txt (inverted)".into()),
            shown: false,
            offset_db: 0.0,
            ir_peak: None,
            sweep_seconds: None,
        }
    }

    fn resp(nan_ends: bool, phase: bool) -> Response {
        let freq: Vec<f64> = (0..20).map(|i| 20.0 * 1.3_f64.powi(i)).collect();
        let mut db: Vec<f64> = (0..20)
            .map(|i| (i as f64 * 0.7).sin() * 5.0 - 3.0)
            .collect();
        if nan_ends {
            db[0] = f64::NAN;
            db[1] = f64::NAN;
            db[19] = f64::NAN;
        }
        let phase = phase.then(|| (0..20).map(|i| i as f64 * 3.0 - 90.0).collect());
        Response { freq, db, phase }
    }

    fn assert_close(orig: &Response, got: &Response) {
        for (i, f) in orig.freq.iter().enumerate() {
            if orig.db[i].is_nan() {
                continue;
            }
            // Text keeps 6 decimals of frequency: clamp so end points stay inside the range.
            let fq = f.clamp(got.freq[0], *got.freq.last().unwrap());
            assert!((got.interp_db(fq) - orig.db[i]).abs() <= 1e-4, "db at {f}");
            if let (Some(op), Some(gp)) = (&orig.phase, &got.phase) {
                let k = got.freq.iter().position(|x| (x - f).abs() < 1e-5).unwrap();
                assert!((gp[k] - op[i]).abs() <= 1e-3, "phase at {f}");
            }
        }
        assert_eq!(orig.phase.is_some(), got.phase.is_some());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn old_meta_still_loads() {
        let text = "id = \"1-0\"\nname = \"n\"\nkind = \"Sweep\"\nclipped = false\ncreated = 5\nsources = []\n";
        let m: Meta = toml::from_str(text).unwrap();
        assert_eq!(m.offset_db, 0.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("m")).unwrap();
        let ms = [
            Measurement {
                meta: meta("1-0", "one", 10),
                response: resp(false, false),
            },
            Measurement {
                meta: meta("2-0", "two", 20),
                response: resp(true, true),
            },
        ];
        // Saved out of order: load sorts by created.
        s.save(&ms[1], None).unwrap();
        s.save(&ms[0], None).unwrap();
        let (got, errs) = s.load_all();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(got.len(), 2);
        for (g, o) in got.iter().zip(&ms) {
            assert_eq!(g.meta, o.meta);
            assert_close(&o.response, &g.response);
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn save_writes_float_wav_when_ir_given() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let m = Measurement {
            meta: meta("3-0", "w", 1),
            response: resp(false, false),
        };
        let ir = ImpulseResponse {
            rate: 48_000,
            samples: vec![0.0, 0.5, -0.25],
            peak: 1,
        };
        s.save(&m, Some(&ir)).unwrap();
        let r = hound::WavReader::open(dir.path().join("3-0.wav")).unwrap();
        assert_eq!(r.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(r.spec().bits_per_sample, 32);
        assert_eq!(r.spec().sample_rate, 48_000);
        let v: Vec<f32> = r.into_samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(v, ir.samples);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn save_rejects_fewer_than_two_points() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let r = Response {
            freq: vec![100.0, 200.0, 300.0],
            db: vec![f64::NAN, 1.0, f64::NAN],
            phase: None,
        };
        let m = Measurement {
            meta: meta("4-0", "x", 1),
            response: r,
        };
        assert!(s.save(&m, None).is_err());
        assert!(s.load_all().0.is_empty());
        assert!(!dir.path().join("4-0.toml").exists());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn bad_file_is_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let m = Measurement {
            meta: meta("1-0", "ok", 1),
            response: resp(false, false),
        };
        s.save(&m, None).unwrap();
        std::fs::write(dir.path().join("bad.toml"), "garbage = [").unwrap();
        std::fs::write(dir.path().join("bad.txt"), "1 2\n3 4\n").unwrap();
        let (got, errs) = s.load_all();
        assert_eq!(got.len(), 1);
        assert_eq!(errs.len(), 1, "{errs:?}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn orphans_reported_and_tmp_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let m = Measurement {
            meta: meta("1-0", "ok", 1),
            response: resp(false, false),
        };
        s.save(&m, None).unwrap();
        std::fs::copy(dir.path().join("1-0.toml"), dir.path().join("2-0.toml")).unwrap();
        std::fs::write(dir.path().join("3-0.txt"), "1 2\n3 4\n").unwrap();
        std::fs::write(dir.path().join("4-0.toml.tmp"), "junk").unwrap();
        std::fs::write(dir.path().join("4-0.txt.tmp"), "junk").unwrap();
        std::fs::write(dir.path().join(".tmpAb12Cd"), "junk").unwrap();
        let (got, errs) = s.load_all();
        assert_eq!(got.len(), 1);
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn old_metadata_without_the_new_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let m = Measurement {
            meta: meta("5-0", "old", 1),
            response: resp(false, false),
        };
        s.save(&m, None).unwrap();
        // The metadata as written before these fields existed.
        let old = "id = \"5-0\"\nname = \"old\"\nkind = \"Average\"\nclipped = false\n\
                   created = 1\nsources = [\"1-0\", \"2-0\"]\n";
        std::fs::write(dir.path().join("5-0.toml"), old).unwrap();
        let (got, errs) = s.load_all();
        assert!(errs.is_empty(), "{errs:?}");
        let meta = &got[0].meta;
        assert_eq!(meta.kind, Kind::Average);
        assert!(!meta.compensable);
        assert_eq!(meta.baked_compensation, None);
        assert!(meta.shown);
        assert!(!meta.takes_compensation());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn ids_carry_the_process_and_old_ids_still_load() {
        let id = Store::new_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(
            u32::from_str_radix(parts[1], 16).unwrap(),
            std::process::id()
        );
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        for id in ["18c2f3a1b20-0", id.as_str()] {
            let m = Measurement {
                meta: meta(id, "x", 1),
                response: resp(false, false),
            };
            s.save(&m, None).unwrap();
        }
        let (got, errs) = s.load_all();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(got.len(), 2);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_stored_text_names_baked_compensation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().to_path_buf()).unwrap();
        let mut m = Measurement {
            meta: meta("7-0", "avg", 1),
            response: resp(false, false),
        };
        s.save(&m, None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("7-0.txt")).unwrap();
        assert!(
            text.contains("* Compensation: mic.txt (inverted)\n"),
            "{text}"
        );
        m.meta.baked_compensation = None;
        s.save(&m, None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("7-0.txt")).unwrap();
        assert!(text.contains("* Compensation: none\n"), "{text}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn ids_are_unique_and_safe() {
        let a = Store::new_id();
        let b = Store::new_id();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(check_id("../evil").is_err());
        assert!(check_id("a/b").is_err());
        assert!(check_id("").is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn data_dir_resolution() {
        use std::ffi::OsStr;
        let home = OsStr::new("/home/u");
        assert_eq!(
            dir_from(Some(OsStr::new("/data")), Some(home)).unwrap(),
            PathBuf::from("/data/fastgraph/measurements")
        );
        for bad in [None, Some(OsStr::new("")), Some(OsStr::new("rel"))] {
            assert_eq!(
                dir_from(bad, Some(home)).unwrap(),
                PathBuf::from("/home/u/.local/share/fastgraph/measurements")
            );
        }
        assert!(dir_from(None, None).is_err());
    }
}

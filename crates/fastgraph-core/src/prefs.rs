//! User preferences, persisted as TOML.

use crate::fsutil::{resolve_dir, write_atomic};
use crate::response::Smoothing;
use crate::signal::SweepLength;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Device fields hold `node.name`; channel fields hold position labels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub input: Option<String>,
    pub input_channel: Option<String>,
    pub output: Option<String>,
    pub output_channels: Vec<String>,
    pub level_db: f64,
    pub length: SweepLength,
    pub smoothing: Smoothing,
    pub compensation: Option<PathBuf>,
    pub comp_enabled: bool,
    pub comp_invert: bool,
    pub align: bool,
    pub export_rate: u32,
    /// `None` means [`default_documents_dir`].
    pub sets_dir: Option<PathBuf>,
    pub ask_save: bool,
    /// `None` means [`default_documents_dir`].
    pub exports_dir: Option<PathBuf>,
    pub ask_export: bool,
    /// `#rrggbb`; `None` is the theme's.
    pub accent: Option<String>,
    /// Eight `#rrggbb` colours; `None` is the theme's.
    pub trace_colors: Option<Vec<String>>,
    pub align_hz: f64,
    /// Width of the alignment band; 0 aligns at the single point.
    pub align_octaves: f64,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            input: None,
            input_channel: None,
            output: None,
            output_channels: Vec::new(),
            level_db: -12.0,
            length: SweepLength::default(),
            smoothing: Smoothing::default(),
            compensation: None,
            comp_enabled: false,
            comp_invert: false,
            align: true,
            export_rate: 48_000,
            sets_dir: None,
            ask_save: false,
            exports_dir: None,
            ask_export: false,
            accent: None,
            trace_colors: None,
            align_hz: 1000.0,
            align_octaves: 0.0,
        }
    }
}

/// The rates a stimulus WAV can be exported at.
pub const EXPORT_RATES: [u32; 3] = [44_100, 48_000, 96_000];

/// The export rate nearest to `rate`; 48 kHz on a tie.
pub fn nearest_export_rate(rate: u32) -> u32 {
    let distance = |r: u32| r.abs_diff(rate);
    EXPORT_RATES
        .into_iter()
        .min_by_key(|&r| (distance(r), r != 48_000))
        .unwrap_or(48_000)
}

/// The alignment band widths, in octaves; 0 is a single point.
pub const ALIGN_WIDTHS: [f64; 6] = [0.0, 1.0 / 3.0, 0.5, 1.0, 2.0, 3.0];

/// A positive finite frequency such as `1000`, `1k` or `2.5k`.
pub fn parse_hz(text: &str) -> Option<f64> {
    let t = text.trim();
    let (number, scale) = match t.strip_suffix(['k', 'K']) {
        Some(n) => (n, 1000.0),
        None => (t, 1.0),
    };
    let hz = number.trim().parse::<f64>().ok()? * scale;
    (hz.is_finite() && hz > 0.0).then_some(hz)
}

/// A frequency as the Align box shows it: `500`, `1k`, `2.5k`. It reads back
/// as the same number: the `k` form is used only when it does.
pub fn format_hz(hz: f64) -> String {
    let khz = hz / 1000.0;
    if hz >= 1000.0 && khz * 1000.0 == hz {
        format!("{khz}k")
    } else {
        hz.to_string()
    }
}

pub(crate) fn documents_dir_from(home: Option<&OsStr>) -> Result<PathBuf> {
    let home = match home {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => anyhow::bail!("HOME is not set"),
    };
    let documents = home.join("Documents");
    Ok(if documents.is_dir() {
        documents.join("fastgraph")
    } else {
        home.join("fastgraph")
    })
}

/// `~/Documents/fastgraph` if `~/Documents` exists, else `~/fastgraph`. Not created.
pub fn default_documents_dir() -> Result<PathBuf> {
    documents_dir_from(std::env::var_os("HOME").as_deref())
}

pub(crate) fn path_from(xdg_config: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    Ok(resolve_dir(xdg_config, home, ".config")?
        .join("fastgraph")
        .join("preferences.toml"))
}

impl Prefs {
    /// `$XDG_CONFIG_HOME/fastgraph/preferences.toml` (absolute values only), else `~/.config/...`.
    pub fn path() -> Result<PathBuf> {
        path_from(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Missing file gives defaults; an unparsable file is an error. An
    /// unsupported export rate becomes the nearest supported one, and a
    /// level outside the slider's −40..=0 dBFS the default.
    pub fn load(path: &Path) -> Result<Prefs> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Prefs::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut prefs: Prefs =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        prefs.export_rate = nearest_export_rate(prefs.export_rate);
        // Also rejects NaN and infinities.
        if !(-40.0..=0.0).contains(&prefs.level_db) {
            prefs.level_db = Prefs::default().level_db;
        }
        Ok(prefs)
    }

    /// The export rate to use: one of [`EXPORT_RATES`].
    pub fn checked_export_rate(&self) -> u32 {
        nearest_export_rate(self.export_rate)
    }

    /// Where sets are saved.
    pub fn sets_dir(&self) -> Result<PathBuf> {
        match &self.sets_dir {
            Some(d) => Ok(d.clone()),
            None => default_documents_dir(),
        }
    }

    /// Where exports are written.
    pub fn exports_dir(&self) -> Result<PathBuf> {
        match &self.exports_dir {
            Some(d) => Ok(d.clone()),
            None => default_documents_dir(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising preferences")?;
        write_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn prefs_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = Prefs::load(&dir.path().join("missing.toml")).unwrap();
        assert_eq!(p.level_db, -12.0);
        assert!(p.align);
        assert_eq!(p.export_rate, 48_000);
        assert_eq!(p, Prefs::default());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn prefs_missing_fields_use_real_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.toml");
        std::fs::write(&f, "comp_invert = true\n").unwrap();
        let p = Prefs::load(&f).unwrap();
        assert!(p.comp_invert);
        assert_eq!(p.level_db, -12.0);
        assert!(p.align);
        assert_eq!(p.export_rate, 48_000);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn export_rate_is_clamped_to_supported_rates() {
        for (given, want) in [
            (0, 44_100),
            (8_000, 44_100),
            (44_100, 44_100),
            (46_050, 48_000),
            (47_999, 48_000),
            (48_000, 48_000),
            (72_000, 48_000),
            (88_200, 96_000),
            (192_000, 96_000),
            (u32::MAX, 96_000),
        ] {
            assert_eq!(nearest_export_rate(given), want, "{given}");
        }
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.toml");
        std::fs::write(&f, "export_rate = 12345\n").unwrap();
        assert_eq!(Prefs::load(&f).unwrap().export_rate, 44_100);
        let p = Prefs {
            export_rate: 1,
            ..Prefs::default()
        };
        assert_eq!(p.checked_export_rate(), 44_100);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_level_outside_the_slider_becomes_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.toml");
        for (given, want) in [
            ("6.0", -12.0),
            ("-41.0", -12.0),
            ("nan", -12.0),
            ("inf", -12.0),
            ("-inf", -12.0),
            ("0.0", 0.0),
            ("-40.0", -40.0),
            ("-20.5", -20.5),
        ] {
            std::fs::write(&f, format!("level_db = {given}\n")).unwrap();
            assert_eq!(Prefs::load(&f).unwrap().level_db, want, "{given}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn prefs_unparsable_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.toml");
        std::fs::write(&f, "this is = not [ toml").unwrap();
        assert!(Prefs::load(&f).is_err());
        // Never overwritten by load.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "this is = not [ toml");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn prefs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/p.toml");
        let p = Prefs {
            input: Some("alsa_input.usb".into()),
            input_channel: Some("FL".into()),
            output: Some("bluez_output.x".into()),
            output_channels: vec!["FL".into(), "FR".into()],
            level_db: -20.5,
            length: SweepLength::Long,
            smoothing: Smoothing::None,
            compensation: Some("/tmp/mic.txt".into()),
            comp_enabled: true,
            comp_invert: true,
            align: false,
            export_rate: 44_100,
            sets_dir: Some("/s".into()),
            ask_save: true,
            exports_dir: Some("/e".into()),
            ask_export: true,
            accent: Some("#112233".into()),
            trace_colors: Some(vec!["#000000".into(); 8]),
            align_hz: 2500.0,
            align_octaves: 0.5,
        };
        p.save(&f).unwrap();
        assert_eq!(Prefs::load(&f).unwrap(), p);
        assert_eq!(std::fs::read_dir(f.parent().unwrap()).unwrap().count(), 1);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn old_prefs_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.toml");
        std::fs::write(
            &f,
            "level_db = -20.0\nalign = false\nexport_rate = 44100\nsmoothing = \"Oct12\"\n",
        )
        .unwrap();
        let p = Prefs::load(&f).unwrap();
        assert_eq!(p.align_hz, 1000.0);
        assert_eq!(p.align_octaves, 0.0);
        assert!(!p.ask_save);
        assert!(!p.ask_export);
        assert_eq!(p.sets_dir, None);
        assert_eq!(p.exports_dir, None);
        assert_eq!(p.accent, None);
        assert_eq!(p.trace_colors, None);
        assert_eq!(p.level_db, -20.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn parse_hz_accepts_k_suffix() {
        assert_eq!(parse_hz("1000"), Some(1000.0));
        assert_eq!(parse_hz("1k"), Some(1000.0));
        assert_eq!(parse_hz("2.5k"), Some(2500.0));
        assert_eq!(parse_hz(" 500 "), Some(500.0));
        for bad in ["", "abc", "0", "-5", "k", "nan", "inf", "1kk"] {
            assert_eq!(parse_hz(bad), None, "{bad:?}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn documents_dir_prefers_existing_documents() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().as_os_str();
        assert_eq!(
            documents_dir_from(Some(h)).unwrap(),
            home.path().join("fastgraph")
        );
        std::fs::create_dir(home.path().join("Documents")).unwrap();
        assert_eq!(
            documents_dir_from(Some(h)).unwrap(),
            home.path().join("Documents/fastgraph")
        );
        // Never created.
        assert!(!home.path().join("Documents/fastgraph").exists());
        assert!(documents_dir_from(None).is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn explicit_dirs_override_the_default() {
        let p = Prefs {
            sets_dir: Some("/x/sets".into()),
            exports_dir: Some("/x/out".into()),
            ..Prefs::default()
        };
        assert_eq!(p.sets_dir().unwrap(), PathBuf::from("/x/sets"));
        assert_eq!(p.exports_dir().unwrap(), PathBuf::from("/x/out"));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn config_path_resolution() {
        let abs = OsStr::new("/cfg");
        let home = OsStr::new("/home/u");
        assert_eq!(
            path_from(Some(abs), Some(home)).unwrap(),
            PathBuf::from("/cfg/fastgraph/preferences.toml")
        );
        for bad in [None, Some(OsStr::new("")), Some(OsStr::new("rel/dir"))] {
            assert_eq!(
                path_from(bad, Some(home)).unwrap(),
                PathBuf::from("/home/u/.config/fastgraph/preferences.toml")
            );
        }
        assert!(path_from(None, None).is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn format_hz_round_trips_through_parse_hz() {
        assert_eq!(format_hz(1000.0), "1k");
        assert_eq!(format_hz(2500.0), "2.5k");
        assert_eq!(format_hz(500.0), "500");
        assert_eq!(format_hz(12_345.0), "12.345k");
        for hz in [20.0, 500.0, 1000.0, 2500.0, 12_345.0] {
            assert_eq!(parse_hz(&format_hz(hz)), Some(hz));
        }
    }

    // Every frequency `parse_hz` accepts comes back unchanged, tiny, huge or
    // with more digits than the old six decimals.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn format_hz_keeps_every_valid_frequency() {
        for hz in [
            1e-7,
            0.1 + 0.2,
            1_234.567_890_1,
            1e300,
            f64::MAX,
            999.999_999_9,
        ] {
            assert_eq!(parse_hz(&format_hz(hz)), Some(hz), "{}", format_hz(hz));
        }
    }
}

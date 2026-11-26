//! REW-compatible text import/export of frequency responses.

use crate::response::{Response, Smoothing};
use anyhow::bail;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ExportHeader {
    pub name: String,
    pub smoothing: Smoothing,
    pub compensation: Option<String>,
    pub created: SystemTime,
    /// A constant shift applied to the levels, in dB; written only when nonzero.
    pub offset_db: f64,
}

pub fn read_curve(text: &str) -> anyhow::Result<Response> {
    let mut freq = Vec::new();
    let mut db = Vec::new();
    let mut phase = Vec::new();
    let mut all_phase = true;
    // A byte-order mark would make the first row look like a header.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('*') {
            continue;
        }
        // Fields are separated by commas or whitespace; trailing delimiters are
        // ignored, but an empty field between two commas is invalid.
        let line = line.trim_end_matches(|c: char| c == ',' || c.is_whitespace());
        let mut fields = Vec::new();
        for part in line.split(',') {
            let part = part.trim();
            if part.is_empty() {
                fields.push(part);
            } else {
                fields.extend(part.split_whitespace());
            }
        }
        let cols: Result<Vec<f64>, _> = fields.iter().map(|t| t.parse::<f64>()).collect();
        let cols = match cols {
            Ok(cols) => cols,
            // Non-numeric lines before the data are headers.
            Err(_) if freq.is_empty() => continue,
            Err(_) => bail!("line {}: expected numbers, found {line:?}", n + 1),
        };
        if !(2..=3).contains(&cols.len()) {
            bail!(
                "line {}: expected 2 or 3 columns, found {}",
                n + 1,
                cols.len()
            );
        }
        if cols.iter().any(|v| !v.is_finite()) {
            bail!("line {}: non-finite value", n + 1);
        }
        if cols[0] <= 0.0 {
            bail!("line {}: frequency must be positive", n + 1);
        }
        if freq.last().is_some_and(|prev| cols[0] <= *prev) {
            bail!("line {}: frequencies must be strictly increasing", n + 1);
        }
        freq.push(cols[0]);
        db.push(cols[1]);
        match cols.get(2) {
            Some(p) => phase.push(*p),
            None => all_phase = false,
        }
    }
    if freq.len() < 2 {
        bail!("no data: need at least two numeric rows");
    }
    Ok(Response {
        freq,
        db,
        phase: all_phase.then_some(phase),
    })
}

/// REW-style text: `*` comment header, then `freq dB [phase]` rows. `NaN` rows are skipped.
pub fn write_rew(resp: &Response, header: &ExportHeader) -> String {
    let phase = resp.phase.as_ref().filter(|p| p.len() == resp.freq.len());
    let one_line = |s: &str| s.replace(['\r', '\n'], " ");
    let mut out = String::new();
    out.push_str("* fastgraph export\n");
    out.push_str(&format!("* Measurement: {}\n", one_line(&header.name)));
    out.push_str(&format!("* Smoothing: {}\n", header.smoothing.label()));
    out.push_str(&format!(
        "* Compensation: {}\n",
        header
            .compensation
            .as_deref()
            .map_or_else(|| "none".to_string(), one_line)
    ));
    if header.offset_db != 0.0 {
        out.push_str(&format!("* Offset: {:+.1} dB\n", header.offset_db));
    }
    out.push_str(&format!("* Dated: {}\n", local_time_string(header.created)));
    out.push_str("*\n");
    out.push_str(if phase.is_some() {
        "* Freq(Hz) SPL(dB) Phase(degrees)\n"
    } else {
        "* Freq(Hz) SPL(dB)\n"
    });
    for (i, (f, d)) in resp.freq.iter().zip(&resp.db).enumerate() {
        if d.is_nan() {
            continue;
        }
        match phase {
            // The shortest text that reads back as `f`: close frequencies stay distinct.
            Some(p) => out.push_str(&format!("{f} {d:.4} {:.4}\n", p[i])),
            None => out.push_str(&format!("{f} {d:.4}\n")),
        }
    }
    out
}

/// `t` in the local time zone, as the locale's date and time of day (`%x %X`;
/// under en_US, `11/14/2023 10:13:20 PM`). The locale comes from `LC_TIME`
/// once the program has called `setlocale(LC_TIME, "")`; before that it is
/// the "C" locale.
pub fn local_time_string(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let format: &[u8] = b"%x %X\0";
    let time = secs as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in, and `buf` is
    // large enough for `strftime`, which reports how many bytes it wrote.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return "unknown time".to_owned();
        }
        let mut buf = [0u8; 128];
        let n = libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            format.as_ptr().cast(),
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC`.
pub fn utc_string(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn parses_all_three_formats() {
        let c = read_curve(&fixture("coupler.txt")).unwrap();
        assert!(c.phase.is_none());
        assert_eq!(c.freq[0], 10.0);
        assert_eq!(c.freq.len(), c.db.len());
        assert!((c.db[0] - -0.3587).abs() < 1e-9);

        let h = read_curve(&fixture("rew_header.txt")).unwrap();
        assert!(h.phase.is_none());
        assert_eq!(h.freq[0], 10.0);
        assert!((h.db[0] - -1.5).abs() < 1e-9);

        let p = read_curve(&fixture("rew_3col.txt")).unwrap();
        assert_eq!(p.freq[0], 20.0);
        assert!((p.db[0] - 71.0).abs() < 1e-9);
        let ph = p.phase.as_ref().unwrap();
        assert_eq!(ph.len(), p.freq.len());
        assert!((ph[0] - -180.5).abs() < 1e-9);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn rejects_malformed_curves() {
        for bad in [
            "",
            "100\n200\n300\n",
            "100 1.0\n200 NaN\n",
            "100 1.0\n50 2.0\n",
            "100 1.0\n100 2.0\n",
            "0 1.0\n100 2.0\n",
            "-5 1.0\n100 2.0\n",
            "Freq dB\n",
        ] {
            assert!(read_curve(bad).is_err(), "{bad:?}");
        }
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn a_bad_row_after_the_data_starts_is_an_error() {
        let header = "Freq(Hz), SPL(dB), Phase\nmeasured today\n";
        let ok = read_curve(&format!("{header}100, 1, 10\n200, 2, 20\n")).unwrap();
        assert_eq!(ok.freq, vec![100.0, 200.0]);
        for (rows, line) in [
            ("100, 1\n1000,,-15\n2000, -14\n", 4),
            ("100 1\n200 1.0x\n300 2\n", 4),
            ("100 1\nFreq dB\n300 2\n", 4),
            ("100, 1\n200,,2\n", 4),
        ] {
            let err = read_curve(&format!("{header}{rows}")).unwrap_err();
            assert!(
                err.to_string().starts_with(&format!("line {line}:")),
                "{rows:?}: {err}"
            );
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn trailing_delimiters_are_tolerated_but_interior_gaps_are_not() {
        let r = read_curve("100,-15,\n1000\t-14\t\n2000, -13, 5 ,\n").unwrap();
        assert_eq!(r.freq, vec![100.0, 1000.0, 2000.0]);
        assert_eq!(r.db, vec![-15.0, -14.0, -13.0]);
        assert!(r.phase.is_none());
        let err = read_curve("100,-15,\n1000,,-15\n").unwrap_err();
        assert!(err.to_string().starts_with("line 2:"), "{err}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn mixed_columns_drop_phase() {
        let r = read_curve("100 1 10\n200 2\n300,3,30\n").unwrap();
        assert!(r.phase.is_none());
        assert_eq!(r.freq, vec![100.0, 200.0, 300.0]);
    }

    fn header() -> ExportHeader {
        ExportHeader {
            name: "Left cup".into(),
            smoothing: Smoothing::Oct12,
            compensation: Some("Coupler".into()),
            created: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            offset_db: 0.0,
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn round_trip() {
        for fx in ["rew_header.txt", "rew_3col.txt"] {
            let r = read_curve(&fixture(fx)).unwrap();
            let text = write_rew(&r, &header());
            assert!(text.starts_with("* fastgraph export\n"));
            assert!(text.contains("* Smoothing: 1/12"));
            assert!(text.contains("* Compensation: Coupler"));
            let dated = format!("* Dated: {}\n", local_time_string(header().created));
            assert!(text.contains(&dated));
            let back = read_curve(&text).unwrap();
            assert_eq!(back.freq.len(), r.freq.len());
            for i in 0..r.freq.len() {
                assert!((back.freq[i] / r.freq[i] - 1.0).abs() < 1e-6);
                assert!((back.db[i] - r.db[i]).abs() < 1e-4);
            }
            assert_eq!(back.phase.is_some(), r.phase.is_some());
            if let (Some(a), Some(b)) = (&back.phase, &r.phase) {
                assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4));
            }
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn leading_bom_keeps_first_row() {
        let r = read_curve("\u{feff}20 1.5\n40,2.5\n80 3.5\n").unwrap();
        assert_eq!(r.freq, vec![20.0, 40.0, 80.0]);
        assert_eq!(r.db, vec![1.5, 2.5, 3.5]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn write_skips_nan_rows() {
        let r = Response {
            freq: vec![100.0, 200.0, 300.0],
            db: vec![1.0, f64::NAN, 3.0],
            phase: None,
        };
        let mut h = header();
        h.compensation = None;
        let text = write_rew(&r, &h);
        assert!(text.contains("* Compensation: none"));
        assert_eq!(read_curve(&text).unwrap().freq, vec![100.0, 300.0]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn utc_format() {
        assert_eq!(
            utc_string(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14 22:13:20 UTC"
        );
        assert_eq!(
            utc_string(UNIX_EPOCH + Duration::from_secs(951_782_400 + 86_399)),
            "2000-02-29 23:59:59 UTC"
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn local_time_follows_tz() {
        // TZ is process-wide, so each zone is checked in its own process.
        if let Ok(zone) = std::env::var("FASTGRAPH_TZ_CHILD") {
            let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            // The "C" locale's `%x %X`; the zone shifts the hour.
            let expected = match zone.as_str() {
                "UTC0" => "11/14/23 22:13:20",
                _ => "11/14/23 17:13:20",
            };
            assert_eq!(local_time_string(t), expected);
            return;
        }
        for zone in ["UTC0", "EST5"] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "curve_io::tests::local_time_follows_tz"])
                .env("FASTGRAPH_TZ_CHILD", zone)
                .env("TZ", zone)
                .env_remove("LC_ALL")
                .env_remove("LC_TIME")
                .output()
                .unwrap();
            assert!(out.status.success(), "{zone}: {out:?}");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains("1 passed"), "{zone}: child did not run");
        }
    }
}

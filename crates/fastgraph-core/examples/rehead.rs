//! Copies a WAV file with a different declared sample rate and identical samples, to simulate
//! a player whose clock is off: `cargo run -p fastgraph-core --example rehead -- IN OUT RATE`.
//! A 48 kHz sweep declared at 48 014 Hz plays 292 ppm fast (fastgraph reports about −292 ppm).

use anyhow::{Context, bail};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, rate] = args.as_slice() else {
        bail!("usage: rehead IN.wav OUT.wav RATE");
    };
    let rate: u32 = rate.parse().context("RATE must be a whole number of Hz")?;
    let mut reader = hound::WavReader::open(input).with_context(|| format!("opening {input}"))?;
    let spec = hound::WavSpec {
        sample_rate: rate,
        ..reader.spec()
    };
    let mut writer =
        hound::WavWriter::create(output, spec).with_context(|| format!("creating {output}"))?;
    match spec.sample_format {
        hound::SampleFormat::Int => {
            for s in reader.samples::<i32>() {
                writer.write_sample(s?)?;
            }
        }
        hound::SampleFormat::Float => {
            for s in reader.samples::<f32>() {
                writer.write_sample(s?)?;
            }
        }
    }
    writer.finalize()?;
    println!(
        "{output}: {} Hz (was {} Hz)",
        rate,
        reader.spec().sample_rate
    );
    Ok(())
}

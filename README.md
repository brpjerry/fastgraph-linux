# fastgraph

A small, fast headphone frequency-response measurement tool for Linux, using PipeWire and a
Wayland-native window.

## Features

- Log-sweep measurement played from this computer, or from any other device (a phone, a
  Bluetooth source, another computer) while fastgraph listens
- RTA mode with pink noise, for when a sweep can't be used
- Response, Phase and Distortion (H2–H9, THD, noise floor) views
- Microphone or coupler compensation curve, smoothing, alignment and per-trace offsets
- Averages of the shown traces
- REW-compatible text import and export
- Measurement sets saved as `.fgset` files, with crash recovery for unsaved sessions

## Requirements

- PipeWire, with a session manager such as WirePlumber
- A Wayland compositor (there is no X11 support)
- xdg-desktop-portal with a FileChooser backend (such as `xdg-desktop-portal-gtk`, `-kde` or
  `-gnome`) for the file dialogs

## Install (Arch)

```sh
git clone https://github.com/brpjerry/fastgraph-linux.git && cd fastgraph-linux/packaging/arch && makepkg -si
```

This builds and installs the `fastgraph-git` package, with a desktop entry and icon.

Each tagged release also has a prebuilt package on the
[Releases](https://github.com/brpjerry/fastgraph-linux/releases) page, built by GitHub Actions.
Install it with `sudo pacman -U fastgraph-git-*.pkg.tar.zst`.

## Build from source

You need a current stable Rust toolchain (edition 2024), `clang` (libclang), `pkgconf`
and the libpipewire headers (`libpipewire` on Arch, `libpipewire-0.3-dev` on Debian and Ubuntu).

```sh
cargo build --release -p fastgraph
```

The binary is `target/release/fastgraph`. A plain `cargo build --release` builds only the
core library, since it is the workspace's default member.

## Run tests

```sh
cargo test --workspace
```

The GUI tests render headless. Set `ICED_TEST_BACKEND=tiny-skia` to render them on the CPU if
you have no GPU.

Optional live tests play into a temporary PipeWire null sink, `fastgraph_test`, and nothing
reaches a real device. They need `pactl` (from `pipewire-pulse`) and `pw-play`:

```sh
cargo test -p fastgraph-core --test pipewire_loop -- --ignored --nocapture
cargo test -p fastgraph-core --test loopback_e2e -- --ignored --nocapture --test-threads 1
```

To check the app itself without hardware, run `scripts/loopback-check.sh` with one or more
exported sweep WAVs. It creates the same null sink and waits until you pick **Monitor of
fastgraph_test** as the input and press **Listen**. Then it plays each file into the sink after
a random 2–6 s delay, and removes the sink when it exits.

```sh
scripts/loopback-check.sh fastgraph-sweep-normal-48k.wav
```

## Usage

Start `fastgraph` from the application menu or a terminal. The left pane has three tabs:
**Measure**, **Sets** and **Settings**.

### Set up the devices

1. Under **Input**, pick the measurement microphone and its **Channel**. Monitors of the
   outputs are listed too. When an audio interface's profile hides some inputs, its raw node
   appears as "(all channels)" with every hardware channel.
2. Under **Output**, pick the headphones.
3. Pick the ear you are measuring with **Side** (**L** or **R**). Side also picks the output
   channel that plays: Left plays on FL and Right on FR, or the mono channel, or the first or
   second channel. The caption under the output shows the channel in use.
4. Check the input meter. It turns red when the input clips, and a clipped measurement is
   marked "input clipped" in the list.

### Measure with a local sweep

1. Leave **Mode** on **Sweep**.
2. Set **Level**, the sweep's peak in dBFS (−12 dB by default).
3. Pick a **Length**: Short (3 s), Normal (6 s, the default) or Long (11 s).
4. Press **Measure**. The sweep plays on the chosen output and the result appears in the list,
   named for the side it played on. Press **Stop** to stop.

### Measure with a sweep played elsewhere

1. Press **Export sweep WAV…** and save the file. It is written at 48 kHz. To use 44.1 or
   96 kHz instead, set `export_rate = 44100` or `96000` in the preferences file.
2. Copy the file to the device that will play it.
3. In fastgraph, pick the **Length** the file was exported with, and press **Listen**.
4. Play the file whenever you like. Each sweep heard becomes a measurement, labelled with
   the Side shown at the time. Press **Stop listening** when you are done.

The sweep can start at any time, and latency doesn't matter. Clock drift between the player
and the microphone is measured and corrected. A file of another Length is not detected.

### RTA mode

Switch **Mode** to **RTA**.

- Press **Start noise** to loop pink noise on the output. The spectrum is averaged once the
  noise plays.
- Or press **Export noise WAV…**, play the file on another device, and press **Listen**.

The average count goes up as the trace settles. Press **Capture** to keep the average as a
measurement, or **Reset** to start it again. An RTA's absolute level is arbitrary, so compare
RTA traces with **Align** on.

### Process the traces

These controls sit above the graph:

- **Smoothing**: None, or 1/48 to 1/3 octave (1/24 by default). It applies to every view.
- **Comp**: loads a compensation curve, a REW text file of frequency and dB. With **Apply**
  on, the curve is subtracted from sweep and RTA measurements and from averages of them.
  **Invert** adds it instead. Imported curves are never compensated.
- **Align**: shifts the traces so they match at the frequency in the box (`1000`, `1k`,
  `2.5k`; 1 kHz by default). The width picker sets the band they are matched over, from
  **Point** to 3 octaves. Align is on by default and does not change exports.
- **Offsets**: each row in the list has a dB box with **−** and **+** buttons (0.5 dB steps).
  Type a value and press Enter to apply it.

Comp, Align and the offsets apply only to the Response view.

### Averages, import and export

- **Average shown** averages the ticked traces (two or more), aligned first when Align is on.
- **Import…** adds REW text exports as traces.
- **Export…** on a row writes that trace as REW text: compensated, smoothed and offset, but not
  aligned. It is written to `<exports folder>/<set> - <trace>.txt` (`<trace>.txt` when the
  session has no set name), or a save dialog opens if "Ask where to export" is on.
- Untick a row to hide its trace. **Rename** edits a row's name (Enter keeps it, Esc cancels).
  **Delete** removes the row.

### Views

The tabs over the graph switch between three views:

- **Response**: the level of every shown trace.
- **Phase**: the phase of the shown traces that have phase. It is unwrapped when the traces fit,
  and wrapped to ±180° when they don't.
- **Distortion**: H2–H9, THD and the noise floor of one measurement. Click a row's name to
  select it (click again to deselect); without a selection, the first shown measurement with
  data is used. Distortion needs a sweep measured with this version of fastgraph. **Units**
  switches between **%** of the fundamental and absolute **dB**.

Hover over the graph to read values at the pointer. The Response view lists every trace's level
at that frequency, loudest first. The Phase and Distortion views show the nearest trace or curve.

### Clear

**Clear**, under the list, empties the session and returns the app to its first-opened state.
Your settings are kept. Unsaved work stays available under **Unsaved sessions**.

### Sets

- To save the session, type a name in the **Set name** box on the Measure tab and press
  **Save**. This writes `<sets folder>/<name>.fgset`, replacing a set of the same name, or opens
  a save dialog if "Ask where to save" is on. The label reads "Set name · unsaved" while there
  are unsaved changes.
- The **Sets** tab lists the sets in the sets folder, newest first. Click a set to open it in
  place of the session, or press **Import** to add its traces to the session.
- **Unsaved sessions** lists the sessions earlier runs left unsaved. You can open or import
  them the same way.

### Settings

- **Sets folder** and **Exports folder**: type a path and press Enter, or use **Browse…**.
  Leave a folder empty to use the default (see Files). `~/` is accepted; other relative paths
  are not.
- **Ask where to save** and **Ask where to export**: show a file dialog for Save and Export.
- **Accent colour** and **Trace colours**: `#rrggbb` values, each with a **Reset** button.

### Keyboard

- **Esc** cancels a rename.

## Files

| What | Where |
| --- | --- |
| Preferences | `$XDG_CONFIG_HOME/fastgraph/preferences.toml` (default `~/.config/fastgraph/preferences.toml`) |
| Recovery files | `$XDG_DATA_HOME/fastgraph/recovery/` (default `~/.local/share/fastgraph/recovery/`) |
| Sets and exports | `~/Documents/fastgraph` if `~/Documents` exists, else `~/fastgraph`, unless changed in Settings |

Each run keeps its session in a recovery file, rewritten after every change and removed once
the session is saved or empty. The five newest recovery files are kept, plus any from the last
14 days.

## License

GPL-3.0-or-later; see `LICENSE`. Parts of the theme and plot code are adapted from fastpeq-gui
(WTFPL). Bundled third-party license notices are in `packaging/arch/licenses`.
